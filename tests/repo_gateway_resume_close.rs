#![recursion_limit = "256"]

//! The vendored twilight-gateway's patch (vendor/twilight-gateway/PATCHES.md)
//! in Discord's usual case: a shard restored from a saved session (as after a
//! restart) sends RESUME, and the gateway answers by closing with 4007
//! (invalid sequence) or 4009 (session timed out). The shard must then
//! IDENTIFY a new session instead of resuming the dead one forever, while a
//! resumable close (4000) still resumes.
//!
//! A bare twilight `Shard` against the fake gateway of
//! `tests/support/fake_discord.rs`: no Redis or broker, so these tests always
//! run. twilight waits 1 s before every connection, so each takes about 2 s.

#[path = "support/fake_discord.rs"]
mod fake_discord;

use fake_discord::{FakeDiscord, GatewayConnection};
use futures_util::StreamExt;
use serde_json::json;
use std::time::Duration;
use tokio::task::JoinHandle;
use twilight_gateway::{queue::InMemoryQueue, ConfigBuilder, Intents, Session, Shard, ShardId};

/// The saved session the shard is restored with.
const SESSION: &str = "dead";
const SEQUENCE: u64 = 5;
/// No heartbeat is due during a test.
const HEARTBEAT_MS: u64 = 86_400_000;

/// Polls the shard until it ends; what it receives does not matter here.
async fn drive(mut shard: Shard<InMemoryQueue>) {
    while shard.next().await.is_some() {}
}

/// Starts a shard restored from `SESSION` at `SEQUENCE` against a new fake
/// gateway and answers its RESUME by closing with `code`. Returns the fake,
/// the shard's next connection and the task polling the shard.
async fn resume_answered_with(code: u16) -> (FakeDiscord, GatewayConnection, JoinHandle<()>) {
    // main() installs it; twilight's TLS connector panics without one.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let fake = FakeDiscord::start().await;
    // A max_concurrency of 0 grants every IDENTIFY at once.
    let queue = InMemoryQueue::new(0, 1000, Duration::from_secs(86_400), 1000);
    let config = ConfigBuilder::new("token".to_owned(), Intents::GUILDS)
        .proxy_url(fake.gateway.url())
        .queue(queue)
        .session(Session::new(SEQUENCE, SESSION.to_owned()))
        .resume_url(fake.gateway.resume_url())
        .build();
    let task = tokio::spawn(drive(Shard::with_config(ShardId::ONE, config)));

    let mut conn = fake.gateway.accept().await;
    assert!(conn.is_resume(), "connected to {} instead", conn.path);
    conn.hello(HEARTBEAT_MS).await;
    let resume = conn.expect_resume().await;
    assert_eq!(resume["session_id"], SESSION);
    assert_eq!(resume["seq"], SEQUENCE);
    conn.close(code, "resume refused").await;
    assert_eq!(conn.expect_close().await, Some(code), "the client's echo");

    let next = fake.gateway.accept().await;
    (fake, next, task)
}

/// The shard dropped the dead session: it connects to the gateway URL, not
/// the session's resume URL, and IDENTIFYs.
async fn identifies_after(code: u16) {
    let (_fake, mut next, task) = resume_answered_with(code).await;
    assert_eq!(next.path, "/", "not the dead session's resume URL");
    next.hello(HEARTBEAT_MS).await;
    // expect_identify fails on a RESUME.
    assert_eq!(next.expect_identify().await["shard"], json!([0, 1]));
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_answered_with_4007_identifies() {
    identifies_after(4007).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_answered_with_4009_identifies() {
    identifies_after(4009).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resume_answered_with_4000_resumes_again() {
    let (_fake, mut next, task) = resume_answered_with(4000).await;
    assert!(next.is_resume(), "connected to {} instead", next.path);
    next.hello(HEARTBEAT_MS).await;
    let resume = next.expect_resume().await;
    assert_eq!(resume["session_id"], SESSION);
    assert_eq!(resume["seq"], SEQUENCE);
    task.abort();
}
