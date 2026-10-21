//! Gateway commands received from the broker (`gateway.send`): their
//! validation, and the consumer that runs them on this process' shards or
//! forwards them to the process running theirs.
//!
//! Anything sent to the gateway that Discord rejects closes the shard's
//! connection (4001 unknown opcode, 4002 decode error, 4005 already
//! authenticated...), so the opcode and the payload size are checked before a
//! command reaches a shard. The payload itself is not: a command whose `d`
//! Discord cannot decode still closes the connection with 4002, and the shard
//! resumes.

use crate::{
    amqp::{declare_consume, Amqp},
    constants::{QUEUE_SEND, SHARD_EXCHANGE},
    gateway::registry::Registry,
    metrics::GATEWAY_COMMANDS,
    util::Throttle,
};

use futures_util::StreamExt;
use lapin::{
    options::{BasicAckOptions, BasicConsumeOptions, BasicNackOptions, BasicPublishOptions},
    types::FieldTable,
    BasicProperties, Channel, Consumer,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_repr::{Deserialize_repr, Serialize_repr};
use std::{sync::Arc, time::Duration};
use twilight_gateway::CloseFrame;

/// Discord rejects gateway payloads larger than this.
/// <https://discord.com/developers/docs/events/gateway#sending-events>
pub const MAX_PAYLOAD_BYTES: usize = 4096;

/// Gateway opcodes a client may send itself. Heartbeat, Identify and Resume
/// are owned by the shard and must never be injected from outside.
///
/// 3 Presence Update, 4 Voice State Update, 8 Request Guild Members,
/// 31 Request Soundboard Sounds, 43 Request Channel Info.
pub const ALLOWED_OPCODES: [u64; 5] = [3, 4, 8, 31, 43];

/// What a delivery asks for: its `op`, a JSON number.
#[derive(Clone, Copy, Debug, Deserialize_repr, Eq, PartialEq, Serialize_repr)]
#[repr(u8)]
pub enum DeliveryOpcode {
    /// Send the delivery's `data` over the shard's websocket.
    Send = 0,
    /// Reconnect the shard; it resumes its session.
    Reconnect = 1,
}

/// A delivery on `gateway.send`, as producers publish it:
/// `{"op": 0, "shard": 3, "data": {"op": 3, "d": {...}}}` to send a command,
/// `{"op": 1, "shard": 3}` to reconnect a shard.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DeliveryInfo {
    /// What to do.
    pub op: DeliveryOpcode,
    /// The shard to do it on. A delivery for a shard the bot does not have is
    /// dropped.
    pub shard: u32,
    /// For a send, the gateway payload: an object with Discord's `op` and
    /// `d`. Missing is the same as `null`; a reconnect ignores it.
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

/// Why a delivery was rejected. The `Display` text is the reason logged when
/// the command is dropped.
#[derive(Debug, Eq, PartialEq)]
pub enum InvalidCommand {
    /// The body is not a [`DeliveryInfo`] in JSON (not JSON at all, an
    /// unknown `op`, a shard that is not a `u32`...); holds the JSON error.
    Malformed(String),
    /// A send without `data`, or with `null`.
    MissingData,
    /// The `data` of a send is not a JSON object.
    NotAnObject,
    /// The `op` in `data` is not one of [`ALLOWED_OPCODES`]; `None` when it
    /// is missing or not an unsigned integer.
    OpcodeNotAllowed(Option<u64>),
    /// `data` is longer than [`MAX_PAYLOAD_BYTES`] as compact JSON; holds its
    /// length in bytes.
    TooLarge(usize),
}

impl std::fmt::Display for InvalidCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(err) => write!(f, "malformed delivery: {err}"),
            Self::MissingData => f.write_str("send delivery without data"),
            Self::NotAnObject => f.write_str("data is not a JSON object"),
            Self::OpcodeNotAllowed(Some(op)) => write!(f, "gateway opcode {op} may not be sent"),
            Self::OpcodeNotAllowed(None) => f.write_str("data has no unsigned integer op"),
            Self::TooLarge(len) => {
                write!(
                    f,
                    "payload is {len} bytes, Discord's limit is {MAX_PAYLOAD_BYTES}"
                )
            }
        }
    }
}

impl std::error::Error for InvalidCommand {}

/// Parse a raw delivery body.
///
/// # Errors
///
/// [`InvalidCommand::Malformed`] when the body is not a [`DeliveryInfo`] in
/// JSON.
pub fn parse(body: &[u8]) -> Result<DeliveryInfo, InvalidCommand> {
    serde_json::from_slice(body).map_err(|err| InvalidCommand::Malformed(err.to_string()))
}

/// Turn a delivery into the command to run on its shard. A send becomes the
/// compact JSON of its `data`.
///
/// # Errors
///
/// A reconnect never fails. A send fails with [`InvalidCommand::MissingData`]
/// without `data`, [`InvalidCommand::NotAnObject`] when `data` is not an
/// object, [`InvalidCommand::OpcodeNotAllowed`] when its `op` is not one of
/// [`ALLOWED_OPCODES`], and [`InvalidCommand::TooLarge`] when it is longer
/// than [`MAX_PAYLOAD_BYTES`].
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
            // A `Value` displays as its compact JSON, which cannot fail.
            let json = data.to_string();
            if json.len() > MAX_PAYLOAD_BYTES {
                return Err(InvalidCommand::TooLarge(json.len()));
            }
            Ok(ShardCommand::Send(json))
        }
    }
}

/// What to do with a delivery.
#[derive(Debug, Eq, PartialEq)]
pub enum Plan {
    /// Run the command on one of this process' shards.
    Run(u32, ShardCommand),
    /// Another process runs the shard: route the command there.
    Forward(u32),
    /// Malformed, not allowed or for a shard that does not exist: drop it.
    Drop,
}

/// Rate limit for the warnings about dropped deliveries: a producer stuck on
/// an invalid command must not flood the log. The consumer still counts every
/// drop in `gateway_commands`.
static DROP_WARNINGS: Throttle = Throttle::new(Duration::from_secs(5));

/// Decide what to do with a delivery. `owned` says whether this process runs
/// the shard; `total` is the shard count of the bot.
pub fn plan(body: &[u8], owned: impl Fn(u32) -> bool, total: u32) -> Plan {
    let delivery = match parse(body) {
        Ok(delivery) => delivery,
        Err(err) => {
            if let Some(suppressed) = DROP_WARNINGS.check() {
                tracing::warn!(
                    "Dropping gateway command: {} ({} similar suppressed)",
                    err,
                    suppressed
                );
            }
            return Plan::Drop;
        }
    };
    if delivery.shard >= total {
        if let Some(suppressed) = DROP_WARNINGS.check() {
            tracing::warn!(
                "Dropping gateway command for invalid shard {} ({} similar suppressed)",
                delivery.shard,
                suppressed
            );
        }
        return Plan::Drop;
    }
    if !owned(delivery.shard) {
        return Plan::Forward(delivery.shard);
    }
    match validate(&delivery) {
        Ok(command) => Plan::Run(delivery.shard, command),
        Err(err) => {
            if let Some(suppressed) = DROP_WARNINGS.check() {
                tracing::warn!(
                    "Dropping gateway command for shard {}: {} ({} similar suppressed)",
                    delivery.shard,
                    err,
                    suppressed
                );
            }
            Plan::Drop
        }
    }
}

/// Time limit for acknowledging or forwarding one delivery.
const ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// Extra time to subscribe per shard of this process: binding its queue to
/// each shard is one more broker round trip.
const SUBSCRIBE_PER_BINDING: Duration = Duration::from_millis(50);

/// The longest wait between two attempts to subscribe.
const MAX_SUBSCRIBE_BACKOFF: Duration = Duration::from_secs(30);

/// Time limit for subscribing with `shards` bindings. The declarations, the
/// bindings and the two consumers are sequential round trips, so a flat limit
/// could never be met by a process running many shards.
fn subscribe_timeout(shards: usize) -> Duration {
    let bindings = u32::try_from(shards).unwrap_or(u32::MAX);
    ACK_TIMEOUT.saturating_add(SUBSCRIBE_PER_BINDING.saturating_mul(bindings))
}

/// Consume gateway commands until the connection is closed.
///
/// Producers publish to `gateway.send`, which every process consumes. A
/// process runs commands for its own shards and forwards the others to the
/// `gateway.send.shards` exchange with the shard id as routing key; each
/// process binds an exclusive queue there for the shards it runs. So with
/// several processes every command reaches the one running its shard (and is
/// dropped by the broker if none does), though commands for one shard are not
/// guaranteed to run in the order they were published.
///
/// The consumer follows the AMQP supervisor's current link, so it moves to a
/// new connection as soon as one is installed. Deliveries are acknowledged
/// only after they were handled; one that could not be forwarded is requeued,
/// and the connection is replaced.
pub async fn run_consumer(amqp: Arc<Amqp>, registry: Arc<Registry>, total: u32) {
    let shards: Vec<u32> = registry.handles().map(|handle| handle.id).collect();
    let mut links = amqp.subscribe();
    let mut backoff = Duration::from_millis(250);

    while !amqp.is_closing() {
        // Also wakes up when the connection is closed while there is no link,
        // as the link then stays `None`.
        let woken = links.wait_for(|link| link.is_some() || amqp.is_closing());
        let link = match woken.await {
            Ok(link) => link.clone(),
            Err(_) => return,
        };
        let Some(link) = link else { return };
        let channel = link.consume_channel().clone();

        // The queues are declared on every (re)subscribe, in case one was
        // deleted.
        let limit = subscribe_timeout(shards.len());
        let subscribed = tokio::time::timeout(limit, consume_commands(&channel, &shards)).await;
        let (mut send_consumer, mut own_consumer) = match subscribed {
            Ok(Ok(consumers)) => consumers,
            Ok(Err(err)) => {
                tracing::warn!("Failed to consume gateway commands: {}", err);
                amqp.mark_broken(Some(link.generation));
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_SUBSCRIBE_BACKOFF);
                continue;
            }
            Err(_) => {
                tracing::warn!("Timed out subscribing to gateway commands");
                amqp.mark_broken(Some(link.generation));
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_SUBSCRIBE_BACKOFF);
                continue;
            }
        };
        backoff = Duration::from_millis(250);

        loop {
            let (delivery, routed_to_us) = tokio::select! {
                // A new link (or none): leave this connection.
                _ = links.changed() => break,
                delivery = send_consumer.next() => (delivery, false),
                delivery = own_consumer.next() => (delivery, true),
            };
            let delivery = match delivery {
                Some(Ok(delivery)) => delivery,
                Some(Err(err)) => {
                    tracing::warn!("Gateway command consumer failed: {}", err);
                    amqp.mark_broken(Some(link.generation));
                    break;
                }
                // Cancelled by the broker (e.g. a queue was deleted).
                None => break,
            };

            let plan = plan(&delivery.data, |shard| registry.get(shard).is_some(), total);
            let forward_failed = match plan {
                Plan::Run(shard, command) => {
                    run_on_shard(&registry, shard, command);
                    false
                }
                Plan::Forward(shard) if !routed_to_us => {
                    !forward(&channel, shard, &delivery.data, &delivery.properties).await
                }
                Plan::Forward(shard) => {
                    tracing::warn!("Dropping a command routed here for shard {}, which this process does not run", shard);
                    false
                }
                Plan::Drop => {
                    GATEWAY_COMMANDS.with_label_values(&["", "invalid"]).inc();
                    false
                }
            };

            let answered = if forward_failed {
                let requeue = BasicNackOptions {
                    multiple: false,
                    requeue: true,
                };
                tokio::time::timeout(ACK_TIMEOUT, delivery.acker.nack(requeue)).await
            } else {
                tokio::time::timeout(ACK_TIMEOUT, delivery.acker.ack(BasicAckOptions::default()))
                    .await
            };
            let acked = match answered {
                Ok(Ok(_)) => true,
                Ok(Err(err)) => {
                    tracing::warn!("Failed to acknowledge a gateway command: {}", err);
                    false
                }
                Err(_) => {
                    tracing::warn!("Timed out acknowledging a gateway command");
                    false
                }
            };
            if forward_failed || !acked {
                tracing::warn!("Failed to handle a gateway command; reconnecting");
                amqp.mark_broken(Some(link.generation));
                break;
            }
        }
    }
}

/// Declare the queues (see [`declare_consume`]) and consume `gateway.send`
/// and this process' own queue, in that order.
async fn consume_commands(
    channel: &Channel,
    shards: &[u32],
) -> Result<(Consumer, Consumer), lapin::Error> {
    let own_queue = declare_consume(channel, shards.iter().copied()).await?;
    let send_consumer = channel
        .basic_consume(
            QUEUE_SEND.into(),
            "".into(),
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await?;
    let own_consumer = channel
        .basic_consume(
            own_queue.as_str().into(),
            "".into(),
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await?;
    Ok((send_consumer, own_consumer))
}

/// Run a command on one of this process' shards and count it as sent, or as
/// dropped when the shard stopped for good.
fn run_on_shard(registry: &Registry, shard: u32, command: ShardCommand) {
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
    let result = if sent { "sent" } else { "dropped" };
    GATEWAY_COMMANDS
        .with_label_values(&[shard.to_string().as_str(), result])
        .inc();
    if !sent {
        // The shard stopped for good (fatal close code).
        tracing::warn!("Shard {} is not running; dropping its command", shard);
    }
}

/// Forward a command to the process running `shard`, through
/// [`SHARD_EXCHANGE`], and count it. Returns whether the broker took it.
async fn forward(
    channel: &Channel,
    shard: u32,
    payload: &[u8],
    properties: &BasicProperties,
) -> bool {
    let shard_id = shard.to_string();
    let publish = channel.basic_publish(
        SHARD_EXCHANGE.into(),
        shard_id.as_str().into(),
        BasicPublishOptions::default(),
        payload,
        properties.clone(),
    );
    match tokio::time::timeout(ACK_TIMEOUT, publish).await {
        Ok(Ok(_)) => {
            GATEWAY_COMMANDS
                .with_label_values(&[shard_id.as_str(), "forwarded"])
                .inc();
            true
        }
        Ok(Err(err)) => {
            tracing::warn!("Failed to forward a command for shard {}: {}", shard, err);
            false
        }
        Err(_) => {
            tracing::warn!("Timed out forwarding a command for shard {}", shard);
            false
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

    /// Discord closes the connection for payloads that "exceed 4096 bytes",
    /// so one of exactly 4096 bytes is still sent.
    #[test]
    fn the_size_limit_is_inclusive() {
        let data = |len: usize| {
            let overhead = json!({"op": 3, "d": {"status": ""}}).to_string().len();
            json!({"op": 3, "d": {"status": "x".repeat(len - overhead)}})
        };
        let send = |data: &Value| validate(&delivery(json!({"op": 0, "shard": 0, "data": data})));

        let largest = data(MAX_PAYLOAD_BYTES);
        assert_eq!(largest.to_string().len(), MAX_PAYLOAD_BYTES);
        assert_eq!(send(&largest), Ok(ShardCommand::Send(largest.to_string())));
        assert_eq!(
            send(&data(MAX_PAYLOAD_BYTES + 1)),
            Err(InvalidCommand::TooLarge(MAX_PAYLOAD_BYTES + 1))
        );
    }

    /// Dropped commands are logged with the reason, which has to say what
    /// was wrong with them.
    #[test]
    fn rejections_say_what_is_wrong() {
        let reasons: Vec<String> = [
            InvalidCommand::Malformed("expected value at line 1".into()),
            InvalidCommand::MissingData,
            InvalidCommand::NotAnObject,
            InvalidCommand::OpcodeNotAllowed(Some(2)),
            InvalidCommand::OpcodeNotAllowed(None),
            InvalidCommand::TooLarge(5000),
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        let distinct: std::collections::HashSet<&String> = reasons.iter().collect();
        assert_eq!(distinct.len(), reasons.len(), "{reasons:?}");
        assert!(
            reasons[0].contains("expected value at line 1"),
            "{reasons:?}"
        );
        assert!(reasons[3].contains('2'), "{reasons:?}");
        assert!(
            reasons[5].contains("5000") && reasons[5].contains(&MAX_PAYLOAD_BYTES.to_string()),
            "{reasons:?}"
        );
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

        assert!(matches!(
            plan(&body(1), owned, 4),
            Plan::Run(1, ShardCommand::Send(_))
        ));
        assert_eq!(plan(&body(3), owned, 4), Plan::Forward(3));
        assert_eq!(plan(&body(9), owned, 4), Plan::Drop);
        assert_eq!(plan(b"junk", owned, 4), Plan::Drop);

        let invalid = json!({"op": 0, "shard": 0, "data": {"op": 2}})
            .to_string()
            .into_bytes();
        assert_eq!(plan(&invalid, owned, 4), Plan::Drop);
    }
}

#[cfg(test)]
mod subscribe_timeout_tests {
    use super::*;

    /// Subscribing binds the process' queue once per shard, one broker round
    /// trip each, so the time limit grows with the shards: a flat 5 s was
    /// never met by a process running a few hundred shards.
    #[test]
    fn the_subscribe_limit_grows_with_the_shards() {
        assert_eq!(subscribe_timeout(0), ACK_TIMEOUT);
        assert_eq!(subscribe_timeout(1), ACK_TIMEOUT + SUBSCRIBE_PER_BINDING);
        assert_eq!(
            subscribe_timeout(256),
            Duration::from_secs(5) + Duration::from_millis(50) * 256
        );
        assert!(subscribe_timeout(1000) > subscribe_timeout(256));
    }

    /// More bindings than a `u32` holds count as `u32::MAX`: the limit
    /// saturates instead of overflowing.
    #[test]
    fn the_subscribe_limit_saturates() {
        let most = usize::try_from(u32::MAX).unwrap();
        assert_eq!(subscribe_timeout(usize::MAX), subscribe_timeout(most));
        assert!(subscribe_timeout(most) > subscribe_timeout(1_000_000));
    }
}
