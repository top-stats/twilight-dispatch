//! AMQP connection supervision (production broker: LavinMQ).
//!
//! lapin connections and channels never come back on their own once they are
//! closed. Before, a single broker restart made every later publish fail and
//! silently ended the command consumer.
//!
//! One supervisor task owns the connection: it notices failures, reconnects
//! with backoff and re-declares the topology, and it is never cancelled.
//! Publishers and the consumer only wait for its result through a `watch`
//! channel, so a caller giving up (a timeout) can never leave a half-open
//! connection behind. Publishing waits for a reconnect for at most
//! [`OUTAGE_BUDGET`] per outage (not per message); after that publishes fail
//! immediately until the broker is back, so an outage cannot stall the
//! workers (and with them the shards).

use crate::{
    constants::{EXCHANGE, QUEUE_RECV, QUEUE_SEND},
    metrics::{AMQP_PUBLISH_FAILURES, AMQP_RECONNECTS},
    models::{ApiError, ApiResult},
};

use lapin::{
    options::{
        BasicPublishOptions, BasicQosOptions, ExchangeDeclareOptions, QueueBindOptions,
        QueueDeclareOptions,
    },
    types::FieldTable,
    BasicProperties, Channel, Connection, ConnectionProperties, ExchangeKind,
};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::{Duration, Instant},
};
use tokio::sync::{watch, Notify};
use tracing::{info, warn};

/// How long publishes wait for the broker during one outage.
pub const OUTAGE_BUDGET: Duration = Duration::from_secs(20);
/// Time limit for connecting (TCP + AMQP handshake + topology).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Time limit for handing one message to the connection.
pub const PUBLISH_TIMEOUT: Duration = Duration::from_secs(5);
/// Attempts per message; a message the broker keeps rejecting is dropped.
pub const PUBLISH_ATTEMPTS: u32 = 3;
/// Prefetch for the gateway command consumer.
pub const PREFETCH: u16 = 100;
/// How often the supervisor checks the link without being woken.
const CHECK_INTERVAL: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// A connection with its channels.
#[derive(Clone)]
pub struct Link {
    pub generation: u64,
    connection: Arc<Connection>,
    publish: Channel,
    consume: Channel,
}

impl Link {
    fn is_healthy(&self) -> bool {
        self.connection.status().connected() && self.publish.status().connected()
    }

    /// The channel the gateway command consumer uses.
    pub fn consume_channel(&self) -> &Channel {
        &self.consume
    }
}

pub struct Amqp {
    url: String,
    default_queue: bool,
    link: watch::Sender<Option<Link>>,
    wake: Notify,
    generation: AtomicU64,
    closing: AtomicBool,
    /// When the current outage started, if there is one. Only changed
    /// together with `link` (inside its `send_modify`).
    down_since: StdMutex<Option<Instant>>,
}

impl Amqp {
    /// Connect, declare the topology and start the supervisor. Fails if the
    /// broker is unreachable.
    pub async fn connect(url: String, default_queue: bool) -> ApiResult<Arc<Self>> {
        let amqp = Arc::new(Self {
            url,
            default_queue,
            link: watch::Sender::new(None),
            wake: Notify::new(),
            generation: AtomicU64::new(0),
            closing: AtomicBool::new(false),
            down_since: StdMutex::new(None),
        });
        let link = amqp.open().await?;
        amqp.install(link);
        tokio::spawn(supervise(Arc::clone(&amqp)));
        Ok(amqp)
    }

    pub fn is_connected(&self) -> bool {
        self.link.borrow().as_ref().is_some_and(Link::is_healthy)
    }

    pub fn is_closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }

    /// Watch the current link (the consumer follows it).
    pub fn subscribe(&self) -> watch::Receiver<Option<Link>> {
        self.link.subscribe()
    }

    fn install(&self, link: Link) {
        self.link.send_modify(|current| {
            *current = Some(link);
            if let Ok(mut down_since) = self.down_since.lock() {
                *down_since = None;
            }
        });
    }

    /// Drop the link if it still is `generation` (`None`: whatever it is) and
    /// wake the supervisor. The old connection is not closed gracefully: that
    /// stops lapin's heartbeat, and against a silent peer it then never ends.
    pub fn mark_broken(&self, generation: Option<u64>) {
        self.link.send_if_modified(|current| {
            let matches = match (current.as_ref(), generation) {
                (Some(link), Some(generation)) => link.generation == generation,
                (Some(_), None) => true,
                (None, _) => false,
            };
            if matches {
                *current = None;
                if let Ok(mut down_since) = self.down_since.lock() {
                    down_since.get_or_insert_with(Instant::now);
                }
            }
            matches
        });
        self.wake.notify_one();
    }

    /// How much of the outage budget is left (all of it when connected).
    fn outage_budget_left(&self) -> Duration {
        let down_since = self.down_since.lock().ok().and_then(|d| *d);
        match down_since {
            Some(since) => OUTAGE_BUDGET.saturating_sub(since.elapsed()),
            None => OUTAGE_BUDGET,
        }
    }

    async fn open(&self) -> ApiResult<Link> {
        let attempt = async {
            let connection = Connection::connect(
                &self.url,
                ConnectionProperties::default().with_connection_name("twilight-dispatch".into()),
            )
            .await?;
            let publish = connection.create_channel().await?;
            let consume = connection.create_channel().await?;
            declare(&publish, &consume, self.default_queue).await?;
            Ok::<_, ApiError>(Link {
                generation: self.generation.fetch_add(1, Ordering::SeqCst) + 1,
                connection: Arc::new(connection),
                publish,
                consume,
            })
        };
        match tokio::time::timeout(CONNECT_TIMEOUT, attempt).await {
            Ok(result) => result,
            Err(_) => Err("timed out connecting to the AMQP broker".into()),
        }
    }

    /// A healthy link, waiting for the supervisor within the outage budget.
    async fn healthy_link(&self) -> Option<Link> {
        let mut rx = self.link.subscribe();
        let (healthy, generation) = {
            let current = rx.borrow();
            match current.as_ref() {
                Some(link) if link.is_healthy() => (Some(link.clone()), None),
                Some(link) => (None, Some(link.generation)),
                None => (None, None),
            }
        };
        if healthy.is_some() {
            return healthy;
        }
        // The link is unhealthy but nobody noticed yet.
        if generation.is_some() {
            self.mark_broken(generation);
        }
        let budget = self.outage_budget_left();
        if budget.is_zero() {
            return None;
        }
        self.wake.notify_one();
        let ready = rx.wait_for(|link| link.as_ref().is_some_and(Link::is_healthy));
        let link = match tokio::time::timeout(budget, ready).await {
            Ok(Ok(link)) => link.clone(),
            _ => None,
        };
        link
    }

    /// Publish to the gateway exchange. Waits for a reconnect within the
    /// outage budget and tries a message at most [`PUBLISH_ATTEMPTS`] times.
    pub async fn publish(
        &self,
        routing_key: &str,
        payload: &[u8],
        properties: BasicProperties,
    ) -> ApiResult<()> {
        let mut last_error: ApiError = "the AMQP broker is unavailable".into();
        for _ in 0..PUBLISH_ATTEMPTS {
            if self.is_closing() {
                last_error = "the AMQP connection is shutting down".into();
                break;
            }
            let Some(link) = self.healthy_link().await else {
                break;
            };
            let sent = tokio::time::timeout(
                PUBLISH_TIMEOUT,
                link.publish.basic_publish(
                    EXCHANGE.into(),
                    routing_key.into(),
                    BasicPublishOptions::default(),
                    payload,
                    properties.clone(),
                ),
            )
            .await;
            match sent {
                Ok(Ok(_confirm)) => return Ok(()),
                Ok(Err(err)) => last_error = err.into(),
                Err(_) => last_error = "timed out publishing to the AMQP broker".into(),
            }
            self.mark_broken(Some(link.generation));
        }
        AMQP_PUBLISH_FAILURES.inc();
        Err(last_error)
    }

    /// Close the publishing channel underneath the supervisor, the way a
    /// channel exception does. Only for tests.
    #[doc(hidden)]
    pub async fn break_channel_for_tests(&self) {
        let link = self.link.borrow().clone();
        if let Some(link) = link {
            let _ = link.publish.close(200, "test".into()).await;
        }
    }

    pub async fn close(&self) {
        self.closing.store(true, Ordering::SeqCst);
        self.wake.notify_one();
        let link = self.link.send_replace(None);
        if let Some(link) = link {
            let _ = tokio::time::timeout(
                Duration::from_secs(1),
                link.connection.close(200, "shutting down".into()),
            )
            .await;
        }
    }
}

/// Keep a healthy link installed until the connection is closed.
async fn supervise(amqp: Arc<Amqp>) {
    let mut backoff = Duration::from_millis(250);
    loop {
        if amqp.is_closing() {
            return;
        }
        let healthy = amqp.link.borrow().as_ref().map(Link::is_healthy);
        match healthy {
            Some(true) => {
                backoff = Duration::from_millis(250);
                tokio::select! {
                    _ = amqp.wake.notified() => {}
                    _ = tokio::time::sleep(CHECK_INTERVAL) => {}
                }
                continue;
            }
            Some(false) => amqp.mark_broken(None),
            None => {}
        }

        match amqp.open().await {
            Ok(link) if amqp.is_closing() => {
                let _ = link.connection.close(200, "shutting down".into()).await;
                return;
            }
            Ok(link) => {
                info!("Connected to the AMQP broker");
                AMQP_RECONNECTS.inc();
                amqp.install(link);
            }
            Err(err) => {
                warn!("AMQP reconnect failed, retrying in {:?}: {}", backoff, err);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

/// The durable queue options used by every version of this service; the
/// broker rejects a declaration with different flags.
const DURABLE_QUEUE: QueueDeclareOptions = QueueDeclareOptions {
    passive: false,
    durable: true,
    exclusive: false,
    auto_delete: false,
    nowait: false,
};

/// Declare `gateway.send` and set the prefetch on the consume channel.
pub async fn declare_consume(consume: &Channel) -> Result<(), lapin::Error> {
    consume
        .queue_declare(QUEUE_SEND.into(), DURABLE_QUEUE, FieldTable::default())
        .await?;
    consume
        .basic_qos(PREFETCH, BasicQosOptions::default())
        .await?;
    Ok(())
}

/// Declare the exchange and queues (idempotent).
async fn declare(publish: &Channel, consume: &Channel, default_queue: bool) -> ApiResult<()> {
    publish
        .exchange_declare(
            EXCHANGE.into(),
            ExchangeKind::Topic,
            ExchangeDeclareOptions {
                passive: false,
                durable: true,
                auto_delete: false,
                internal: false,
                nowait: false,
            },
            FieldTable::default(),
        )
        .await?;
    declare_consume(consume).await?;

    if default_queue {
        publish
            .queue_declare(QUEUE_RECV.into(), DURABLE_QUEUE, FieldTable::default())
            .await?;
        publish
            .queue_bind(
                QUEUE_RECV.into(),
                EXCHANGE.into(),
                "#".into(),
                QueueBindOptions::default(),
                FieldTable::default(),
            )
            .await?;
    }

    Ok(())
}
