#![recursion_limit = "256"]

//! Cache lifecycle behaviour against a real Redis (set TEST_REDIS_URL).

mod common;

use common::*;
use serde_json::json;

/// One test asserts on server-wide command statistics, so these run one at a
/// time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A removed thread must also leave every index that listed it, or the
/// indexes point at keys that no longer exist.
async fn assert_thread_unindexed(conn: &mut redis::aio::ConnectionManager, thread: u64) {
    let key = format!("thread:1000:{thread}");
    assert!(!exists(conn, &key).await, "{key}");
    for set in ["thread_keys:1000", "guild_keys:1000", "thread_keys"] {
        assert!(
            !members(conn, set).await.contains(&key),
            "{key} still in {set}"
        );
    }
    let messages = format!("channel_keys:{thread}");
    assert!(members(conn, &messages).await.is_empty(), "{messages}");
}

#[tokio::test]
async fn guild_create_keeps_messages_of_channels_it_still_lists() {
    let _serial = SERIAL.lock().await;
    let Some(mut conn) = redis(1).await else {
        return;
    };
    let state = state_all();
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;
    apply(
        &mut conn,
        &state,
        "MESSAGE_CREATE",
        message(1, 1003, Some(1000), "kept"),
    )
    .await;
    apply(
        &mut conn,
        &state,
        "MESSAGE_CREATE",
        message(2, 1004, Some(1000), "gone"),
    )
    .await;

    // Re-sent after a reconnect, without channel 1004 (deleted meanwhile).
    let mut again = guild(1000, "g");
    again["channels"] = json!([channel(1003, None, "general")]);
    apply(&mut conn, &state, "GUILD_CREATE", again).await;

    assert!(
        exists(&mut conn, "message:1003:1").await,
        "message of a listed channel"
    );
    assert!(
        !exists(&mut conn, "message:1004:2").await,
        "message of a removed channel"
    );
    assert!(!exists(&mut conn, "channel:1004").await);
    assert!(members(&mut conn, "channel_keys:1004").await.is_empty());
    assert_eq!(
        members(&mut conn, "channel_keys:1003").await,
        vec!["message:1003:1"]
    );
}

#[tokio::test]
async fn deleting_a_channel_removes_its_threads() {
    let _serial = SERIAL.lock().await;
    let Some(mut conn) = redis(2).await else {
        return;
    };
    let state = state_all();
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;
    apply(
        &mut conn,
        &state,
        "MESSAGE_CREATE",
        message(1, 1005, Some(1000), "in thread"),
    )
    .await;

    apply(
        &mut conn,
        &state,
        "CHANNEL_DELETE",
        channel(1003, Some(1000), "general"),
    )
    .await;
    assert!(
        !exists(&mut conn, "thread:1000:1005").await,
        "thread of the deleted channel"
    );
    assert!(!exists(&mut conn, "message:1005:1").await);
    assert_thread_unindexed(&mut conn, 1005).await;
    assert!(members(&mut conn, "message_keys").await.is_empty());
    assert!(
        exists(&mut conn, "channel:1004").await,
        "other channels stay"
    );
}

#[tokio::test]
async fn deleting_a_thread_removes_it_from_every_index() {
    let _serial = SERIAL.lock().await;
    let Some(mut conn) = redis(7).await else {
        return;
    };
    let state = state_all();
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;
    apply(&mut conn, &state, "THREAD_CREATE", thread(1006, 1000, 1004)).await;
    apply(
        &mut conn,
        &state,
        "MESSAGE_CREATE",
        message(1, 1005, Some(1000), "in thread"),
    )
    .await;

    apply(
        &mut conn,
        &state,
        "THREAD_DELETE",
        json!({"id": "1005", "guild_id": "1000", "parent_id": "1003", "type": 11}),
    )
    .await;
    assert!(!exists(&mut conn, "message:1005:1").await);
    assert_thread_unindexed(&mut conn, 1005).await;
    assert_eq!(
        members(&mut conn, "thread_keys:1000").await,
        vec!["thread:1000:1006"],
        "other threads stay"
    );
}

#[tokio::test]
async fn only_active_threads_are_cached() {
    let _serial = SERIAL.lock().await;
    let Some(mut conn) = redis(3).await else {
        return;
    };
    let state = state_all();
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;
    assert!(exists(&mut conn, "thread:1000:1005").await);

    // The sync lists every active thread of channel 1003: 1005 is gone.
    apply(
        &mut conn,
        &state,
        "THREAD_LIST_SYNC",
        json!({"guild_id": "1000", "channel_ids": ["1003"], "threads": [thread(1006, 1000, 1003)], "members": []}),
    )
    .await;
    assert!(!exists(&mut conn, "thread:1000:1005").await);
    assert_thread_unindexed(&mut conn, 1005).await;
    assert!(exists(&mut conn, "thread:1000:1006").await);
    assert_eq!(
        members(&mut conn, "thread_keys:1000").await,
        vec!["thread:1000:1006"]
    );

    let mut archived = thread(1006, 1000, 1003);
    archived["thread_metadata"]["archived"] = json!(true);
    apply(&mut conn, &state, "THREAD_UPDATE", archived).await;
    assert!(
        !exists(&mut conn, "thread:1000:1006").await,
        "archived threads are dropped"
    );
    assert_thread_unindexed(&mut conn, 1006).await;
    assert!(members(&mut conn, "thread_keys").await.is_empty());
}

#[tokio::test]
async fn offline_presences_in_member_chunks_are_removed() {
    let _serial = SERIAL.lock().await;
    let Some(mut conn) = redis(4).await else {
        return;
    };
    let state = state_all();
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;
    assert!(exists(&mut conn, "presence:1000:10").await);

    apply(
        &mut conn,
        &state,
        "GUILD_MEMBERS_CHUNK",
        json!({
            "guild_id": "1000", "members": [member(10, &[])], "chunk_index": 0,
            "chunk_count": 1, "not_found": [], "presences": [presence(1000, 10, "offline")],
        }),
    )
    .await;
    assert!(!exists(&mut conn, "presence:1000:10").await);
    assert!(exists(&mut conn, "member:1000:10").await);
}

#[tokio::test]
async fn guild_update_after_ready_makes_the_stub_available() {
    let _serial = SERIAL.lock().await;
    let Some(mut conn) = redis(5).await else {
        return;
    };
    let state = state_all();
    let id = 5u64 << 22;
    apply(&mut conn, &state, "READY", ready([0, 1], &[id])).await;
    assert_eq!(
        get(&mut conn, &format!("guild:{id}")).await.unwrap()["unavailable"],
        true
    );

    let mut update = guild(id, "named");
    for field in [
        "joined_at",
        "large",
        "member_count",
        "channels",
        "threads",
        "members",
        "presences",
        "voice_states",
        "unavailable",
    ] {
        update.as_object_mut().unwrap().remove(field);
    }
    apply(&mut conn, &state, "GUILD_UPDATE", update).await;
    let blob = get(&mut conn, &format!("guild:{id}")).await.unwrap();
    assert_eq!(blob["name"], "named");
    assert_eq!(blob["unavailable"], false);
    assert_eq!(blob["channels"], json!([]));
}

#[tokio::test]
async fn each_update_is_one_transaction() {
    let _serial = SERIAL.lock().await;
    let Some(mut conn) = redis(6).await else {
        return;
    };
    let state = state_all();
    let _: () = redis::cmd("CONFIG")
        .arg("RESETSTAT")
        .query_async(&mut conn)
        .await
        .unwrap();

    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;

    let stats: String = redis::cmd("INFO")
        .arg("commandstats")
        .query_async(&mut conn)
        .await
        .unwrap();
    let calls = |name: &str| -> u64 {
        stats
            .lines()
            .find(|line| line.starts_with(&format!("cmdstat_{name}:")))
            .and_then(|line| line.split("calls=").nth(1))
            .and_then(|rest| rest.split(',').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    };
    assert_eq!(calls("multi"), 1, "{stats}");
    assert_eq!(calls("exec"), 1, "{stats}");
    // Every write went through the transaction (none outside it).
    assert!(calls("mset") >= 1 && calls("sadd") >= 1);
}
