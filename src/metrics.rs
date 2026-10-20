//! Prometheus metrics, the /metrics + /healthcheck server and the metrics job.
//!
//! Metric names of earlier versions are unchanged. New: `gateway_parse_errors`,
//! `state_update_errors`, `amqp_publish_failures`, `amqp_reconnects`,
//! `amqp_connected`, `gateway_shard_fatal`, `gateway_shard_silence_seconds`.

use crate::{
    amqp::Amqp,
    cache::keys::type_index,
    constants::{
        CHANNEL_KEY, EMOJI_KEY, GUILD_KEY, MEMBER_KEY, MESSAGE_KEY, METRICS_JOB_INTERVAL_MS,
        PRESENCE_KEY, ROLE_KEY, VOICE_KEY,
    },
    gateway::registry::{Registry, ShardSnapshot, STATUS_LABELS},
    models::ApiResult,
};

use bytes::Bytes;
use http_body_util::Full;
use hyper::{
    body::Incoming,
    header::{HeaderValue, CONTENT_TYPE},
    server::conn::http1,
    service::service_fn,
    Method, Request, Response, StatusCode,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use prometheus::{
    register_int_counter, register_int_counter_vec, register_int_gauge, register_int_gauge_vec,
    Encoder, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, TextEncoder,
};
use redis::aio::ConnectionLike;
use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{Arc, LazyLock, RwLock},
    time::Duration,
};
use tokio::net::TcpListener;
use tracing::warn;

macro_rules! counter_vec {
    ($name:ident, $metric:literal, $help:literal, $labels:expr) => {
        pub static $name: LazyLock<IntCounterVec> =
            LazyLock::new(|| register_int_counter_vec!($metric, $help, $labels).unwrap());
    };
}
macro_rules! counter {
    ($name:ident, $metric:literal, $help:literal) => {
        pub static $name: LazyLock<IntCounter> =
            LazyLock::new(|| register_int_counter!($metric, $help).unwrap());
    };
}
macro_rules! gauge {
    ($name:ident, $metric:literal, $help:literal) => {
        pub static $name: LazyLock<IntGauge> =
            LazyLock::new(|| register_int_gauge!($metric, $help).unwrap());
    };
}
macro_rules! gauge_vec {
    ($name:ident, $metric:literal, $help:literal, $labels:expr) => {
        pub static $name: LazyLock<IntGaugeVec> =
            LazyLock::new(|| register_int_gauge_vec!($metric, $help, $labels).unwrap());
    };
}

counter_vec!(
    GATEWAY_EVENTS,
    "gateway_events",
    "Events received through the Discord gateway",
    &["type", "shard"]
);
counter_vec!(
    SHARD_EVENTS,
    "gateway_shard_events",
    "Discord shard connection events",
    &["type"]
);
counter_vec!(
    GUILD_EVENTS,
    "gateway_guild_events",
    "Discord guild join and leave events",
    &["type"]
);
gauge!(
    GATEWAY_SHARDS,
    "gateway_shards",
    "Number of gateway connections with Discord"
);
gauge_vec!(
    GATEWAY_STATUSES,
    "gateway_statuses",
    "Status of the gateway connections",
    &["type"]
);
gauge_vec!(
    GATEWAY_LATENCIES,
    "gateway_latencies",
    "API latency with the Discord gateway",
    &["shard"]
);
gauge!(
    STATE_GUILDS,
    "state_guilds",
    "Number of guilds in state cache"
);
gauge!(
    STATE_CHANNELS,
    "state_channels",
    "Number of channels in state cache"
);
gauge!(
    STATE_MESSAGES,
    "state_messages",
    "Number of messages in state cache"
);
gauge!(STATE_ROLES, "state_roles", "Number of roles in state cache");
gauge!(
    STATE_EMOJIS,
    "state_emojis",
    "Number of emojis in state cache"
);
gauge!(
    STATE_MEMBERS,
    "state_members",
    "Number of members in state cache"
);
gauge!(
    STATE_PRESENCES,
    "state_presences",
    "Number of presences in state cache"
);
gauge!(
    STATE_VOICES,
    "state_voices",
    "Number of voices in state cache"
);
counter_vec!(
    GATEWAY_PARSE_ERRORS,
    "gateway_parse_errors",
    "Dispatch payloads that could not be parsed for the cache",
    &["type"]
);
counter_vec!(
    STATE_UPDATE_ERRORS,
    "state_update_errors",
    "Failed or skipped state cache updates",
    &["reason"]
);
counter!(
    AMQP_PUBLISH_FAILURES,
    "amqp_publish_failures",
    "Gateway events that could not be published"
);
counter!(
    AMQP_RECONNECTS,
    "amqp_reconnects",
    "Reconnects to the AMQP broker"
);
gauge!(
    AMQP_CONNECTED,
    "amqp_connected",
    "1 while connected to the AMQP broker"
);
gauge_vec!(
    SHARD_FATAL,
    "gateway_shard_fatal",
    "1 when a shard was closed with a non-reconnectable code",
    &["shard"]
);
gauge_vec!(
    SHARD_SILENCE,
    "gateway_shard_silence_seconds",
    "Seconds since a shard last received anything",
    &["shard"]
);

/// What /healthcheck reports.
///
/// `problems` make it return 503, which makes Docker / Swarm restart the
/// container, so only conditions a restart fixes belong there (a shard that is
/// connected but stuck). Broker outages and fatal close codes are `warnings`:
/// restarting would only re-identify every shard, which burns the daily
/// identify limit (Discord resets the token when it runs out).
#[derive(Clone, Debug, Default)]
pub struct Health {
    pub problems: Vec<String>,
    pub warnings: Vec<String>,
}

static HEALTH: LazyLock<RwLock<Health>> = LazyLock::new(|| RwLock::new(Health::default()));

pub fn set_health(health: Health) {
    if let Ok(mut current) = HEALTH.write() {
        *current = health;
    }
}

fn reply(status: StatusCode, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    let mut res = Response::new(Full::new(body.into()));
    *res.status_mut() = status;
    res
}

fn metrics_response() -> Response<Full<Bytes>> {
    let mut buffer = Vec::new();
    if let Err(err) = TextEncoder::new().encode(&prometheus::gather(), &mut buffer) {
        warn!("Failed to encode metrics: {}", err);
        return reply(StatusCode::INTERNAL_SERVER_ERROR, Bytes::new());
    }
    let mut res = reply(StatusCode::OK, buffer);
    res.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static(prometheus::TEXT_FORMAT),
    );
    res
}

/// The /healthcheck status and body. A healthy process without warnings still
/// answers exactly `{"status":"OK"}` like earlier versions.
pub fn health_body(health: &Health) -> (StatusCode, String) {
    match (health.problems.is_empty(), health.warnings.is_empty()) {
        (true, true) => (StatusCode::OK, r#"{"status":"OK"}"#.to_owned()),
        (true, false) => (
            StatusCode::OK,
            serde_json::json!({"status": "OK", "warnings": health.warnings}).to_string(),
        ),
        (false, _) => (
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({
                "status": "UNHEALTHY",
                "problems": health.problems,
                "warnings": health.warnings,
            })
            .to_string(),
        ),
    }
}

fn health_response() -> Response<Full<Bytes>> {
    let health = HEALTH.read().map(|h| h.clone()).unwrap_or_default();
    let (status, body) = health_body(&health);
    let mut res = reply(status, body);
    res.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    res
}

async fn serve(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
    Ok(match (req.method(), req.uri().path()) {
        (&Method::GET, "/metrics") => metrics_response(),
        (&Method::GET, "/healthcheck") => health_response(),
        _ => reply(StatusCode::NOT_FOUND, Bytes::new()),
    })
}

/// Serve /metrics and /healthcheck. Bind the listener first so a bind error
/// fails startup.
pub async fn run_server(listener: TcpListener) {
    loop {
        let (stream, _) = match listener.accept().await {
            Ok(conn) => conn,
            Err(err) => {
                warn!("Metrics accept error: {}", err);
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        tokio::spawn(async move {
            let mut builder = http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(10));
            let conn = builder.serve_connection(TokioIo::new(stream), service_fn(serve));
            if let Err(err) = conn.await {
                if !err.is_incomplete_message() {
                    warn!("Metrics connection error: {}", err);
                }
            }
        });
    }
}

/// What /healthcheck reports for the current shard snapshots.
pub fn compute_health(
    snapshots: &[(u32, ShardSnapshot)],
    amqp_connected: bool,
    shard_silence: Duration,
) -> Health {
    let mut health = Health::default();
    for (shard, snapshot) in snapshots {
        let silence = snapshot.silence();
        if snapshot.fatal {
            health.warnings.push(format!(
                "shard {shard} was closed by Discord with a non-reconnectable code{}",
                snapshot
                    .close_code
                    .map(|code| format!(" ({code})"))
                    .unwrap_or_default()
            ));
        } else if snapshot.active && silence.is_some_and(|s| s > shard_silence) {
            health.problems.push(format!(
                "shard {shard} is connected but received nothing for {}s",
                silence.map_or(0, |s| s.as_secs())
            ));
        }
    }
    if !amqp_connected {
        health
            .warnings
            .push("not connected to the AMQP broker".to_owned());
    }
    health
}

async fn state_counts<C: ConnectionLike + Send>(conn: &mut C) -> ApiResult<Vec<u64>> {
    let mut pipe = redis::pipe();
    for prefix in [
        GUILD_KEY,
        CHANNEL_KEY,
        MESSAGE_KEY,
        ROLE_KEY,
        EMOJI_KEY,
        MEMBER_KEY,
        PRESENCE_KEY,
        VOICE_KEY,
    ] {
        pipe.cmd("SCARD").arg(type_index(prefix));
    }
    Ok(pipe.query_async(conn).await?)
}

/// Update gauges and the health state every second.
pub async fn run_jobs<C: ConnectionLike + Send>(
    mut conn: C,
    registry: Arc<Registry>,
    amqp: Arc<Amqp>,
    shard_silence: Duration,
) {
    loop {
        let snapshots = registry.snapshots();
        GATEWAY_SHARDS.set(snapshots.len() as i64);

        let mut statuses: HashMap<&'static str, i64> =
            STATUS_LABELS.iter().map(|label| (*label, 0)).collect();
        for (shard, snapshot) in &snapshots {
            let label = shard.to_string();
            *statuses.entry(snapshot.status).or_default() += 1;
            GATEWAY_LATENCIES
                .with_label_values(&[label.as_str()])
                .set(snapshot.latency_ms as i64);
            SHARD_FATAL
                .with_label_values(&[label.as_str()])
                .set(i64::from(snapshot.fatal));
            SHARD_SILENCE
                .with_label_values(&[label.as_str()])
                .set(snapshot.silence().map_or(0, |s| s.as_secs() as i64));
        }
        for (status, count) in statuses {
            GATEWAY_STATUSES.with_label_values(&[status]).set(count);
        }
        AMQP_CONNECTED.set(i64::from(amqp.is_connected()));
        set_health(compute_health(
            &snapshots,
            amqp.is_connected(),
            shard_silence,
        ));

        match state_counts(&mut conn).await {
            Ok(counts) if counts.len() == 8 => {
                for (gauge, value) in [
                    &*STATE_GUILDS,
                    &*STATE_CHANNELS,
                    &*STATE_MESSAGES,
                    &*STATE_ROLES,
                    &*STATE_EMOJIS,
                    &*STATE_MEMBERS,
                    &*STATE_PRESENCES,
                    &*STATE_VOICES,
                ]
                .into_iter()
                .zip(counts)
                {
                    gauge.set(value as i64);
                }
            }
            Ok(_) => {}
            Err(err) => warn!("Failed to get state stats: {}", err),
        }

        tokio::time::sleep(Duration::from_millis(METRICS_JOB_INTERVAL_MS)).await;
    }
}

/// The addresses the healthcheck probe tries: loopback (IPv4, then IPv6)
/// when the server listens on every address, the configured address
/// otherwise.
pub fn probe_addresses(host: Option<&str>) -> Vec<std::net::IpAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    let loopback = vec![
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
    ];
    match host.and_then(crate::config::parse_host) {
        Some(ip) if ip.is_unspecified() => loopback,
        Some(ip) => vec![ip],
        None => loopback,
    }
}

/// `twilight-dispatch healthcheck`: exit 0 when /healthcheck returns 200.
/// Used as the Docker HEALTHCHECK, since the runtime image has no curl.
pub async fn probe(address: std::net::IpAddr, port: u16) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let attempt = async {
        let mut stream = tokio::net::TcpStream::connect((address, port)).await?;
        stream
            .write_all(b"GET /healthcheck HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await?;
        Ok::<_, std::io::Error>(response.starts_with(b"HTTP/1.1 200"))
    };
    matches!(
        tokio::time::timeout(Duration::from_secs(3), attempt).await,
        Ok(Ok(true))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_body_reports_problems_and_warnings() {
        assert_eq!(
            health_body(&Health::default()),
            (StatusCode::OK, r#"{"status":"OK"}"#.to_owned())
        );

        let warning = Health {
            problems: vec![],
            warnings: vec!["broker down".into()],
        };
        let (status, body) = health_body(&warning);
        assert_eq!(
            status,
            StatusCode::OK,
            "warnings must not trigger a restart"
        );
        assert!(body.contains("broker down"));

        let stuck = Health {
            problems: vec!["shard 1 is stuck".into()],
            warnings: vec![],
        };
        let (status, body) = health_body(&stuck);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["problems"][0], "shard 1 is stuck");
    }

    fn snapshot(active: bool, fatal: bool, silent_secs: u64) -> ShardSnapshot {
        ShardSnapshot {
            active,
            fatal,
            close_code: fatal.then_some(4014),
            last_message: std::time::Instant::now().checked_sub(Duration::from_secs(silent_secs)),
            ..ShardSnapshot::default()
        }
    }

    #[test]
    fn only_stuck_connected_shards_fail_the_healthcheck() {
        let silence = Duration::from_secs(180);
        let healthy = compute_health(&[(0, snapshot(true, false, 10))], true, silence);
        assert!(healthy.problems.is_empty() && healthy.warnings.is_empty());

        let stuck = compute_health(&[(0, snapshot(true, false, 600))], true, silence);
        assert_eq!(stuck.problems.len(), 1);

        // Disconnected shards are twilight's to reconnect; a restart would not help.
        let reconnecting = compute_health(&[(0, snapshot(false, false, 600))], true, silence);
        assert!(reconnecting.problems.is_empty());

        // Fatal closes and broker outages are warnings: restarting would only
        // burn identifies.
        let fatal = compute_health(&[(0, snapshot(false, true, 600))], false, silence);
        assert!(fatal.problems.is_empty());
        assert_eq!(fatal.warnings.len(), 2);
        assert!(fatal.warnings[0].contains("4014"));
    }

    #[test]
    fn probe_targets_the_listening_address() {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        let loopback = vec![
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ];
        assert_eq!(probe_addresses(None), loopback);
        assert_eq!(probe_addresses(Some("0.0.0.0")), loopback);
        assert_eq!(probe_addresses(Some("::")), loopback);
        assert_eq!(
            probe_addresses(Some("localhost")),
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]
        );
        assert_eq!(
            probe_addresses(Some(" 10.0.0.5 ")),
            vec!["10.0.0.5".parse::<IpAddr>().unwrap()]
        );
        assert_eq!(
            probe_addresses(Some("::1")),
            vec![IpAddr::V6(Ipv6Addr::LOCALHOST)]
        );
    }

    #[tokio::test]
    async fn server_answers_metrics_and_health() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(run_server(listener));

        set_health(Health::default());
        GATEWAY_SHARDS.set(3);
        let local = probe_addresses(None)[0];
        assert!(probe(local, port).await);

        set_health(Health {
            problems: vec!["broken".into()],
            warnings: vec![],
        });
        assert!(!probe(local, port).await);
        set_health(Health {
            problems: vec![],
            warnings: vec!["warn".into()],
        });
        assert!(probe(local, port).await);
        set_health(Health::default());

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.contains("gateway_shards 3"));

        server.abort();
    }
}
