//! Background cache maintenance: expiring members and messages.
//!
//! Expiry deadlines live in the sorted set [`EXPIRY_ZSET`] (stored key scored
//! by unix milliseconds). Earlier versions kept them in the hash
//! [`EXPIRY_KEYS`] and read the whole hash every second, which is O(n) on
//! Redis' single thread; [`migrate_legacy_expiry`] moves those entries over.
//!
//! Members get a deadline from `STATE_MEMBER_TTL`, or with a TTL of 0 from a
//! `GUILD_CREATE` re-sync that does not list them
//! ([`super::MEMBER_RESYNC_GRACE_MS`]); messages from `STATE_MESSAGE_TTL`.
//! [`expire_due`] removes a due key together with its index entries: for a
//! member, `member_keys` and `guild_keys:<guild>`.
//!
//! The service runs these jobs whatever `STATE_ENABLED` says: an instance
//! with the cache disabled still expires keys left in [`EXPIRY_ZSET`] (for
//! example by an earlier run with the cache on) and still migrates
//! [`EXPIRY_KEYS`].

use super::keys::plan;
use crate::{
    constants::{EXPIRY_BATCH, EXPIRY_KEYS, EXPIRY_ZSET},
    models::{ApiResult, FormattedDateTime},
};

use redis::{aio::ConnectionLike, Script};
use std::sync::LazyLock;

/// Delete keys whose deadline has passed, together with their index entries.
///
/// The deadline is re-checked inside the script, so a key whose expiry was
/// refreshed after we read the candidates is left alone.
///
/// ARGV: zset, now, then for each key: key, number of index sets, sets...
const EXPIRE_SCRIPT: &str = r"
local zset = ARGV[1]
local now = tonumber(ARGV[2])
local i = 3
local removed = 0
while i <= #ARGV do
  local key = ARGV[i]
  local sets = tonumber(ARGV[i + 1])
  local score = redis.call('ZSCORE', zset, key)
  if score and tonumber(score) <= now then
    redis.call('DEL', key)
    for j = 1, sets do
      redis.call('SREM', ARGV[i + 1 + j], key)
    end
    redis.call('ZREM', zset, key)
    removed = removed + 1
  end
  i = i + 2 + sets
end
return removed
";

static EXPIRE: LazyLock<Script> = LazyLock::new(|| Script::new(EXPIRE_SCRIPT));

/// The script arguments for a list of expired stored keys.
///
/// `keys` are stored keys. Planning one gives its index sets only when it is
/// stored under its logical name (every prefix but guild channels), so guild
/// channels must never get an expiry; only members and messages do.
fn expire_args(keys: &[String], now_ms: i64) -> Vec<String> {
    let mut args = vec![EXPIRY_ZSET.to_owned(), now_ms.to_string()];
    for key in keys {
        let plan = plan(key);
        args.push(plan.stored);
        args.push(plan.indexes.len().to_string());
        args.extend(plan.indexes.into_iter().map(|(set, _)| set));
    }
    args
}

/// Remove keys whose deadline is at or before `now_ms`, `EXPIRY_BATCH` at a
/// time, until a batch is not full or removes nothing; anything left is for
/// the next run. Returns how many keys were removed.
///
/// # Errors
///
/// Fails when a Redis command or the expiry script fails. Batches removed
/// before the failure stay removed.
pub async fn expire_due<C: ConnectionLike + Send>(conn: &mut C, now_ms: i64) -> ApiResult<u64> {
    let mut removed = 0;
    loop {
        let due: Vec<String> = redis::cmd("ZRANGEBYSCORE")
            .arg(EXPIRY_ZSET)
            .arg("-inf")
            .arg(now_ms)
            .arg("LIMIT")
            .arg(0)
            .arg(EXPIRY_BATCH)
            .query_async(conn)
            .await?;
        if due.is_empty() {
            return Ok(removed);
        }

        let mut invocation = EXPIRE.prepare_invoke();
        for arg in expire_args(&due, now_ms) {
            invocation.arg(arg);
        }
        let count: u64 = invocation.invoke_async(conn).await?;
        removed += count;

        if due.len() < EXPIRY_BATCH || count == 0 {
            return Ok(removed);
        }
    }
}

/// Move entries of the legacy expiry hash into the sorted set. Migrated
/// fields are removed one batch at a time (not the whole hash), so entries an
/// older version writes meanwhile are picked up by the next run. A field an
/// older version rewrites between the HSCAN that read it and the HDEL is
/// deleted with it, so its new deadline is lost (the one read is scheduled).
/// Fields whose value is not a legacy deadline are deleted without being
/// scheduled. Returns how many entries had a valid deadline.
///
/// # Errors
///
/// Fails when a Redis command or a batch's transaction fails. Batches
/// migrated before the failure stay migrated; the rest is left in the hash
/// for the next run.
pub async fn migrate_legacy_expiry<C: ConnectionLike + Send>(conn: &mut C) -> ApiResult<u64> {
    let mut cursor: u64 = 0;
    let mut migrated = 0;
    let mut gt = None;
    loop {
        let (next, entries): (u64, Vec<(String, String)>) = redis::cmd("HSCAN")
            .arg(EXPIRY_KEYS)
            .arg(cursor)
            .arg("COUNT")
            .arg(EXPIRY_BATCH)
            .query_async(conn)
            .await?;

        if !entries.is_empty() {
            let scored: Vec<(i64, &String)> = entries
                .iter()
                .filter_map(|(key, value)| Some((legacy_deadline(value)?, key)))
                .collect();
            // GT keeps the later deadline when an older instance refreshed
            // an entry that was migrated before (Redis >= 6.2; NX before).
            // Probed up front: a MULTI with an unsupported flag would still
            // run its HDEL and lose the entries.
            if gt.is_none() {
                gt = Some(supports_zadd_gt(conn).await?);
            }
            let mode = if gt == Some(true) { "GT" } else { "NX" };
            migrate_batch(conn, &entries, &scored, mode).await?;
            migrated += scored.len() as u64;
        }

        cursor = next;
        if cursor == 0 {
            break;
        }
    }
    Ok(migrated)
}

/// Whether the server knows `ZADD ... GT` (Redis 6.2+).
async fn supports_zadd_gt<C: ConnectionLike + Send>(conn: &mut C) -> ApiResult<bool> {
    const PROBE: &str = "expiry_zset_probe";
    let result: redis::RedisResult<()> = redis::cmd("ZADD")
        .arg(PROBE)
        .arg("GT")
        .arg(0)
        .arg("probe")
        .query_async(conn)
        .await;
    let _: redis::RedisResult<()> = redis::cmd("DEL").arg(PROBE).query_async(conn).await;
    match result {
        Ok(()) => Ok(true),
        Err(err)
            if err.kind() == redis::ErrorKind::Server(redis::ServerErrorKind::ResponseError) =>
        {
            Ok(false)
        }
        Err(err) => Err(err.into()),
    }
}

async fn migrate_batch<C: ConnectionLike + Send>(
    conn: &mut C,
    entries: &[(String, String)],
    scored: &[(i64, &String)],
    mode: &str,
) -> ApiResult<()> {
    let mut pipe = redis::pipe();
    pipe.atomic();
    if !scored.is_empty() {
        let zadd = pipe.cmd("ZADD").arg(EXPIRY_ZSET).arg(mode);
        for (deadline, key) in scored {
            zadd.arg(*deadline).arg(*key);
        }
        zadd.ignore();
    }
    let hdel = pipe.cmd("HDEL").arg(EXPIRY_KEYS);
    for (key, _) in entries {
        hdel.arg(key);
    }
    hdel.ignore();
    pipe.query_async::<()>(conn).await?;
    Ok(())
}

/// Legacy hash values are JSON strings holding a [`FormattedDateTime`].
fn legacy_deadline(value: &str) -> Option<i64> {
    serde_json::from_str::<FormattedDateTime>(value)
        .ok()
        .map(FormattedDateTime::unix_millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_arguments_carry_index_sets() {
        let keys = vec!["member:1:2".to_owned(), "message:3:4".to_owned()];
        assert_eq!(
            expire_args(&keys, 99),
            vec![
                EXPIRY_ZSET,
                "99",
                "member:1:2",
                "2",
                "member_keys",
                "guild_keys:1",
                "message:3:4",
                "2",
                "message_keys",
                "channel_keys:3",
            ]
        );
    }

    #[test]
    fn legacy_values_parse() {
        assert_eq!(legacy_deadline(r#""1970-01-01T00:00:01.5""#), Some(1500));
        assert_eq!(legacy_deadline("garbage"), None);
    }
}

#[cfg(test)]
mod index_set_counts {
    use super::*;

    /// Each key carries its own number of index sets (0, 1 and 3 here), which
    /// the script uses to find the next key.
    #[test]
    fn script_arguments_count_each_keys_own_index_sets() {
        let keys = ["bot_user", "guild:5", "thread:1:9"].map(str::to_owned);
        assert_eq!(
            expire_args(&keys, 7),
            vec![
                EXPIRY_ZSET,
                "7",
                "bot_user",
                "0",
                "guild:5",
                "1",
                "guild_keys",
                "thread:1:9",
                "3",
                "thread_keys",
                "guild_keys:1",
                "thread_keys:1",
            ]
        );
    }
}
