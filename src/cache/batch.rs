//! A set of cache writes that is committed as one atomic MULTI/EXEC pipeline.
//!
//! Before, every update was several independent round trips (MSET, one SADD
//! per index set, HSET for expiries...). A connection error in between left
//! keys that no index referenced, so they could never be cleaned up. A batch
//! is planned in memory first, so it is also cheap to unit test.

use super::keys::plan;
use crate::{constants::EXPIRY_ZSET, models::ApiResult};

use redis::aio::ConnectionLike;
use serde::Serialize;
use std::collections::BTreeMap;

/// Upper bound for keys / members per variadic command. Redis <= 6.2 closes the
/// connection when a command has more than 1M arguments.
pub const CHUNK: usize = 1000;

#[derive(Debug, Default)]
pub struct Batch {
    deletes: Vec<String>,
    unindex: BTreeMap<String, Vec<String>>,
    unexpire: Vec<String>,
    writes: Vec<(String, String)>,
    index: BTreeMap<String, Vec<String>>,
    expiries: Vec<(String, i64)>,
}

impl Batch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.deletes.is_empty()
            && self.unindex.is_empty()
            && self.unexpire.is_empty()
            && self.writes.is_empty()
            && self.index.is_empty()
            && self.expiries.is_empty()
    }

    /// Store `value` as JSON under a logical key and index it.
    pub fn set(&mut self, key: &str, value: &impl Serialize) -> ApiResult<()> {
        let json = serde_json::to_string(value)?;
        self.set_json(key, json);
        Ok(())
    }

    /// Store pre-serialized JSON under a logical key and index it.
    pub fn set_json(&mut self, key: &str, json: String) {
        let plan = plan(key);
        for (set, member) in plan.indexes {
            self.index.entry(set).or_default().push(member);
        }
        self.writes.push((plan.stored, json));
    }

    /// Delete a logical key, its index entries and its expiry.
    pub fn del(&mut self, key: &str) {
        let plan = plan(key);
        for (set, member) in plan.indexes {
            self.unindex.entry(set).or_default().push(member);
        }
        self.unexpire.push(plan.stored.clone());
        self.deletes.push(plan.stored);
    }

    /// Delete a key verbatim, without index planning (used for index sets).
    pub fn del_raw(&mut self, key: impl Into<String>) {
        self.deletes.push(key.into());
    }

    /// Expire a logical key `ttl_ms` after `now_ms`. A TTL of 0 means "never".
    pub fn expire(&mut self, key: &str, ttl_ms: u64, now_ms: i64) {
        if ttl_ms == 0 {
            return;
        }
        let deadline = now_ms.saturating_add(i64::try_from(ttl_ms).unwrap_or(i64::MAX));
        self.expiries.push((plan(key).stored, deadline));
    }

    /// Remove any pending expiry of a logical key.
    pub fn persist(&mut self, key: &str) {
        self.unexpire.push(plan(key).stored);
    }

    /// The commands this batch sends, in order. Deletes run before writes so a
    /// key that is cleared and re-written in one batch ends up written.
    pub fn commands(&self) -> Vec<Vec<String>> {
        let mut out = Vec::new();

        for chunk in self.deletes.chunks(CHUNK) {
            out.push(command("DEL", chunk.iter().cloned()));
        }
        for (set, members) in &self.unindex {
            for chunk in members.chunks(CHUNK) {
                out.push(command(
                    "SREM",
                    std::iter::once(set.clone()).chain(chunk.iter().cloned()),
                ));
            }
        }
        for chunk in self.unexpire.chunks(CHUNK) {
            out.push(command(
                "ZREM",
                std::iter::once(EXPIRY_ZSET.to_owned()).chain(chunk.iter().cloned()),
            ));
        }
        for chunk in self.writes.chunks(CHUNK) {
            out.push(command(
                "MSET",
                chunk.iter().flat_map(|(k, v)| [k.clone(), v.clone()]),
            ));
        }
        for (set, members) in &self.index {
            for chunk in members.chunks(CHUNK) {
                out.push(command(
                    "SADD",
                    std::iter::once(set.clone()).chain(chunk.iter().cloned()),
                ));
            }
        }
        for chunk in self.expiries.chunks(CHUNK) {
            out.push(command(
                "ZADD",
                std::iter::once(EXPIRY_ZSET.to_owned())
                    .chain(chunk.iter().flat_map(|(k, d)| [d.to_string(), k.clone()])),
            ));
        }

        out
    }

    /// Send the batch as one MULTI/EXEC transaction.
    pub async fn commit<C: ConnectionLike + Send>(self, conn: &mut C) -> ApiResult<()> {
        if self.is_empty() {
            return Ok(());
        }

        let mut pipe = redis::pipe();
        pipe.atomic();
        for args in self.commands() {
            let mut iter = args.into_iter();
            let name = iter.next().expect("command name");
            let cmd = pipe.cmd(&name);
            for arg in iter {
                cmd.arg(arg);
            }
            cmd.ignore();
        }
        pipe.query_async::<()>(conn).await?;

        Ok(())
    }
}

fn command(name: &str, args: impl Iterator<Item = String>) -> Vec<String> {
    std::iter::once(name.to_owned()).chain(args).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn empty_batch_sends_nothing() {
        let batch = Batch::new();
        assert!(batch.is_empty());
        assert!(batch.commands().is_empty());
    }

    #[test]
    fn set_writes_value_and_indexes() {
        let mut batch = Batch::new();
        batch
            .set("channel:1:2", &serde_json::json!({"id": "2"}))
            .unwrap();

        assert_eq!(
            batch.commands(),
            vec![
                strs(&["MSET", "channel:2", r#"{"id":"2"}"#]),
                strs(&["SADD", "channel_keys", "channel:2"]),
                strs(&["SADD", "guild_keys:1", "channel:2"]),
            ]
        );
    }

    #[test]
    fn del_removes_value_indexes_and_expiry() {
        let mut batch = Batch::new();
        batch.del("member:1:3");

        assert_eq!(
            batch.commands(),
            vec![
                strs(&["DEL", "member:1:3"]),
                strs(&["SREM", "guild_keys:1", "member:1:3"]),
                strs(&["SREM", "member_keys", "member:1:3"]),
                strs(&["ZREM", EXPIRY_ZSET, "member:1:3"]),
            ]
        );
    }

    #[test]
    fn deletes_run_before_writes() {
        let mut batch = Batch::new();
        batch.set_json("role:1:2", "{}".into());
        batch.del("role:1:2");

        let commands = batch.commands();
        let del = commands.iter().position(|c| c[0] == "DEL").unwrap();
        let set = commands.iter().position(|c| c[0] == "MSET").unwrap();
        let srem = commands.iter().position(|c| c[0] == "SREM").unwrap();
        let sadd = commands.iter().position(|c| c[0] == "SADD").unwrap();
        assert!(del < set);
        assert!(srem < sadd);
    }

    #[test]
    fn expire_uses_deadline_and_skips_zero_ttl() {
        let mut batch = Batch::new();
        batch.expire("member:1:3", 60_000, 1_000);
        batch.expire("member:1:4", 0, 1_000);

        assert_eq!(
            batch.commands(),
            vec![strs(&["ZADD", EXPIRY_ZSET, "61000", "member:1:3"])]
        );
    }

    #[test]
    fn persist_removes_expiry_only() {
        let mut batch = Batch::new();
        batch.persist("member:1:3");
        assert_eq!(
            batch.commands(),
            vec![strs(&["ZREM", EXPIRY_ZSET, "member:1:3"])]
        );
    }

    #[test]
    fn large_batches_are_chunked() {
        let mut batch = Batch::new();
        for i in 0..(CHUNK * 2 + 1) {
            batch.set_json(&format!("member:1:{i}"), "{}".into());
        }

        let commands = batch.commands();
        let mset: Vec<_> = commands.iter().filter(|c| c[0] == "MSET").collect();
        assert_eq!(mset.len(), 3);
        assert!(commands.iter().all(|c| c.len() <= CHUNK * 2 + 1));
    }
}
