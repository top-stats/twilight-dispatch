//! Gateway commands received from the broker (`gateway.send`): their
//! validation, and the consumer that runs them on this process' shards or
//! forwards them to the process running theirs. The consumer also posts
//! messages to channels through Discord's REST API.
//!
//! Anything sent to the gateway that Discord rejects closes the shard's
//! connection (4001 unknown opcode, 4002 decode error, 4005 already
//! authenticated...), so the opcode and the payload size are checked before a
//! command reaches a shard. The payload itself is not: a command whose `d`
//! Discord cannot decode still closes the connection with 4002, and the shard
//! resumes.
//!
//! A message (`op` 2, see [`parse_message`]) is for no shard: whichever
//! process consumes it posts it, through the one REST request a delivery can
//! make (create message, `POST /channels/{id}/messages`), with every mention
//! suppressed. Its content and embeds are checked against Discord's limits
//! first, with twilight-validate (the checks twilight-http makes). A message
//! that Discord rejects anyway, or whose request fails, is logged and
//! dropped, never retried: Discord may have posted it all the same.

use crate::{
    amqp::{declare_consume, Amqp},
    constants::{QUEUE_SEND, SHARD_EXCHANGE},
    gateway::registry::Registry,
    metrics::GATEWAY_COMMANDS,
    util::{ErrorChain, Throttle},
};

use futures_util::StreamExt;
use lapin::{
    options::{BasicAckOptions, BasicConsumeOptions, BasicNackOptions, BasicPublishOptions},
    types::FieldTable,
    BasicProperties, Channel, Consumer,
};
use serde::{de, Deserialize, Deserializer, Serialize};
use serde_json::Value;
use serde_repr::{Deserialize_repr, Serialize_repr};
use std::{sync::Arc, time::Duration};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};
use tokio::sync::Semaphore;
use twilight_gateway::CloseFrame;
use twilight_http::Client;
use twilight_model::{
    channel::message::{
        embed::{EmbedAuthor, EmbedField, EmbedFooter, EmbedImage, EmbedThumbnail},
        AllowedMentions, Embed,
    },
    id::{marker::ChannelMarker, Id},
    util::Timestamp,
};
use twilight_validate::message::{self as limits, MESSAGE_CONTENT_LENGTH_MAX};

/// Discord rejects gateway payloads larger than this.
/// <https://discord.com/developers/docs/events/gateway#sending-events>
pub const MAX_PAYLOAD_BYTES: usize = 4096;

/// Gateway opcodes a client may send itself. Heartbeat, Identify and Resume
/// are owned by the shard and must never be injected from outside.
///
/// 3 Presence Update, 4 Voice State Update, 8 Request Guild Members,
/// 31 Request Soundboard Sounds, 43 Request Channel Info.
pub const ALLOWED_OPCODES: [u64; 5] = [3, 4, 8, 31, 43];

/// The `op` of a delivery that posts a message to a channel (see
/// [`parse_message`]). It is not a [`DeliveryOpcode`]: a message is for no
/// shard.
pub const CREATE_MESSAGE: u64 = 2;

/// What a delivery for a shard asks for: its `op`, a JSON number. A delivery
/// whose `op` is [`CREATE_MESSAGE`] is a message instead.
#[derive(Clone, Copy, Debug, Deserialize_repr, Eq, PartialEq, Serialize_repr)]
#[repr(u8)]
pub enum DeliveryOpcode {
    /// Send the delivery's `data` over the shard's websocket.
    Send = 0,
    /// Reconnect the shard; it resumes its session.
    Reconnect = 1,
}

/// A delivery on `gateway.send` for a shard, as producers publish it:
/// `{"op": 0, "shard": 3, "data": {"op": 3, "d": {...}}}` to send a command,
/// `{"op": 1, "shard": 3}` to reconnect a shard. A message (`op` 2) is not
/// one: see [`parse_message`].
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
    /// A message without embeds whose content is missing, empty or only
    /// whitespace.
    EmptyMessage,
    /// A message whose content is longer than Discord allows (2000
    /// characters); holds its length in characters.
    ContentTooLong(usize),
    /// A message whose embeds Discord would reject: more than 10, or over the
    /// limits of an embed (title 256 characters, description 4096, 25 fields,
    /// field name 256 and value 1024, footer 2048, author name 256, 6000 in
    /// all) or a color above `0xFFFFFF`; holds twilight-validate's reason.
    InvalidEmbeds(String),
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
            Self::EmptyMessage => f.write_str("message has neither content nor embeds"),
            Self::ContentTooLong(chars) => {
                write!(
                    f,
                    "content is {chars} characters, the limit is {MESSAGE_CONTENT_LENGTH_MAX}"
                )
            }
            Self::InvalidEmbeds(reason) => write!(f, "embeds over Discord's limits: {reason}"),
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

/// A delivery's `op` alone, read before the rest: a message has no shard, so
/// it is not a `DeliveryInfo`.
#[derive(Deserialize)]
struct DeliveryOp {
    /// The delivery's `op`.
    op: u64,
}

/// Whether a delivery is a message: its `op` is [`CREATE_MESSAGE`], whatever
/// else it holds.
pub fn is_message(body: &[u8]) -> bool {
    let delivery = serde_json::from_slice::<DeliveryOp>(body);
    delivery.is_ok_and(|delivery| delivery.op == CREATE_MESSAGE)
}

/// A message delivery: `{"op": 2, "data": {...}}`. Its other fields, such as
/// a `shard`, are not read.
#[derive(Deserialize)]
struct MessageDelivery {
    /// The message.
    data: MessageData,
}

/// The `data` of a message delivery. Other fields (`tts`, `components`...)
/// are rejected: they are not supported.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MessageData {
    /// The channel to post in: a snowflake, as a string or a number.
    channel_id: Id<ChannelMarker>,
    /// The text; `None` (missing or `null`) is the same as empty.
    content: Option<String>,
    /// Discord embed objects; `None` (missing or `null`) is the same as none.
    embeds: Option<Vec<MessageEmbed>>,
}

/// An embed of a message, as Discord documents its embed object. Fields that
/// only Discord sets (`type`, always "rich" for a bot, `provider`, `video`)
/// and unknown ones are ignored.
#[derive(Deserialize)]
struct MessageEmbed {
    /// The author: `name`, and optionally `url` and `icon_url`.
    author: Option<EmbedAuthor>,
    /// An RGB color.
    color: Option<u32>,
    /// The text under the title.
    description: Option<String>,
    /// Fields: `name`, `value` and `inline`; `null` is the same as none.
    fields: Option<Vec<EmbedField>>,
    /// The footer: `text`, and optionally `icon_url`.
    footer: Option<EmbedFooter>,
    /// The image: `url`.
    image: Option<EmbedImage>,
    /// The thumbnail: `url`.
    thumbnail: Option<EmbedThumbnail>,
    /// When the embed's content happened: any RFC 3339 time.
    #[serde(default, deserialize_with = "rfc3339")]
    timestamp: Option<Timestamp>,
    /// The title.
    title: Option<String>,
    /// The link of the title.
    url: Option<String>,
}

impl From<MessageEmbed> for Embed {
    fn from(embed: MessageEmbed) -> Self {
        Self {
            author: embed.author,
            color: embed.color,
            description: embed.description,
            fields: embed.fields.unwrap_or_default(),
            footer: embed.footer,
            image: embed.image,
            kind: "rich".to_owned(),
            provider: None,
            thumbnail: embed.thumbnail,
            timestamp: embed.timestamp,
            title: embed.title,
            url: embed.url,
            video: None,
        }
    }
}

/// An embed's `timestamp`, which may be any RFC 3339 time: twilight's own
/// parser rejects JavaScript's `toISOString()` (`2026-01-01T00:00:00.000Z`).
/// `null` is the same as missing.
fn rfc3339<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Timestamp>, D::Error> {
    let Some(text) = Option::<String>::deserialize(deserializer)? else {
        return Ok(None);
    };
    parse_rfc3339(&text).map(Some).map_err(de::Error::custom)
}

/// An RFC 3339 time as a timestamp, to the microsecond.
fn parse_rfc3339(text: &str) -> Result<Timestamp, String> {
    let time = OffsetDateTime::parse(text, &Rfc3339).map_err(|err| format!("{err}: {text}"))?;
    let micros = time.unix_timestamp_nanos() / 1000;
    let micros = i64::try_from(micros).map_err(|err| format!("{err}: {text}"))?;
    Timestamp::from_micros(micros).map_err(|err| format!("{err}: {text}"))
}

/// A message to post, validated by [`parse_message`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelMessage {
    /// The channel to post in.
    pub channel_id: Id<ChannelMarker>,
    /// The text; empty for a message of embeds only.
    pub content: String,
    /// At most 10 embeds, all of type "rich".
    pub embeds: Vec<Embed>,
}

/// Parse and validate a message delivery (one [`is_message`] accepts):
/// `{"op": 2, "data": {"channel_id": "123", "content": "...", "embeds": [...]}}`.
///
/// `channel_id` is a snowflake (a string or a number), `content` text and
/// `embeds` Discord embed objects; either may be missing or `null`. Other
/// fields of `data` are rejected, and those of the delivery (a `shard`) are
/// not read.
///
/// # Errors
///
/// [`InvalidCommand::Malformed`] when the body is not of that shape (no
/// `data`, no `channel_id` or one that is not a snowflake, an unknown field,
/// an embed field of the wrong type, a `timestamp` that is not an RFC 3339
/// time...), [`InvalidCommand::EmptyMessage`] when there are no embeds and
/// the content is missing, empty or only whitespace,
/// [`InvalidCommand::ContentTooLong`] when the content is over 2000
/// characters and [`InvalidCommand::InvalidEmbeds`] when the embeds are over
/// Discord's limits.
pub fn parse_message(body: &[u8]) -> Result<ChannelMessage, InvalidCommand> {
    let malformed = |err: serde_json::Error| InvalidCommand::Malformed(err.to_string());
    let delivery: MessageDelivery = serde_json::from_slice(body).map_err(malformed)?;
    let data = delivery.data;
    let content = data.content.unwrap_or_default();
    let embeds = data.embeds.unwrap_or_default();
    let embeds: Vec<Embed> = embeds.into_iter().map(Embed::from).collect();
    if content.trim().is_empty() && embeds.is_empty() {
        return Err(InvalidCommand::EmptyMessage);
    }
    if limits::content(&content).is_err() {
        return Err(InvalidCommand::ContentTooLong(content.chars().count()));
    }
    if let Err(err) = limits::embeds(&embeds) {
        let reason = format!("{err} ({:?})", err.kind());
        return Err(InvalidCommand::InvalidEmbeds(reason));
    }
    Ok(ChannelMessage {
        channel_id: data.channel_id,
        content,
        embeds,
    })
}

/// What to do with a delivery.
#[derive(Debug, Eq, PartialEq)]
pub enum Plan {
    /// Run the command on one of this process' shards.
    Run(u32, ShardCommand),
    /// Another process runs the shard: route the command there.
    Forward(u32),
    /// Post the message from this process: it is for no shard, so it is
    /// never forwarded.
    Post(ChannelMessage),
    /// Malformed, not allowed or for a shard that does not exist: drop it.
    Drop,
}

/// Rate limit for the warnings about dropped deliveries: a producer stuck on
/// an invalid command must not flood the log. The consumer still counts every
/// drop in `gateway_commands`.
static DROP_WARNINGS: Throttle = Throttle::new(Duration::from_secs(5));

/// Decide what to do with a delivery. `owned` says whether this process runs
/// the shard; `total` is the shard count of the bot. A message is posted by
/// whichever process consumes it, whatever its `shard` says.
pub fn plan(body: &[u8], owned: impl Fn(u32) -> bool, total: u32) -> Plan {
    if is_message(body) {
        return plan_message(body);
    }
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

/// Rate limit for the warnings about dropped messages, as [`DROP_WARNINGS`]
/// for commands: a producer of messages has a throttle of its own, so its
/// mistakes are not hidden by those of a producer of commands.
static MESSAGE_DROP_WARNINGS: Throttle = Throttle::new(Duration::from_secs(5));

/// Decide what to do with a message delivery: post it, or drop it when it is
/// invalid.
fn plan_message(body: &[u8]) -> Plan {
    match parse_message(body) {
        Ok(message) => Plan::Post(message),
        Err(err) => {
            if let Some(suppressed) = MESSAGE_DROP_WARNINGS.check() {
                tracing::warn!(
                    "Dropping message delivery: {} ({} similar suppressed)",
                    err,
                    suppressed
                );
            }
            Plan::Drop
        }
    }
}

/// How many messages may be posting at once. While that many wait for
/// Discord the consumer waits too, so a burst of messages stays in the broker
/// rather than in memory.
const MAX_POSTING: usize = 8;

/// The messages posting now, at most [`MAX_POSTING`].
static POSTING: Semaphore = Semaphore::const_new(MAX_POSTING);

/// Rate limit for the warnings about messages that could not be posted (a
/// producer posting to channels the bot cannot see, a Discord outage). Every
/// failure is still counted in `gateway_commands`.
static POST_WARNINGS: Throttle = Throttle::new(Duration::from_secs(5));

/// Post a message through Discord's REST API, with every mention suppressed
/// (no `@everyone`, role or user is pinged whatever the content says), and
/// count it in `gateway_commands` as `posted` or `failed`.
///
/// twilight-http waits for its rate limits. A failure (an error response such
/// as Missing Access or Unknown Channel, a timeout, no connection) is logged
/// at most once per 5 s and not retried: Discord may have posted the message
/// anyway, and a message must never be posted twice.
pub async fn post(client: &Client, message: &ChannelMessage) {
    let mentions = AllowedMentions::default();
    let mut request = client
        .create_message(message.channel_id)
        .allowed_mentions(Some(&mentions));
    if !message.content.is_empty() {
        request = request.content(&message.content);
    }
    if !message.embeds.is_empty() {
        request = request.embeds(&message.embeds);
    }
    let Err(err) = request.await else {
        GATEWAY_COMMANDS.with_label_values(&["", "posted"]).inc();
        return;
    };
    GATEWAY_COMMANDS.with_label_values(&["", "failed"]).inc();
    if let Some(suppressed) = POST_WARNINGS.check() {
        tracing::warn!(
            "Failed to post a message to channel {}: {} ({} similar suppressed)",
            message.channel_id,
            ErrorChain(&err),
            suppressed
        );
    }
}

/// Post a message in a task of its own, so that the consumer moves on while
/// Discord answers. Waits first while [`MAX_POSTING`] messages are posting.
async fn post_in_background(client: &Arc<Client>, message: ChannelMessage) {
    // POSTING is never closed, so this only waits for a permit.
    if let Ok(permit) = POSTING.acquire().await {
        let client = client.clone();
        tokio::spawn(async move {
            post(&client, &message).await;
            drop(permit);
        });
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

/// Consume gateway commands and messages until the connection is closed.
///
/// Producers publish to `gateway.send`, which every process consumes. A
/// process runs commands for its own shards and forwards the others to the
/// `gateway.send.shards` exchange with the shard id as routing key; each
/// process binds an exclusive queue there for the shards it runs. So with
/// several processes every command reaches the one running its shard (and is
/// dropped by the broker if none does), though commands for one shard are not
/// guaranteed to run in the order they were published.
///
/// A message is posted through `client` by the process that consumes it (see
/// [`post`]), in a task of its own, at most 8 at a time. It is posted only
/// once its delivery was acknowledged, and never requeued after that, so it
/// is not posted twice.
///
/// The consumer follows the AMQP supervisor's current link, so it moves to a
/// new connection as soon as one is installed. Deliveries are acknowledged
/// only after they were handled; one that could not be forwarded is requeued,
/// and the connection is replaced.
#[allow(
    clippy::too_many_lines,
    reason = "one loop that subscribes, then plans, runs and acknowledges each delivery"
)]
pub async fn run_consumer(
    amqp: Arc<Amqp>,
    registry: Arc<Registry>,
    total: u32,
    client: Arc<Client>,
) {
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
            // Posted once its delivery is acknowledged: a delivery that comes
            // back after a failed acknowledgement was never posted.
            let mut message = None;
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
                Plan::Post(outgoing) => {
                    message = Some(outgoing);
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
            if let Some(message) = message {
                post_in_background(&client, message).await;
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
            InvalidCommand::EmptyMessage,
            InvalidCommand::ContentTooLong(2001),
            InvalidCommand::InvalidEmbeds("message has too many embeds".into()),
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
        assert!(
            reasons[7].contains("2001") && reasons[7].contains("2000"),
            "{reasons:?}"
        );
        assert!(reasons[8].ends_with("too many embeds"), "{reasons:?}");
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
mod message_tests {
    use super::*;
    use serde_json::json;

    /// A message delivery with this `data`, parsed and validated.
    fn parse_data(data: Value) -> Result<ChannelMessage, InvalidCommand> {
        parse_message(json!({"op": 2, "data": data}).to_string().as_bytes())
    }

    /// The `data` of a message with only this embed.
    fn with_embed(embed: Value) -> Value {
        json!({"channel_id": "1", "embeds": [embed]})
    }

    fn is_malformed(data: Value) -> bool {
        matches!(parse_data(data), Err(InvalidCommand::Malformed(_)))
    }

    fn invalid_embeds(data: Value) -> bool {
        matches!(parse_data(data), Err(InvalidCommand::InvalidEmbeds(_)))
    }

    /// What the bot posts when it joins a guild: content and an embed as
    /// discord.js builds it, without `type` and with a JavaScript timestamp.
    /// The embed is posted as it was sent, as a rich embed, without the
    /// fields only Discord sets.
    #[test]
    fn a_message_is_posted_as_it_was_sent() {
        let mut embed = json!({
            "title": "Thanks for inviting the bot",
            "description": "Stats are on their way.",
            "url": "https://example.com",
            "color": 0x0058_65F2,
            "fields": [{"name": "Docs", "value": "https://example.com/docs", "inline": true}],
            "footer": {"text": "footer", "icon_url": "https://example.com/icon.png"},
            "author": {"name": "author", "url": "https://example.com/author"},
            "image": {"url": "https://example.com/image.png"},
            "thumbnail": {"url": "https://example.com/thumbnail.png"},
        });
        let mut sent = embed.clone();
        embed["timestamp"] = json!("2026-10-30T12:34:56.789Z");
        embed["provider"] = json!({"name": "ignored"});
        let data = json!({"channel_id": "123", "content": "Hi @everyone", "embeds": [embed]});
        let message = parse_data(data).unwrap();
        assert_eq!(message.channel_id, Id::new(123));
        assert_eq!(message.content, "Hi @everyone");

        sent["type"] = json!("rich");
        sent["timestamp"] = json!("2026-10-30T12:34:56.789000+00:00");
        let posted = serde_json::to_value(&message.embeds).unwrap();
        assert_eq!(posted, json!([sent]));
    }

    /// A channel id is a snowflake, as a string or a number; zero, anything
    /// else and none are malformed.
    #[test]
    fn channel_ids_are_snowflakes() {
        let content = |channel: Value| json!({"channel_id": channel, "content": "hi"});
        for channel in [json!("123"), json!(123)] {
            let message = parse_data(content(channel)).unwrap();
            assert_eq!(message.channel_id, Id::new(123));
        }
        for channel in [json!("0"), json!("x"), json!(-1), Value::Null] {
            assert!(is_malformed(content(channel.clone())), "{channel}");
        }
        assert!(is_malformed(json!({"content": "hi"})));
    }

    /// A message is for no shard: one with a `shard`, valid or not, is posted
    /// by the process that consumes it, which need not run any shard.
    #[test]
    fn the_shard_of_a_message_is_not_read() {
        let data = json!({"channel_id": "123", "content": "hi"});
        let expected = parse_data(data.clone()).unwrap();
        for shard in [json!(0), json!(99), json!("x"), Value::Null] {
            let body = json!({"op": 2, "shard": shard, "data": data}).to_string();
            assert!(is_message(body.as_bytes()), "{body}");
            let planned = plan(body.as_bytes(), |_| false, 1);
            assert_eq!(planned, Plan::Post(expected.clone()), "{body}");
        }
    }

    /// Only `op` 2 is a message; an invalid message is dropped.
    #[test]
    fn messages_are_told_apart_by_their_op() {
        let body = json!({"op": 2, "data": {"channel_id": "1", "content": "hi"}}).to_string();
        let posted = plan(body.as_bytes(), |_| true, 1);
        assert!(matches!(posted, Plan::Post(_)), "{posted:?}");
        let invalid = json!({"op": 2, "data": {"channel_id": "1"}}).to_string();
        assert_eq!(plan(invalid.as_bytes(), |_| true, 1), Plan::Drop);
        for body in [r#"{"op":0,"shard":0}"#, r#"{"op":"2"}"#, "2"] {
            assert!(!is_message(body.as_bytes()), "{body}");
        }
    }

    /// Discord refuses a message with nothing to show, and so does the
    /// consumer: no embeds, and content that is missing, `null`, empty or
    /// blank.
    #[test]
    fn a_message_needs_content_or_embeds() {
        let empty = [
            json!({"channel_id": "1"}),
            json!({"channel_id": "1", "content": null, "embeds": null}),
            json!({"channel_id": "1", "content": "", "embeds": []}),
            json!({"channel_id": "1", "content": " \n\t"}),
        ];
        for data in empty {
            let parsed = parse_data(data.clone());
            assert_eq!(parsed, Err(InvalidCommand::EmptyMessage), "{data}");
        }
        let data = json!({"channel_id": "1", "content": " ", "embeds": [{"title": "t"}]});
        assert!(parse_data(data).is_ok());
        let data = json!({"channel_id": "1", "content": "hi"});
        assert!(parse_data(data).unwrap().embeds.is_empty());
    }

    /// Content and embeds over Discord's limits are dropped before any
    /// request is made; up to the limits they are posted.
    #[test]
    fn discords_limits_are_checked_before_posting() {
        let content = |len: usize| json!({"channel_id": "1", "content": "é".repeat(len)});
        assert!(parse_data(content(2000)).is_ok());
        assert_eq!(
            parse_data(content(2001)),
            Err(InvalidCommand::ContentTooLong(2001))
        );

        let embeds = |count: usize| {
            let embed = json!({"title": "t"});
            json!({"channel_id": "1", "embeds": vec![embed; count]})
        };
        assert!(parse_data(embeds(10)).is_ok());
        assert!(invalid_embeds(embeds(11)));

        let field = |name: usize, value: usize| json!({"name": "n".repeat(name), "value": "v".repeat(value)});
        let within = [
            json!({"title": "t".repeat(256), "description": "d".repeat(4096)}),
            json!({"fields": vec![field(1, 1); 25], "color": 0x00FF_FFFF}),
            json!({"fields": [field(256, 1024)]}),
            json!({"footer": {"text": "f".repeat(2048)}}),
            json!({"author": {"name": "a".repeat(256)}}),
        ];
        for embed in within {
            let parsed = parse_data(with_embed(embed));
            assert!(parsed.is_ok(), "{parsed:?}");
        }
        let over = [
            json!({"title": "t".repeat(257)}),
            json!({"description": "d".repeat(4097)}),
            json!({"fields": vec![field(1, 1); 26]}),
            json!({"fields": [field(257, 1)]}),
            json!({"fields": [field(1, 1025)]}),
            json!({"footer": {"text": "f".repeat(2049)}}),
            json!({"author": {"name": "a".repeat(257)}}),
            json!({"color": 0x0100_0000}),
        ];
        for embed in over {
            assert!(invalid_embeds(with_embed(embed.clone())), "{embed}");
        }

        // 6000 characters in all the embeds of a message.
        let long = json!({"description": "d".repeat(4000)});
        let data = json!({"channel_id": "1", "embeds": [long, long]});
        assert!(invalid_embeds(data));
    }

    /// `data` must be of the documented shape. What the consumer does not
    /// support (`tts`, `components`...) is rejected rather than ignored.
    #[test]
    fn other_shapes_are_malformed() {
        let malformed = [
            Value::Null,
            json!("hi"),
            json!({"channel_id": "1", "content": 5}),
            json!({"channel_id": "1", "embeds": {"title": "t"}}),
            json!({"channel_id": "1", "embeds": [{"fields": [{"name": "n"}]}]}),
            json!({"channel_id": "1", "embeds": [{"timestamp": "yesterday"}]}),
            json!({"channel_id": "1", "embeds": [{"color": -1}]}),
        ];
        for data in malformed {
            assert!(is_malformed(data.clone()), "{data}");
        }

        let tts = json!({"channel_id": "1", "content": "hi", "tts": true});
        let reason = parse_data(tts).unwrap_err().to_string();
        assert!(reason.contains("unknown field `tts`"), "{reason}");
        let missing = parse_message(br#"{"op":2,"shard":0}"#).unwrap_err();
        let reason = missing.to_string();
        assert!(reason.contains("missing field `data`"), "{reason}");
    }

    /// An embed's timestamp may be any RFC 3339 time, in any offset; `null`
    /// is the same as none, and so are `null` fields.
    #[test]
    fn embed_timestamps_are_rfc3339_times() {
        let at = |timestamp: Value| {
            let data = with_embed(json!({"title": "t", "timestamp": timestamp}));
            parse_data(data).unwrap().embeds[0].timestamp
        };
        let utc = Timestamp::parse("2026-10-30T12:34:56.789000+00:00").ok();
        assert!(utc.is_some());
        assert_eq!(at(json!("2026-10-30T12:34:56.789Z")), utc);
        assert_eq!(at(json!("2026-10-30T14:34:56.789+02:00")), utc);
        assert_eq!(at(json!("2026-10-30T12:34:56.789000+00:00")), utc);
        assert_eq!(at(Value::Null), None);

        let data = with_embed(json!({"title": "t", "fields": null}));
        assert!(parse_data(data).unwrap().embeds[0].fields.is_empty());
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
