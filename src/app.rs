//! Startup and shutdown.

use crate::{
    amqp::Amqp,
    cache, commands,
    config::Config,
    constants::{CACHE_JOB_INTERVAL_MS, EXPIRY_KEYS, SHARDS_KEY, STARTED_KEY},
    discord_log::DiscordLog,
    gateway::{
        self,
        registry::{Registry, ShardHandle},
        runner::{self, RunnerContext},
        sessions,
        worker::{self, Breaker, WorkerContext},
    },
    metrics,
    models::{ApiResult, FormattedDateTime},
    redis_conn,
    util::Throttle,
};

use std::{
    future::Future,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::{net::TcpListener, sync::watch, task::JoinSet};
use tracing::{info, warn};
use twilight_http::Client;

/// Shutdown budget. Docker and Swarm kill the container 10 s after SIGTERM by
/// default (raise stop_grace_period for more headroom).
const RUNNER_SHUTDOWN: Duration = Duration::from_secs(6);
const WORKER_SHUTDOWN: Duration = Duration::from_secs(2);
const SAVE_SHUTDOWN: Duration = Duration::from_secs(1);

/// Legacy expiry entries are migrated again this often (in job ticks), in case
/// an older version is still running next to this one (rolling update).
const LEGACY_EXPIRY_EVERY: u32 = 60;

/// Run until SIGTERM / SIGINT.
pub async fn run(config: Config) -> ApiResult<()> {
    run_until(config, shutdown_signal()).await
}

/// Run until `shutdown` completes, then shut down gracefully (the shards are
/// closed resumably and their sessions saved).
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

    // Fail fast on a bad metrics address, before touching any service.
    let address = SocketAddr::new(
        config
            .prometheus_host
            .parse::<IpAddr>()
            .map_err(|err| format!("invalid PROMETHEUS_HOST: {err}"))?,
        config.prometheus_port,
    );
    let listener = TcpListener::bind(address).await?;

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

async fn until_stopped<T>(
    stopped: &mut (impl Future<Output = ()> + Unpin),
    work: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        _ = stopped => None,
        result = work => Some(result),
    }
}

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

async fn start(config: Config, listener: TcpListener) -> ApiResult<Started> {
    let redis = redis_conn::connect(&config.redis_url).await?;
    migrate_legacy_expiry(&mut redis.clone()).await;

    let amqp = Amqp::connect(config.amqp_url.clone(), config.default_queue).await?;

    let http = Arc::new(http_client(&config));
    let user = http.current_user().await?.model().await?;
    info!("Logged in as {} ({})", user.name, user.id);

    let mut conn = redis.clone();
    let previous_total: Option<String> = redis::cmd("GET")
        .arg(SHARDS_KEY)
        .query_async(&mut conn)
        .await?;
    let saved = match (config.resume, config.shards) {
        // Sessions are only valid for the shard count they were created with.
        (true, Some(range))
            if previous_total.as_deref() == Some(range.total.to_string().as_str()) =>
        {
            sessions::load(&mut conn, range.ids()).await?
        }
        _ => Default::default(),
    };

    let created = gateway::create(&config, &http, saved).await?;
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

    let handles: Vec<Arc<ShardHandle>> = created
        .shards
        .iter()
        .map(|shard| {
            let id = shard.id().number();
            let resumed = created.resumed.get(&id);
            let handle = ShardHandle::new(id, Some(shard.sender()), resumed.map(|s| s.sequence));
            if let Some(resumed) = resumed {
                handle.update(|s| {
                    s.session_id = Some(resumed.session_id.clone());
                    s.resume_url = resumed.resume_url.clone();
                });
            }
            Arc::new(handle)
        })
        .collect();
    let registry = Arc::new(Registry::new(handles.iter().cloned()));

    redis::pipe()
        .cmd("SET")
        .arg(STARTED_KEY)
        .arg(serde_json::to_string(&FormattedDateTime::now())?)
        .ignore()
        .cmd("SET")
        .arg(SHARDS_KEY)
        .arg(range.total.to_string())
        .ignore()
        .query_async::<()>(&mut conn)
        .await?;

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
    });
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut runners = JoinSet::new();
    for (shard, handle) in created.shards.into_iter().zip(handles) {
        runners.spawn(runner::run(
            shard,
            handle,
            runner_ctx.clone(),
            shutdown_rx.clone(),
        ));
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

async fn migrate_legacy_expiry(conn: &mut redis_conn::Redis) {
    match cache::jobs::migrate_legacy_expiry(conn).await {
        Ok(0) => {}
        Ok(count) => info!("Migrated {} expiry entries from {}", count, EXPIRY_KEYS),
        Err(err) => warn!("Failed to migrate legacy expiry entries: {}", err),
    }
}

async fn expiry_job(mut conn: redis_conn::Redis) {
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
        if ticks.is_multiple_of(LEGACY_EXPIRY_EVERY) {
            migrate_legacy_expiry(&mut conn).await;
        }
        tokio::time::sleep(Duration::from_millis(CACHE_JOB_INTERVAL_MS)).await;
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
