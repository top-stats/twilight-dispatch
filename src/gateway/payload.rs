//! Raw gateway payload handling: envelope parsing, routing and re-publishing.
//!
//! The published message keeps the historical shape `{"op", "t", "d", "old"?}`.
//! `d` is passed through byte for byte (it is never re-serialized), so the
//! queue sees exactly what Discord sent.

use serde::{Deserialize, Serialize};
use serde_json::{value::RawValue, Value};
use std::borrow::Cow;

/// The outer fields of a gateway payload.
#[derive(Debug, Deserialize)]
pub struct Envelope<'a> {
    pub op: u8,
    #[serde(default)]
    pub s: Option<u64>,
    #[serde(borrow, default)]
    pub t: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    pub d: Option<&'a RawValue>,
}

impl<'a> Envelope<'a> {
    pub fn parse(json: &'a str) -> serde_json::Result<Self> {
        serde_json::from_str(json)
    }

    pub fn kind(&self) -> Option<&str> {
        self.t.as_deref()
    }
}

/// What orders a payload relative to others.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Route {
    Guild(u64),
    Channel(u64),
    Unrouted,
}

impl Route {
    /// The worker that processes this payload. Every payload of one guild (or
    /// one DM channel) goes to the same worker, so they stay in order.
    pub fn worker(self, workers: usize) -> usize {
        let workers = workers.max(1) as u64;
        match self {
            // The low bits of a snowflake are a per-process increment that is
            // usually 0, so `id % workers` would load a few workers only.
            Self::Guild(id) | Self::Channel(id) => (mix(id) % workers) as usize,
            Self::Unrouted => 0,
        }
    }
}

/// splitmix64 finalizer: spreads every input bit over the output.
const fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

#[derive(Deserialize)]
struct Hint<'a> {
    #[serde(borrow, default)]
    guild_id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    id: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    channel_id: Option<Cow<'a, str>>,
}

/// Work out which guild (or channel) a dispatch belongs to from its raw `d`.
pub fn route(kind: Option<&str>, d: Option<&RawValue>) -> Route {
    let Some(d) = d else {
        return Route::Unrouted;
    };
    let Ok(hint) = serde_json::from_str::<Hint<'_>>(d.get()) else {
        return Route::Unrouted;
    };
    let parse = |id: Option<Cow<'_, str>>| id.and_then(|id| id.parse::<u64>().ok());

    let (guild, channel) = match kind {
        Some("GUILD_CREATE" | "GUILD_UPDATE" | "GUILD_DELETE") => {
            (parse(hint.id), parse(hint.channel_id))
        }
        // A DM channel's own events carry it in `id` (and no guild), while its
        // messages name it in `channel_id`: route both by the channel so they
        // reach the same worker in order.
        Some("CHANNEL_CREATE" | "CHANNEL_UPDATE" | "CHANNEL_DELETE") => (
            parse(hint.guild_id),
            parse(hint.id).or_else(|| parse(hint.channel_id)),
        ),
        _ => (parse(hint.guild_id), parse(hint.channel_id)),
    };

    match (guild, channel) {
        (Some(guild), _) => Route::Guild(guild),
        (None, Some(channel)) => Route::Channel(channel),
        (None, None) => Route::Unrouted,
    }
}

#[derive(Serialize)]
struct Outgoing<'a> {
    op: u8,
    t: Option<&'a str>,
    d: &'a RawValue,
    #[serde(skip_serializing_if = "Option::is_none")]
    old: Option<&'a Value>,
}

/// Build the message published to the broker.
pub fn outgoing(envelope: &Envelope<'_>, old: Option<&Value>) -> serde_json::Result<Vec<u8>> {
    let null = RawValue::from_string("null".to_owned())?;
    serde_json::to_vec(&Outgoing {
        op: envelope.op,
        t: envelope.kind(),
        d: envelope.d.unwrap_or(&null),
        old,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MESSAGE: &str = r#"{"t":"MESSAGE_CREATE","s":42,"op":0,"d":{"id":"5","channel_id":"3","guild_id":"7","content":"hi é","nested":{"guild_id":"999"}}}"#;

    #[test]
    fn parses_envelope() {
        let env = Envelope::parse(MESSAGE).unwrap();
        assert_eq!(env.op, 0);
        assert_eq!(env.s, Some(42));
        assert_eq!(env.kind(), Some("MESSAGE_CREATE"));
        assert!(env.d.unwrap().get().starts_with(r#"{"id":"5""#));
    }

    #[test]
    fn parses_non_dispatch_envelopes() {
        let hello =
            Envelope::parse(r#"{"t":null,"s":null,"op":10,"d":{"heartbeat_interval":41250}}"#)
                .unwrap();
        assert_eq!((hello.op, hello.s, hello.kind()), (10, None, None));

        let ack = Envelope::parse(r#"{"op":11}"#).unwrap();
        assert!(ack.d.is_none());

        let invalid = Envelope::parse(r#"{"t":null,"s":null,"op":9,"d":false}"#).unwrap();
        assert_eq!(invalid.d.unwrap().get(), "false");
    }

    #[test]
    fn routes_by_guild_then_channel() {
        let env = Envelope::parse(MESSAGE).unwrap();
        assert_eq!(route(env.kind(), env.d), Route::Guild(7));

        let dm = RawValue::from_string(r#"{"id":"5","channel_id":"3"}"#.into()).unwrap();
        assert_eq!(route(Some("MESSAGE_CREATE"), Some(&dm)), Route::Channel(3));

        let user = RawValue::from_string(r#"{"id":"5","username":"x"}"#.into()).unwrap();
        assert_eq!(route(Some("USER_UPDATE"), Some(&user)), Route::Unrouted);
        assert_eq!(route(Some("RESUMED"), None), Route::Unrouted);
    }

    #[test]
    fn guild_events_route_by_their_id() {
        let guild =
            RawValue::from_string(r#"{"id":"11","name":"g","unavailable":false}"#.into()).unwrap();
        for kind in ["GUILD_CREATE", "GUILD_UPDATE", "GUILD_DELETE"] {
            assert_eq!(route(Some(kind), Some(&guild)), Route::Guild(11));
        }
        // Other events never treat `id` as a guild id.
        assert_eq!(
            route(Some("GUILD_ROLE_CREATE"), Some(&guild)),
            Route::Unrouted
        );
    }

    #[test]
    fn dm_channel_events_route_with_the_channels_messages() {
        let dm =
            RawValue::from_string(r#"{"id":"3","type":1,"last_message_id":"5"}"#.into()).unwrap();
        for kind in ["CHANNEL_CREATE", "CHANNEL_UPDATE", "CHANNEL_DELETE"] {
            assert_eq!(route(Some(kind), Some(&dm)), Route::Channel(3), "{kind}");
        }
        let message = RawValue::from_string(r#"{"id":"5","channel_id":"3"}"#.into()).unwrap();
        assert_eq!(
            route(Some("MESSAGE_CREATE"), Some(&message)),
            Route::Channel(3)
        );
        let pins = RawValue::from_string(r#"{"channel_id":"3"}"#.into()).unwrap();
        assert_eq!(
            route(Some("CHANNEL_PINS_UPDATE"), Some(&pins)),
            Route::Channel(3)
        );

        // A guild's channels stay with the guild.
        let text = RawValue::from_string(r#"{"id":"3","guild_id":"7","type":0}"#.into()).unwrap();
        for kind in ["CHANNEL_CREATE", "CHANNEL_UPDATE", "CHANNEL_DELETE"] {
            assert_eq!(route(Some(kind), Some(&text)), Route::Guild(7), "{kind}");
        }

        // Without `id` a channel id still counts, as for every other event.
        assert_eq!(
            route(Some("CHANNEL_UPDATE"), Some(&pins)),
            Route::Channel(3)
        );
        let empty = RawValue::from_string("{}".into()).unwrap();
        assert_eq!(route(Some("CHANNEL_DELETE"), Some(&empty)), Route::Unrouted);
        // Only these three are channels themselves; other events' `id` is not one.
        let deleted = RawValue::from_string(r#"{"id":"3"}"#.into()).unwrap();
        assert_eq!(
            route(Some("MESSAGE_DELETE"), Some(&deleted)),
            Route::Unrouted
        );
        assert_eq!(
            route(Some("THREAD_DELETE"), Some(&deleted)),
            Route::Unrouted
        );
    }

    #[test]
    fn only_top_level_snowflakes_route() {
        let cases = [
            // A guild wins over a channel, and only top-level fields count.
            (
                Some("MESSAGE_CREATE"),
                r#"{"guild_id":"7","channel_id":"3"}"#,
                Route::Guild(7),
            ),
            (
                Some("MESSAGE_CREATE"),
                r#"{"channel_id":"3","message":{"guild_id":"9"}}"#,
                Route::Channel(3),
            ),
            // Ids that are not snowflakes count as missing.
            (
                Some("MESSAGE_CREATE"),
                r#"{"guild_id":"x","channel_id":"3"}"#,
                Route::Channel(3),
            ),
            (
                Some("TYPING_START"),
                r#"{"guild_id":"","channel_id":"-3"}"#,
                Route::Unrouted,
            ),
            // The guild events are routed by `id` only.
            (Some("GUILD_UPDATE"), r#"{"guild_id":"7"}"#, Route::Unrouted),
            // Without a dispatch type, `id` never counts.
            (None, r#"{"id":"1","guild_id":"7"}"#, Route::Guild(7)),
            (None, r#"{"id":"1"}"#, Route::Unrouted),
            // `d` that is not an object.
            (Some("MESSAGE_CREATE"), "null", Route::Unrouted),
            (Some("MESSAGE_CREATE"), r#""3""#, Route::Unrouted),
        ];
        for (kind, d, expected) in cases {
            let raw = RawValue::from_string(d.to_owned()).unwrap();
            assert_eq!(route(kind, Some(&raw)), expected, "{kind:?} {d}");
        }
    }

    #[test]
    fn unparseable_hints_are_unrouted_not_errors() {
        let weird = RawValue::from_string(r#"{"guild_id":12345}"#.into()).unwrap();
        assert_eq!(route(Some("X"), Some(&weird)), Route::Unrouted);
        let array = RawValue::from_string("[1,2]".into()).unwrap();
        assert_eq!(route(Some("X"), Some(&array)), Route::Unrouted);
    }

    #[test]
    fn same_guild_always_same_worker() {
        for workers in [1, 2, 7, 32] {
            assert_eq!(
                Route::Guild(123).worker(workers),
                Route::Guild(123).worker(workers)
            );
            assert!(Route::Guild(u64::MAX).worker(workers) < workers);
            assert_eq!(Route::Unrouted.worker(workers), 0);
        }
        assert_eq!(Route::Guild(5).worker(0), 0);
    }

    #[test]
    fn snowflakes_spread_over_workers() {
        // Real-looking guild ids: timestamps a few ms apart, worker/process 0,
        // increment 0, like most guild ids.
        let workers = 32;
        let mut counts = vec![0usize; workers];
        for i in 0..32_000u64 {
            let id = (1_420_070_400_000u64 + i * 7_919) << 22;
            counts[Route::Guild(id).worker(workers)] += 1;
        }
        let (min, max) = (counts.iter().min().unwrap(), counts.iter().max().unwrap());
        assert!(*min > 800 && *max < 1200, "uneven: {counts:?}");
    }

    #[test]
    fn outgoing_keeps_shape_and_raw_data() {
        let env = Envelope::parse(MESSAGE).unwrap();
        let bytes = outgoing(&env, None).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.starts_with(r#"{"op":0,"t":"MESSAGE_CREATE","d":{"id":"5""#));
        // `d` is passed through untouched, escapes included.
        assert!(text.contains(r#""content":"hi é""#));
        assert!(!text.contains("\"old\""));
        assert!(!text.contains("\"s\""));
    }

    #[test]
    fn outgoing_attaches_old() {
        let env = Envelope::parse(MESSAGE).unwrap();
        let old = json!({"id": "5", "content": "before"});
        let value: Value = serde_json::from_slice(&outgoing(&env, Some(&old)).unwrap()).unwrap();
        assert_eq!(value["old"], old);
        assert_eq!(value["d"]["content"], "hi é");
    }

    #[test]
    fn outgoing_without_data_sends_null() {
        let env = Envelope::parse(r#"{"op":0,"t":"X","s":1}"#).unwrap();
        let value: Value = serde_json::from_slice(&outgoing(&env, None).unwrap()).unwrap();
        assert_eq!(value, json!({"op": 0, "t": "X", "d": null}));
    }
}
