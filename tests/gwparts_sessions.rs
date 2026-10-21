#![recursion_limit = "256"]

//! What a restart resumes from: the legacy `gateway_sessions` blob next to
//! the per-shard `gateway_session:<shard>` keys (against a real Redis, set
//! TEST_REDIS_URL), and the resume point a shard reports while its payloads
//! are completed or fail.

mod common;

use common::*;
use redis::aio::ConnectionManager;
use serde_json::json;
use std::sync::Arc;
use twilight_dispatch::gateway::{
    registry::{Registry, ShardHandle},
    sessions,
};

const LEGACY: &str = "gateway_sessions";

fn handle(id: u32, session: Option<&str>, seq: u64) -> Arc<ShardHandle> {
    let handle = ShardHandle::new(id, None, Some(seq));
    if let Some(session) = session {
        handle.update(|s| {
            s.session_id = Some(session.to_owned());
            s.resume_url = Some(format!("wss://resume-{id}.discord.gg"));
        });
    }
    Arc::new(handle)
}

async fn set(conn: &mut ConnectionManager, key: &str, value: &str) {
    let _: () = redis::cmd("SET")
        .arg(key)
        .arg(value)
        .query_async(conn)
        .await
        .unwrap();
}

/// The string stored at `key`, unparsed.
async fn raw_value(conn: &mut ConnectionManager, key: &str) -> Option<String> {
    redis::cmd("GET").arg(key).query_async(conn).await.unwrap()
}

/// Older versions and consumers parse the legacy blob as an object, so once
/// its last session is removed it is `{}`.
#[tokio::test]
async fn removing_the_last_session_leaves_an_empty_object() {
    let Some(mut conn) = redis(1).await else {
        return;
    };
    let registry = Registry::new([handle(0, Some("abc"), 42)]);
    assert_eq!(sessions::save(&mut conn, &registry).await.unwrap(), 1);
    let registry = Registry::new([handle(0, None, 0)]);
    assert_eq!(sessions::save(&mut conn, &registry).await.unwrap(), 0);

    assert_eq!(raw_value(&mut conn, LEGACY).await.as_deref(), Some("{}"));
    assert_eq!(get(&mut conn, LEGACY).await, Some(json!({})));
    assert!(!exists(&mut conn, "gateway_session:0").await);
}

/// A legacy blob that is not a JSON object, or a key of another type, does not
/// stop sessions from being saved or loaded: the next save replaces it.
#[tokio::test]
async fn an_unreadable_legacy_blob_is_replaced() {
    let Some(mut conn) = redis(2).await else {
        return;
    };
    let expected = json!({
        "0": {"session_id": "abc", "sequence": 42, "resume_url": "wss://resume-0.discord.gg"}
    });
    for blob in ["not json", "42"] {
        set(&mut conn, LEGACY, blob).await;
        let registry = Registry::new([handle(0, Some("abc"), 42)]);
        assert_eq!(sessions::save(&mut conn, &registry).await.unwrap(), 1);
        let legacy = get(&mut conn, LEGACY).await;
        assert_eq!(legacy.as_ref(), Some(&expected), "{blob}");
    }

    // GET fails on a hash: `load` falls back to the per-shard keys only, and
    // the merge into the blob overwrites it.
    let _: i64 = redis::cmd("DEL")
        .arg(LEGACY)
        .query_async(&mut conn)
        .await
        .unwrap();
    let _: i64 = redis::cmd("HSET")
        .arg(LEGACY)
        .arg("0")
        .arg("x")
        .query_async(&mut conn)
        .await
        .unwrap();
    let loaded = sessions::load(&mut conn, 0..1).await.unwrap();
    assert_eq!(loaded[&0].session_id, "abc", "from gateway_session:0");
    assert_eq!(loaded[&0].sequence, 42);

    let registry = Registry::new([handle(0, Some("def"), 7)]);
    assert_eq!(sessions::save(&mut conn, &registry).await.unwrap(), 1);
    let legacy = get(&mut conn, LEGACY).await.unwrap();
    assert_eq!(legacy["0"]["session_id"], "def");
    assert_eq!(legacy["0"]["sequence"], 7);
}

/// A per-shard key wins over the legacy blob only when it holds a resumable
/// session, and `load` returns only the shards it was asked for.
#[tokio::test]
async fn only_usable_per_shard_keys_shadow_the_legacy_blob() {
    let Some(mut conn) = redis(3).await else {
        return;
    };
    let legacy = json!({
        "0": {"session_id": "legacy-0", "sequence": 7},
        "1": {"session_id": "legacy-1", "sequence": 3},
        "2": {"session_id": "legacy-2", "sequence": 4},
        "5": {"session_id": "other", "sequence": 9},
    });
    set(&mut conn, LEGACY, &legacy.to_string()).await;
    set(&mut conn, "gateway_session:0", "not json").await;
    let lost = r#"{"session_id":"","sequence":0}"#;
    set(&mut conn, "gateway_session:1", lost).await;
    let usable = r#"{"session_id":"per-shard-2","sequence":11}"#;
    set(&mut conn, "gateway_session:2", usable).await;

    let loaded = sessions::load(&mut conn, 0..3).await.unwrap();
    assert_eq!(loaded.len(), 3, "{loaded:?}");
    // Neither an unparseable nor a non-resumable key shadows the blob.
    assert_eq!(loaded[&0].session_id, "legacy-0");
    assert_eq!(loaded[&0].sequence, 7);
    assert_eq!(loaded[&1].session_id, "legacy-1");
    assert_eq!(loaded[&1].sequence, 3);
    // A usable one does.
    assert_eq!(loaded[&2].session_id, "per-shard-2");
    assert_eq!(loaded[&2].sequence, 11);
    // Shard 5 is another process' one.
    assert!(!loaded.contains_key(&5));
}

/// A payload reported twice (completed again, or completed after it failed)
/// changes nothing: it cannot release a payload that is still in flight, so
/// the saved sequence never passes one.
#[test]
fn a_payload_reported_twice_does_not_move_the_resume_point() {
    let shard = handle(0, Some("abc"), 1);
    let resume_seq = || shard.session_info().map(|session| session.sequence);
    let t2 = shard.dispatch(2).expect("tracked");
    let t3 = shard.dispatch(3).expect("tracked");
    shard.complete(t3);
    shard.complete(t3);
    assert_eq!(shard.in_flight(), 1);
    assert_eq!(resume_seq(), Some(1), "2 is still in flight");

    shard.complete(t2);
    assert_eq!(shard.in_flight(), 0);
    assert_eq!(resume_seq(), Some(3));

    let t4 = shard.dispatch(4).expect("tracked");
    shard.fail(t4);
    shard.complete(t4);
    assert_eq!(shard.in_flight(), 0);
    assert_eq!(resume_seq(), Some(3), "a restart replays 4");
}
