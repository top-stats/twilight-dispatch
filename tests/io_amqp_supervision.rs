#![recursion_limit = "256"]

//! More of `amqp::Amqp` against a real broker (set TEST_AMQP_URL; CI uses
//! LavinMQ, the production broker): `gateway.recv` with and without
//! DEFAULT_QUEUE, the `amqp_reconnects` counter, the supervisor's backoff
//! once a link lived, and publishes without confirms that cannot be
//! written. The proxy is a copy of the one in tests/confirms.rs.

mod common;

use lapin::{
    options::{BasicGetOptions, QueueDeclareOptions, QueueDeleteOptions, QueuePurgeOptions},
    types::FieldTable,
    BasicProperties, Connection, ConnectionProperties,
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener, TcpStream,
    },
    sync::watch,
    task::JoinHandle,
};
use twilight_dispatch::{
    amqp::{Amqp, PUBLISH_TIMEOUT},
    constants::QUEUE_RECV,
    metrics::AMQP_RECONNECTS,
    models::ApiResult,
};

/// The tests share `gateway.recv`, the reconnect counter and the captured
/// warnings, so they run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The generation of the installed link, `None` while there is none.
fn generation(amqp: &Amqp) -> Option<u64> {
    amqp.subscribe()
        .borrow()
        .as_ref()
        .map(|link| link.generation)
}

async fn connect(url: &str) -> Connection {
    Connection::connect(url, ConnectionProperties::default())
        .await
        .unwrap()
}

/// Poll `check` for up to 10 s. The caller asserts afterwards, so a timeout
/// reports the values.
async fn wait_until(check: impl Fn() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Drop the current link and wait until the supervisor installed another.
async fn reconnect(amqp: &Amqp) {
    amqp.mark_broken(None);
    assert!(!amqp.is_connected(), "the link was not dropped");
    wait_until(|| amqp.is_connected()).await;
    assert!(amqp.is_connected(), "not reconnected within 10 s");
}

/// Whether `queue` exists. Each check has a channel of its own: the broker
/// closes the channel of a passive declare that finds no queue.
async fn queue_exists(connection: &Connection, queue: &str) -> bool {
    let channel = connection.create_channel().await.unwrap();
    channel
        .queue_declare(
            queue.into(),
            QueueDeclareOptions {
                passive: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .is_ok()
}

/// Empty `gateway.recv` if it exists: another test may have left it bound
/// to every event, and the large messages below are no use to anyone.
async fn purge_recv(url: &str) {
    let connection = connect(url).await;
    let channel = connection.create_channel().await.unwrap();
    let _ = channel
        .queue_purge(QUEUE_RECV.into(), QueuePurgeOptions::default())
        .await;
    let _ = connection.close(200, "done".into()).await;
}

/// The warnings this test binary logged.
static WARNINGS: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());

struct WarningWriter;

impl std::io::Write for WarningWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        WARNINGS.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Collect warnings in [`WARNINGS`]; returns how many bytes there already
/// are, so a test can read only its own.
fn capture_warnings() -> usize {
    let _ = tracing_subscriber::fmt()
        .with_writer(|| WarningWriter)
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .try_init();
    WARNINGS.lock().unwrap().len()
}

fn warnings_since(start: usize) -> String {
    String::from_utf8_lossy(&WARNINGS.lock().unwrap()[start..]).into_owned()
}

// ---------------------------------------------------------------------------
// Topology and reconnects

/// `gateway.recv` is declared and bound to every event only with
/// DEFAULT_QUEUE: a deployment whose consumers have queues of their own
/// would otherwise fill a durable queue that nothing consumes.
#[tokio::test]
async fn only_default_queue_declares_and_binds_gateway_recv() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let admin = connect(&url).await;
    let channel = admin.create_channel().await.unwrap();
    channel
        .queue_delete(QUEUE_RECV.into(), QueueDeleteOptions::default())
        .await
        .unwrap();

    let without = Amqp::connect(url.clone(), false).await.unwrap();
    assert!(
        !queue_exists(&admin, QUEUE_RECV).await,
        "DEFAULT_QUEUE=false declared {QUEUE_RECV}"
    );
    // Unroutable, and published all the same: the broker drops it.
    without
        .publish("ANY_EVENT", b"{}", BasicProperties::default())
        .await
        .unwrap();
    assert!(
        !queue_exists(&admin, QUEUE_RECV).await,
        "publishing declared {QUEUE_RECV}"
    );
    without.close().await;

    let with = Amqp::connect(url, true).await.unwrap();
    assert!(
        queue_exists(&admin, QUEUE_RECV).await,
        "DEFAULT_QUEUE=true did not declare {QUEUE_RECV}"
    );
    with.publish("ANY_EVENT", b"{}", BasicProperties::default())
        .await
        .unwrap();
    let mut routing_key = None;
    for _ in 0..100 {
        let message = channel
            .basic_get(QUEUE_RECV.into(), BasicGetOptions { no_ack: true })
            .await
            .unwrap();
        if let Some(message) = message {
            routing_key = Some(message.delivery.routing_key.to_string());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        routing_key.as_deref(),
        Some("ANY_EVENT"),
        "{QUEUE_RECV} is bound to every event"
    );
    with.close().await;
    let _ = admin.close(200, "done".into()).await;
}

/// `amqp_reconnects` counts the links the supervisor opens after one broke,
/// not the first connection, which would count every start as a reconnect.
#[tokio::test]
async fn reconnects_are_counted_but_the_first_connect_is_not() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let before = AMQP_RECONNECTS.get();
    let amqp = Amqp::connect(url, false).await.unwrap();
    assert_eq!(
        AMQP_RECONNECTS.get(),
        before,
        "the first connect counted as a reconnect"
    );

    for round in 1..=2 {
        amqp.mark_broken(None);
        assert!(!amqp.is_connected(), "round {round}: still connected");
        // Nothing was awaited since, so there is no link: a report without
        // one, and breaking the channel of none, change nothing.
        amqp.mark_broken(None);
        amqp.break_channel_for_tests().await;
        wait_until(|| amqp.is_connected()).await;
        assert!(amqp.is_connected(), "round {round}: not reconnected");
        assert_eq!(AMQP_RECONNECTS.get(), before + round, "round {round}");
    }
    amqp.close().await;
}

/// The supervisor's warning before it reconnects after a link that broke
/// right after connecting, with a backoff of 250 ms.
const RETRY_AFTER_250_MS: &str = "broke right after connecting; retrying in 250ms";
/// The same with a backoff of 500 ms.
const RETRY_AFTER_500_MS: &str = "broke right after connecting; retrying in 500ms";

/// The backoff for links that break right after connecting starts over once
/// a link lived for 5 s (MIN_LINK_LIFETIME): a broker that fails long after
/// it last did is retried quickly again.
#[tokio::test]
async fn the_backoff_starts_over_once_a_link_lived() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let logged = capture_warnings();
    let amqp = Amqp::connect(url, false).await.unwrap();

    // The first link was installed by `connect`, so the first reconnect
    // does not wait; the second follows a link that lived only briefly, and
    // doubles the backoff to 500 ms.
    reconnect(&amqp).await;
    reconnect(&amqp).await;
    let warnings = warnings_since(logged);
    assert!(warnings.contains(RETRY_AFTER_250_MS), "{warnings}");

    // The supervisor checks the link every second, and resets the backoff
    // at its first check after the link lived 5 s.
    tokio::time::sleep(Duration::from_secs(8)).await;
    let logged = capture_warnings();
    // After the link that lived no wait; then after one that broke right
    // away the backoff is 250 ms again, not 500 ms.
    reconnect(&amqp).await;
    reconnect(&amqp).await;
    let warnings = warnings_since(logged);
    assert!(warnings.contains(RETRY_AFTER_250_MS), "{warnings}");
    assert!(!warnings.contains(RETRY_AFTER_500_MS), "{warnings}");
    amqp.close().await;
}

// ---------------------------------------------------------------------------
// Publishes without confirms that cannot be written

/// Bigger than everything that buffers a write on its way to the broker
/// (lapin's send buffer of 32 frames of 128 KiB, the sockets' buffers), and
/// smaller than LavinMQ's `max_message_size` (128 MiB).
const UNWRITABLE: usize = 32 * 1024 * 1024;

/// Publish a message of [`UNWRITABLE`] bytes in the background, as a worker
/// does.
fn publish_large(amqp: &Arc<Amqp>) -> JoinHandle<ApiResult<()>> {
    let amqp = Arc::clone(amqp);
    tokio::spawn(async move {
        let payload = vec![0_u8; UNWRITABLE];
        amqp.publish("IO_UNWRITABLE", &payload, BasicProperties::default())
            .await
    })
}

/// The publish resolves within `limit`.
async fn finishes<T>(task: JoinHandle<T>, limit: Duration) -> T {
    tokio::time::timeout(limit, task)
        .await
        .unwrap_or_else(|_| panic!("the publish did not finish within {limit:?}"))
        .unwrap()
}

/// What [`Proxy`] holds back. Held bytes wait in the proxy (and are lost if
/// the connection is closed meanwhile); the rest flows.
#[derive(Clone, Copy, Debug, Default)]
struct Gate {
    /// The client's frames: publishes never reach the broker.
    requests: bool,
    /// The broker's frames: confirms (and the handshake of new connections)
    /// never reach the client.
    replies: bool,
    /// Bumped to close every connection forwarded so far.
    epoch: u64,
}

/// A TCP proxy in front of the test broker that can hold back each
/// direction of the connections it forwards. The gate applies to every
/// connection, also those opened later.
struct Proxy {
    /// TEST_AMQP_URL with the proxy's address.
    url: String,
    gate: watch::Sender<Gate>,
    listener: JoinHandle<()>,
}

impl Proxy {
    async fn start(url: &str) -> Self {
        // amqp://user:pass@host:port/vhost
        let (credentials, rest) = url.split_once('@').expect("credentials in TEST_AMQP_URL");
        let (broker, vhost) = rest.split_once('/').unwrap_or((rest, ""));
        let broker = broker.to_owned();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("{credentials}@{}/{vhost}", listener.local_addr().unwrap());
        let gate = watch::Sender::new(Gate::default());
        let listener = tokio::spawn({
            let gate = gate.subscribe();
            async move {
                while let Ok((client, _)) = listener.accept().await {
                    tokio::spawn(relay(client, broker.clone(), gate.clone()));
                }
            }
        });
        Self {
            url,
            gate,
            listener,
        }
    }

    fn hold(&self, requests: bool, replies: bool) {
        self.gate.send_modify(|gate| {
            gate.requests = requests;
            gate.replies = replies;
        });
    }

    /// Close every forwarded connection, losing what it held back, as a
    /// broker crash or a dropped network path does; later connections are
    /// forwarded in full. One change, so a connection never sees the gate
    /// open before it is closed (it would deliver the held bytes).
    fn drop_connections(&self) {
        self.gate.send_modify(|gate| {
            *gate = Gate {
                epoch: gate.epoch + 1,
                ..Gate::default()
            }
        });
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        // Dropping `gate` closes the forwarded connections.
        self.listener.abort();
    }
}

async fn relay(client: TcpStream, broker: String, gate: watch::Receiver<Gate>) {
    let epoch = gate.borrow().epoch;
    let Ok(broker) = TcpStream::connect(broker).await else {
        return;
    };
    let (client_read, client_write) = client.into_split();
    let (broker_read, broker_write) = broker.into_split();
    let mut closed = gate.clone();
    tokio::select! {
        _ = closed.wait_for(|gate| gate.epoch != epoch) => {}
        _ = pipe(client_read, broker_write, gate.clone(), epoch, |gate| gate.requests) => {}
        _ = pipe(broker_read, client_write, gate, epoch, |gate| gate.replies) => {}
    }
}

/// Copy one direction of a connection, holding the bytes while `held`.
async fn pipe(
    mut from: OwnedReadHalf,
    mut to: OwnedWriteHalf,
    mut gate: watch::Receiver<Gate>,
    epoch: u64,
    held: fn(&Gate) -> bool,
) {
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = match from.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };
        let open = gate
            .wait_for(|gate| gate.epoch != epoch || !held(gate))
            .await
            .is_ok_and(|gate| gate.epoch == epoch);
        if !open || to.write_all(&buffer[..read]).await.is_err() {
            return;
        }
    }
}

/// Without confirms a publish succeeds once lapin wrote the message to the
/// socket. One that the broker does not take within PUBLISH_TIMEOUT fails
/// that attempt and drops the link, and is retried on the next one.
#[tokio::test]
async fn without_confirms_a_publish_that_cannot_be_written_in_time_is_retried() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let proxy = Proxy::start(&url).await;
    let amqp = Amqp::connect(proxy.url.clone(), false).await.unwrap();
    let link = generation(&amqp).unwrap();

    // The broker gets nothing more, so the write stalls once the buffers on
    // the way are full.
    proxy.hold(true, false);
    let started = Instant::now();
    let published = publish_large(&amqp);
    wait_until(|| generation(&amqp) != Some(link)).await;
    let dropped = started.elapsed();
    assert!(
        dropped >= PUBLISH_TIMEOUT - Duration::from_millis(100)
            && dropped <= PUBLISH_TIMEOUT + Duration::from_secs(2),
        "the link was dropped after {dropped:?}"
    );
    // Waiting for a new link, which cannot connect while requests are held.
    assert!(
        !published.is_finished(),
        "the publish gave up after one attempt"
    );

    proxy.drop_connections();
    finishes(published, Duration::from_secs(15))
        .await
        .expect("published on the next link");
    assert!(
        generation(&amqp).is_some_and(|now| now > link),
        "published again on a new link"
    );
    amqp.close().await;
    purge_recv(&url).await;
}

/// Without confirms, a publish whose connection drops while it is being
/// written was not published: it fails that attempt and is retried on the
/// next link, rather than reported as published.
#[tokio::test]
async fn without_confirms_a_publish_cut_off_with_its_connection_is_retried() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let proxy = Proxy::start(&url).await;
    let amqp = Amqp::connect(proxy.url.clone(), false).await.unwrap();
    let link = generation(&amqp).unwrap();

    proxy.hold(true, false);
    let published = publish_large(&amqp);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!published.is_finished(), "returned unwritten");
    assert_eq!(generation(&amqp), Some(link), "still the same link");

    proxy.drop_connections();
    finishes(published, PUBLISH_TIMEOUT + Duration::from_secs(10))
        .await
        .expect("published on the next link");
    assert!(
        generation(&amqp).is_some_and(|now| now > link),
        "published again on a new link"
    );
    amqp.close().await;
    purge_recv(&url).await;
}
