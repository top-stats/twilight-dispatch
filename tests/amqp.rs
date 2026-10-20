#![recursion_limit = "256"]

//! AMQP supervision against a real broker (set TEST_AMQP_URL; CI uses
//! LavinMQ, the production broker).

mod common;

use futures_util::StreamExt;
use lapin::{
    options::{BasicAckOptions, BasicConsumeOptions, QueueDeclareOptions, QueuePurgeOptions},
    types::FieldTable,
    BasicProperties, Connection, ConnectionProperties,
};
use std::time::Duration;
use twilight_dispatch::{amqp::Amqp, constants::QUEUE_RECV};

/// The tests share the `gateway.recv` queue, so they run one at a time.
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

async fn purge(url: &str) {
    let connection = Connection::connect(url, ConnectionProperties::default())
        .await
        .unwrap();
    let channel = connection.create_channel().await.unwrap();
    channel
        .queue_declare(
            QUEUE_RECV.into(),
            QueueDeclareOptions {
                durable: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .unwrap();
    channel
        .queue_purge(QUEUE_RECV.into(), QueuePurgeOptions::default())
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
    purge(&url).await;

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
    purge(&url).await;

    amqp.break_channel_for_tests().await;
    amqp.publish("GUILD_CREATE", b"{}", BasicProperties::default())
        .await
        .unwrap();
    assert!(amqp.is_connected());
    let (routing_key, _) = receive_one(&url).await;
    assert_eq!(routing_key, "GUILD_CREATE");
    amqp.close().await;
}

/// Minimal HTTP/1.1 request to the broker's management API.
async fn http(host: &str, method: &str, path: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(host).await.unwrap();
    // guest:guest
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Basic Z3Vlc3Q6Z3Vlc3Q=\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

/// Close every connection named "twilight-dispatch" from the broker side,
/// as a broker restart or an operator would.
async fn kill_our_connections(api: &str) {
    let response = http(api, "GET", "/api/connections").await;
    let body = &response[response.find("\r\n\r\n").map_or(0, |i| i + 4)..];
    let start = body.find('[').expect("a JSON array of connections");
    let end = body.rfind(']').expect("end of the array");
    let connections: serde_json::Value = serde_json::from_str(&body[start..=end]).unwrap();
    let ours: Vec<String> = connections
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["client_properties"]["connection_name"] == "twilight-dispatch")
        .filter_map(|c| c["name"].as_str().map(ToOwned::to_owned))
        .collect();
    assert!(!ours.is_empty(), "our connection is listed: {connections}");
    for name in ours {
        let encoded: String = name
            .bytes()
            .map(|b| {
                if b.is_ascii_alphanumeric() {
                    (b as char).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect();
        let response = http(api, "DELETE", &format!("/api/connections/{encoded}")).await;
        assert!(
            response.starts_with("HTTP/1.1 2"),
            "close connection: {response}"
        );
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
}

#[tokio::test]
async fn publishing_survives_the_broker_closing_the_connection() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let Ok(api) = std::env::var("TEST_AMQP_HTTP") else {
        common::skip("TEST_AMQP_HTTP");
        return;
    };
    let amqp = Amqp::connect(url.clone(), true).await.unwrap();
    purge(&url).await;

    kill_our_connections(&api).await;
    amqp.publish("GUILD_DELETE", b"{}", BasicProperties::default())
        .await
        .unwrap();
    let (routing_key, _) = receive_one(&url).await;
    assert_eq!(routing_key, "GUILD_DELETE");
    amqp.close().await;
}

/// Messages ready in gateway.send.
async fn pending_commands(url: &str) -> u32 {
    let connection = Connection::connect(url, ConnectionProperties::default())
        .await
        .unwrap();
    let channel = connection.create_channel().await.unwrap();
    let queue = channel
        .queue_declare(
            twilight_dispatch::constants::QUEUE_SEND.into(),
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

async fn send_command(url: &str, body: &[u8]) {
    let connection = Connection::connect(url, ConnectionProperties::default())
        .await
        .unwrap();
    let channel = connection.create_channel().await.unwrap();
    channel
        .basic_publish(
            "".into(),
            twilight_dispatch::constants::QUEUE_SEND.into(),
            lapin::options::BasicPublishOptions::default(),
            body,
            BasicProperties::default(),
        )
        .await
        .unwrap();
    let _ = connection.close(200, "done".into()).await;
}

async fn wait_until_consumed(url: &str) -> bool {
    for _ in 0..100 {
        if pending_commands(url).await == 0 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test]
async fn the_command_consumer_follows_reconnects() {
    let Some(url) = common::amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let Ok(api) = std::env::var("TEST_AMQP_HTTP") else {
        common::skip("TEST_AMQP_HTTP");
        return;
    };
    let amqp = Amqp::connect(url.clone(), true).await.unwrap();
    let registry = std::sync::Arc::new(twilight_dispatch::gateway::registry::Registry::new([
        std::sync::Arc::new(twilight_dispatch::gateway::registry::ShardHandle::new(
            0, None, None,
        )),
    ]));
    let consumer = tokio::spawn(twilight_dispatch::commands::run_consumer(
        amqp.clone(),
        registry,
        1,
    ));

    let command = br#"{"op":0,"shard":0,"data":{"op":3,"d":{"status":"online","activities":[],"afk":false,"since":null}}}"#;
    send_command(&url, command).await;
    assert!(
        wait_until_consumed(&url).await,
        "consumed before the reconnect"
    );

    // The broker drops the connection; the consumer must move to the new one.
    kill_our_connections(&api).await;
    send_command(&url, command).await;
    assert!(
        wait_until_consumed(&url).await,
        "consumed after the reconnect"
    );

    // A command for a shard no process runs is dropped, not cycled forever.
    send_command(&url, br#"{"op":0,"shard":5,"data":{"op":3,"d":{}}}"#).await;
    assert!(wait_until_consumed(&url).await, "invalid shard dropped");

    consumer.abort();
    amqp.close().await;
}
