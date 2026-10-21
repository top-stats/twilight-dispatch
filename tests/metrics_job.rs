#![recursion_limit = "256"]

//! The metrics job's gateway gauges, read from the text exposition in a
//! process of their own: reading a label through `with_label_values` creates
//! it at 0, so only a binary that never does that for `gateway_statuses`
//! shows which labels the job itself sets. Nothing in this binary may call
//! `GATEWAY_STATUSES.with_label_values`.
//!
//! Skipped unless TEST_AMQP_URL is set: the job needs a broker connection.

mod common;

use common::*;
use prometheus::Encoder;
use redis::{aio::ConnectionLike, Cmd, Pipeline, RedisFuture, Value};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use twilight_dispatch::{
    amqp::Amqp,
    gateway::registry::{Registry, ShardHandle, STATUS_LABELS},
    metrics,
};

/// Answers every command of a pipeline with 0: an empty cache.
struct Zeros;

impl ConnectionLike for Zeros {
    fn req_packed_command<'a>(&'a mut self, _cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        Box::pin(async { Ok(Value::Int(0)) })
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        _pipeline: &'a Pipeline,
        _offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        Box::pin(async move { Ok(vec![Value::Int(0); count]) })
    }

    fn get_db(&self) -> i64 {
        0
    }
}

/// What /metrics lists. Gathering creates no child.
fn exposition() -> String {
    let mut buffer = Vec::new();
    prometheus::TextEncoder::new()
        .encode(&prometheus::gather(), &mut buffer)
        .unwrap();
    String::from_utf8(buffer).unwrap()
}

/// Whether /metrics has exactly this line.
fn has(line: &str) -> bool {
    exposition().lines().any(|l| l == line)
}

fn status_line(label: &str, count: i64) -> String {
    format!("gateway_statuses{{type=\"{label}\"}} {count}")
}

/// Poll `check` until it holds; the job runs once a second.
async fn wait_until(what: &str, check: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !check() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}\n{}",
            exposition()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// The default current-thread runtime runs the job only while this test
// awaits, and a run sets every gateway gauge without awaiting, so a check
// never sees a run half done.
#[tokio::test]
async fn every_status_is_listed_and_one_no_shard_has_goes_back_to_0() {
    let Some(url) = amqp_url() else {
        return;
    };
    let handle = Arc::new(ShardHandle::new(0, None, None));
    handle.update(|s| {
        s.status = "Connected";
        s.active = true;
        // Too large for a gauge.
        s.latency_ms = u64::MAX;
        s.last_message = Some(Instant::now());
    });
    let amqp = Amqp::connect(url, false).await.unwrap();
    let job = tokio::spawn(metrics::run_jobs(
        Zeros,
        Arc::new(Registry::new([handle.clone()])),
        amqp.clone(),
        Duration::from_secs(180),
    ));

    wait_until("a first run", || has(&status_line("Connected", 1))).await;
    for label in STATUS_LABELS {
        if label != "Connected" {
            assert!(
                has(&status_line(label, 0)),
                "{label} is listed as 0\n{}",
                exposition()
            );
        }
    }
    assert!(has("gateway_shards 1"), "{}", exposition());
    assert_eq!(
        metrics::GATEWAY_LATENCIES.with_label_values(&["0"]).get(),
        i64::MAX,
        "a latency too large for a gauge saturates instead of wrapping"
    );

    handle.update(|s| {
        s.status = "Resuming";
        s.active = false;
    });
    wait_until("the next run", || has(&status_line("Resuming", 1))).await;
    assert!(
        has(&status_line("Connected", 0)),
        "a status no shard has any more goes back to 0\n{}",
        exposition()
    );

    job.abort();
    amqp.close().await;
}
