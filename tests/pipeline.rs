#![recursion_limit = "256"]

//! Worker pipeline: cache first, then publish, per guild in order; the Redis
//! breaker (set TEST_REDIS_URL).

mod common;

use common::*;
use redis::aio::ConnectionManager;
use serde_json::{json, Value};
use std::{
    sync::{atomic::AtomicBool, Arc, Mutex},
    time::{Duration, Instant},
};
use twilight_dispatch::{
    discord_log::DiscordLog,
    gateway::{
        payload::Route,
        registry::{Registry, ShardHandle},
        runner::parse_event,
        worker::{self, Breaker, Job, Publisher, WorkerContext, BREAKER_COOLDOWN},
    },
    metrics::{GUILD_EVENTS, STATE_UPDATE_ERRORS},
    models::ApiResult,
    util::Throttle,
};

/// CLIENT PAUSE affects the whole server, and the tests compare metric
/// counters, so these tests run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Pauses every client of the test Redis (CLIENT PAUSE ALL) until dropped.
///
/// It has its own blocking connection, so it can unpause from `Drop` (a failed
/// assertion cannot leave the server paused for the next test) and does not
/// depend on the connection under test. Redis postpones UNPAUSE itself until a
/// PAUSE ALL is over, so dropping it waits for the end of the pause.
struct Pause(redis::Connection);

impl Pause {
    fn all(duration: Duration) -> Self {
        let url = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL");
        let mut conn = redis::Client::open(url)
            .and_then(|client| client.get_connection())
            .expect("connect to test redis");
        conn.set_read_timeout(Some(duration + Duration::from_secs(5)))
            .unwrap();
        redis::cmd("CLIENT")
            .arg("PAUSE")
            .arg(duration.as_millis() as u64)
            .arg("ALL")
            .query::<()>(&mut conn)
            .expect("pause test redis");
        Self(conn)
    }
}

impl Drop for Pause {
    fn drop(&mut self) {
        let unpaused = redis::cmd("CLIENT").arg("UNPAUSE").query::<()>(&mut self.0);
        if let Err(err) = unpaused {
            eprintln!("CLIENT UNPAUSE failed: {err}");
        }
    }
}

fn update_errors(reason: &str) -> u64 {
    STATE_UPDATE_ERRORS.with_label_values(&[reason]).get()
}

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

fn context<P>(
    conn: ConnectionManager,
    registry: Arc<Registry>,
    publisher: Arc<P>,
    cache_timeout: Duration,
) -> WorkerContext<ConnectionManager, P> {
    WorkerContext {
        redis: conn,
        publisher,
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
    let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
    let registry = Arc::new(Registry::new([handle.clone()]));
    // Never touches Redis, so only the cache update can wait for it.
    let recorder = Arc::new(Recorder::default());
    let timeout = Duration::from_millis(200);
    let ctx = context(conn.clone(), registry, recorder.clone(), timeout);
    let (timeouts, skipped) = (update_errors("timeout"), update_errors("skipped"));

    let _pause = Pause::all(Duration::from_millis(1500));
    let started = Instant::now();
    worker::process(
        &ctx,
        &mut conn,
        job(&handle, "GUILD_CREATE", 2, guild(1000, "g"), true),
    )
    .await;
    assert!(started.elapsed() >= timeout, "{:?}", started.elapsed());
    assert_eq!(update_errors("timeout") - timeouts, 1);
    assert!(!ctx.breaker.allow(), "a timeout opens the breaker");

    // Within the cooldown the next update is skipped instead of waiting again.
    let started = Instant::now();
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
        started.elapsed() < timeout / 2,
        "an open breaker must not wait for Redis: {:?}",
        started.elapsed()
    );
    assert_eq!(update_errors("skipped") - skipped, 1);
    assert_eq!(update_errors("timeout") - timeouts, 1);

    let published = recorder.published.lock().unwrap().clone();
    assert_eq!(published.len(), 2, "both events were still published");
    assert!(
        published
            .iter()
            .all(|(_, body, _)| body.get("old").is_none()),
        "nothing was read from the cache"
    );
    assert_eq!(handle.in_flight(), 0);
}

/// Every worker shares the breaker. After the cooldown one of them probes a
/// stalled Redis while the others keep skipping; if every worker probed, each
/// would wait for the timeout after every cooldown, and with it every event
/// queued behind it.
#[tokio::test]
async fn a_stalled_redis_is_probed_by_one_worker_at_a_time() {
    const WORKERS: usize = 8;
    let _serial = SERIAL.lock().await;
    let Some(mut conn) = redis(6).await else {
        return;
    };
    let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
    let registry = Arc::new(Registry::new([handle.clone()]));
    let recorder = Arc::new(Recorder::default());
    let timeout = Duration::from_millis(250);
    // One context, so one breaker, for every worker, as in production.
    let ctx = Arc::new(context(conn.clone(), registry, recorder.clone(), timeout));
    let (queues, workers) = worker::spawn(WORKERS, ctx.clone());
    let guilds: Vec<u64> = (0..WORKERS)
        .map(|w| {
            (1000u64..)
                .find(|&g| Route::Guild(g).worker(WORKERS) == w)
                .unwrap()
        })
        .collect();

    let pause = Duration::from_millis(3000);
    let before = update_errors("timeout");
    let paused = Pause::all(pause);
    let started = Instant::now();
    let (mut seq, mut opened, mut first_wave) = (1, None, None);
    // A steady stream for every worker until the pause is over.
    while started.elapsed() < pause + Duration::from_millis(200) {
        for (worker, guild) in guilds.iter().enumerate() {
            seq += 1;
            let d = message(seq, guild + 3, Some(*guild), "x");
            let job = job(&handle, "MESSAGE_CREATE", seq, d, true);
            queues[worker].send(job).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        match opened {
            None if !ctx.breaker.allow() => opened = Some(Instant::now()),
            // Updates let through before the breaker opened have ended (they
            // time out), and the first probe starts a cooldown after it.
            Some(at) if first_wave.is_none() && at.elapsed() > timeout + timeout / 2 => {
                first_wave = Some(update_errors("timeout") - before);
            }
            _ => {}
        }
    }
    drop(paused);
    let first_wave = first_wave.expect("the breaker opened");

    // After the cooldown that follows the last probe, the next update probes
    // a Redis that answers again and closes the breaker.
    tokio::time::sleep(BREAKER_COOLDOWN + timeout).await;
    seq += 1;
    let last = format!("message:{}:{seq}", guilds[0] + 3);
    let d = message(seq, guilds[0] + 3, Some(guilds[0]), "x");
    queues[0]
        .send(job(&handle, "MESSAGE_CREATE", seq, d, true))
        .await
        .unwrap();
    drop(queues);
    for worker in workers {
        worker.await.unwrap();
    }
    assert!(exists(&mut conn, &last).await, "the breaker closed again");
    assert!(ctx.breaker.allow());

    let probes = update_errors("timeout") - before - first_wave;
    let cycles = pause
        .as_millis()
        .div_ceil((timeout + BREAKER_COOLDOWN).as_millis()) as u64;
    assert!(
        (1..=cycles + 1).contains(&probes),
        "{probes} timeouts after the breaker opened (at most {} expected; \
         {first_wave} before)",
        cycles + 1
    );
    assert_eq!(
        recorder.published.lock().unwrap().len(),
        seq as usize - 1,
        "every event was published"
    );
    assert_eq!(handle.in_flight(), 0);
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
