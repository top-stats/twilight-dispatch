#![recursion_limit = "256"]

//! Session persistence against a real Redis (set TEST_REDIS_URL), and which
//! saved sessions `gateway::create` resumes (against a fake Discord).

mod common;
#[path = "support/fake_discord.rs"]
mod fake_discord;

use common::*;
use fake_discord::{FakeDiscord, Options, SessionStartLimit};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use twilight_dispatch::{
    app,
    config::Config,
    gateway::{
        self,
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

// ---------------------------------------------------------------------------
// gateway::create

fn session(id: &str, sequence: u64, resume_url: Option<&str>) -> SessionInfo {
    SessionInfo {
        session_id: id.to_owned(),
        sequence,
        resume_url: resume_url.map(str::to_owned),
    }
}

/// What `tracing` logs on this thread while the guard lives.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn capture() -> (Self, tracing::subscriber::DefaultGuard) {
        let logs = Self::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        (logs, tracing::subscriber::set_default(subscriber))
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// Discord's answer to `GET /gateway/bot`: `remaining` of 1000 identifies.
fn limit(remaining: u32, max_concurrency: u16) -> SessionStartLimit {
    SessionStartLimit {
        total: 1000,
        remaining,
        reset_after: 86_400_000,
        max_concurrency,
    }
}

/// `gateway::create` for shards 0 and 1 of 2 (plus `overrides`) against a
/// fake Discord that recommends `recommended` shards, and the warnings it
/// logged. Nothing connects: shards only do once they are polled.
async fn create(
    recommended: u32,
    limit: SessionStartLimit,
    overrides: &[(&'static str, &str)],
    saved: HashMap<u32, SessionInfo>,
) -> (gateway::Shards, String) {
    // main() installs it; twilight's rustls connectors panic without one.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let fake = FakeDiscord::with_options(Options {
        shards: recommended,
        session_start_limit: limit,
        ..Options::default()
    })
    .await;
    let mut vars: HashMap<&'static str, String> = [
        ("BOT_TOKEN", "sessions-token"),
        ("SHARDS_START", "0"),
        ("SHARDS_END", "1"),
        ("SHARDS_TOTAL", "2"),
        ("INTENTS", "513"),
        ("REDIS_URL", "redis://127.0.0.1:6379"),
        ("AMQP_URL", "amqp://127.0.0.1:5672/%2f"),
    ]
    .into_iter()
    .map(|(name, value)| (name, value.to_owned()))
    .collect();
    vars.extend(fake.env());
    for &(name, value) in overrides {
        vars.insert(name, value.to_owned());
    }
    let config = Config::from_lookup(&|name| vars.get(name).cloned()).expect("valid config");
    let http = app::http_client(&config);
    let (logs, _guard) = Logs::capture();
    let created = gateway::create(&config, &http, saved)
        .await
        .expect("create the shards");
    (created, logs.text())
}

/// A session can only be resumed by the shard that made it: `create` resumes
/// what was saved for its own shards and identifies the others.
#[tokio::test]
async fn shards_resume_only_their_own_saved_sessions() {
    let saved = HashMap::from([
        (0, session("abc", 42, Some("wss://resume-0.discord.gg"))),
        // Nothing was received in that session.
        (1, session("lost", 0, None)),
        // Another process' shard.
        (5, session("other", 9, None)),
    ]);
    let (created, _) = create(2, limit(1000, 1), &[], saved.clone()).await;
    let ids: Vec<u32> = created.shards.iter().map(|s| s.id().number()).collect();
    assert_eq!(ids, [0, 1]);
    assert_eq!(created.resumed, HashMap::from([(0, saved[&0].clone())]));
    let resumed = created.shards[0].session().expect("shard 0 resumes");
    assert_eq!((resumed.id(), resumed.sequence()), ("abc", 42));
    assert_eq!(
        created.shards[0].resume_url(),
        Some("wss://resume-0.discord.gg")
    );
    assert!(created.shards[1].session().is_none(), "shard 1 identifies");

    let (created, _) = create(2, limit(1000, 1), &[("RESUME", "false")], saved).await;
    assert!(created.resumed.is_empty());
    assert!(created.shards.iter().all(|s| s.session().is_none()));
}

/// Startup warns when the configuration costs what Discord would have
/// avoided (disconnects with 4011, a wait for the identify limit, slower
/// identifies), and only then: each warning is checked on both sides of its
/// threshold.
#[tokio::test]
async fn startup_warns_about_too_few_shards_identifies_and_concurrency() {
    const TOO_FEW_SHARDS: &str = "SHARDS_TOTAL is 2 but Discord recommends 3 shards";
    const NO_IDENTIFIES: &str = "Only 0 of 1000 daily identifies left but 1 shards need one";
    const LOWERED: &str =
        "SHARDS_CONCURRENCY=3 does not divide Discord's max_concurrency 16; using 2";
    type Case = (
        &'static str,
        u32,
        SessionStartLimit,
        &'static [(&'static str, &'static str)],
        HashMap<u32, SessionInfo>,
        &'static [&'static str],
    );
    let none = HashMap::new;
    let shard_0 = || HashMap::from([(0, session("abc", 42, None))]);
    let cases: [Case; 9] = [
        (
            "fewer shards",
            3,
            limit(1000, 1),
            &[],
            none(),
            &[TOO_FEW_SHARDS],
        ),
        ("recommended shards", 2, limit(1000, 1), &[], none(), &[]),
        ("more shards", 1, limit(1000, 1), &[], none(), &[]),
        // Shard 0 resumes, so only shard 1 needs an identify.
        (
            "no identify left",
            2,
            limit(0, 1),
            &[],
            shard_0(),
            &[NO_IDENTIFIES],
        ),
        ("one identify left", 2, limit(1, 1), &[], shard_0(), &[]),
        ("two identifies left", 2, limit(2, 1), &[], none(), &[]),
        (
            "concurrency lowered",
            2,
            limit(1000, 16),
            &[("SHARDS_CONCURRENCY", "3")],
            none(),
            &[LOWERED],
        ),
        (
            "concurrency divides",
            2,
            limit(1000, 16),
            &[("SHARDS_CONCURRENCY", "4")],
            none(),
            &[],
        ),
        (
            "Discord's concurrency",
            2,
            limit(1000, 16),
            &[],
            none(),
            &[],
        ),
    ];
    for (case, recommended, limit, overrides, saved, expected) in cases {
        let (_, logs) = create(recommended, limit, overrides, saved).await;
        let warned: Vec<&str> = [TOO_FEW_SHARDS, NO_IDENTIFIES, LOWERED]
            .into_iter()
            .filter(|warning| logs.contains(warning))
            .collect();
        assert_eq!(warned, expected, "{case}: {logs}");
        assert_eq!(
            logs.lines().filter(|line| line.contains("WARN")).count(),
            expected.len(),
            "{case}: {logs}"
        );
    }
}
