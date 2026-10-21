#![recursion_limit = "256"]
//! The binary itself: the `healthcheck` subcommand the image's HEALTHCHECK
//! runs, exit codes, and (on Unix) a full run that `docker stop`'s SIGTERM
//! ends with a resumable close and a saved session.

mod common;
#[path = "support/fake_discord.rs"]
mod fake_discord;

use fake_discord::FakeDiscord;
use std::{
    path::PathBuf,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const BIN: &str = env!("CARGO_BIN_EXE_twilight-dispatch");

/// The binary with an empty environment, run outside the repository so a
/// developer's `.env` is not loaded (dotenvy looks in the working directory).
fn binary(args: &[&str], vars: &[(&str, String)]) -> Command {
    let dir: PathBuf = std::env::temp_dir().join("twilight-dispatch-cli-test");
    std::fs::create_dir_all(&dir).unwrap();
    let mut command = Command::new(BIN);
    command
        .args(args)
        .env_clear()
        .envs(vars.iter().map(|(name, value)| (name, value)))
        .current_dir(dir)
        .stdin(Stdio::null());
    command
}

fn output_text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// A metrics server stand-in that answers every request with `status`.
async fn answering(status: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut request = [0; 1024];
                let _ = stream.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}"
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    port
}

/// The variables every start needs, as tests/e2e.rs uses them.
fn base_vars() -> Vec<(&'static str, String)> {
    [
        ("BOT_TOKEN", "token"),
        ("SHARDS_START", "0"),
        ("SHARDS_END", "0"),
        ("SHARDS_TOTAL", "1"),
        ("INTENTS", "513"),
        ("LARGE_THRESHOLD", "100"),
        ("STATUS", "online"),
        ("ACTIVITY_TYPE", "3"),
        ("ACTIVITY_NAME", "the tests"),
        ("PROMETHEUS_HOST", "127.0.0.1"),
    ]
    .into_iter()
    .map(|(name, value)| (name, value.to_owned()))
    .chain([("PROMETHEUS_PORT", free_port().to_string())])
    .collect()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Run a blocking command off the runtime, so the test's servers keep
/// answering it.
async fn run(mut command: Command) -> Output {
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthcheck_exits_with_the_health_of_the_metrics_server() {
    let healthy = answering("200 OK").await;
    let unhealthy = answering("503 Service Unavailable").await;
    let closed = free_port();
    for (port, host, success) in [
        (healthy, Some("127.0.0.1"), true),
        // 0.0.0.0 is probed on loopback, as in the container.
        (healthy, Some("0.0.0.0"), true),
        (healthy, None, true),
        (unhealthy, Some("127.0.0.1"), false),
        (closed, Some("127.0.0.1"), false),
    ] {
        let mut vars = vec![("PROMETHEUS_PORT", port.to_string())];
        if let Some(host) = host {
            vars.push(("PROMETHEUS_HOST", host.to_owned()));
        }
        let output = run(binary(&["healthcheck"], &vars)).await;
        assert_eq!(
            output.status.success(),
            success,
            "port {port}, host {host:?}: {}",
            output_text(&output)
        );
    }
}

/// Without PROMETHEUS_PORT the probe uses the default port 8005, and
/// healthcheck needs no other configuration (no BOT_TOKEN).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthcheck_defaults_to_port_8005() {
    let Ok(listener) = TcpListener::bind("127.0.0.1:8005").await else {
        eprintln!("port 8005 is in use; skipped");
        return;
    };
    let accepted = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 1024];
        let read = stream.read(&mut request).await.unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        String::from_utf8_lossy(&request[..read]).into_owned()
    });
    let output = run(binary(
        &["healthcheck"],
        &[("PROMETHEUS_HOST", "127.0.0.1".into())],
    ))
    .await;
    assert!(output.status.success(), "{}", output_text(&output));
    assert!(accepted
        .await
        .unwrap()
        .starts_with("GET /healthcheck HTTP/1.1\r\n"));
}

#[test]
fn a_configuration_error_exits_with_failure_and_says_why() {
    let mut vars = base_vars();
    vars.retain(|(name, _)| *name != "BOT_TOKEN");
    vars.extend([
        ("REDIS_URL", "redis://127.0.0.1".to_owned()),
        ("AMQP_URL", "amqp://127.0.0.1/%2f".to_owned()),
    ]);
    let output = binary(&[], &vars).output().unwrap();
    assert!(!output.status.success());
    let text = output_text(&output);
    assert!(
        text.contains("missing environment variable BOT_TOKEN"),
        "{text}"
    );
}

/// A service that cannot reach Redis fails its startup (and exits non-zero,
/// so Swarm restarts it) instead of hanging; obsolete variables are named.
#[test]
fn startup_failures_exit_with_failure() {
    let mut vars = base_vars();
    vars.extend([
        ("REDIS_URL", format!("redis://127.0.0.1:{}", free_port())),
        ("AMQP_URL", format!("amqp://127.0.0.1:{}/%2f", free_port())),
        ("CLUSTERS", "2".into()),
    ]);
    let started = Instant::now();
    let output = binary(&[], &vars).output().unwrap();
    assert!(!output.status.success());
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
    let text = output_text(&output);
    assert!(text.contains("CLUSTERS is no longer used"), "{text}");
}

/// `docker stop` sends SIGTERM to the process: the shard closes resumably
/// (4000), the session is saved for the next start, and the process exits 0
/// well within Docker's 10 s grace period.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sigterm_closes_resumably_saves_the_session_and_exits() {
    use redis::AsyncCommands;

    let Some(mut redis) = common::redis(15).await else {
        return;
    };
    let Some(amqp) = common::amqp_url() else {
        return;
    };
    let redis_url = std::env::var("TEST_REDIS_URL").unwrap();
    let _ = rustls::crypto::ring::default_provider().install_default();
    let fake = FakeDiscord::start().await;
    let mut vars = base_vars();
    vars.extend([
        (
            "REDIS_URL",
            format!("{}/15", redis_url.trim_end_matches('/')),
        ),
        ("AMQP_URL", amqp),
        ("RUST_LOG", "info".into()),
    ]);
    vars.extend(fake.env());
    let child = binary(&[], &vars)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut conn = fake.gateway.accept().await;
    conn.hello(41_250).await;
    conn.expect_identify().await;
    conn.ready(1, "session", [0, 1], &[]).await;
    conn.dispatch(
        "TYPING_START",
        2,
        &serde_json::json!({"channel_id": "1", "user_id": "2"}),
    )
    .await;
    // Saved sessions show the process has the session; wait for it.
    let deadline = Instant::now() + fake_discord::TIMEOUT;
    loop {
        let saved: Option<String> = redis.get("gateway_session:0").await.unwrap();
        if saved.is_some_and(|saved| saved.contains("\"session\"")) {
            break;
        }
        assert!(Instant::now() < deadline, "no session saved");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let stopped = Instant::now();
    let kill = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(kill.success());
    assert_eq!(conn.expect_close().await, Some(4000), "closed resumably");
    let status = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
        .await
        .unwrap();
    assert!(
        status.status.success(),
        "exit {:?}: {}",
        status.status,
        output_text(&status)
    );
    assert!(
        stopped.elapsed() < Duration::from_secs(10),
        "{:?}",
        stopped.elapsed()
    );

    let saved: String = redis.get("gateway_session:0").await.unwrap();
    let saved: serde_json::Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(saved["session_id"], "session");
    assert_eq!(saved["sequence"], 2);
    assert_eq!(saved["resume_url"], fake.gateway.resume_url());
}
