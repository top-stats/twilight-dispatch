//! Startup failures of `app::run` and `app::run_until` that come before any
//! service is contacted, and the REST client `app::http_client` builds
//! without `DISCORD_API_PROXY`. No service is needed: nothing listens where
//! the configuration points Redis and the broker.

use std::collections::HashMap;
use twilight_dispatch::{app, config::Config, models::ApiError};

type Vars = HashMap<&'static str, String>;

/// The variables of tests/cli.rs `base_vars`, with the metrics server on
/// `prometheus_port` and Redis and the broker on port 1, where nothing
/// listens: reaching them would fail differently (and slowly).
fn vars(prometheus_port: u16) -> Vars {
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
        ("REDIS_URL", "redis://127.0.0.1:1/0"),
        ("AMQP_URL", "amqp://127.0.0.1:1/%2f"),
    ]
    .into_iter()
    .map(|(name, value)| (name, value.to_owned()))
    .chain([("PROMETHEUS_PORT", prometheus_port.to_string())])
    .collect()
}

fn config(vars: &Vars) -> Config {
    Config::from_lookup(&|name| vars.get(name).cloned()).expect("valid config")
}

/// A port nothing listens on right now.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// The startup error of a metrics port that is taken names the address and
/// the variables that set it, and keeps the I/O error and its kind.
#[tokio::test]
async fn a_busy_metrics_port_is_named_in_the_error() {
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = busy.local_addr().unwrap().port();

    let err = app::run_until(config(&vars(port)), std::future::pending::<()>())
        .await
        .expect_err("the port is taken");

    let ApiError::Io(io) = &err else {
        panic!("not an I/O error: {err}");
    };
    assert_eq!(io.kind(), std::io::ErrorKind::AddrInUse, "{err}");
    let message = io.to_string();
    let named = format!("cannot listen on 127.0.0.1:{port} (PROMETHEUS_HOST, PROMETHEUS_PORT): ");
    assert!(message.starts_with(&named), "{message}");
    assert!(
        message.len() > named.len(),
        "the OS error follows: {message}"
    );
    drop(busy);
}

/// `run`, which `main` calls and which waits for SIGTERM or SIGINT once
/// started, returns a startup error at once instead of waiting for a signal.
#[tokio::test]
async fn run_returns_a_startup_error_without_waiting_for_a_signal() {
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = busy.local_addr().unwrap().port();

    let err = app::run(config(&vars(port)))
        .await
        .expect_err("the port is taken");

    assert!(matches!(err, ApiError::Io(_)), "{err}");
    assert!(
        err.to_string().contains(&format!("127.0.0.1:{port}")),
        "{err}"
    );
    drop(busy);
}

/// `Config::from_lookup` stores PROMETHEUS_HOST as an IP address; a Config
/// built or changed by hand need not hold one, and is rejected before
/// anything is bound or contacted.
#[tokio::test]
async fn a_hand_built_metrics_host_that_is_no_ip_address_is_rejected() {
    let mut config = config(&vars(free_port()));
    config.prometheus_host = "metrics.internal".to_owned();

    let err = app::run_until(config, std::future::pending::<()>())
        .await
        .expect_err("not an IP address");

    let ApiError::Other(message) = &err else {
        panic!("not a described failure: {err}");
    };
    assert!(message.starts_with("invalid PROMETHEUS_HOST: "), "{err}");
}

/// Without `DISCORD_API_PROXY` the REST client talks to Discord itself. It is
/// only built here: nothing may reach Discord from a test.
#[tokio::test]
async fn the_rest_client_is_built_without_a_proxy() {
    // main() installs it; twilight's rustls connectors panic without one.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let config = config(&vars(free_port()));
    assert!(config.discord_api_proxy.is_none());

    let _client = app::http_client(&config);
}
