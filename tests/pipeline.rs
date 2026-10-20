#![recursion_limit = "256"]

//! Worker pipeline: cache first, then publish, per guild in order
//! (set TEST_REDIS_URL).

mod common;

use common::*;
use serde_json::{json, Value};
use std::{
    sync::{atomic::AtomicBool, Arc, Mutex},
    time::Duration,
};
use twilight_dispatch::{
    discord_log::DiscordLog,
    gateway::{
        registry::{Registry, ShardHandle},
        runner::parse_event,
        worker::{self, Breaker, Job, Publisher, WorkerContext},
    },
    metrics::GUILD_EVENTS,
    models::ApiResult,
    util::Throttle,
};

/// CLIENT PAUSE affects the whole server, so these tests run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Default)]
struct Recorder {
    published: Mutex<Vec<(String, Value, Option<String>)>>,
    fail: AtomicBool,
}

impl Publisher for Recorder {
    async fn publish(
        &self,
        routing_key: &str,
        payload: Vec<u8>,
        message_id: Option<String>,
    ) -> ApiResult<()> {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("broker down".into());
        }
        let value = serde_json::from_slice(&payload)?;
        self.published
            .lock()
            .unwrap()
            .push((routing_key.to_owned(), value, message_id));
        Ok(())
    }
}

fn job(handle: &ShardHandle, kind: &str, seq: u64, d: Value, typed: bool) -> Job {
    let json = raw(kind, seq, d);
    let event = if typed {
        parse_event(&json).unwrap()
    } else {
        None
    };
    Job {
        shard: 0,
        seq: Some(seq),
        token: handle.dispatch(seq),
        session: Some(Arc::from("s1")),
        json,
        event,
    }
}

fn context(
    conn: redis::aio::ConnectionManager,
    registry: Arc<Registry>,
    recorder: Arc<Recorder>,
    cache_timeout: Duration,
) -> WorkerContext<redis::aio::ConnectionManager, Recorder> {
    WorkerContext {
        redis: conn,
        publisher: recorder,
        registry,
        state: state_all(),
        bot_id: bot_id(),
        cache_timeout,
        discord: DiscordLog::disabled(),
        breaker: Breaker::default(),
        cache_errors: Throttle::default(),
        publish_errors: Throttle::default(),
    }
}

#[tokio::test]
async fn events_are_cached_before_they_are_published() {
    let _serial = SERIAL.lock().await;
    let Some(conn) = redis(1).await else { return };
    let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
    let registry = Arc::new(Registry::new([handle.clone()]));
    let recorder = Arc::new(Recorder::default());
    let ctx = Arc::new(context(
        conn.clone(),
        registry,
        recorder.clone(),
        Duration::from_secs(5),
    ));
    let (queues, workers) = worker::spawn(4, ctx);

    // Same guild, so the same worker and strict order.
    let guild_id = 1000u64;
    let target = twilight_dispatch::gateway::payload::Route::Guild(guild_id).worker(4);
    let joins_before = GUILD_EVENTS.with_label_values(&["Join"]).get();
    let jobs = vec![
        job(&handle, "GUILD_CREATE", 2, guild(guild_id, "g"), true),
        job(
            &handle,
            "MESSAGE_CREATE",
            3,
            message(1, 1003, Some(guild_id), "hello"),
            true,
        ),
        job(
            &handle,
            "MESSAGE_UPDATE",
            4,
            message(1, 1003, Some(guild_id), "edited"),
            true,
        ),
        job(
            &handle,
            "MESSAGE_DELETE",
            5,
            json!({"id": "1", "channel_id": "1003", "guild_id": "1000"}),
            true,
        ),
        job(
            &handle,
            "TYPING_START",
            6,
            json!({"channel_id": "1003", "guild_id": "1000", "user_id": "1", "timestamp": 1}),
            false,
        ),
        // A re-sent GUILD_CREATE is not a second join.
        job(&handle, "GUILD_CREATE", 7, guild(guild_id, "g"), true),
    ];
    assert_eq!(handle.in_flight(), 6);
    for job in jobs {
        queues[target].send(job).await.unwrap();
    }
    drop(queues);
    for worker in workers {
        worker.await.unwrap();
    }

    let published = recorder.published.lock().unwrap().clone();
    let kinds: Vec<&str> = published.iter().map(|(k, _, _)| k.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "GUILD_CREATE",
            "MESSAGE_CREATE",
            "MESSAGE_UPDATE",
            "MESSAGE_DELETE",
            "TYPING_START",
            "GUILD_CREATE"
        ]
    );

    // STATE_OLD values reach consumers (they were always missing before).
    assert!(
        published[0].1.get("old").is_none(),
        "nothing cached before the first GUILD_CREATE"
    );
    assert_eq!(published[2].1["old"]["content"], "hello");
    assert_eq!(published[3].1["old"]["content"], "edited");
    assert_eq!(published[3].1["d"]["id"], "1");
    assert_eq!(published[1].1["op"], 0);
    // Unique per session, stable across replays within it.
    assert_eq!(published[1].2.as_deref(), Some("0:s1:3"));

    // The delete was applied before it was published.
    assert!(!exists(&mut conn.clone(), "message:1003:1").await);

    // One join for the first GUILD_CREATE only.
    assert_eq!(
        GUILD_EVENTS.with_label_values(&["Join"]).get() - joins_before,
        1
    );

    // Every payload finished, so the resume point is the last sequence.
    assert_eq!(handle.in_flight(), 0);
    assert_eq!(handle.session_info(), None, "no session id yet");
    handle.update(|s| s.session_id = Some("abc".into()));
    assert_eq!(handle.session_info().unwrap().sequence, 7);
}

#[tokio::test]
async fn a_stalled_redis_does_not_hold_events_back() {
    let _serial = SERIAL.lock().await;
    let Some(mut conn) = redis(2).await else {
        return;
    };
    let mut admin = conn.clone();
    let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
    let registry = Arc::new(Registry::new([handle.clone()]));
    let recorder = Arc::new(Recorder::default());
    let ctx = context(
        conn.clone(),
        registry,
        recorder.clone(),
        Duration::from_millis(200),
    );

    // Every other client's commands now hang for 2 s.
    let _: () = redis::cmd("CLIENT")
        .arg("PAUSE")
        .arg(2000)
        .arg("ALL")
        .query_async(&mut admin)
        .await
        .unwrap();

    let started = std::time::Instant::now();
    worker::process(
        &ctx,
        &mut conn,
        job(&handle, "GUILD_CREATE", 2, guild(1000, "g"), true),
    )
    .await;
    assert!(!ctx.breaker.allow(), "a timeout opens the breaker");
    // Within the cooldown the next update is skipped instead of waiting again.
    worker::process(
        &ctx,
        &mut conn,
        job(
            &handle,
            "MESSAGE_CREATE",
            3,
            message(1, 1003, Some(1000), "x"),
            true,
        ),
    )
    .await;
    assert!(
        started.elapsed() < Duration::from_millis(1500),
        "{:?}",
        started.elapsed()
    );

    let published = recorder.published.lock().unwrap().clone();
    assert_eq!(published.len(), 2, "both events were still published");
    assert_eq!(handle.in_flight(), 0);

    let _: () = redis::cmd("CLIENT")
        .arg("UNPAUSE")
        .query_async(&mut admin)
        .await
        .unwrap();
}

#[tokio::test]
async fn failed_publishes_hold_the_resume_point() {
    let _serial = SERIAL.lock().await;
    let Some(mut conn) = redis(3).await else {
        return;
    };
    let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
    handle.update(|s| s.session_id = Some("abc".into()));
    let registry = Arc::new(Registry::new([handle.clone()]));
    let recorder = Arc::new(Recorder::default());
    let ctx = context(
        conn.clone(),
        registry,
        recorder.clone(),
        Duration::from_secs(5),
    );

    recorder
        .fail
        .store(true, std::sync::atomic::Ordering::SeqCst);
    worker::process(
        &ctx,
        &mut conn,
        job(
            &handle,
            "TYPING_START",
            2,
            json!({"channel_id": "1"}),
            false,
        ),
    )
    .await;
    recorder
        .fail
        .store(false, std::sync::atomic::Ordering::SeqCst);
    worker::process(
        &ctx,
        &mut conn,
        job(
            &handle,
            "TYPING_START",
            3,
            json!({"channel_id": "1"}),
            false,
        ),
    )
    .await;

    assert_eq!(handle.in_flight(), 0);
    // A restart now resumes before the lost event so Discord replays it.
    assert_eq!(handle.session_info().unwrap().sequence, 1);
}

#[tokio::test]
async fn deletes_of_a_previous_session_are_still_applied() {
    let _serial = SERIAL.lock().await;
    let Some(mut conn) = redis(4).await else {
        return;
    };
    let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
    let registry = Arc::new(Registry::new([handle.clone()]));
    let recorder = Arc::new(Recorder::default());
    let ctx = context(
        conn.clone(),
        registry,
        recorder.clone(),
        Duration::from_secs(5),
    );

    worker::process(
        &ctx,
        &mut conn,
        job(
            &handle,
            "MESSAGE_CREATE",
            2,
            message(1, 1003, Some(1000), "x"),
            true,
        ),
    )
    .await;
    // Queued before the session ended, processed after the new READY.
    let delete = job(
        &handle,
        "MESSAGE_DELETE",
        3,
        json!({"id": "1", "channel_id": "1003", "guild_id": "1000"}),
        true,
    );
    let create = job(
        &handle,
        "MESSAGE_CREATE",
        4,
        message(2, 1003, Some(1000), "y"),
        true,
    );
    handle.new_session(1);
    worker::process(&ctx, &mut conn, delete).await;
    worker::process(&ctx, &mut conn, create).await;

    // Nothing replays the delete, so it is applied; the stale create is not.
    assert!(!exists(&mut conn, "message:1003:1").await);
    assert!(!exists(&mut conn, "message:1003:2").await);
    assert_eq!(
        recorder.published.lock().unwrap().len(),
        3,
        "both are still published"
    );
}
