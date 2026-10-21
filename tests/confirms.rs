#![recursion_limit = "256"]

//! Publisher confirms against a real broker (set TEST_AMQP_URL; CI uses
//! LavinMQ, the production broker).
//!
//! An event counts as published only once the broker confirmed it: the
//! worker then lets the resume point pass it, so an event that was only
//! written to the socket would be lost with the connection. The tests play a
//! broker whose confirm is late, lost with the connection or never comes,
//! through a TCP proxy ([`Proxy`]) that can hold back either direction of
//! the connections it forwards.

mod common;

use lapin::{
    options::{
        BasicGetOptions, QueueBindOptions, QueueDeclareOptions, QueueDeleteOptions,
        QueuePurgeOptions,
    },
    types::{AMQPValue, FieldTable},
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
    amqp::{Amqp, OUTAGE_BUDGET, PUBLISH_TIMEOUT},
    constants::{EXCHANGE, QUEUE_RECV},
    metrics::AMQP_PUBLISH_FAILURES,
    models::ApiResult,
};

/// The tests share the `gateway.recv` queue, so they run one at a time.
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

/// Declare a durable queue (with the service's flags) and empty it.
async fn purge(url: &str, queue: &str) {
    let connection = connect(url).await;
    let channel = connection.create_channel().await.unwrap();
    channel
        .queue_declare(
            queue.into(),
            QueueDeclareOptions {
                durable: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .unwrap();
    channel
        .queue_purge(queue.into(), QueuePurgeOptions::default())
        .await
        .unwrap();
    let _ = connection.close(200, "done".into()).await;
}

/// How many messages wait in `gateway.recv`. Nothing consumes it, so the
/// ready count of a passive declare is every message the broker took.
async fn stored(url: &str) -> u32 {
    let connection = connect(url).await;
    let channel = connection.create_channel().await.unwrap();
    let queue = channel
        .queue_declare(
            QUEUE_RECV.into(),
            QueueDeclareOptions {
                passive: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .unwrap();
    let _ = connection.close(200, "done".into()).await;
    queue.message_count()
}

/// Wait up to 10 s until `gateway.recv` holds `count` messages.
async fn wait_for_stored(url: &str, count: u32) {
    let mut now = 0;
    for _ in 0..200 {
        now = stored(url).await;
        if now == count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("expected {count} messages in {QUEUE_RECV}, got {now}");
}

/// Take every message from `gateway.recv`: (routing key, body, message id).
async fn take_all(url: &str) -> Vec<(String, Vec<u8>, Option<String>)> {
    let connection = connect(url).await;
    let channel = connection.create_channel().await.unwrap();
    let mut messages = Vec::new();
    while let Some(message) = channel
        .basic_get(QUEUE_RECV.into(), BasicGetOptions { no_ack: true })
        .await
        .unwrap()
    {
        let delivery = message.delivery;
        messages.push((
            delivery.routing_key.to_string(),
            delivery.data,
            delivery
                .properties
                .message_id()
                .as_ref()
                .map(ToString::to_string),
        ));
    }
    let _ = connection.close(200, "done".into()).await;
    messages
}

/// Publish an event in the background, as a worker does, with a message id.
fn publish(amqp: &Arc<Amqp>, routing_key: &'static str, id: &str) -> JoinHandle<ApiResult<()>> {
    let amqp = Arc::clone(amqp);
    let properties = BasicProperties::default().with_message_id(id.into());
    tokio::spawn(async move { amqp.publish(routing_key, b"{}", properties).await })
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

/// The publish resolves within `limit`.
async fn finishes<T>(task: JoinHandle<T>, limit: Duration) -> T {
    tokio::time::timeout(limit, task)
        .await
        .unwrap_or_else(|_| panic!("the publish did not finish within {limit:?}"))
        .unwrap()
}

// ---------------------------------------------------------------------------
// A broker whose confirms are late, lost or never sent

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
/// direction of the connections it forwards, so a test can delay or lose a
/// confirm after the broker took the message, or lose the message itself.
/// The gate applies to every connection, also those opened later.
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

/// A publish returns only once the broker confirmed it: while the confirm
/// is held back it keeps waiting, although the broker already stored the
/// message. Before, it returned as soon as the frames were written.
#[tokio::test]
async fn publishes_wait_for_the_confirm() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let proxy = Proxy::start(&url).await;
    let amqp = Amqp::connect(proxy.url.clone(), true).await.unwrap();
    purge(&url, QUEUE_RECV).await;
    let link = generation(&amqp);

    proxy.hold(false, true);
    let published = publish(&amqp, "MESSAGE_CREATE", "0:a:1");
    wait_for_stored(&url, 1).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !published.is_finished(),
        "returned before the broker confirmed"
    );

    proxy.hold(false, false);
    finishes(published, Duration::from_secs(2)).await.unwrap();
    assert_eq!(generation(&amqp), link, "confirmed on the same link");
    let messages = take_all(&url).await;
    assert_eq!(messages.len(), 1, "{messages:?}");

    // Without a delay: the message is stored when the publish returns.
    publish(&amqp, "MESSAGE_UPDATE", "0:a:2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored(&url).await, 1);
    amqp.close().await;
}

/// A publish that was written to the socket but never reached the broker
/// (lost with the connection) is not reported as published: it is retried
/// on the next link and stored once. Before, the publish had already
/// returned, so the worker let the resume point pass an event the broker
/// never got.
#[tokio::test]
async fn a_publish_lost_with_its_connection_is_retried() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let proxy = Proxy::start(&url).await;
    let amqp = Amqp::connect(proxy.url.clone(), true).await.unwrap();
    purge(&url, QUEUE_RECV).await;
    let link = generation(&amqp).unwrap();

    proxy.hold(true, true);
    let published = publish(&amqp, "GUILD_CREATE", "0:a:1");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!published.is_finished(), "returned without a confirm");
    assert_eq!(stored(&url).await, 0, "the broker never got it");

    proxy.drop_connections();
    finishes(published, Duration::from_secs(5)).await.unwrap();
    assert!(
        generation(&amqp).is_some_and(|now| now > link),
        "published again on a new link"
    );
    let messages = take_all(&url).await;
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0].0, "GUILD_CREATE");
    amqp.close().await;
}

/// When the connection drops after the broker stored a message but before
/// its confirm arrived, the publish is retried on the next link: the
/// message is delivered twice, with the same message id (at-least-once;
/// consumers dedupe by message_id), rather than possibly not at all.
#[tokio::test]
async fn a_publish_whose_confirm_is_lost_is_delivered_at_least_once() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let proxy = Proxy::start(&url).await;
    let amqp = Amqp::connect(proxy.url.clone(), true).await.unwrap();
    purge(&url, QUEUE_RECV).await;
    let link = generation(&amqp).unwrap();

    proxy.hold(false, true);
    let published = publish(&amqp, "GUILD_UPDATE", "3:session:42");
    wait_for_stored(&url, 1).await;
    assert!(!published.is_finished(), "returned without a confirm");

    proxy.drop_connections();
    finishes(published, Duration::from_secs(5)).await.unwrap();
    assert!(
        generation(&amqp).is_some_and(|now| now > link),
        "published again on a new link"
    );
    let messages = take_all(&url).await;
    let copy = (
        "GUILD_UPDATE".to_owned(),
        b"{}".to_vec(),
        Some("3:session:42".to_owned()),
    );
    assert_eq!(messages, vec![copy.clone(), copy]);
    amqp.close().await;
}

/// A broker that takes publishes but never confirms them (it answers
/// nothing at all) fails each attempt after PUBLISH_TIMEOUT, which bounds
/// the write and the confirm together. The link is then dropped and the
/// publish waits for a new one only within the outage budget, as for any
/// broken link, and finally fails: the worker keeps the resume point before
/// the event, so Discord replays it.
#[tokio::test]
async fn a_broker_that_never_confirms_fails_the_publish() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let proxy = Proxy::start(&url).await;
    let amqp = Amqp::connect(proxy.url.clone(), true).await.unwrap();
    purge(&url, QUEUE_RECV).await;
    let failures = AMQP_PUBLISH_FAILURES.get();

    proxy.hold(false, true);
    let started = Instant::now();
    let published = publish(&amqp, "TYPING_START", "0:a:1");
    wait_until(|| generation(&amqp).is_none()).await;
    let dropped = started.elapsed();
    assert!(
        dropped >= PUBLISH_TIMEOUT - Duration::from_millis(100)
            && dropped <= PUBLISH_TIMEOUT + Duration::from_secs(1),
        "the attempt failed after {dropped:?}"
    );
    assert_eq!(stored(&url).await, 1, "the broker took the message");

    let limit = PUBLISH_TIMEOUT + OUTAGE_BUDGET + Duration::from_secs(3);
    let result = finishes(published, limit).await;
    assert!(result.is_err(), "reported as published without a confirm");
    assert_eq!(AMQP_PUBLISH_FAILURES.get(), failures + 1);
    amqp.close().await;
}

/// A queue the broker nacks every message for: full, rejecting publishes.
const NACKED: &str = "CONFIRMS_NACKED";

/// Declare a queue for [`NACKED`] that rejects every publish (max length 0,
/// overflow `reject-publish`), so the broker nacks those messages.
async fn declare_nacking_queue(url: &str) -> Connection {
    let connection = connect(url).await;
    let channel = connection.create_channel().await.unwrap();
    let mut arguments = FieldTable::default();
    arguments.insert("x-max-length".into(), AMQPValue::LongLongInt(0));
    arguments.insert(
        "x-overflow".into(),
        AMQPValue::LongString("reject-publish".into()),
    );
    let queue = "confirms.nacked";
    channel
        .queue_delete(queue.into(), QueueDeleteOptions::default())
        .await
        .unwrap();
    channel
        .queue_declare(queue.into(), QueueDeclareOptions::default(), arguments)
        .await
        .unwrap();
    channel
        .queue_bind(
            queue.into(),
            EXCHANGE.into(),
            NACKED.into(),
            QueueBindOptions::default(),
            FieldTable::default(),
        )
        .await
        .unwrap();
    connection
}

/// A nack is a failed attempt: the publish fails after its attempts and the
/// worker keeps the resume point before the event. It says nothing about
/// the connection (the broker could not store this message), so the link
/// is kept and other events keep flowing.
#[tokio::test]
async fn a_nacked_publish_fails_and_keeps_the_link() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let amqp = Amqp::connect(url.clone(), true).await.unwrap();
    let nacking = declare_nacking_queue(&url).await;
    purge(&url, QUEUE_RECV).await;
    let link = generation(&amqp);
    let failures = AMQP_PUBLISH_FAILURES.get();

    let started = Instant::now();
    let nacked = publish(&amqp, NACKED, "0:a:1").await.unwrap();
    assert!(nacked.is_err(), "a nacked message counts as published");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(AMQP_PUBLISH_FAILURES.get(), failures + 1);
    assert_eq!(generation(&amqp), link, "the link is kept");
    assert!(amqp.is_connected());

    publish(&amqp, "GUILD_CREATE", "0:a:2")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(generation(&amqp), link);
    let _ = nacking.close(200, "done".into()).await;
    purge(&url, QUEUE_RECV).await;
    amqp.close().await;
}

/// Workers publish at the same time on one channel, and the broker confirms
/// messages by their position on it. Each publish must get the confirm of
/// its own message: lapin numbers a publish and queues its frames in two
/// steps, and two workers on different threads could swap them, so a nack
/// for one message would fail another and the nacked one count as stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_publishes_get_their_own_confirms() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let amqp = Amqp::connect(url.clone(), true).await.unwrap();
    let nacking = declare_nacking_queue(&url).await;
    purge(&url, QUEUE_RECV).await;

    const TASKS: usize = 16;
    const EACH: usize = 100;
    let tasks: Vec<_> = (0..TASKS)
        .map(|task| {
            let amqp = Arc::clone(&amqp);
            tokio::spawn(async move {
                let mut wrong = Vec::new();
                for round in 0..EACH {
                    let nack = (task + round) % 2 == 0;
                    let key = if nack { NACKED } else { "MESSAGE_CREATE" };
                    let result = amqp.publish(key, b"{}", BasicProperties::default()).await;
                    if result.is_err() != nack {
                        wrong.push((key, result.err().map(|err| err.to_string())));
                    }
                }
                wrong
            })
        })
        .collect();
    let mut wrong = Vec::new();
    for task in tasks {
        wrong.extend(task.await.unwrap());
    }
    let count = wrong.len();
    wrong.truncate(5);
    assert_eq!(count, 0, "wrong results, the first ones: {wrong:?}");
    let _ = nacking.close(200, "done".into()).await;
    purge(&url, QUEUE_RECV).await;
    amqp.close().await;
}

/// Workers await each publish (events of a guild stay in order), so a
/// confirm adds a broker round trip per event, and LavinMQ confirms only
/// after syncing its data directory (unless it runs with `--no-sync`): the
/// rate depends mostly on the broker's disk. Prints the rate of one worker
/// and of 32 (the default WORKERS) and checks a floor far below it
/// (`cargo test --test confirms -- --nocapture throughput` shows it). The
/// workers' confirms overlap: a lock held until the confirm arrived would
/// make 32 workers barely faster than one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn throughput() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let amqp = Amqp::connect(url.clone(), true).await.unwrap();
    purge(&url, QUEUE_RECV).await;
    let body = vec![b'x'; 1024];

    const SEQUENTIAL: usize = 500;
    let started = Instant::now();
    for _ in 0..SEQUENTIAL {
        amqp.publish("MESSAGE_CREATE", &body, BasicProperties::default())
            .await
            .unwrap();
    }
    let sequential = SEQUENTIAL as f64 / started.elapsed().as_secs_f64();

    const WORKERS: usize = 32;
    const EACH: usize = 100;
    let started = Instant::now();
    let workers: Vec<_> = (0..WORKERS)
        .map(|_| {
            let (amqp, body) = (Arc::clone(&amqp), body.clone());
            tokio::spawn(async move {
                for _ in 0..EACH {
                    amqp.publish("MESSAGE_CREATE", &body, BasicProperties::default())
                        .await
                        .unwrap();
                }
            })
        })
        .collect();
    for worker in workers {
        worker.await.unwrap();
    }
    let concurrent = (WORKERS * EACH) as f64 / started.elapsed().as_secs_f64();

    println!("throughput: {sequential:.0} events/s from 1 worker, {concurrent:.0} events/s from {WORKERS} workers");
    assert!(sequential > 10.0, "{sequential:.0} events/s from 1 worker");
    assert!(
        concurrent > 2.0 * sequential,
        "{concurrent:.0} events/s from {WORKERS} workers, {sequential:.0} from one"
    );
    purge(&url, QUEUE_RECV).await;
    amqp.close().await;
}
