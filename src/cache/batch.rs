//! A set of cache writes that is committed as one atomic MULTI/EXEC pipeline.
//!
//! Before, every update was several independent round trips (MSET, one SADD
//! per index set, HSET for expiries...). A connection error in between left
//! keys that no index referenced, so they could never be cleaned up. A batch
//! is planned in memory first, so it is also cheap to unit test.

use super::keys::{plan, stored_key};
use crate::{constants::EXPIRY_ZSET, models::ApiResult};

use redis::aio::ConnectionLike;
use serde::Serialize;
use std::collections::BTreeMap;

/// Upper bound for keys / members per variadic command. Redis <= 6.2 closes the
/// connection when a command has more than 1M arguments.
pub const CHUNK: usize = 1000;

/// Cache writes planned in memory and sent by [`Batch::commit`] as one
/// transaction. Keys are logical keys (see [`super::keys`]), except for
/// [`Batch::del_raw`]; the batch plans their stored keys, index entries and
/// expiry entries.
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
    /// An empty batch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the batch holds no command, in which case [`Batch::commit`]
    /// sends nothing.
    pub fn is_empty(&self) -> bool {
        self.deletes.is_empty()
            && self.unindex.is_empty()
            && self.unexpire.is_empty()
            && self.writes.is_empty()
            && self.index.is_empty()
            && self.expiries.is_empty()
    }

    /// Store `value` as JSON under a logical key and index it.
    ///
    /// # Errors
    ///
    /// Fails when `value` cannot be serialized to JSON; the batch is then
    /// left unchanged.
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

    /// Expire a logical key `ttl_ms` after `now_ms` (unix milliseconds). A TTL
    /// of 0 means "never": it adds no deadline but keeps one set earlier (call
    /// [`Batch::persist`] to drop it).
    pub fn expire(&mut self, key: &str, ttl_ms: u64, now_ms: i64) {
        if let Some(deadline) = deadline(ttl_ms, now_ms) {
            self.expiries.push((stored_key(key), deadline));
        }
    }

    /// Remove any pending expiry of a logical key.
    pub fn persist(&mut self, key: &str) {
        self.unexpire.push(stored_key(key));
    }

    /// The commands this batch sends, in order. Deletes run before writes so a
    /// key that is cleared and re-written in one batch ends up written.
    ///
    /// Within one batch every delete and `persist` runs before every write and
    /// expiry, whatever the call order: a `set` followed by a `del` of the same
    /// key leaves it written, and an `expire` wins over a `persist`.
    pub fn commands(&self) -> Vec<Vec<String>> {
        let mut out = Vec::new();
        self.for_each_command(|name, args| {
            let mut command = Vec::with_capacity(args.len() + 1);
            command.push(name.to_owned());
            command.extend(args.iter().map(|arg| (*arg).to_owned()));
            out.push(command);
        });
        out
    }

    /// Send the batch as one MULTI/EXEC transaction. An empty batch sends
    /// nothing.
    ///
    /// # Errors
    ///
    /// Fails when the connection fails or Redis returns an error. Part or all
    /// of the batch may have been applied anyway (the connection was lost
    /// after EXEC, or one command failed inside the transaction).
    pub async fn commit<C: ConnectionLike + Send>(self, conn: &mut C) -> ApiResult<()> {
        if self.is_empty() {
            return Ok(());
        }

        let mut pipe = redis::pipe();
        pipe.atomic();
        self.for_each_command(|name, args| {
            let cmd = pipe.cmd(name);
            for arg in args {
                cmd.arg(*arg);
            }
            cmd.ignore();
        });
        pipe.query_async::<()>(conn).await?;

        Ok(())
    }

    /// Call `emit` with the name and arguments of each command, in send order.
    /// [`Batch::commands`] and [`Batch::commit`] both use it, so what the tests
    /// inspect is exactly what is sent.
    fn for_each_command(&self, mut emit: impl FnMut(&str, &[&str])) {
        for chunk in self.deletes.chunks(CHUNK) {
            let args: Vec<&str> = chunk.iter().map(String::as_str).collect();
            emit("DEL", &args);
        }
        for (set, members) in &self.unindex {
            for chunk in members.chunks(CHUNK) {
                let args: Vec<&str> = std::iter::once(set.as_str())
                    .chain(chunk.iter().map(String::as_str))
                    .collect();
                emit("SREM", &args);
            }
        }
        for chunk in self.unexpire.chunks(CHUNK) {
            let args: Vec<&str> = std::iter::once(EXPIRY_ZSET)
                .chain(chunk.iter().map(String::as_str))
                .collect();
            emit("ZREM", &args);
        }
        for chunk in self.writes.chunks(CHUNK) {
            let args: Vec<&str> = chunk
                .iter()
                .flat_map(|(key, value)| [key.as_str(), value.as_str()])
                .collect();
            emit("MSET", &args);
        }
        for (set, members) in &self.index {
            for chunk in members.chunks(CHUNK) {
                let args: Vec<&str> = std::iter::once(set.as_str())
                    .chain(chunk.iter().map(String::as_str))
                    .collect();
                emit("SADD", &args);
            }
        }
        for chunk in self.expiries.chunks(CHUNK) {
            let deadlines: Vec<String> = chunk
                .iter()
                .map(|(_, deadline)| deadline.to_string())
                .collect();
            let mut args: Vec<&str> = Vec::with_capacity(1 + 2 * chunk.len());
            args.push(EXPIRY_ZSET);
            for ((key, _), deadline) in chunk.iter().zip(&deadlines) {
                args.push(deadline);
                args.push(key);
            }
            emit("ZADD", &args);
        }
    }
}

/// The expiry deadline (unix milliseconds) of a key written at `now_ms` with
/// a TTL of `ttl_ms`, `None` for a TTL of 0 ("never"). The write that does not
/// go through a batch (`cache::update_stale`) uses it too, so both set the
/// same deadline.
pub(crate) fn deadline(ttl_ms: u64, now_ms: i64) -> Option<i64> {
    (ttl_ms != 0).then(|| now_ms.saturating_add(i64::try_from(ttl_ms).unwrap_or(i64::MAX)))
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
