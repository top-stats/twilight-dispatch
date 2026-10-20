//! Validation of gateway commands received from the broker (`gateway.send`).
//!
//! Anything sent to the gateway that Discord rejects closes the shard's
//! connection (4001 unknown opcode, 4002 decode error, 4005 already
//! authenticated...), so commands are checked before they reach a shard.

use crate::constants::QUEUE_SEND;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_repr::{Deserialize_repr, Serialize_repr};

/// Discord rejects gateway payloads larger than this.
/// <https://discord.com/developers/docs/events/gateway#sending-events>
pub const MAX_PAYLOAD_BYTES: usize = 4096;

/// Gateway opcodes a client may send itself. Heartbeat, Identify and Resume
/// are owned by the shard and must never be injected from outside.
///
/// 3 Presence Update, 4 Voice State Update, 8 Request Guild Members,
/// 31 Request Soundboard Sounds, 43 Request Channel Info.
pub const ALLOWED_OPCODES: [u64; 5] = [3, 4, 8, 31, 43];

#[derive(Clone, Copy, Debug, Deserialize_repr, Eq, PartialEq, Serialize_repr)]
#[repr(u8)]
pub enum DeliveryOpcode {
    Send = 0,
    Reconnect = 1,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeliveryInfo {
    pub op: DeliveryOpcode,
    pub shard: u32,
    #[serde(default)]
    pub data: Option<Value>,
}

/// A validated command for one shard.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShardCommand {
    /// Raw JSON to send over the shard's websocket.
    Send(String),
    /// Close the connection with a resumable code; the shard reconnects and
    /// resumes on its own.
    Reconnect,
}

#[derive(Debug, Eq, PartialEq)]
pub enum InvalidCommand {
    Malformed(String),
    MissingData,
    NotAnObject,
    OpcodeNotAllowed(Option<u64>),
    TooLarge(usize),
}

impl std::fmt::Display for InvalidCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(err) => write!(f, "malformed delivery: {err}"),
            Self::MissingData => f.write_str("send delivery without data"),
            Self::NotAnObject => f.write_str("data is not a JSON object"),
            Self::OpcodeNotAllowed(op) => write!(f, "gateway opcode {op:?} may not be sent"),
            Self::TooLarge(len) => {
                write!(
                    f,
                    "payload is {len} bytes, Discord's limit is {MAX_PAYLOAD_BYTES}"
                )
            }
        }
    }
}

/// Parse a raw delivery body.
pub fn parse(body: &[u8]) -> Result<DeliveryInfo, InvalidCommand> {
    serde_json::from_slice(body).map_err(|err| InvalidCommand::Malformed(err.to_string()))
}

/// Turn a delivery into the command to run on its shard.
pub fn validate(delivery: &DeliveryInfo) -> Result<ShardCommand, InvalidCommand> {
    match delivery.op {
        DeliveryOpcode::Reconnect => Ok(ShardCommand::Reconnect),
        DeliveryOpcode::Send => {
            let data = delivery.data.as_ref().ok_or(InvalidCommand::MissingData)?;
            let object = data.as_object().ok_or(InvalidCommand::NotAnObject)?;
            let op = object.get("op").and_then(Value::as_u64);
            if !op.is_some_and(|op| ALLOWED_OPCODES.contains(&op)) {
                return Err(InvalidCommand::OpcodeNotAllowed(op));
            }
            let json = serde_json::to_string(data)
                .map_err(|err| InvalidCommand::Malformed(err.to_string()))?;
            if json.len() > MAX_PAYLOAD_BYTES {
                return Err(InvalidCommand::TooLarge(json.len()));
            }
            Ok(ShardCommand::Send(json))
        }
    }
}

/// What to do with a delivery once it was looked at.
#[derive(Debug, Eq, PartialEq)]
pub enum Disposition {
    /// Handled (or permanently invalid): acknowledge.
    Ack,
    /// Another process owns the shard: give it back to the queue.
    Requeue,
}

/// Decide what to run for a delivery. `owned` says whether this process runs
/// the shard; `total` is the shard count of the bot.
pub fn plan(
    body: &[u8],
    owned: impl Fn(u32) -> bool,
    total: u32,
) -> (Disposition, Option<(u32, ShardCommand)>) {
    let delivery = match parse(body) {
        Ok(delivery) => delivery,
        Err(err) => {
            tracing::warn!("Dropping gateway command: {}", err);
            return (Disposition::Ack, None);
        }
    };
    if delivery.shard >= total {
        tracing::warn!(
            "Dropping gateway command for invalid shard {}",
            delivery.shard
        );
        return (Disposition::Ack, None);
    }
    if !owned(delivery.shard) {
        return (Disposition::Requeue, None);
    }
    match validate(&delivery) {
        Ok(command) => (Disposition::Ack, Some((delivery.shard, command))),
        Err(err) => {
            tracing::warn!(
                "Dropping gateway command for shard {}: {}",
                delivery.shard,
                err
            );
            (Disposition::Ack, None)
        }
    }
}

/// Time limit for acknowledging one delivery.
const ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Consume `gateway.send` until the connection is closed. The consumer
/// follows the AMQP supervisor's current link, so it moves to a new
/// connection as soon as one is installed. Commands are acknowledged only
/// after they were handed to a shard.
///
/// A command for a shard this process does not run is returned to the queue
/// once (another process may own the shard); when it comes back again it is
/// dropped, so such commands can never fill the prefetch window.
pub async fn run_consumer(
    amqp: std::sync::Arc<crate::amqp::Amqp>,
    registry: std::sync::Arc<crate::gateway::registry::Registry>,
    total: u32,
) {
    use futures_util::StreamExt;
    use lapin::{
        options::{BasicAckOptions, BasicConsumeOptions, BasicNackOptions},
        types::FieldTable,
    };
    use std::time::Duration;
    use twilight_gateway::CloseFrame;

    let dropped = crate::util::Throttle::default();
    let mut links = amqp.subscribe();
    let mut backoff = Duration::from_millis(250);

    while !amqp.is_closing() {
        let link = match links.wait_for(Option::is_some).await {
            Ok(link) => link.clone(),
            Err(_) => return,
        };
        let Some(link) = link else { continue };
        let channel = link.consume_channel().clone();

        // Re-declare the queue in case it was deleted (the consumer would
        // otherwise retry a 404 forever).
        let consumer = async {
            crate::amqp::declare_consume(&channel).await?;
            channel
                .basic_consume(
                    QUEUE_SEND.into(),
                    "".into(),
                    BasicConsumeOptions::default(),
                    FieldTable::default(),
                )
                .await
        };
        let mut consumer = match tokio::time::timeout(ACK_TIMEOUT, consumer).await {
            Ok(Ok(consumer)) => consumer,
            Ok(Err(err)) => {
                tracing::warn!("Failed to consume {}: {}", QUEUE_SEND, err);
                amqp.mark_broken(Some(link.generation));
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
            Err(_) => {
                tracing::warn!("Timed out subscribing to {}", QUEUE_SEND);
                amqp.mark_broken(Some(link.generation));
                continue;
            }
        };
        backoff = Duration::from_millis(250);

        loop {
            let delivery = tokio::select! {
                // A new link (or none): leave this connection.
                _ = links.changed() => break,
                delivery = consumer.next() => delivery,
            };
            let delivery = match delivery {
                Some(Ok(delivery)) => delivery,
                Some(Err(err)) => {
                    tracing::warn!("Gateway command consumer failed: {}", err);
                    amqp.mark_broken(Some(link.generation));
                    break;
                }
                None => {
                    // Cancelled by the broker (e.g. the queue was deleted).
                    break;
                }
            };

            let (mut disposition, command) =
                plan(&delivery.data, |shard| registry.get(shard).is_some(), total);
            if let Some((shard, command)) = command {
                let sender = registry
                    .get(shard)
                    .and_then(|handle| handle.sender.as_ref());
                let sent = match (sender, command) {
                    (Some(sender), ShardCommand::Send(json)) => sender.send(json).is_ok(),
                    (Some(sender), ShardCommand::Reconnect) => {
                        tracing::info!("Reconnecting shard {} on request", shard);
                        sender.close(CloseFrame::RESUME).is_ok()
                    }
                    (None, _) => false,
                };
                if !sent {
                    // The shard stopped for good (fatal close code).
                    tracing::warn!("Shard {} is not running; dropping its command", shard);
                }
            }
            if disposition == Disposition::Requeue && delivery.redelivered {
                if let Some(suppressed) = dropped.check() {
                    tracing::warn!(
                        "Dropping a gateway command for a shard no running process took ({} similar suppressed)",
                        suppressed
                    );
                }
                disposition = Disposition::Ack;
            }

            let result = match disposition {
                Disposition::Ack => {
                    tokio::time::timeout(
                        ACK_TIMEOUT,
                        delivery.acker.ack(BasicAckOptions::default()),
                    )
                    .await
                }
                Disposition::Requeue => {
                    let requeue = BasicNackOptions {
                        multiple: false,
                        requeue: true,
                    };
                    tokio::time::timeout(ACK_TIMEOUT, delivery.acker.nack(requeue)).await
                }
            };
            match result {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    tracing::warn!("Failed to acknowledge gateway command: {}", err);
                    amqp.mark_broken(Some(link.generation));
                    break;
                }
                Err(_) => {
                    tracing::warn!("Timed out acknowledging a gateway command");
                    amqp.mark_broken(Some(link.generation));
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn delivery(value: Value) -> DeliveryInfo {
        parse(value.to_string().as_bytes()).unwrap()
    }

    #[test]
    fn every_documented_client_opcode_is_allowed() {
        for op in [3, 4, 8, 31, 43] {
            let d = delivery(json!({"op": 0, "shard": 0, "data": {"op": op, "d": {}}}));
            assert!(validate(&d).is_ok(), "op {op}");
        }
    }

    #[test]
    fn request_guild_members_is_forwarded_verbatim() {
        let data = json!({"op": 8, "d": {"guild_id": "1", "query": "", "limit": 0}});
        let cmd = validate(&delivery(json!({"op": 0, "shard": 3, "data": data}))).unwrap();
        assert_eq!(cmd, ShardCommand::Send(data.to_string()));
    }

    #[test]
    fn reconnect_needs_no_data() {
        let d = delivery(json!({"op": 1, "shard": 0}));
        assert_eq!(d.shard, 0);
        assert_eq!(validate(&d), Ok(ShardCommand::Reconnect));
    }

    #[test]
    fn send_without_data_is_rejected() {
        assert_eq!(
            validate(&delivery(json!({"op": 0, "shard": 0}))),
            Err(InvalidCommand::MissingData)
        );
        assert_eq!(
            validate(&delivery(json!({"op": 0, "shard": 0, "data": null}))),
            Err(InvalidCommand::MissingData)
        );
    }

    #[test]
    fn session_opcodes_are_rejected() {
        for op in [1, 2, 6, 7, 99] {
            let d = delivery(json!({"op": 0, "shard": 0, "data": {"op": op, "d": {}}}));
            assert_eq!(
                validate(&d),
                Err(InvalidCommand::OpcodeNotAllowed(Some(op)))
            );
        }
        let d = delivery(json!({"op": 0, "shard": 0, "data": {"d": {}}}));
        assert_eq!(validate(&d), Err(InvalidCommand::OpcodeNotAllowed(None)));
        let d = delivery(json!({"op": 0, "shard": 0, "data": [1]}));
        assert_eq!(validate(&d), Err(InvalidCommand::NotAnObject));
    }

    #[test]
    fn oversized_payloads_are_rejected() {
        let big = "x".repeat(MAX_PAYLOAD_BYTES);
        let d = delivery(json!({"op": 0, "shard": 0, "data": {"op": 3, "d": {"status": big}}}));
        assert!(matches!(validate(&d), Err(InvalidCommand::TooLarge(_))));
    }

    #[test]
    fn malformed_bodies_are_errors() {
        assert!(matches!(
            parse(b"not json"),
            Err(InvalidCommand::Malformed(_))
        ));
        assert!(matches!(
            parse(br#"{"op":5,"shard":0}"#),
            Err(InvalidCommand::Malformed(_))
        ));
        assert!(matches!(
            parse(br#"{"op":0,"shard":-1}"#),
            Err(InvalidCommand::Malformed(_))
        ));
    }

    #[test]
    fn planning_routes_by_ownership() {
        let body = |shard: u32| {
            json!({"op": 0, "shard": shard, "data": {"op": 3, "d": {}}})
                .to_string()
                .into_bytes()
        };
        let owned = |shard: u32| shard < 2;

        let (disposition, command) = plan(&body(1), owned, 4);
        assert_eq!(disposition, Disposition::Ack);
        assert!(matches!(command, Some((1, ShardCommand::Send(_)))));

        assert_eq!(plan(&body(3), owned, 4), (Disposition::Requeue, None));
        assert_eq!(plan(&body(9), owned, 4), (Disposition::Ack, None));
        assert_eq!(plan(b"junk", owned, 4), (Disposition::Ack, None));

        let invalid = json!({"op": 0, "shard": 0, "data": {"op": 2}})
            .to_string()
            .into_bytes();
        assert_eq!(plan(&invalid, owned, 4), (Disposition::Ack, None));
    }
}
