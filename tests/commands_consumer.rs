#![recursion_limit = "256"]

//! The gateway command consumer against a real broker (set TEST_AMQP_URL; CI
//! uses LavinMQ, the production broker) when subscribing fails, times out or
//! is cancelled by the broker, when a command cannot be forwarded, and when
//! the connection is closed during an outage. A slow broker, one that stops
//! answering or taking writes and one that went away are played by a TCP
//! proxy ([`Proxy`]).

mod common;

use lapin::{
    options::{BasicPublishOptions, QueueDeclareOptions, QueueDeleteOptions, QueuePurgeOptions},
    types::{AMQPValue, FieldTable},
    BasicProperties, Connection, ConnectionProperties,
};
use std::{
    sync::{Arc, Mutex, Once},
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
    amqp::Amqp,
    commands::run_consumer,
    constants::QUEUE_SEND,
    gateway::registry::{Registry, ShardHandle},
    metrics::GATEWAY_COMMANDS,
};
use twilight_gateway::{Intents, Shard, ShardId};

/// The tests share `gateway.send` and the command counters of shard 0, so
/// they run one at a time.
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

/// Declare `gateway.send` (with the service's flags) and empty it.
async fn purge(url: &str) {
    let connection = connect(url).await;
    let channel = connection.create_channel().await.unwrap();
    channel
        .queue_declare(
            QUEUE_SEND.into(),
            QueueDeclareOptions {
                durable: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .unwrap();
    channel
        .queue_purge(QUEUE_SEND.into(), QueuePurgeOptions::default())
        .await
        .unwrap();
    let _ = connection.close(200, "done".into()).await;
}

/// Replace `gateway.send` by an empty durable queue. With `ttl` it has a
/// message TTL, which the service's declaration (without arguments) does not
/// match, so the broker refuses that; without, it is declared as the service
/// declares it.
async fn redeclare(url: &str, ttl: bool) {
    let connection = connect(url).await;
    let channel = connection.create_channel().await.unwrap();
    let mut arguments = FieldTable::default();
    if ttl {
        arguments.insert("x-message-ttl".into(), AMQPValue::LongLongInt(60_000));
    }
    channel
        .queue_delete(QUEUE_SEND.into(), QueueDeleteOptions::default())
        .await
        .unwrap();
    channel
        .queue_declare(
            QUEUE_SEND.into(),
            QueueDeclareOptions {
                durable: true,
                ..Default::default()
            },
            arguments,
        )
        .await
        .unwrap();
    let _ = connection.close(200, "done".into()).await;
}

/// Publish a command to `gateway.send`, as a producer does.
async fn send_command(url: &str, body: &[u8]) {
    let connection = connect(url).await;
    let channel = connection.create_channel().await.unwrap();
    channel
        .basic_publish(
            "".into(),
            QUEUE_SEND.into(),
            BasicPublishOptions::default(),
            body,
            BasicProperties::default(),
        )
        .await
        .unwrap();
    let _ = connection.close(200, "done".into()).await;
}

fn presence(shard: u32, since: u64) -> String {
    format!(
        r#"{{"op":0,"shard":{shard},"data":{{"op":3,"d":{{"status":"online","activities":[],"afk":false,"since":{since}}}}}}}"#
    )
}

/// The commands sent to shard 0 so far.
fn sent() -> u64 {
    GATEWAY_COMMANDS.with_label_values(&["0", "sent"]).get()
}

/// Publish commands for shard 0 to `gateway.send` until one reached the
/// shard: one at a time, each given 1 s, at most `attempts` times. A command
/// published while the queue does not exist is dropped by the broker, hence
/// the retries.
async fn send_until_sent(url: &str, attempts: u64) -> bool {
    let before = sent();
    for attempt in 0..attempts {
        send_command(url, presence(0, attempt).as_bytes()).await;
        for _ in 0..20 {
            if sent() > before {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    false
}

/// A shard that is never polled, so it never connects. Its sender accepts
/// commands, which is all the consumer needs.
fn idle_shard(id: u32, total: u32) -> Shard {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Shard::new(ShardId::new(id, total), "token".into(), Intents::empty())
}

/// A registry with only `shard`.
fn registry(shard: &Shard) -> Arc<Registry> {
    Arc::new(Registry::new([Arc::new(ShardHandle::new(
        shard.id().number(),
        Some(shard.sender()),
        None,
    ))]))
}

/// The warnings this test binary logged.
static WARNINGS: Mutex<Vec<u8>> = Mutex::new(Vec::new());

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
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_writer(|| WarningWriter)
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .try_init();
    });
    WARNINGS.lock().unwrap().len()
}

fn warnings_since(start: usize) -> String {
    String::from_utf8_lossy(&WARNINGS.lock().unwrap()[start..]).into_owned()
}

/// The warnings logged since `start`, as soon as they contain `text` or
/// after `limit`.
async fn warnings_with(start: usize, text: &str, limit: Duration) -> String {
    let deadline = Instant::now() + limit;
    loop {
        let warnings = warnings_since(start);
        if warnings.contains(text) || Instant::now() >= deadline {
            return warnings;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// The broker cancels the consumer, or refuses to declare gateway.send

/// Deleting gateway.send (an operator in the management UI) cancels the
/// consumer. It subscribes again on the same connection, which declares the
/// queue again, rather than replacing the connection or stopping.
#[tokio::test]
async fn the_consumer_resubscribes_after_gateway_send_is_deleted() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    purge(&url).await;
    let shard = idle_shard(0, 1);
    let amqp = Amqp::connect(url.clone(), false).await.unwrap();
    let consumer = tokio::spawn(run_consumer(amqp.clone(), registry(&shard), 1));
    assert!(send_until_sent(&url, 10).await, "consumed before");
    // Nothing is unacknowledged or in flight when the queue goes.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let link = generation(&amqp);
    assert!(link.is_some());

    let operator = connect(&url).await;
    let channel = operator.create_channel().await.unwrap();
    channel
        .queue_delete(QUEUE_SEND.into(), QueueDeleteOptions::default())
        .await
        .unwrap();
    let _ = operator.close(200, "done".into()).await;

    assert!(
        send_until_sent(&url, 10).await,
        "consumed after gateway.send was deleted"
    );
    assert_eq!(
        generation(&amqp),
        link,
        "resubscribed on the same connection"
    );
    consumer.abort();
    amqp.close().await;
}

/// While gateway.send cannot be declared (here it exists with other
/// arguments) subscribing fails: the consumer warns, gives the link up and
/// retries with a backoff, and it consumes once the queue can be declared.
/// Publishing does not depend on gateway.send and keeps working meanwhile.
#[tokio::test]
async fn the_consumer_recovers_once_gateway_send_can_be_declared() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let logged = capture_warnings();
    let shard = idle_shard(0, 1);
    let amqp = Amqp::connect(url.clone(), false).await.unwrap();
    redeclare(&url, true).await;
    let consumer = tokio::spawn(run_consumer(amqp.clone(), registry(&shard), 1));
    tokio::time::sleep(Duration::from_secs(2)).await;
    let published = amqp
        .publish("TYPING_START", b"{}", BasicProperties::default())
        .await;
    let warnings = warnings_since(logged);
    // Before any assertion, so that a failure cannot leave a gateway.send
    // that the service cannot declare to the tests that run after this one.
    redeclare(&url, false).await;

    assert!(
        warnings.contains("Failed to consume gateway commands"),
        "{warnings}"
    );
    assert!(published.is_ok(), "{published:?}");
    assert!(
        send_until_sent(&url, 20).await,
        "consumed once gateway.send could be declared"
    );
    consumer.abort();
    amqp.close().await;
}

// ---------------------------------------------------------------------------
// A broker that is slow, stops answering or taking writes, or goes away

/// What [`Proxy`] does with the connections it forwards. Held bytes wait in
/// the proxy (and are lost if the connection is closed meanwhile); the rest
/// flows.
#[derive(Clone, Copy, Debug, Default)]
struct Gate {
    /// How late each chunk is passed on, both ways.
    delay: Duration,
    /// The client's bytes are held back: nothing more reaches the broker, so
    /// the client's writes stall once the buffers on the way are full.
    requests: bool,
    /// The broker's bytes are held back: requests reach the broker but no
    /// answer comes back, as from a broker that stopped answering.
    replies: bool,
    /// Every connection is closed and new ones are refused, as by a broker
    /// that went away.
    down: bool,
    /// Bumped to close every connection forwarded so far.
    epoch: u64,
}

/// A TCP proxy in front of the test broker that can delay or hold back each
/// direction of the connections it forwards, or play a broker that is gone.
/// The gate applies to every connection, also those opened later. Dropping
/// the proxy closes the connections it forwards.
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

    /// Pass every chunk on `delay` late, both ways: each round trip to the
    /// broker takes at least twice that.
    fn slow_down(&self, delay: Duration) {
        self.gate.send_modify(|gate| gate.delay = delay);
    }

    /// Hold back what the client sends and what the broker sends, or pass
    /// it on again (with what was held).
    fn hold(&self, requests: bool, replies: bool) {
        self.gate.send_modify(|gate| {
            gate.requests = requests;
            gate.replies = replies;
        });
    }

    /// Close every connection forwarded so far, losing what it held back, as
    /// a dropped network path does; later connections are forwarded in full.
    /// One change, so a connection never sees the gate open before it is
    /// closed (it would pass on the held bytes).
    fn drop_connections(&self) {
        self.gate.send_modify(|gate| {
            *gate = Gate {
                epoch: gate.epoch + 1,
                ..Gate::default()
            }
        });
    }

    /// Close every connection and refuse new ones.
    fn go_away(&self) {
        self.gate.send_modify(|gate| gate.down = true);
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        // Dropping `gate` closes the forwarded connections.
        self.listener.abort();
    }
}

async fn relay(client: TcpStream, broker: String, gate: watch::Receiver<Gate>) {
    let current = *gate.borrow();
    if current.down {
        return;
    }
    let epoch = current.epoch;
    let Ok(broker) = TcpStream::connect(broker).await else {
        return;
    };
    let (client_read, client_write) = client.into_split();
    let (broker_read, broker_write) = broker.into_split();
    let mut closed = gate.clone();
    tokio::select! {
        _ = closed.wait_for(|gate| gate.down || gate.epoch != epoch) => {}
        _ = pipe(client_read, broker_write, gate.clone(), epoch, |gate| gate.requests) => {}
        _ = pipe(broker_read, client_write, gate, epoch, |gate| gate.replies) => {}
    }
}

/// Copy one direction of a connection, each chunk the gate's delay late,
/// holding the bytes while `held`.
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
        let delay = gate.borrow().delay;
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        let open = gate
            .wait_for(|gate| gate.epoch != epoch || !held(gate))
            .await
            .is_ok_and(|gate| gate.epoch == epoch);
        if !open || to.write_all(&buffer[..read]).await.is_err() {
            return;
        }
    }
}

/// Subscribing binds the process' queue once per shard, one broker round
/// trip each. With 200 shards and a round trip of 30 ms that takes over 6 s,
/// longer than the flat 5 s limit the consumer used to have: it gave the
/// connection up after every attempt and never consumed. The limit grows with
/// the shards, so the first attempt succeeds.
#[tokio::test]
async fn a_process_with_many_shards_subscribes_through_a_slow_broker() {
    const SHARDS: u32 = 200;
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let logged = capture_warnings();
    purge(&url).await;
    let proxy = Proxy::start(&url).await;
    // 205 sequential round trips (3 declarations, 200 bindings, 2 consumers)
    // of at least 30 ms take 6.15 s; the limit is 5 s + 50 ms per shard, 15 s.
    proxy.slow_down(Duration::from_millis(15));
    let shard = idle_shard(0, SHARDS);
    let others = (1..SHARDS).map(|id| Arc::new(ShardHandle::new(id, None, None)));
    let first = Arc::new(ShardHandle::new(0, Some(shard.sender()), None));
    let handles = Arc::new(Registry::new(std::iter::once(first).chain(others)));
    let amqp = Amqp::connect(proxy.url.clone(), false).await.unwrap();
    let consumer = tokio::spawn(run_consumer(amqp.clone(), handles, SHARDS));

    let consumed = send_until_sent(&url, 30).await;
    let warnings = warnings_since(logged);
    assert!(consumed, "never consumed:\n{warnings}");
    assert!(
        !warnings.contains("Timed out subscribing to gateway commands"),
        "{warnings}"
    );
    consumer.abort();
    amqp.close().await;
}

/// A subscribe the broker never answers times out: the consumer warns, gives
/// the link up and, after the backoff, subscribes on the next link.
#[tokio::test]
async fn a_subscribe_that_times_out_is_retried_on_a_new_link() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let logged = capture_warnings();
    purge(&url).await;
    let proxy = Proxy::start(&url).await;
    let shard = idle_shard(0, 1);
    let amqp = Amqp::connect(proxy.url.clone(), false).await.unwrap();
    let link = generation(&amqp);
    assert!(link.is_some());

    // The declarations of the subscribe reach the broker, its answers do not.
    proxy.hold(false, true);
    let consumer = tokio::spawn(run_consumer(amqp.clone(), registry(&shard), 1));
    let timed_out = "Timed out subscribing to gateway commands";
    let warnings = warnings_with(logged, timed_out, Duration::from_secs(30)).await;
    proxy.hold(false, false);
    assert!(warnings.contains(timed_out), "{warnings}");

    assert!(
        send_until_sent(&url, 20).await,
        "consumed after the subscribe timed out"
    );
    assert!(generation(&amqp) > link, "on a new link");
    consumer.abort();
    amqp.close().await;
}

/// The connection is closed while there is no link, because the broker is
/// away: the consumer returns rather than waiting for a link that never
/// comes (it used to wait for one forever).
#[tokio::test]
async fn the_consumer_returns_when_closed_during_an_outage() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    purge(&url).await;
    let proxy = Proxy::start(&url).await;
    let shard = idle_shard(0, 1);
    let amqp = Amqp::connect(proxy.url.clone(), false).await.unwrap();
    let consumer = tokio::spawn(run_consumer(amqp.clone(), registry(&shard), 1));
    assert!(send_until_sent(&url, 10).await, "the consumer runs");

    proxy.go_away();
    let deadline = Instant::now() + Duration::from_secs(20);
    while generation(&amqp).is_some() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(generation(&amqp), None, "the link is dropped");
    // The consumer noticed and waits for a new link, which cannot connect.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(generation(&amqp), None, "the broker is still away");

    amqp.close().await;
    tokio::time::timeout(Duration::from_secs(5), consumer)
        .await
        .expect("the consumer returns once the connection is closed")
        .unwrap();
}

/// Bigger than everything that buffers a write on its way to the broker
/// (lapin's send buffer of 32 frames of 128 KiB, the sockets' buffers), and
/// smaller than LavinMQ's `max_message_size` (128 MiB).
const UNWRITABLE: usize = 32 * 1024 * 1024;

/// The commands forwarded to the process running shard 1 so far.
fn forwarded() -> u64 {
    GATEWAY_COMMANDS
        .with_label_values(&["1", "forwarded"])
        .get()
}

/// A command for another process' shard that cannot be forwarded in time
/// (the broker takes nothing more) is not counted as forwarded. It cannot be
/// requeued in time either, so the connection is given up; the broker then
/// requeues it, and it is forwarded on the next connection. Each failure is
/// logged with what failed.
#[tokio::test]
async fn a_command_that_cannot_be_forwarded_is_requeued() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let logged = capture_warnings();
    purge(&url).await;
    let proxy = Proxy::start(&url).await;
    // This process runs shard 0 of 2. Nothing runs shard 1, so the broker
    // drops what is forwarded to it.
    let shard = idle_shard(0, 2);
    let amqp = Amqp::connect(proxy.url.clone(), false).await.unwrap();
    let link = generation(&amqp);
    let consumer = tokio::spawn(run_consumer(amqp.clone(), registry(&shard), 2));
    assert!(send_until_sent(&url, 10).await, "the consumer runs");
    // Every acknowledgement is written before the broker takes nothing more.
    tokio::time::sleep(Duration::from_secs(1)).await;

    let before = forwarded();
    proxy.hold(true, false);
    let mut command = br#"{"op":0,"shard":1,"data":{"op":3,"d":{"padding":""#.to_vec();
    command.resize(command.len() + UNWRITABLE, b'x');
    command.extend_from_slice(br#""}}}"#);
    send_command(&url, &command).await;
    let reconnecting = "Failed to handle a gateway command; reconnecting";
    let warnings = warnings_with(logged, reconnecting, Duration::from_secs(40)).await;
    let counted = forwarded();
    let replaced = generation(&amqp) != link;
    // What the proxy held back is lost with the connection, and the broker
    // requeues the unacknowledged command.
    proxy.drop_connections();
    assert!(
        warnings.contains("Timed out forwarding a command for shard 1"),
        "{warnings}"
    );
    assert!(
        warnings.contains("Timed out acknowledging a gateway command"),
        "{warnings}"
    );
    assert!(warnings.contains(reconnecting), "{warnings}");
    assert_eq!(counted, before, "counted as forwarded");
    assert!(replaced, "the link is given up");

    let deadline = Instant::now() + Duration::from_secs(30);
    while forwarded() == before && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(forwarded(), before + 1, "forwarded on the next link");
    // Its acknowledgement is written before the consumer stops.
    tokio::time::sleep(Duration::from_secs(1)).await;
    consumer.abort();
    amqp.close().await;
}
