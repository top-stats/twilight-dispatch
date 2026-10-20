#![recursion_limit = "256"]

//! AMQP supervision against a real broker (set TEST_AMQP_URL; CI uses
//! LavinMQ, the production broker). The tests that close connections from
//! the broker side or check acknowledgements also use its management API
//! (TEST_AMQP_HTTP, e.g. `127.0.0.1:15672`).

mod common;

use futures_util::StreamExt;
use lapin::{
    options::{
        BasicAckOptions, BasicConsumeOptions, BasicPublishOptions, QueueBindOptions,
        QueueDeclareOptions, QueuePurgeOptions,
    },
    types::FieldTable,
    BasicProperties, Connection, ConnectionProperties,
};
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use twilight_dispatch::{
    amqp::Amqp,
    commands::run_consumer,
    constants::{QUEUE_RECV, QUEUE_SEND, SHARD_EXCHANGE},
    gateway::registry::{Registry, ShardHandle},
    metrics::GATEWAY_COMMANDS,
};
use twilight_gateway::{Intents, Shard, ShardId};

/// The tests share the `gateway.recv` and `gateway.send` queues and the
/// command counters, so they run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn receive_one(url: &str) -> (String, Vec<u8>) {
    let connection = Connection::connect(url, ConnectionProperties::default())
        .await
        .unwrap();
    let channel = connection.create_channel().await.unwrap();
    let mut consumer = channel
        .basic_consume(
            QUEUE_RECV.into(),
            "test".into(),
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await
        .unwrap();
    let delivery = tokio::time::timeout(Duration::from_secs(10), consumer.next())
        .await
        .expect("a message within 10s")
        .unwrap()
        .unwrap();
    delivery
        .acker
        .ack(BasicAckOptions::default())
        .await
        .unwrap();
    let _ = connection.close(200, "done".into()).await;
    (delivery.routing_key.to_string(), delivery.data)
}

/// Declare a durable queue (with the service's flags) and empty it.
async fn purge(url: &str, queue: &str) {
    let connection = Connection::connect(url, ConnectionProperties::default())
        .await
        .unwrap();
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

#[tokio::test]
async fn publishes_to_the_gateway_exchange() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let amqp = Amqp::connect(url.clone(), true).await.unwrap();
    purge(&url, QUEUE_RECV).await;

    amqp.publish("MESSAGE_CREATE", br#"{"op":0}"#, BasicProperties::default())
        .await
        .unwrap();
    let (routing_key, body) = receive_one(&url).await;
    assert_eq!(routing_key, "MESSAGE_CREATE");
    assert_eq!(body, br#"{"op":0}"#);
    amqp.close().await;
}

#[tokio::test]
async fn publishing_survives_a_closed_channel() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let amqp = Amqp::connect(url.clone(), true).await.unwrap();
    purge(&url, QUEUE_RECV).await;

    amqp.break_channel_for_tests().await;
    amqp.publish("GUILD_CREATE", b"{}", BasicProperties::default())
        .await
        .unwrap();
    assert!(amqp.is_connected());
    let (routing_key, _) = receive_one(&url).await;
    assert_eq!(routing_key, "GUILD_CREATE");
    amqp.close().await;
}

// ---------------------------------------------------------------------------
// The broker's management API

/// The management API's `host:port`, or `None` (after a skip notice).
fn management_api() -> Option<String> {
    let api = std::env::var("TEST_AMQP_HTTP").ok();
    if api.is_none() {
        common::skip("TEST_AMQP_HTTP");
    }
    api
}

/// Minimal HTTP/1.1 request to the broker's management API: the status and
/// the body.
async fn http(host: &str, method: &str, path: &str) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(host).await.unwrap();
    // guest:guest
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Basic Z3Vlc3Q6Z3Vlc3Q=\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();

    let end = find(&response, b"\r\n\r\n").expect("a complete response");
    let head = String::from_utf8_lossy(&response[..end]).to_ascii_lowercase();
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|status| status.parse().ok())
        .unwrap_or_else(|| panic!("no status in {head}"));
    let mut rest = &response[end + 4..];
    if !head.contains("transfer-encoding: chunked") {
        return (status, String::from_utf8(rest.to_vec()).unwrap());
    }
    // LavinMQ sends JSON chunked: `<hex size>\r\n<data>\r\n`, up to size 0.
    let mut body = Vec::new();
    loop {
        let line = find(rest, b"\r\n").expect("a chunk size");
        let size = std::str::from_utf8(&rest[..line])
            .ok()
            .and_then(|size| usize::from_str_radix(size.trim(), 16).ok())
            .expect("a hexadecimal chunk size");
        if size == 0 {
            break;
        }
        body.extend_from_slice(&rest[line + 2..line + 2 + size]);
        rest = &rest[line + 2 + size + 2..];
    }
    (status, String::from_utf8(body).unwrap())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// GET a management API path as JSON; `None` for a 404.
async fn api_get(api: &str, path: &str) -> Option<Value> {
    let (status, body) = http(api, "GET", path).await;
    if status == 404 {
        return None;
    }
    assert_eq!(status, 200, "GET {path}: {body}");
    Some(serde_json::from_str(&body).unwrap())
}

/// Percent-encode a path segment (connection names contain spaces, `:` and
/// `>`).
fn encode(segment: &str) -> String {
    segment
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// The broker's names of the connections named "twilight-dispatch".
async fn our_connections(api: &str) -> Vec<String> {
    let connections = api_get(api, "/api/connections")
        .await
        .expect("the connection list");
    connections
        .as_array()
        .expect("a JSON array of connections")
        .iter()
        .filter(|c| c["client_properties"]["connection_name"] == "twilight-dispatch")
        .filter_map(|c| c["name"].as_str().map(ToOwned::to_owned))
        .collect()
}

/// Close every connection named "twilight-dispatch" from the broker side,
/// as a broker restart or an operator would, and wait until the broker
/// dropped them (the supervisor then connects again, under a new name).
async fn kill_our_connections(api: &str) {
    let killed = our_connections(api).await;
    assert!(!killed.is_empty(), "our connection is listed");
    for name in &killed {
        let path = format!("/api/connections/{}", encode(name));
        let (status, body) = http(api, "DELETE", &path).await;
        assert!(
            (200..300).contains(&status),
            "close connection: {status} {body}"
        );
    }
    for _ in 0..200 {
        let listed = our_connections(api).await;
        if !killed.iter().any(|name| listed.contains(name)) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the broker still lists {killed:?}");
}

/// A queue's (ready, unacknowledged, consumers), or `None` while it does not
/// exist. A passive declare only reports ready messages, so a delivery that
/// is never acknowledged would look consumed.
async fn queue_stats(api: &str, queue: &str) -> Option<(u64, u64, u64)> {
    let stats = api_get(api, &format!("/api/queues/%2F/{}", encode(queue))).await?;
    let count = |field: &str| {
        stats[field]
            .as_u64()
            .unwrap_or_else(|| panic!("no {field} in {stats}"))
    };
    Some((
        count("messages_ready"),
        count("messages_unacknowledged"),
        count("consumers"),
    ))
}

/// Wait until `count` processes consume gateway.send. The consumer declares
/// the queue and binds its own queue before it subscribes, so from then on
/// no command is dropped as unroutable.
async fn wait_for_consumers(api: &str, count: u64) {
    let mut stats = None;
    for _ in 0..200 {
        stats = queue_stats(api, QUEUE_SEND).await;
        if stats.is_some_and(|(_, _, consumers)| consumers == count) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("expected {count} consumers of gateway.send, got (ready, unacked, consumers) {stats:?}");
}

/// Wait until a queue holds nothing: every delivery was acknowledged, none
/// is waiting or being requeued.
async fn wait_until_acked(api: &str, queue: &str) {
    let mut stats = None;
    for _ in 0..200 {
        stats = queue_stats(api, queue).await;
        if stats.is_some_and(|(ready, unacked, _)| ready == 0 && unacked == 0) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{queue} still holds commands (ready, unacked, consumers): {stats:?}");
}

/// The queue the process running `shard` bound to [`SHARD_EXCHANGE`]. The
/// queues of closed connections can take a moment to go away.
async fn queue_of_shard(api: &str, shard: u32) -> String {
    let path = format!(
        "/api/exchanges/%2F/{}/bindings/source",
        encode(SHARD_EXCHANGE)
    );
    let key = shard.to_string();
    let mut queues = Vec::new();
    for _ in 0..200 {
        let bindings = api_get(api, &path).await.expect("the shard exchange");
        queues = bindings
            .as_array()
            .expect("a JSON array of bindings")
            .iter()
            .filter(|b| b["routing_key"] == key.as_str())
            .filter_map(|b| b["destination"].as_str().map(ToOwned::to_owned))
            .collect::<Vec<String>>();
        if queues.len() == 1 {
            return queues.remove(0);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("expected one queue for shard {shard}, got {queues:?}");
}

#[tokio::test]
async fn publishing_survives_the_broker_closing_the_connection() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let Some(api) = management_api() else {
        return;
    };
    let amqp = Amqp::connect(url.clone(), true).await.unwrap();
    purge(&url, QUEUE_RECV).await;

    kill_our_connections(&api).await;
    amqp.publish("GUILD_DELETE", b"{}", BasicProperties::default())
        .await
        .unwrap();
    let (routing_key, _) = receive_one(&url).await;
    assert_eq!(routing_key, "GUILD_DELETE");
    amqp.close().await;
}

// ---------------------------------------------------------------------------
// The gateway command consumer

async fn publish_command(url: &str, exchange: &str, routing_key: &str, body: &[u8]) {
    let connection = Connection::connect(url, ConnectionProperties::default())
        .await
        .unwrap();
    let channel = connection.create_channel().await.unwrap();
    channel
        .basic_publish(
            exchange.into(),
            routing_key.into(),
            BasicPublishOptions::default(),
            body,
            BasicProperties::default(),
        )
        .await
        .unwrap();
    let _ = connection.close(200, "done".into()).await;
}

async fn send_command(url: &str, body: &[u8]) {
    publish_command(url, "", QUEUE_SEND, body).await;
}

/// The command counter for a shard ("" for invalid commands) and result.
fn commands(shard: &str, result: &str) -> u64 {
    GATEWAY_COMMANDS.with_label_values(&[shard, result]).get()
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

/// A shard that is never polled, so it never connects. Its sender accepts
/// commands, which is all the consumer needs.
fn idle_shard(id: u32, total: u32) -> Shard {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Shard::new(ShardId::new(id, total), "token".into(), Intents::empty())
}

fn presence(shard: u32, since: u64) -> String {
    format!(
        r#"{{"op":0,"shard":{shard},"data":{{"op":3,"d":{{"status":"online","activities":[],"afk":false,"since":{since}}}}}}}"#
    )
}

#[tokio::test]
async fn the_command_consumer_follows_reconnects() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let Some(api) = management_api() else {
        return;
    };
    let shard = idle_shard(0, 1);
    purge(&url, QUEUE_SEND).await;
    let amqp = Amqp::connect(url.clone(), true).await.unwrap();
    let registry = Arc::new(Registry::new([Arc::new(ShardHandle::new(
        0,
        Some(shard.sender()),
        None,
    ))]));
    let consumer = tokio::spawn(run_consumer(amqp.clone(), registry, 1));
    wait_for_consumers(&api, 1).await;

    let sent = commands("0", "sent");
    send_command(&url, presence(0, 1).as_bytes()).await;
    wait_until(|| commands("0", "sent") > sent).await;
    assert_eq!(commands("0", "sent"), sent + 1, "sent to the shard");
    wait_until_acked(&api, QUEUE_SEND).await;

    // The broker drops the connection; the consumer must move to the new one.
    kill_our_connections(&api).await;
    send_command(&url, presence(0, 2).as_bytes()).await;
    wait_until(|| commands("0", "sent") > sent + 1).await;
    assert_eq!(commands("0", "sent"), sent + 2, "sent after the reconnect");
    wait_until_acked(&api, QUEUE_SEND).await;

    // A command for a shard no process runs is dropped (and acknowledged),
    // not requeued forever.
    let invalid = commands("", "invalid");
    send_command(&url, presence(5, 3).as_bytes()).await;
    wait_until(|| commands("", "invalid") > invalid).await;
    wait_until_acked(&api, QUEUE_SEND).await;
    assert_eq!(commands("", "invalid"), invalid + 1, "dropped once");
    assert_eq!(commands("0", "sent"), sent + 2);

    consumer.abort();
    amqp.close().await;
}

/// A command for a shard another process runs reaches that process' queue
/// (the test plays that process). One routed here for a shard this process
/// does not run is dropped rather than forwarded again, so a stale binding
/// cannot make commands bounce between processes.
#[tokio::test]
async fn commands_for_other_shards_are_forwarded_once() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let Some(api) = management_api() else {
        return;
    };
    let shard = idle_shard(0, 2);
    purge(&url, QUEUE_SEND).await;
    let amqp = Amqp::connect(url.clone(), true).await.unwrap();
    let registry = Arc::new(Registry::new([Arc::new(ShardHandle::new(
        0,
        Some(shard.sender()),
        None,
    ))]));
    let consumer = tokio::spawn(run_consumer(amqp.clone(), registry, 2));
    wait_for_consumers(&api, 1).await;

    // The process running shard 1.
    let other = Connection::connect(&url, ConnectionProperties::default())
        .await
        .unwrap();
    let channel = other.create_channel().await.unwrap();
    let queue = channel
        .queue_declare(
            "".into(),
            QueueDeclareOptions {
                exclusive: true,
                auto_delete: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .unwrap();
    channel
        .queue_bind(
            queue.name().as_str().into(),
            SHARD_EXCHANGE.into(),
            "1".into(),
            QueueBindOptions::default(),
            FieldTable::default(),
        )
        .await
        .unwrap();
    let mut received = channel
        .basic_consume(
            queue.name().as_str().into(),
            "shard 1".into(),
            BasicConsumeOptions {
                no_ack: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .unwrap();

    let (sent, forwarded) = (commands("0", "sent"), commands("1", "forwarded"));
    let command = presence(1, 1);
    send_command(&url, command.as_bytes()).await;
    let delivery = tokio::time::timeout(Duration::from_secs(10), received.next())
        .await
        .expect("forwarded within 10s")
        .unwrap()
        .unwrap();
    assert_eq!(delivery.data, command.as_bytes(), "forwarded verbatim");
    assert_eq!(delivery.exchange.as_str(), SHARD_EXCHANGE);
    assert_eq!(delivery.routing_key.as_str(), "1");
    wait_until(|| commands("1", "forwarded") > forwarded).await;
    assert_eq!(commands("1", "forwarded"), forwarded + 1, "counted");
    wait_until_acked(&api, QUEUE_SEND).await;

    // Through this process' own queue: a command for shard 1, then one for
    // shard 0. They are handled in order, so once the second was sent the
    // first was handled.
    let own = queue_of_shard(&api, 0).await;
    publish_command(&url, SHARD_EXCHANGE, "0", presence(1, 2).as_bytes()).await;
    publish_command(&url, SHARD_EXCHANGE, "0", presence(0, 3).as_bytes()).await;
    wait_until(|| commands("0", "sent") > sent).await;
    assert_eq!(commands("0", "sent"), sent + 1, "sent to the shard");
    assert_eq!(
        commands("1", "forwarded"),
        forwarded + 1,
        "a command routed here is not forwarded again"
    );
    wait_until_acked(&api, &own).await;

    let _ = other.close(200, "done".into()).await;
    consumer.abort();
    amqp.close().await;
}

/// Several processes share gateway.send: every command must reach the
/// process running its shard, whichever process consumes it first.
#[tokio::test]
async fn commands_reach_the_process_running_the_shard() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let Some(api) = management_api() else {
        return;
    };

    const PROCESSES: u32 = 3;
    const PER_SHARD: u64 = 30;
    let shards: Vec<Shard> = (0..PROCESSES).map(|id| idle_shard(id, PROCESSES)).collect();
    purge(&url, QUEUE_SEND).await;
    let mut processes = Vec::new();
    for shard in &shards {
        let id = shard.id().number();
        let amqp = Amqp::connect(url.clone(), true).await.unwrap();
        let registry = Arc::new(Registry::new([Arc::new(ShardHandle::new(
            id,
            Some(shard.sender()),
            None,
        ))]));
        let consumer = tokio::spawn(run_consumer(amqp.clone(), registry, PROCESSES));
        processes.push((amqp, consumer));
    }
    wait_for_consumers(&api, PROCESSES.into()).await;

    let sent = |shard: u32| commands(&shard.to_string(), "sent");
    let forwarded = |shard: u32| commands(&shard.to_string(), "forwarded");
    let sent_before: Vec<u64> = (0..PROCESSES).map(sent).collect();
    let forwarded_before: u64 = (0..PROCESSES).map(forwarded).sum();
    // The commands of one shard in a row, so the broker hands them to every
    // process in turn and most are forwarded.
    for shard in 0..PROCESSES {
        for round in 0..PER_SHARD {
            send_command(&url, presence(shard, round).as_bytes()).await;
        }
    }

    let delivered = || -> Vec<u64> {
        (0..PROCESSES)
            .map(|s| sent(s) - sent_before[s as usize])
            .collect()
    };
    wait_until(|| delivered() == vec![PER_SHARD; PROCESSES as usize]).await;
    assert_eq!(
        delivered(),
        vec![PER_SHARD; PROCESSES as usize],
        "commands per owner"
    );
    let forwarded_now: u64 = (0..PROCESSES).map(forwarded).sum();
    assert!(
        forwarded_now > forwarded_before,
        "commands consumed by another process are forwarded"
    );
    wait_until_acked(&api, QUEUE_SEND).await;

    for (amqp, consumer) in processes {
        consumer.abort();
        amqp.close().await;
    }
}
