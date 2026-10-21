//! The /metrics and /healthcheck server and what /healthcheck reports: which
//! connection errors the server logs, how it gets through an accept error
//! and a scrape it cannot encode, and the precedence and limits of
//! `compute_health_at`. No services needed.

use prometheus::{
    core::{Collector, Desc},
    proto::{Metric, MetricFamily},
};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use twilight_dispatch::{
    gateway::registry::ShardSnapshot,
    metrics::{self, compute_health_at, Health, CLOSE_WAIT_LIMIT},
};

/// What `tracing` logs on this thread while the guard lives. A
/// `tokio::test` runtime runs the server's tasks on the test's thread, so the
/// server logs here.
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

/// A metrics server on a free port of 127.0.0.1.
async fn metrics_server() -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (port, tokio::spawn(metrics::run_server(listener)))
}

/// Send `request` on a new connection and read until the server closes it.
async fn send(port: u16, request: &[u8]) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream.write_all(request).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), stream.read_to_end(&mut response))
        .await
        .expect("the server closes the connection")
        .unwrap();
    String::from_utf8_lossy(&response).into_owned()
}

// ---------------------------------------------------------------------------
// The server

#[tokio::test]
async fn an_idle_keep_alive_connection_is_closed_quietly() {
    let (logs, _guard) = Logs::capture();
    let (port, server) = metrics_server().await;

    // No `Connection: close`: HTTP/1.1 keeps the connection open for the next
    // request, as a scraper does. hyper closes it at the 10 s header read
    // timeout, which is no error to log after every scrape.
    let response = send(port, b"GET /healthcheck HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let error = "Metrics connection error";
    assert!(!logs.text().contains(error), "{}", logs.text());

    server.abort();
}

/// A collector whose one family has no name, which the text encoder refuses.
struct Nameless;

impl Collector for Nameless {
    fn desc(&self) -> Vec<&Desc> {
        Vec::new()
    }

    fn collect(&self) -> Vec<MetricFamily> {
        let mut family = MetricFamily::default();
        family.set_metric(vec![Metric::default()]);
        vec![family]
    }
}

#[tokio::test]
async fn a_scrape_that_cannot_be_encoded_answers_500() {
    let (logs, _guard) = Logs::capture();
    let (port, server) = metrics_server().await;
    let scrape = b"GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";

    prometheus::register(Box::new(Nameless)).unwrap();
    let response = send(port, scrape).await;
    // Gone again before anything is asserted, so a failure here cannot
    // break a scrape of another test.
    prometheus::unregister(Box::new(Nameless)).unwrap();
    assert!(response.starts_with("HTTP/1.1 500"), "{response}");
    assert!(
        logs.text().contains("Failed to encode metrics"),
        "{}",
        logs.text()
    );

    // The server answers the next scrape as usual.
    let response = send(port, scrape).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");

    server.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn an_accept_error_is_logged_and_the_server_keeps_accepting() {
    // Running out of file descriptors is the accept error a loaded host
    // meets. That would break every other test of this binary, so the test
    // runs again in a process of its own with a low descriptor limit.
    const FEW_FILES: &str = "TWILIGHT_DISPATCH_TEST_FEW_FILES";
    if std::env::var_os(FEW_FILES).is_none() {
        let test = "an_accept_error_is_logged_and_the_server_keeps_accepting";
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg("ulimit -n 128 && exec \"$0\" \"$@\"")
            .arg(std::env::current_exe().unwrap())
            .args([test, "--exact"])
            .env(FEW_FILES, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&child.stdout);
        assert!(
            child.status.success() && stdout.contains("test result: ok. 1 passed"),
            "{stdout}{}",
            String::from_utf8_lossy(&child.stderr)
        );
        return;
    }

    let (logs, _guard) = Logs::capture();
    // Registers every metric while files can still be opened.
    metrics::init();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    // Connected, and waiting in the listen queue until the server accepts it.
    let mut client = TcpStream::connect(address).await.unwrap();

    // Use up every descriptor left, so that accepting the client fails.
    let mut files = Vec::new();
    while let Ok(file) = std::fs::File::open("/dev/null") {
        files.push(file);
        assert!(files.len() < 1024, "the limit is 128 descriptors");
    }
    let server = tokio::spawn(metrics::run_server(listener));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !logs.text().contains("Metrics accept error") {
        assert!(Instant::now() < deadline, "{}", logs.text());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Once descriptors are free again, the server accepts the waiting client
    // and answers it.
    drop(files);
    let request = b"GET /healthcheck HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
    client.write_all(request).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), client.read_to_end(&mut response))
        .await
        .expect("the waiting client is answered")
        .unwrap();
    let response = String::from_utf8_lossy(&response);
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");

    server.abort();
}

// ---------------------------------------------------------------------------
// compute_health_at

/// How long a connected shard may receive nothing in these tests.
const SILENCE: Duration = Duration::from_secs(180);
const SECOND: Duration = Duration::from_secs(1);

/// What /healthcheck reports for shard `id` alone at `now`, with the broker
/// connected.
fn health_of(id: u32, snapshot: ShardSnapshot, now: Instant) -> Health {
    compute_health_at(&[(id, snapshot)], true, SILENCE, now)
}

#[test]
fn the_limits_are_exclusive() {
    // An hour ahead, so `now - ago` never lies before the host booted.
    let now = Instant::now() + Duration::from_secs(3600);
    let connected = |ago: Duration| ShardSnapshot {
        status: "Connected",
        active: true,
        last_message: Some(now - ago),
        ..ShardSnapshot::default()
    };
    let closing = |ago: Duration| ShardSnapshot {
        close_wait_since: Some(now - ago),
        ..ShardSnapshot::default()
    };
    let problems = |snapshot: ShardSnapshot| health_of(0, snapshot, now).problems;

    assert!(problems(connected(SILENCE)).is_empty());
    assert_eq!(
        problems(connected(SILENCE + SECOND)),
        ["shard 0 is connected but received nothing for 181s"]
    );
    assert!(problems(closing(CLOSE_WAIT_LIMIT)).is_empty());
    assert_eq!(
        problems(closing(CLOSE_WAIT_LIMIT + SECOND)),
        ["shard 0 is stuck waiting for the gateway to close the connection"]
    );

    // Active but nothing received yet: there is no silence to measure.
    let waiting = ShardSnapshot {
        active: true,
        ..ShardSnapshot::default()
    };
    assert!(problems(waiting).is_empty());
}

#[test]
fn one_report_per_shard_by_precedence() {
    let now = Instant::now() + Duration::from_secs(3600);
    let long_ago = Some(now - Duration::from_secs(600));
    let everything = ShardSnapshot {
        status: "Connected",
        active: true,
        fatal: true,
        close_code: Some(4004),
        last_message: long_ago,
        close_wait_since: long_ago,
        ..ShardSnapshot::default()
    };

    // A fatal close wins: a warning, never a restart.
    let fatal = health_of(2, everything.clone(), now);
    assert!(fatal.problems.is_empty(), "{:?}", fatal.problems);
    assert_eq!(
        fatal.warnings,
        ["shard 2 was closed by Discord with a non-reconnectable code (4004)"]
    );

    // Then the close wait, reported instead of the silence.
    let not_fatal = ShardSnapshot {
        fatal: false,
        close_code: None,
        ..everything
    };
    let stuck = health_of(2, not_fatal.clone(), now);
    assert_eq!(
        stuck.problems,
        ["shard 2 is stuck waiting for the gateway to close the connection"]
    );
    assert!(stuck.warnings.is_empty(), "{:?}", stuck.warnings);

    // And the silence only when nothing else applies.
    let only_silent = ShardSnapshot {
        close_wait_since: None,
        ..not_fatal
    };
    let silent = health_of(2, only_silent, now);
    assert_eq!(
        silent.problems,
        ["shard 2 is connected but received nothing for 600s"]
    );
    assert!(silent.warnings.is_empty(), "{:?}", silent.warnings);
}
