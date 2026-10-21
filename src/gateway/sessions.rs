//! Persisting gateway sessions and statuses in Redis.
//!
//! Sessions are stored per shard (`gateway_session:<shard>`), so processes that
//! run different shard ranges no longer overwrite each other. The legacy
//! `gateway_sessions` blob written by older versions is still read as a
//! fallback, so an upgrade resumes the running sessions.

use super::registry::Registry;
use crate::{
    constants::{session_key, CACHE_JOB_INTERVAL_MS, SESSIONS_KEY, STATUSES_KEY},
    models::{ApiResult, FormattedDateTime, SessionInfo, StatusInfo},
};

use redis::aio::ConnectionLike;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tracing::warn;

/// Parse the legacy blob: `{"<shard>": {"session_id", "sequence"}}`.
pub fn parse_legacy(json: &str) -> HashMap<u32, SessionInfo> {
    serde_json::from_str::<HashMap<String, SessionInfo>>(json)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(shard, session)| Some((shard.parse().ok()?, session)))
        .filter(|(_, session)| session.is_resumable())
        .collect()
}

/// Load saved sessions for `shards`; per-shard keys win over the legacy blob.
pub async fn load<C: ConnectionLike + Send>(
    conn: &mut C,
    shards: impl IntoIterator<Item = u32>,
) -> ApiResult<HashMap<u32, SessionInfo>> {
    let shards: Vec<u32> = shards.into_iter().collect();
    if shards.is_empty() {
        return Ok(HashMap::new());
    }

    // Not a string (or unreadable): ignore the legacy blob.
    let legacy: Option<String> = redis::cmd("GET")
        .arg(SESSIONS_KEY)
        .query_async(conn)
        .await
        .unwrap_or_default();
    let mut sessions = legacy.as_deref().map(parse_legacy).unwrap_or_default();
    sessions.retain(|shard, _| shards.contains(shard));

    let keys: Vec<String> = shards.iter().map(|shard| session_key(*shard)).collect();
    let values: Vec<Option<String>> = redis::cmd("MGET").arg(&keys).query_async(conn).await?;
    for (shard, value) in shards.iter().zip(values) {
        let parsed = value.and_then(|json| serde_json::from_str::<SessionInfo>(&json).ok());
        if let Some(session) = parsed.filter(SessionInfo::is_resumable) {
            sessions.insert(*shard, session);
        }
    }

    Ok(sessions)
}

/// Per-shard session keys expire: a session is only resumable for a few
/// minutes after its connection closed, and an expired key cannot shadow a
/// newer legacy blob after a rollback and re-upgrade.
pub const SESSION_TTL_SECS: u64 = 600;

/// Merge this process' shards into the legacy `gateway_sessions` blob, which
/// older versions (a rollback) and existing consumers read. Other processes'
/// shards in the blob are kept. ARGV: shard, session JSON ("" removes it)...
const MERGE_LEGACY: &str = r"
local sessions = {}
local ok, raw = pcall(redis.call, 'GET', KEYS[1])
if ok and raw then
  local decoded, value = pcall(cjson.decode, raw)
  if decoded and type(value) == 'table' then sessions = value end
end
for i = 1, #ARGV, 2 do
  if ARGV[i + 1] == '' then
    sessions[ARGV[i]] = nil
  else
    sessions[ARGV[i]] = cjson.decode(ARGV[i + 1])
  end
end
if next(sessions) == nil then
  redis.call('SET', KEYS[1], '{}')
else
  redis.call('SET', KEYS[1], cjson.encode(sessions))
end
return 1
";

static MERGE: std::sync::LazyLock<redis::Script> =
    std::sync::LazyLock::new(|| redis::Script::new(MERGE_LEGACY));

/// Write the sessions of every shard: resumable ones are stored, the keys of
/// shards without one are removed. Returns how many sessions were stored.
pub async fn save<C: ConnectionLike + Send>(conn: &mut C, registry: &Registry) -> ApiResult<usize> {
    let mut pipe = redis::pipe();
    let mut merge = MERGE.key(SESSIONS_KEY);
    let mut count = 0;
    for handle in registry.handles() {
        match handle.session_info() {
            Some(session) => {
                let json = serde_json::to_string(&session)?;
                pipe.cmd("SET")
                    .arg(session_key(handle.id))
                    .arg(&json)
                    .arg("EX")
                    .arg(SESSION_TTL_SECS)
                    .ignore();
                merge.arg(handle.id.to_string()).arg(json);
                count += 1;
            }
            None => {
                pipe.cmd("DEL").arg(session_key(handle.id)).ignore();
                merge.arg(handle.id.to_string()).arg("");
            }
        }
    }
    if registry.is_empty() {
        return Ok(0);
    }
    pipe.query_async::<()>(conn).await?;
    merge.invoke_async::<()>(conn).await?;
    Ok(count)
}

pub fn statuses(registry: &Registry) -> Vec<StatusInfo> {
    let now = FormattedDateTime::now();
    let mut statuses: Vec<StatusInfo> = registry.handles().map(|h| h.status_info(now)).collect();
    statuses.sort_by_key(|status| status.shard);
    statuses
}

/// Every second: write `gateway_statuses` and the shards' sessions.
pub async fn run_job<C: ConnectionLike + Send>(mut conn: C, registry: Arc<Registry>) {
    loop {
        match serde_json::to_string(&statuses(&registry)) {
            Ok(json) => {
                if let Err(err) = redis::cmd("SET")
                    .arg(STATUSES_KEY)
                    .arg(json)
                    .query_async::<()>(&mut conn)
                    .await
                {
                    warn!("Failed to dump gateway statuses: {}", err);
                }
            }
            Err(err) => warn!("Failed to serialize gateway statuses: {}", err),
        }
        if let Err(err) = save(&mut conn, &registry).await {
            warn!("Failed to dump gateway sessions: {}", err);
        }
        tokio::time::sleep(Duration::from_millis(CACHE_JOB_INTERVAL_MS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_blob_parses_and_drops_empty_sessions() {
        let json = r#"{
            "0": {"session_id": "abc", "sequence": 42},
            "1": {"session_id": "", "sequence": 0},
            "x": {"session_id": "zzz", "sequence": 1},
            "7": {"session_id": "def", "sequence": 9, "resume_url": "wss://r.discord.gg"}
        }"#;
        let sessions = parse_legacy(json);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[&0].sequence, 42);
        assert_eq!(
            sessions[&7].resume_url.as_deref(),
            Some("wss://r.discord.gg")
        );
        assert!(parse_legacy("garbage").is_empty());
    }
}
