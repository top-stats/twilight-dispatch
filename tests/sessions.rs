#![recursion_limit = "256"]

//! Session persistence against a real Redis (set TEST_REDIS_URL).

mod common;

use common::*;
use std::sync::Arc;
use twilight_dispatch::{
    gateway::{
        registry::{Registry, ShardHandle},
        sessions,
    },
    models::SessionInfo,
};

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

#[tokio::test]
async fn sessions_round_trip_with_resume_url() {
    let Some(mut conn) = redis(1).await else {
        return;
    };
    let registry = Registry::new([handle(0, Some("abc"), 42), handle(1, None, 0)]);
    assert_eq!(sessions::save(&mut conn, &registry).await.unwrap(), 1);

    let ttl: i64 = redis::cmd("TTL")
        .arg("gateway_session:0")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(
        ttl > 0 && ttl <= sessions::SESSION_TTL_SECS as i64,
        "ttl {ttl}"
    );

    let loaded = sessions::load(&mut conn, 0..2).await.unwrap();
    assert_eq!(
        loaded.get(&0),
        Some(&SessionInfo {
            session_id: "abc".into(),
            sequence: 42,
            resume_url: Some("wss://resume-0.discord.gg".into()),
        })
    );
    assert!(!loaded.contains_key(&1));

    // The legacy blob is kept for older versions and existing consumers.
    let legacy = get(&mut conn, "gateway_sessions").await.unwrap();
    assert_eq!(legacy["0"]["session_id"], "abc");
    assert_eq!(legacy["0"]["sequence"], 42);
    assert!(legacy.get("1").is_none());
}

#[tokio::test]
async fn other_processes_sessions_in_the_legacy_blob_are_kept() {
    let Some(mut conn) = redis(2).await else {
        return;
    };
    let _: () = redis::cmd("SET")
        .arg("gateway_sessions")
        .arg(r#"{"5":{"session_id":"other","sequence":9},"0":{"session_id":"stale","sequence":1}}"#)
        .query_async(&mut conn)
        .await
        .unwrap();

    let registry = Registry::new([handle(0, Some("abc"), 42)]);
    sessions::save(&mut conn, &registry).await.unwrap();
    let legacy = get(&mut conn, "gateway_sessions").await.unwrap();
    assert_eq!(legacy["0"]["session_id"], "abc");
    assert_eq!(legacy["5"]["session_id"], "other");
}

#[tokio::test]
async fn sessions_written_by_older_versions_are_resumed() {
    let Some(mut conn) = redis(3).await else {
        return;
    };
    let _: () = redis::cmd("SET")
        .arg("gateway_sessions")
        .arg(r#"{"0":{"session_id":"old","sequence":7},"1":{"session_id":"","sequence":0}}"#)
        .query_async(&mut conn)
        .await
        .unwrap();
    let loaded = sessions::load(&mut conn, 0..2).await.unwrap();
    assert_eq!(loaded[&0].session_id, "old");
    assert_eq!(loaded[&0].resume_url, None);
    assert!(!loaded.contains_key(&1), "empty sessions are not resumed");
}

#[tokio::test]
async fn a_lost_session_is_removed() {
    let Some(mut conn) = redis(4).await else {
        return;
    };
    sessions::save(&mut conn, &Registry::new([handle(0, Some("abc"), 42)]))
        .await
        .unwrap();
    // The shard lost its session (e.g. 4009); nothing must resume it.
    sessions::save(&mut conn, &Registry::new([handle(0, None, 0)]))
        .await
        .unwrap();
    assert!(!exists(&mut conn, "gateway_session:0").await);
    assert!(get(&mut conn, "gateway_sessions")
        .await
        .unwrap()
        .get("0")
        .is_none());
    assert!(sessions::load(&mut conn, 0..1).await.unwrap().is_empty());
}
