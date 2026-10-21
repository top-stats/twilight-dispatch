//! Prometheus metrics, the /metrics + /healthcheck server and the metrics job.
//!
//! Metric names of earlier versions are unchanged. New: `gateway_parse_errors`,
//! `state_update_errors`, `amqp_publish_failures`, `amqp_reconnects`,
//! `amqp_connected`, `gateway_shard_fatal`, `gateway_shard_silence_seconds`,
//! `state_threads`.

use crate::{
    amqp::Amqp,
    cache::keys::type_index,
    constants::{
        CHANNEL_KEY, EMOJI_KEY, GUILD_KEY, MEMBER_KEY, MESSAGE_KEY, METRICS_JOB_INTERVAL_MS,
        PRESENCE_KEY, ROLE_KEY, THREAD_KEY, VOICE_KEY,
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
    core::{Collector, Desc, MetricVec, MetricVecBuilder},
    proto::MetricType,
    register_int_counter, register_int_counter_vec, register_int_gauge, register_int_gauge_vec,
    Encoder, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, TextEncoder,
};
use redis::aio::ConnectionLike;
use std::{
    collections::HashMap,
    convert::Infallible,
    ops::Deref,
    sync::{Arc, LazyLock, RwLock},
    time::{Duration, Instant},
};
use tokio::net::TcpListener;
use tracing::warn;

/// A metric that is created and registered on first use, like a `LazyLock`.
///
/// The first use of any metric also registers all the others (see [`init`]),
/// so /metrics lists every metric (vectors once they have a child) instead of
/// only those something happened to touch. [`descriptors`] lists vectors
/// without a child too, so a new metric shows up in the metric names snapshot
/// without the test knowing about it.
pub struct Metric<T>(LazyLock<T>);

impl<T> Metric<T> {
    const fn new(create: fn() -> T) -> Self {
        Self(LazyLock::new(create))
    }
}

impl<T> Deref for Metric<T> {
    type Target = T;

    fn deref(&self) -> &T {
        LazyLock::force(&REGISTERED);
        &self.0
    }
}

/// A vector metric whose number of labels is part of its type.
///
/// prometheus' `with_label_values` panics when it gets a different number of
/// values than the metric has labels, and release builds abort on a panic.
/// `N` is the length of the label array the metric is declared with, so a
/// call site that was not updated when a label was added or removed fails to
/// compile instead of aborting the process the first time it runs:
///
/// ```compile_fail
/// twilight_dispatch::metrics::GATEWAY_EVENTS.with_label_values(&["READY"]);
/// ```
///
/// The same call with both values compiles, so the first example fails for
/// the count rather than for a typo:
///
/// ```no_run
/// twilight_dispatch::metrics::GATEWAY_EVENTS.with_label_values(&["READY", "0"]);
/// ```
///
/// It does not deref to the vector, which would let a call skip the check.
pub struct Labelled<V, const N: usize>(V);

impl<B: MetricVecBuilder, const N: usize> Labelled<MetricVec<B>, N> {
    /// The child for these label values, given in the order the labels are
    /// declared in.
    pub fn with_label_values(&self, values: &[&str; N]) -> B::M {
        self.0.with_label_values(values)
    }
}

/// A metric as it is declared, whether or not /metrics lists it yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Descriptor {
    pub name: String,
    pub help: String,
    pub kind: MetricType,
    /// The label names, in the order `with_label_values` takes the values.
    pub labels: Vec<String>,
}

/// What [`descriptors`] reads from a metric. Its `Desc` has everything but
/// the type, and `Collector::collect` would copy every child to get that.
trait Describe {
    const KIND: MetricType;
    fn descs(&self) -> Vec<&Desc>;
}

macro_rules! describe {
    ($($metric:ty => $kind:ident),*) => {$(
        impl Describe for $metric {
            const KIND: MetricType = MetricType::$kind;
            fn descs(&self) -> Vec<&Desc> {
                self.desc()
            }
        }
    )*};
}
describe!(
    IntCounter => COUNTER,
    IntCounterVec => COUNTER,
    IntGauge => GAUGE,
    IntGaugeVec => GAUGE
);

impl<V: Describe, const N: usize> Describe for Labelled<V, N> {
    const KIND: MetricType = V::KIND;
    fn descs(&self) -> Vec<&Desc> {
        self.0.descs()
    }
}

trait Register: Sync {
    fn register(&self);
    fn describe(&self) -> Vec<Descriptor>;
}

impl<T: Describe + Send + Sync> Register for Metric<T> {
    fn register(&self) {
        // The inner value, not `deref`: that would wait for `REGISTERED`.
        LazyLock::force(&self.0);
    }

    fn describe(&self) -> Vec<Descriptor> {
        let metric = LazyLock::force(&self.0);
        metric
            .descs()
            .into_iter()
            .map(|desc| Descriptor {
                name: desc.fq_name.clone(),
                help: desc.help.clone(),
                kind: T::KIND,
                labels: desc.variable_labels.clone(),
            })
            .collect()
    }
}

static REGISTERED: LazyLock<()> = LazyLock::new(|| ALL.iter().for_each(|m| m.register()));

/// Register every metric now rather than on first use.
pub fn init() {
    LazyLock::force(&REGISTERED);
}

/// Every metric, sorted by name like /metrics. Registers them all, like
/// [`init`].
///
/// Built from the declarations rather than `prometheus::gather()`, which
/// leaves a vector out until it has a child: a new vector is listed here
/// before anything has used it.
pub fn descriptors() -> Vec<Descriptor> {
    init();
    let mut all: Vec<Descriptor> = ALL.iter().flat_map(|metric| metric.describe()).collect();
    all.sort_by(|a, b| a.name.cmp(&b.name));
    all
}

// A vector's `N` is the length of the label array it registers, so the two
// cannot disagree.
macro_rules! counter_vec {
    ($name:ident, $metric:literal, $help:literal, $labels:expr) => {
        pub static $name: Metric<Labelled<IntCounterVec, { $labels.len() }>> =
            Metric::new(|| Labelled(register_int_counter_vec!($metric, $help, $labels).unwrap()));
    };
}
macro_rules! counter {
    ($name:ident, $metric:literal, $help:literal) => {
        pub static $name: Metric<IntCounter> =
            Metric::new(|| register_int_counter!($metric, $help).unwrap());
    };
}
macro_rules! gauge {
    ($name:ident, $metric:literal, $help:literal) => {
        pub static $name: Metric<IntGauge> =
            Metric::new(|| register_int_gauge!($metric, $help).unwrap());
    };
}
macro_rules! gauge_vec {
    ($name:ident, $metric:literal, $help:literal, $labels:expr) => {
        pub static $name: Metric<Labelled<IntGaugeVec, { $labels.len() }>> =
            Metric::new(|| Labelled(register_int_gauge_vec!($metric, $help, $labels).unwrap()));
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
gauge!(
    STATE_THREADS,
    "state_threads",
    "Number of threads in state cache"
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
counter_vec!(
    GATEWAY_COMMANDS,
    "gateway_commands",
    "Gateway commands from gateway.send by outcome (sent, forwarded, dropped, invalid)",
    &["shard", "result"]
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

/// Every metric above. One left out is only registered on its first use; a
/// unit test checks that none is.
static ALL: &[&dyn Register] = &[
    &GATEWAY_EVENTS,
    &SHARD_EVENTS,
    &GUILD_EVENTS,
    &GATEWAY_SHARDS,
    &GATEWAY_STATUSES,
    &GATEWAY_LATENCIES,
    &STATE_GUILDS,
    &STATE_CHANNELS,
    &STATE_MESSAGES,
    &STATE_ROLES,
    &STATE_EMOJIS,
    &STATE_MEMBERS,
    &STATE_PRESENCES,
    &STATE_VOICES,
    &STATE_THREADS,
    &GATEWAY_PARSE_ERRORS,
    &STATE_UPDATE_ERRORS,
    &AMQP_PUBLISH_FAILURES,
    &AMQP_RECONNECTS,
    &GATEWAY_COMMANDS,
    &AMQP_CONNECTED,
    &SHARD_FATAL,
    &SHARD_SILENCE,
];

/// Each state gauge and the cache type it counts (the size of the
/// `<prefix>_keys` index). One table drives both the SCARD pipeline and the
/// assignment, so the two cannot get out of step.
pub static STATE_GAUGES: &[(&str, &Metric<IntGauge>)] = &[
    (GUILD_KEY, &STATE_GUILDS),
    (CHANNEL_KEY, &STATE_CHANNELS),
    (MESSAGE_KEY, &STATE_MESSAGES),
    (ROLE_KEY, &STATE_ROLES),
    (EMOJI_KEY, &STATE_EMOJIS),
    (MEMBER_KEY, &STATE_MEMBERS),
    (PRESENCE_KEY, &STATE_PRESENCES),
    (VOICE_KEY, &STATE_VOICES),
    (THREAD_KEY, &STATE_THREADS),
];

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
    init();
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

/// How long a shard may wait for a connection it closed to finish.
pub const CLOSE_WAIT_LIMIT: Duration = Duration::from_secs(120);

/// What /healthcheck reports for the current shard snapshots.
pub fn compute_health(
    snapshots: &[(u32, ShardSnapshot)],
    amqp_connected: bool,
    shard_silence: Duration,
) -> Health {
    compute_health_at(snapshots, amqp_connected, shard_silence, Instant::now())
}

/// [`compute_health`] at `now`. A test can move `now` ahead instead of
/// moving the snapshots' instants back, which `Instant::checked_sub` refuses
/// on a host that booted more recently than that.
pub fn compute_health_at(
    snapshots: &[(u32, ShardSnapshot)],
    amqp_connected: bool,
    shard_silence: Duration,
    now: Instant,
) -> Health {
    let mut health = Health::default();
    for (shard, snapshot) in snapshots {
        let silence = snapshot.silence_at(now);
        if snapshot.fatal {
            health.warnings.push(format!(
                "shard {shard} was closed by Discord with a non-reconnectable code{}",
                snapshot
                    .close_code
                    .map(|code| format!(" ({code})"))
                    .unwrap_or_default()
            ));
        } else if snapshot
            .close_wait_since
            .is_some_and(|since| now.saturating_duration_since(since) > CLOSE_WAIT_LIMIT)
        {
            // The gateway never finished closing the connection; twilight
            // would wait for it forever.
            health.problems.push(format!(
                "shard {shard} is stuck waiting for the gateway to close the connection"
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

/// Set every state gauge to the size of its cache type.
pub async fn update_state_gauges<C: ConnectionLike + Send>(conn: &mut C) -> ApiResult<()> {
    let mut pipe = redis::pipe();
    for (prefix, _) in STATE_GAUGES {
        pipe.cmd("SCARD").arg(type_index(prefix));
    }
    let counts: Vec<i64> = pipe.query_async(conn).await?;
    // Counts that do not line up with the gauges would land on the wrong ones.
    if counts.len() != STATE_GAUGES.len() {
        return Err(format!(
            "expected {} state counts, got {}",
            STATE_GAUGES.len(),
            counts.len()
        )
        .into());
    }
    for ((_, gauge), count) in STATE_GAUGES.iter().zip(counts) {
        gauge.set(count);
    }
    Ok(())
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

        if let Err(err) = update_state_gauges(&mut conn).await {
            warn!("Failed to get state stats: {}", err);
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

    fn snapshot(now: Instant, active: bool, fatal: bool, silent_secs: u64) -> ShardSnapshot {
        ShardSnapshot {
            active,
            fatal,
            close_code: fatal.then_some(4014),
            last_message: Some(now - Duration::from_secs(silent_secs)),
            ..ShardSnapshot::default()
        }
    }

    #[test]
    fn only_stuck_connected_shards_fail_the_healthcheck() {
        let silence = Duration::from_secs(180);
        // An hour ahead, so the snapshots can lie up to an hour before it
        // however recently the host booted.
        let now = Instant::now() + Duration::from_secs(3600);
        let health = |snapshot, amqp| compute_health_at(&[(0, snapshot)], amqp, silence, now);

        let healthy = health(snapshot(now, true, false, 10), true);
        assert!(healthy.problems.is_empty() && healthy.warnings.is_empty());
        let current = snapshot(Instant::now(), true, false, 0);
        assert!(compute_health(&[(0, current)], true, silence)
            .problems
            .is_empty());

        let stuck = health(snapshot(now, true, false, 600), true);
        assert_eq!(stuck.problems.len(), 1);

        // Disconnected shards are twilight's to reconnect; a restart would not help.
        let reconnecting = health(snapshot(now, false, false, 600), true);
        assert!(reconnecting.problems.is_empty());

        let mut closing = snapshot(now, false, false, 600);
        closing.close_wait_since = Some(now - CLOSE_WAIT_LIMIT * 2);
        let stuck_closing = health(closing, true);
        assert_eq!(
            stuck_closing.problems.len(),
            1,
            "waiting forever for a close"
        );

        // Fatal closes and broker outages are warnings: restarting would only
        // burn identifies.
        let fatal = health(snapshot(now, false, true, 600), false);
        assert!(fatal.problems.is_empty());
        assert_eq!(fatal.warnings.len(), 2);
        assert!(fatal.warnings[0].contains("4014"));
    }

    #[test]
    fn every_metric_is_registered_by_init() {
        let defined = include_str!("metrics.rs")
            .lines()
            .filter(|line| {
                ["counter!(", "counter_vec!(", "gauge!(", "gauge_vec!("]
                    .iter()
                    .any(|call| line.starts_with(call))
            })
            .count();
        assert_eq!(ALL.len(), defined, "every metric is listed in ALL");

        init();
        // Vectors are only listed once they have a child.
        let names: Vec<String> = prometheus::gather()
            .iter()
            .map(|family| family.name().to_owned())
            .collect();
        for name in ["state_threads", "amqp_reconnects", "amqp_publish_failures"] {
            assert!(names.iter().any(|n| n == name), "{name} in {names:?}");
        }

        let mut described: Vec<String> = descriptors().into_iter().map(|d| d.name).collect();
        described.dedup();
        assert_eq!(
            described.len(),
            defined,
            "one name per metric: {described:?}"
        );
    }

    #[test]
    fn a_vector_without_a_child_is_described() {
        // Not registered, so it cannot clash with anything; `describe` only
        // creates it.
        static UNUSED: Metric<Labelled<IntGaugeVec, 2>> = Metric::new(|| {
            Labelled(
                IntGaugeVec::new(prometheus::Opts::new("unused", "Never set"), &["b", "a"])
                    .unwrap(),
            )
        });
        assert_eq!(
            UNUSED.describe(),
            vec![Descriptor {
                name: "unused".into(),
                help: "Never set".into(),
                kind: MetricType::GAUGE,
                labels: vec!["b".into(), "a".into()],
            }]
        );
        let Labelled(vec) = LazyLock::force(&UNUSED.0);
        assert!(
            vec.collect()[0].get_metric().is_empty(),
            "describing creates no child"
        );
    }

    #[test]
    fn state_gauges_count_each_type_once() {
        for (i, (prefix, gauge)) in STATE_GAUGES.iter().enumerate() {
            for (other_prefix, other) in &STATE_GAUGES[i + 1..] {
                assert_ne!(prefix, other_prefix);
                assert!(!std::ptr::eq(*gauge, *other), "{prefix} shares a gauge");
            }
        }
    }

    /// Answers every pipeline with one value.
    struct ShortReply;

    impl ConnectionLike for ShortReply {
        fn req_packed_command<'a>(
            &'a mut self,
            _cmd: &'a redis::Cmd,
        ) -> redis::RedisFuture<'a, redis::Value> {
            Box::pin(async { Ok(redis::Value::Int(1)) })
        }

        fn req_packed_commands<'a>(
            &'a mut self,
            _pipeline: &'a redis::Pipeline,
            _offset: usize,
            _count: usize,
        ) -> redis::RedisFuture<'a, Vec<redis::Value>> {
            Box::pin(async { Ok(vec![redis::Value::Int(1)]) })
        }

        fn get_db(&self) -> i64 {
            0
        }
    }

    #[tokio::test]
    async fn counts_that_do_not_match_the_gauges_are_an_error() {
        STATE_GUILDS.set(-5);
        let result = update_state_gauges(&mut ShortReply).await;
        assert!(
            result
                .as_ref()
                .is_err_and(|err| err.to_string().contains("got 1")),
            "{result:?}"
        );
        assert_eq!(STATE_GUILDS.get(), -5, "no count is assigned to a guess");
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

    /// What is logged while it is the thread's default subscriber.
    #[derive(Clone, Default)]
    struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);

    impl Logs {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    impl std::io::Write for Logs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn only_broken_requests_are_logged() {
        // A `tokio::test` runtime runs the server's tasks on this thread, so
        // they log to this subscriber.
        let logs = Logs::default();
        let writer = logs.clone();
        let _logging = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(move || writer.clone())
                .finish(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(run_server(listener));

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let send = |request: &'static [u8]| async move {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            stream.write_all(request).await.unwrap();
            stream.shutdown().await.unwrap();
            // Until the server closes the connection.
            let _ = stream.read_to_end(&mut Vec::new()).await;
        };
        let error = "Metrics connection error";

        // A scraper or probe that gives up halfway through a request (a
        // timeout, a restart) would otherwise add a warning every time.
        send(b"GET /metrics HTTP/1.1\r\nHost: x").await;
        assert!(!logs.text().contains(error), "{}", logs.text());

        send(b"\x16\x03\x01garbage\r\n\r\n").await;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !logs.text().contains(error) {
            assert!(Instant::now() < deadline, "a broken request is logged");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        server.abort();
    }

    /// Set in the process that `the_first_scrape_lists_every_metric` starts.
    const FRESH_PROCESS: &str = "TWILIGHT_DISPATCH_TEST_FRESH_PROCESS";

    #[tokio::test]
    async fn the_first_scrape_lists_every_metric() {
        // The other tests in this binary use metrics, and the first use of one
        // registers them all, so only a process of its own shows what /metrics
        // lists before anything used a metric.
        if std::env::var_os(FRESH_PROCESS).is_none() {
            let test = "metrics::tests::the_first_scrape_lists_every_metric";
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([test, "--exact"])
                .env(FRESH_PROCESS, "1")
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

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(run_server(listener));
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
        server.abort();

        // A counter that is missing until its first increment hides that
        // increment from increase() and rate(), which only see the change
        // between two scrapes. Vectors cannot be listed before their first
        // child.
        for metric in descriptors().iter().filter(|m| m.labels.is_empty()) {
            let line = format!("{} 0", metric.name);
            assert!(response.lines().any(|l| l == line), "{line} in\n{response}");
        }
    }
}
