//! Startup and shutdown.

use crate::{
    amqp::Amqp,
    cache, commands,
    config::Config,
    constants::{CACHE_JOB_INTERVAL_MS, EXPIRY_KEYS, SHARDS_KEY, STARTED_KEY, STATUSES_KEY},
    discord_log::DiscordLog,
    gateway::{
        self,
        queue::RedisQueue,
        registry::{Registry, ShardHandle},
        runner::{self, RunnerContext},
        sessions,
        worker::{self, Breaker, WorkerContext},
    },
    metrics,
    models::{ApiResult, FormattedDateTime, SessionInfo},
    redis_conn,
    util::Throttle,
};

use redis::aio::ConnectionLike;
use std::{
    collections::HashMap,
    future::Future,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{net::TcpListener, sync::watch, task::JoinSet};
use tracing::{info, info_span, warn, Instrument};
use twilight_gateway::Shard;
use twilight_http::Client;

/// Shutdown budget. Docker and Swarm kill the container 10 s after SIGTERM by
/// default (raise `stop_grace_period` for more headroom). A runner closes its
/// shard within `runner::CLOSE_TIMEOUT`, also when the shutdown finds it in a
/// backpressure pause or in READY's cache update; one aborted before that
/// would not mirror its final session, so the runners get a second more.
/// Then the workers drain and the sessions are saved, 8 s in all, which
/// leaves time to close the broker connection (at most 1 s).
const RUNNER_SHUTDOWN: Duration = runner::CLOSE_TIMEOUT.saturating_add(Duration::from_secs(1));
const WORKER_SHUTDOWN: Duration = Duration::from_secs(2);
const SAVE_SHUTDOWN: Duration = Duration::from_secs(1);

/// Legacy expiry entries are migrated again this often (in job ticks), in case
/// an older version is still running next to this one (rolling update).
const LEGACY_EXPIRY_EVERY: u32 = 60;

/// Run until SIGTERM / SIGINT (see [`shutdown_signal`]), then shut down
/// gracefully as [`run_until`] does.
///
/// # Errors
///
/// As [`run_until`]: when startup fails.
pub async fn run(config: Config) -> ApiResult<()> {
    run_until(config, shutdown_signal()).await
}

/// Run until `shutdown` completes, then shut down gracefully (the shards are
/// closed resumably and their sessions saved). A shutdown during startup
/// stops it and returns `Ok`.
///
/// # Errors
///
/// When startup fails: the metrics address (`PROMETHEUS_HOST`,
/// `PROMETHEUS_PORT`) cannot be bound (`ApiError::Io`, before any service is
/// contacted), or a request to Redis, the broker or Discord fails (for
/// example, Discord rejects the token). Once started, it returns `Ok` when
/// shut down.
pub async fn run_until(
    config: Config,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> ApiResult<()> {
    // Listen for the shutdown right away: as PID 1 the process would
    // otherwise ignore SIGTERM until startup finished.
    let (stop_tx, mut stop_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown.await;
        let _ = stop_tx.send(true);
    });
    let stopped = async move {
        let _ = stop_rx.wait_for(|stop| *stop).await;
    };
    tokio::pin!(stopped);

    // Fail fast on a bad metrics address, before touching any service. The
    // parse only guards hand-built Configs: `Config::from_lookup` already
    // stores PROMETHEUS_HOST as a parsed IP address.
    let address = SocketAddr::new(
        config
            .prometheus_host
            .parse::<IpAddr>()
            .map_err(|err| format!("invalid PROMETHEUS_HOST: {err}"))?,
        config.prometheus_port,
    );
    let listener = TcpListener::bind(address).await.map_err(|err| {
        std::io::Error::new(
            err.kind(),
            format!("cannot listen on {address} (PROMETHEUS_HOST, PROMETHEUS_PORT): {err}"),
        )
    })?;

    let Some(started) = until_stopped(&mut stopped, start(config, listener)).await else {
        info!("Stopped during startup");
        return Ok(());
    };
    let started = started?;

    stopped.await;
    info!("Shutting down");
    started.shutdown().await;
    Ok(())
}

/// `work`'s result, or `None` (dropping `work`) if `stopped` completes first.
async fn until_stopped<T>(
    stopped: &mut (impl Future<Output = ()> + Unpin),
    work: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        () = stopped => None,
        result = work => Some(result),
    }
}

/// Everything a running service owns, for `Started::shutdown`.
struct Started {
    redis: redis_conn::Redis,
    amqp: Arc<Amqp>,
    registry: Arc<Registry>,
    shutdown_tx: watch::Sender<bool>,
    runners: JoinSet<()>,
    workers: Vec<tokio::task::JoinHandle<()>>,
    consumer: JoinSet<()>,
    background: JoinSet<()>,
}

/// Connect to Redis, the broker and Discord, then start the metrics server
/// on `listener`, the background jobs, the workers, the shard runners and
/// the command consumer.
async fn start(config: Config, listener: TcpListener) -> ApiResult<Started> {
    let redis = redis_conn::connect(&config.redis_url).await?;
    migrate_legacy_expiry(&mut redis.clone()).await;

    let amqp = Amqp::connect_with(
        config.amqp_url.clone(),
        config.default_queue,
        config.amqp_confirms,
    )
    .await?;

    let http = Arc::new(http_client(&config));
    let user = http.current_user().await?.model().await?;
    info!("Logged in as {} ({})", user.name, user.id);

    let mut conn = redis.clone();
    let saved = saved_sessions(&mut conn, &config).await?;

    // Discord's identify limit is per bot, so the shards take turns with
    // those of the bot's other processes (they share this Redis). On a
    // connection of its own, so a turn never waits behind the cache's large
    // GUILD_CREATE transactions and falls back to local turns.
    let identify_redis = redis_conn::connect(&config.redis_url).await?;
    let created = gateway::create_with_queue(&config, &http, saved, |local, max_concurrency| {
        RedisQueue::new(local, max_concurrency, identify_redis)
    })
    .await?;
    let range = created.range;
    if config.shards.is_none() {
        info!("Using Discord's recommended shard count of {}", range.total);
    }
    info!(
        "Starting up {} shards ({}..={} of {})",
        range.len(),
        range.start,
        range.end,
        range.total
    );
    info!("Resuming {} sessions", created.resumed.len());

    let handles = shard_handles(&created.shards, &created.resumed);
    let registry = Arc::new(Registry::new(handles.iter().cloned()));
    mark_started(&mut conn, range.total).await?;

    let mut background = JoinSet::new();
    background.spawn(metrics::run_server(listener));
    background.spawn(metrics::run_jobs(
        redis.clone(),
        registry.clone(),
        amqp.clone(),
        config.shard_silence,
    ));
    background.spawn(sessions::run_job(redis.clone(), registry.clone()));
    background.spawn(expiry_job(redis.clone()));

    let discord = DiscordLog::new(http.clone(), config.log_channel, config.log_guild_channel);
    let worker_ctx = Arc::new(WorkerContext {
        redis: redis.clone(),
        publisher: amqp.clone(),
        registry: registry.clone(),
        state: config.state.clone(),
        bot_id: user.id,
        cache_timeout: config.cache_timeout,
        discord: discord.clone(),
        breaker: Breaker::default(),
        cache_errors: Throttle::default(),
        publish_errors: Throttle::default(),
    });
    let (queues, workers) = worker::spawn(config.workers, worker_ctx);

    let runner_ctx = Arc::new(RunnerContext {
        state: config.state.clone(),
        workers: queues,
        redis: redis.clone(),
        discord,
        bot_id: user.id,
        cache_timeout: config.cache_timeout,
        max_held_bytes: runner::MAX_HELD_BYTES,
    });
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut runners = JoinSet::new();
    for (shard, handle) in created.shards.into_iter().zip(handles) {
        // twilight-gateway's own log lines name no shard; the span does.
        let span = info_span!("shard", id = handle.id);
        let task = runner::run(shard, handle, runner_ctx.clone(), shutdown_rx.clone());
        runners.spawn(task.instrument(span));
    }
    drop(runner_ctx);

    let mut consumer = JoinSet::new();
    consumer.spawn(commands::run_consumer(
        amqp.clone(),
        registry.clone(),
        range.total,
    ));

    Ok(Started {
        redis,
        amqp,
        registry,
        shutdown_tx,
        runners,
        workers,
        consumer,
        background,
    })
}

/// The saved sessions of the configured shards, or none when `RESUME` is off,
/// no shard range is configured, or the last start used another shard total.
async fn saved_sessions(
    conn: &mut redis_conn::Redis,
    config: &Config,
) -> ApiResult<HashMap<u32, SessionInfo>> {
    let previous_total = redis::cmd("GET")
        .arg(SHARDS_KEY)
        .query_async::<Option<String>>(conn)
        .await?;
    match (config.resume, config.shards) {
        // Sessions are only valid for the shard count they were created with.
        (true, Some(range))
            if previous_total.as_deref() == Some(range.total.to_string().as_str()) =>
        {
            sessions::load(conn, range.ids()).await
        }
        _ => Ok(HashMap::new()),
    }
}

/// A handle per shard, holding the session the shard resumes, if any.
fn shard_handles<Q>(
    shards: &[Shard<Q>],
    resumed: &HashMap<u32, SessionInfo>,
) -> Vec<Arc<ShardHandle>> {
    shards
        .iter()
        .map(|shard| {
            let id = shard.id().number();
            let session = resumed.get(&id);
            let handle = ShardHandle::new(id, Some(shard.sender()), session.map(|s| s.sequence));
            if let Some(session) = session {
                handle.update(|s| {
                    s.session_id = Some(session.session_id.clone());
                    s.resume_url.clone_from(&session.resume_url);
                });
            }
            Arc::new(handle)
        })
        .collect()
}

/// Record the start time and the shard total; the next start resumes the
/// saved sessions only if its shard total is the same.
///
/// A new shard total also clears the gateway statuses: every process keeps
/// the entries of the shards it does not run, so entries of shards that no
/// longer exist would otherwise keep their last status forever.
async fn mark_started(conn: &mut redis_conn::Redis, shard_total: u32) -> ApiResult<()> {
    let total = shard_total.to_string();
    let previous_total = redis::cmd("GET")
        .arg(SHARDS_KEY)
        .query_async::<Option<String>>(conn)
        .await?;
    let mut pipe = redis::pipe();
    pipe.cmd("SET")
        .arg(STARTED_KEY)
        .arg(serde_json::to_string(&FormattedDateTime::now())?)
        .ignore()
        .cmd("SET")
        .arg(SHARDS_KEY)
        .arg(total.as_str())
        .ignore();
    if previous_total.as_deref() != Some(total.as_str()) {
        pipe.cmd("DEL").arg(STATUSES_KEY).ignore();
    }
    pipe.query_async::<()>(conn).await?;
    Ok(())
}

impl Started {
    async fn shutdown(mut self) {
        // Stop taking gateway commands; unacknowledged ones go back to the
        // queue when the connection closes.
        self.consumer.abort_all();

        let _ = self.shutdown_tx.send(true);
        let closed = tokio::time::timeout(RUNNER_SHUTDOWN, async {
            while self.runners.join_next().await.is_some() {}
        })
        .await;
        if closed.is_err() {
            warn!("Some shards did not close in time");
            self.runners.abort_all();
        }

        // The runners held the worker queues; with them gone the workers
        // drain and stop.
        let drained = tokio::time::timeout(WORKER_SHUTDOWN, async {
            for worker in self.workers {
                let _ = worker.await;
            }
        })
        .await;
        if drained.is_err() {
            warn!("Workers did not drain in time; unpublished events will be replayed on resume");
        }

        // No other writer may overwrite the final sessions.
        self.background.abort_all();
        let mut conn = self.redis.clone();
        match tokio::time::timeout(SAVE_SHUTDOWN, sessions::save(&mut conn, &self.registry)).await {
            Ok(Ok(count)) => info!("Saved {} resumable sessions", count),
            Ok(Err(err)) => warn!("Failed to save sessions: {}", err),
            Err(_) => warn!("Timed out saving sessions"),
        }
        self.amqp.close().await;
    }
}

/// The Discord REST client, through `DISCORD_API_PROXY` when set.
pub fn http_client(config: &Config) -> Client {
    let builder = Client::builder().token(config.bot_token.clone());
    match &config.discord_api_proxy {
        Some(proxy) => builder.proxy(proxy.address.clone(), proxy.use_http).build(),
        None => builder.build(),
    }
}

/// Move the legacy expiry hash into the sorted set, logging the outcome.
async fn migrate_legacy_expiry<C: ConnectionLike + Send>(conn: &mut C) {
    match cache::jobs::migrate_legacy_expiry(conn).await {
        Ok(0) => {}
        Ok(count) => info!("Migrated {} expiry entries from {}", count, EXPIRY_KEYS),
        Err(err) => warn!("Failed to migrate legacy expiry entries: {}", err),
    }
}

/// Every second: delete the cache keys whose deadline passed; every
/// `LEGACY_EXPIRY_EVERY` ticks, also migrate the legacy expiry hash again.
/// Runs until the task is aborted.
async fn expiry_job(conn: redis_conn::Redis) {
    let interval = Duration::from_millis(CACHE_JOB_INTERVAL_MS);
    expiry_loop(conn, interval, LEGACY_EXPIRY_EVERY).await;
}

/// `expiry_job` with its timing as parameters: a tick every `interval`, and
/// the legacy migration after every `legacy_every`th tick.
async fn expiry_loop<C>(mut conn: C, interval: Duration, legacy_every: u32)
where
    C: ConnectionLike + Send,
{
    let throttle = Throttle::default();
    let mut ticks = 0u32;
    loop {
        let now = FormattedDateTime::now().unix_millis();
        if let Err(err) = cache::jobs::expire_due(&mut conn, now).await {
            if let Some(suppressed) = throttle.check() {
                warn!(
                    "Failed to delete expired keys: {} ({} similar suppressed)",
                    err, suppressed
                );
            }
        }
        ticks = ticks.wrapping_add(1);
        if ticks.is_multiple_of(legacy_every) {
            migrate_legacy_expiry(&mut conn).await;
        }
        tokio::time::sleep(interval).await;
    }
}

/// Wait for SIGINT or SIGTERM (what `docker stop` and Swarm send).
pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(err) => {
                warn!("Cannot listen for SIGTERM: {}", err);
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shutdown_fits_dockers_default_stop_grace_period() {
        // A runner is never aborted while close() is still within its deadline.
        assert!(RUNNER_SHUTDOWN >= runner::CLOSE_TIMEOUT + Duration::from_secs(1));
        // Amqp::close waits at most 1 s; Docker kills the container at 10 s.
        let total = RUNNER_SHUTDOWN + WORKER_SHUTDOWN + SAVE_SHUTDOWN + Duration::from_secs(1);
        assert!(total < Duration::from_secs(10), "{total:?}");
    }

    /// `run` shuts down when `docker stop` / Swarm (SIGTERM) or Ctrl-C
    /// (SIGINT) asks, and only then. One test for both: a signal reaches
    /// every listener in the process.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_signal_waits_for_sigterm_or_sigint() {
        for signal in ["TERM", "INT"] {
            let wait = shutdown_signal();
            tokio::pin!(wait);
            // Polled, so its handlers are installed and the signal below
            // does not end the test process.
            let early = tokio::time::timeout(Duration::from_millis(200), &mut wait).await;
            assert!(early.is_err(), "returned without SIG{signal}");
            let status = std::process::Command::new("kill")
                .arg(format!("-{signal}"))
                .arg(std::process::id().to_string())
                .status()
                .expect("kill runs");
            assert!(status.success(), "kill -{signal}: {status}");
            tokio::time::timeout(Duration::from_secs(5), wait)
                .await
                .unwrap_or_else(|_| panic!("SIG{signal} did not end the wait"));
        }
    }
}

#[cfg(test)]
mod expiry_job_tests {
    use super::*;
    use redis::{Arg, Cmd, Pipeline, RedisFuture, RedisResult, Value};
    use std::sync::Mutex;

    /// A Redis for the expiry job that records the name of every command. It
    /// answers that there is nothing to do (no key is due, the legacy hash is
    /// empty), or, when `down`, fails every command like a Redis that cannot
    /// be reached.
    #[derive(Clone, Default)]
    struct Recorder {
        names: Arc<Mutex<Vec<String>>>,
        down: bool,
    }

    impl Recorder {
        fn names(&self) -> Vec<String> {
            self.names.lock().unwrap().clone()
        }
    }

    impl ConnectionLike for Recorder {
        fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
            let name = match cmd.args_iter().next() {
                Some(Arg::Simple(name)) => String::from_utf8_lossy(name).into_owned(),
                _ => "<cursor>".to_owned(),
            };
            let reply: RedisResult<Value> = if self.down {
                let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
                Err(refused.into())
            } else if name == "HSCAN" {
                // Cursor 0 and no fields: the scan is over.
                let done = vec![Value::BulkString(b"0".to_vec()), Value::Array(vec![])];
                Ok(Value::Array(done))
            } else {
                Ok(Value::Array(vec![]))
            };
            self.names.lock().unwrap().push(name);
            Box::pin(async move { reply })
        }

        fn req_packed_commands<'a>(
            &'a mut self,
            _pipeline: &'a Pipeline,
            _offset: usize,
            _count: usize,
        ) -> RedisFuture<'a, Vec<Value>> {
            panic!("nothing is due, so the expiry job sends no pipeline")
        }

        fn get_db(&self) -> i64 {
            0
        }
    }

    /// What `tracing` logs on this thread while the guard of the subscriber
    /// writing to it lives.
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
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    /// The commands `expiry_loop(.., every)` sends in `ticks` ticks: each
    /// tick expires due keys, and every `every`th tick then also migrates
    /// the legacy hash.
    fn expected(ticks: usize, every: usize) -> Vec<&'static str> {
        (1..=ticks)
            .flat_map(|tick| {
                let migrates = tick.is_multiple_of(every).then_some("HSCAN");
                std::iter::once("ZRANGEBYSCORE").chain(migrates)
            })
            .collect()
    }

    /// Run the loop on `conn` for half a second, a tick every millisecond,
    /// and return the commands it sent with the number of ticks. Every reply
    /// is ready at once, so the loop only ever waits in its sleep, where the
    /// timeout drops it after a whole tick.
    async fn run_for_a_while(conn: &Recorder, every: u32) -> (Vec<String>, usize) {
        let job = expiry_loop(conn.clone(), Duration::from_millis(1), every);
        let ended = tokio::time::timeout(Duration::from_millis(500), job).await;
        assert!(ended.is_err(), "the job ended on its own");
        let names = conn.names();
        let ticks = names.iter().filter(|n| *n == "ZRANGEBYSCORE").count();
        (names, ticks)
    }

    /// Every tick expires due keys, and every `legacy_every`th tick also
    /// migrates the legacy hash again, which an older version may still be
    /// writing during a rolling update. The first tick only expires.
    #[tokio::test]
    async fn the_legacy_hash_is_migrated_again_every_few_ticks() {
        let conn = Recorder::default();
        let (names, ticks) = run_for_a_while(&conn, 3).await;
        assert!(ticks >= 3, "{names:?}");
        assert_eq!(names, expected(ticks, 3));
    }

    /// While Redis is down the job keeps its pace. The failed expiry goes
    /// through the throttle (logged once per 5 s, not every tick); a failed
    /// migration, about once a minute in production, is logged each time.
    #[tokio::test]
    async fn the_job_logs_failures_and_carries_on() {
        let logs = Logs::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        // The current-thread runtime polls the job on this thread.
        let _guard = tracing::subscriber::set_default(subscriber);

        let conn = Recorder {
            down: true,
            ..Recorder::default()
        };
        let (names, ticks) = run_for_a_while(&conn, 2).await;
        assert!(ticks >= 4, "{names:?}");
        assert_eq!(names, expected(ticks, 2));

        let text = logs.text();
        let expiry = text.matches("Failed to delete expired keys: ").count();
        assert_eq!(expiry, 1, "{text}");
        assert!(text.contains("(0 similar suppressed)"), "{text}");
        let migrations = text
            .matches("Failed to migrate legacy expiry entries: ")
            .count();
        assert_eq!(migrations, ticks / 2, "{text}");
    }
}
