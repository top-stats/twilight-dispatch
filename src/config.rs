//! Configuration from environment variables.
//!
//! Existing deployments keep working unchanged: every variable that was read
//! before is still read with the same parsing rules (a value is parsed as JSON
//! first and then as a JSON string, so `STATUS=online`, `INTENTS=32767` and
//! `RESUME=true` all work). Differences: problems are reported as errors
//! instead of panics, empty optional values mean "unset", and a few variables
//! became optional or were added (see `.env.example`).

use serde::de::DeserializeOwned;
use std::{fmt, time::Duration};
use twilight_model::gateway::{
    presence::{ActivityType, Status},
    Intents,
};

/// Default of `PROMETHEUS_PORT`: the port of the metrics and `/healthcheck`
/// server, and the one `twilight-dispatch healthcheck` probes when it is unset.
pub const DEFAULT_PROMETHEUS_PORT: u16 = 8005;

/// The service's settings, read from environment variables by
/// [`Config::from_env`]. `.env.example` describes every variable; values that
/// are empty or only whitespace count as unset.
#[derive(Clone, Debug)]
pub struct Config {
    /// Discord bot token (`BOT_TOKEN`, required).
    pub bot_token: String,
    /// The shards this process runs (`SHARDS_START`, `SHARDS_END`,
    /// `SHARDS_TOTAL`). `None` (all three unset) means "use Discord's
    /// recommended shard count, all shards".
    pub shards: Option<ShardRange>,
    /// Upper bound for identify concurrency (`SHARDS_CONCURRENCY`); Discord's
    /// value is used if lower, and a value that does not divide it is lowered
    /// further (see `gateway::concurrency`).
    pub shards_concurrency: Option<u16>,
    /// Whether to declare the `gateway.recv` queue and bind it to every event
    /// (`DEFAULT_QUEUE`, default `true`).
    pub default_queue: bool,
    /// Publisher confirms (`AMQP_CONFIRMS`, default `false`): an event counts
    /// as published only once the broker confirmed it. Off by default, because
    /// with confirms each worker waits for one broker disk sync per event.
    pub amqp_confirms: bool,
    /// Whether shards resume the sessions saved in Redis by the previous run
    /// (`RESUME`, default `true`). Only when `SHARDS_TOTAL` is set and the
    /// same as in that run.
    pub resume: bool,
    /// Gateway intents sent in the identify (`INTENTS`, required, as bits).
    pub intents: Intents,
    /// Member count from which Discord sends a guild without its offline
    /// members (`LARGE_THRESHOLD`, 50 to 250, default 250).
    pub large_threshold: u64,
    /// Presence status (`STATUS`, default `online`).
    pub status: Status,
    /// Activity type (`ACTIVITY_TYPE`, default 0, playing).
    pub activity_type: ActivityType,
    /// Activity name (`ACTIVITY_NAME`, default empty).
    pub activity_name: String,
    /// Activity state (`ACTIVITY_STATE`). A custom status (type 4) is shown
    /// from the state, which then defaults to `activity_name`.
    pub activity_state: Option<String>,
    /// Stream URL of a streaming activity (`ACTIVITY_URL`, type 1).
    pub activity_url: Option<String>,
    /// Discord channel for shard status messages (`LOG_CHANNEL`; 0, the
    /// default, disables them).
    pub log_channel: u64,
    /// Discord channel for guild join and leave messages
    /// (`LOG_GUILD_CHANNEL`; 0, the default, disables them).
    pub log_guild_channel: u64,
    /// What the Redis state cache stores (the `STATE_*` variables).
    pub state: StateConfig,
    /// Broker URL: `AMQP_URL`, or one built from `RABBIT_HOST` (required
    /// then), `RABBIT_PORT` (5672), `RABBIT_USERNAME` and `RABBIT_PASSWORD`
    /// (`guest`) and `RABBIT_VHOST` (`/`), with `heartbeat=30`.
    pub amqp_url: String,
    /// Redis URL: `REDIS_URL`, or one built from `REDIS_HOST` (required then),
    /// `REDIS_PORT` (6379), `REDIS_USERNAME`, `REDIS_PASSWORD` and `REDIS_DB`
    /// (0).
    pub redis_url: String,
    /// IP address the metrics and health server binds (`PROMETHEUS_HOST`,
    /// default `0.0.0.0`; `localhost` and `ip6-localhost` are resolved).
    pub prometheus_host: String,
    /// Port of the metrics and health server (`PROMETHEUS_PORT`, default
    /// [`DEFAULT_PROMETHEUS_PORT`]).
    pub prometheus_port: u16,
    /// Number of ordered workers that cache and publish events (`WORKERS`, 1
    /// to 1024, default 32).
    pub workers: usize,
    /// Time limit for one cache update before the event is published without
    /// it (`CACHE_TIMEOUT_MS`, at least 100 ms, default 10 s).
    pub cache_timeout: Duration,
    /// A shard that has received nothing for this long is reported unhealthy
    /// (`SHARD_SILENCE_SECS`, at least 120 s, default 180 s).
    pub shard_silence: Duration,
    /// Send Discord REST requests to this proxy instead of discord.com
    /// (`DISCORD_API_PROXY`, e.g. twilight-http-proxy), given as
    /// `http://host:port` or `https://host:port`.
    pub discord_api_proxy: Option<DiscordProxy>,
    /// Connect shards to this gateway URL instead of
    /// `wss://gateway.discord.gg` (`DISCORD_GATEWAY_URL`, e.g. a gateway
    /// proxy). Resumes still go to Discord's resume URL. Trimmed and without
    /// a trailing `/`, as twilight appends `/?v=10&...`.
    pub discord_gateway_url: Option<String>,
}

/// A REST proxy address as twilight-http takes it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscordProxy {
    /// `host:port`, without the scheme.
    pub address: String,
    /// Whether to talk plain HTTP to it.
    pub use_http: bool,
}

impl DiscordProxy {
    fn parse(value: &str) -> Option<Self> {
        let (scheme, address) = value.trim().split_once("://")?;
        let use_http = match scheme {
            "http" => true,
            "https" => false,
            _ => return None,
        };
        let address = address.trim_end_matches('/');
        // twilight-http sends every request to `{scheme}://{address}/api/v10/...`,
        // so anything but `host[:port]` ends up in the middle of those URLs: a
        // path, query or fragment moves or swallows the API path (every
        // request fails, long after the start) and userinfo is ignored.
        if address.contains(|c: char| c.is_whitespace() || matches!(c, '/' | '?' | '#' | '@')) {
            return None;
        }
        let url = format!(
            "{scheme}://{address}/api/v{}/gateway/bot",
            twilight_http::API_VERSION
        );
        let uri: hyper::Uri = url.parse().ok()?;
        is_host_and_port(&uri).then(|| Self {
            address: address.to_owned(),
            use_http,
        })
    }
}

/// A `DISCORD_GATEWAY_URL` as twilight's `ConfigBuilder::proxy_url` takes it:
/// `ws://` or `wss://`, a host, an optional port and path, trimmed and
/// without a trailing `/`.
///
/// twilight appends `/?v=10&encoding=json&compress=zstd-stream` and
/// `expect`s the result to parse inside the shard's connect future, so a
/// value it cannot parse aborts the process (`panic = "abort"`). That can
/// happen hours after the start: resumed shards connect to Discord's resume
/// URL and only fall back to this one when a session is lost. A query or
/// fragment would swallow twilight's `v` and `encoding`, so both are
/// rejected too.
fn parse_gateway_url(value: &str) -> Option<String> {
    let url = value.trim().trim_end_matches('/');
    if url.contains(['?', '#']) {
        return None;
    }
    let uri: hyper::Uri = url.parse().ok()?;
    let twilight = format!(
        "{url}/?v={}&encoding=json&compress=zstd-stream",
        twilight_gateway::API_VERSION
    );
    (matches!(uri.scheme_str(), Some("ws" | "wss"))
        && is_host_and_port(&uri)
        && twilight.parse::<hyper::Uri>().is_ok())
    .then(|| url.to_owned())
}

/// Whether the authority of `uri` is a host with an optional 16-bit port and
/// nothing else. `http::Uri` also accepts an empty host, userinfo and ports
/// such as `99999` or `x`; twilight's clients ignore the userinfo and quietly
/// connect to the scheme's default port when the port does not parse.
fn is_host_and_port(uri: &hyper::Uri) -> bool {
    let Some(authority) = uri.authority() else {
        return false;
    };
    let host = authority.host();
    !host.is_empty()
        && match authority.as_str().strip_prefix(host) {
            Some("") => true,
            Some(port) => port
                .strip_prefix(':')
                .is_some_and(|port| port.parse::<u16>().is_ok()),
            // Userinfo before the host.
            None => false,
        }
}

/// `value` with everything between the scheme's `://` (or the start, when
/// there is none) and the last `@` replaced by `***`, so that a rejected URL
/// does not put its credentials into the logs, even ones with an unescaped
/// `/`, `?` or `#`. A value without an `@` is returned unchanged.
fn redact_userinfo(value: &str) -> String {
    let Some(at) = value.rfind('@') else {
        return value.to_owned();
    };
    let start = value[..at].find("://").map_or(0, |i| i + "://".len());
    format!("{}***{}", &value[..start], &value[at..])
}

/// The part of the configuration the state cache needs (the `STATE_*`
/// variables). In one from [`Config::from_lookup`] every flag is off when
/// `enabled` is.
#[allow(
    clippy::struct_excessive_bools,
    reason = "one independent switch per STATE_* variable; tests build it field by field"
)]
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StateConfig {
    /// Whether to keep a state cache in Redis at all (`STATE_ENABLED`,
    /// default `true`). Without it events are published without `old`.
    pub enabled: bool,
    /// Cache guild members (`STATE_MEMBER`, default `false`); the bot's own
    /// member is cached either way.
    pub member: bool,
    /// Milliseconds after which a cached member expires; 0 means never
    /// (`STATE_MEMBER_TTL`, default 0).
    pub member_ttl: u64,
    /// Cache messages (`STATE_MESSAGE`, default `false`).
    pub message: bool,
    /// Milliseconds after which a cached message expires; 0 means never
    /// (`STATE_MESSAGE_TTL`, default 0).
    pub message_ttl: u64,
    /// Cache presences (`STATE_PRESENCE`, default `false`).
    pub presence: bool,
    /// Cache emojis (`STATE_EMOJI`, default `false`).
    pub emoji: bool,
    /// Cache voice states (`STATE_VOICE`, default `false`).
    pub voice: bool,
    /// Attach the previous cached value as `old` to published events
    /// (`STATE_OLD`, default `false`).
    pub old: bool,
}

/// The shards one process runs: `start..=end` of `total`. A range from
/// [`Config::from_lookup`] is never empty: `start <= end < total`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShardRange {
    /// First shard id (`SHARDS_START`, default 0).
    pub start: u32,
    /// Last shard id, inclusive (`SHARDS_END`, default `total - 1`).
    pub end: u32,
    /// Shard count of the whole bot, across all its processes
    /// (`SHARDS_TOTAL`).
    pub total: u32,
}

impl ShardRange {
    /// The shard ids, `start..=end`.
    pub fn ids(self) -> std::ops::RangeInclusive<u32> {
        self.start..=self.end
    }

    /// The number of shards, `end - start + 1`. Assumes `start <= end`, which
    /// holds for every range [`Config::from_lookup`] returns.
    #[allow(
        clippy::len_without_is_empty,
        reason = "a range always has at least one shard"
    )]
    pub fn len(self) -> u32 {
        self.end - self.start + 1
    }

    /// Whether `shard` is one of the range's shards.
    pub fn contains(self, shard: u32) -> bool {
        self.ids().contains(&shard)
    }
}

/// Why the configuration cannot be used. `Display` names the variable, so
/// the error can be logged as it is.
#[derive(Debug, Eq, PartialEq)]
pub enum ConfigError {
    /// This required variable is unset or blank.
    Missing(&'static str),
    /// A variable has a value that cannot be used.
    Invalid {
        /// The variable.
        name: &'static str,
        /// The value as set, except that the checks of a parsed number report
        /// that number (for `SHARDS_END`, the effective last shard) and that
        /// the credentials of a rejected `DISCORD_API_PROXY` or
        /// `DISCORD_GATEWAY_URL` are replaced by `***`.
        value: String,
        /// What is wrong with it, or which values are accepted.
        reason: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing(name) => write!(f, "missing environment variable {name}"),
            Self::Invalid {
                name,
                value,
                reason,
            } => {
                write!(f, "invalid environment variable {name}={value:?}: {reason}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Reads one environment variable: its value, or `None` when it is unset.
type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

impl Config {
    /// Reads the configuration from the process environment (into which
    /// `main` loads `.env` first).
    ///
    /// # Errors
    ///
    /// The first problem found: [`ConfigError::Missing`] when a required
    /// variable is unset or blank, [`ConfigError::Invalid`] when a value
    /// cannot be parsed or is out of range.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(&|name| std::env::var(name).ok())
    }

    /// Reads the configuration through `lookup`, which returns the value of a
    /// variable or `None` when it is unset (tests pass a map).
    ///
    /// # Errors
    ///
    /// As [`Config::from_env`].
    pub fn from_lookup(lookup: Lookup<'_>) -> Result<Self, ConfigError> {
        let env = Env(lookup);

        let shards = env.shards()?;
        let intents = env.intents()?;
        let large_threshold = env.large_threshold()?;
        let state = env.state()?;
        let amqp_url = env.amqp_url()?;
        let redis_url = env.redis_url()?;

        let workers: usize = env.optional("WORKERS")?.unwrap_or(32);
        if !(1..=1024).contains(&workers) {
            return Err(ConfigError::Invalid {
                name: "WORKERS",
                value: workers.to_string(),
                reason: "need 1 to 1024 workers".into(),
            });
        }
        let cache_timeout_ms: u64 = env.optional("CACHE_TIMEOUT_MS")?.unwrap_or(10_000);
        if cache_timeout_ms < 100 {
            return Err(ConfigError::Invalid {
                name: "CACHE_TIMEOUT_MS",
                value: cache_timeout_ms.to_string(),
                reason: "need at least 100 ms".into(),
            });
        }
        // Discord heartbeats every ~41 s; anything shorter than a few
        // heartbeats would report healthy shards as stuck.
        let shard_silence_secs: u64 = env.optional("SHARD_SILENCE_SECS")?.unwrap_or(180);
        if shard_silence_secs < 120 {
            return Err(ConfigError::Invalid {
                name: "SHARD_SILENCE_SECS",
                value: shard_silence_secs.to_string(),
                reason: "need at least 120 seconds".into(),
            });
        }
        let prometheus_host = env.prometheus_host()?;

        Ok(Self {
            bot_token: env.required_string("BOT_TOKEN")?,
            shards,
            shards_concurrency: env.optional("SHARDS_CONCURRENCY")?,
            default_queue: env.optional("DEFAULT_QUEUE")?.unwrap_or(true),
            amqp_confirms: env.optional("AMQP_CONFIRMS")?.unwrap_or(false),
            resume: env.optional("RESUME")?.unwrap_or(true),
            intents,
            large_threshold,
            status: env.optional("STATUS")?.unwrap_or(Status::Online),
            activity_type: env
                .optional("ACTIVITY_TYPE")?
                .unwrap_or(ActivityType::Playing),
            activity_name: env.string("ACTIVITY_NAME").unwrap_or_default(),
            activity_state: env.string("ACTIVITY_STATE"),
            activity_url: env.string("ACTIVITY_URL"),
            log_channel: env.optional("LOG_CHANNEL")?.unwrap_or(0),
            log_guild_channel: env.optional("LOG_GUILD_CHANNEL")?.unwrap_or(0),
            state,
            amqp_url,
            redis_url,
            prometheus_host,
            prometheus_port: env
                .optional("PROMETHEUS_PORT")?
                .unwrap_or(DEFAULT_PROMETHEUS_PORT),
            workers,
            cache_timeout: Duration::from_millis(cache_timeout_ms),
            shard_silence: Duration::from_secs(shard_silence_secs),
            discord_api_proxy: match env.string("DISCORD_API_PROXY") {
                Some(value) => Some(DiscordProxy::parse(&value).ok_or_else(|| {
                    ConfigError::Invalid {
                        name: "DISCORD_API_PROXY",
                        value: redact_userinfo(&value),
                        reason: "expected http://host:port or https://host:port \
                                 (no path, query or credentials)"
                            .into(),
                    }
                })?),
                None => None,
            },
            discord_gateway_url: match env.string("DISCORD_GATEWAY_URL") {
                Some(value) => Some(parse_gateway_url(&value).ok_or_else(|| {
                    ConfigError::Invalid {
                        name: "DISCORD_GATEWAY_URL",
                        value: redact_userinfo(&value),
                        reason: "expected ws:// or wss:// with a host, an optional port and \
                                 path, and no query, fragment or credentials"
                            .into(),
                    }
                })?),
                None => None,
            },
        })
    }

    /// Variables that older versions required but that no longer do anything,
    /// among those `lookup` finds set to a non-blank value.
    pub fn obsolete_variables(lookup: Lookup<'_>) -> Vec<&'static str> {
        OBSOLETE_VARIABLES
            .iter()
            .copied()
            .filter(|name| lookup(name).is_some_and(|v| !v.trim().is_empty()))
            .collect()
    }
}

/// Reported at startup when set, so deployments learn they can drop them.
const OBSOLETE_VARIABLES: &[&str] = &["CLUSTERS", "SHARDS_WAIT"];

/// The variables, read through a `Lookup`.
struct Env<'a>(Lookup<'a>);

impl Env<'_> {
    /// The value of `name`, or `None` when it is unset or blank. Not trimmed.
    fn string(&self, name: &str) -> Option<String> {
        (self.0)(name).filter(|value| !value.trim().is_empty())
    }

    /// `name` as `string` returns it, or `Missing` when it is unset or blank.
    fn required_string(&self, name: &'static str) -> Result<String, ConfigError> {
        self.string(name).ok_or(ConfigError::Missing(name))
    }

    /// `name` parsed with `parse_value`, or `None` when it is unset or blank.
    fn optional<T: DeserializeOwned>(&self, name: &'static str) -> Result<Option<T>, ConfigError> {
        self.string(name)
            .map(|value| {
                parse_value(&value).ok_or_else(|| ConfigError::Invalid {
                    name,
                    value: value.clone(),
                    reason: format!("cannot parse as {}", std::any::type_name::<T>()),
                })
            })
            .transpose()
    }

    /// `name` parsed with `parse_value`, or `Missing` when it is unset or
    /// blank.
    fn required<T: DeserializeOwned>(&self, name: &'static str) -> Result<T, ConfigError> {
        self.optional(name)?.ok_or(ConfigError::Missing(name))
    }

    /// `SHARDS_START`, `SHARDS_END` and `SHARDS_TOTAL`: `None` when all three
    /// are unset. `SHARDS_START` defaults to the first shard and `SHARDS_END`
    /// to the last; `SHARDS_TOTAL` is required once either is set.
    fn shards(&self) -> Result<Option<ShardRange>, ConfigError> {
        match (
            self.optional::<u32>("SHARDS_START")?,
            self.optional::<u32>("SHARDS_END")?,
            self.optional::<u32>("SHARDS_TOTAL")?,
        ) {
            (None, None, None) => Ok(None),
            (start, end, Some(total)) => {
                let start = start.unwrap_or(0);
                let end = end.unwrap_or(total.saturating_sub(1));
                if total == 0 || start > end || end >= total {
                    return Err(ConfigError::Invalid {
                        name: "SHARDS_END",
                        value: end.to_string(),
                        reason: format!(
                            "need SHARDS_START <= SHARDS_END < SHARDS_TOTAL (got {start}..={end} of {total})"
                        ),
                    });
                }
                Ok(Some(ShardRange { start, end, total }))
            }
            _ => Err(ConfigError::Missing("SHARDS_TOTAL")),
        }
    }

    /// `INTENTS`, which must not have bits of unknown intents.
    fn intents(&self) -> Result<Intents, ConfigError> {
        let intents_bits: u64 = self.required("INTENTS")?;
        Intents::from_bits(intents_bits).ok_or_else(|| ConfigError::Invalid {
            name: "INTENTS",
            value: intents_bits.to_string(),
            reason: format!(
                "unknown intent bits {:#x}",
                intents_bits & !Intents::all().bits()
            ),
        })
    }

    /// `LARGE_THRESHOLD` (default 250), which Discord accepts from 50 to 250.
    fn large_threshold(&self) -> Result<u64, ConfigError> {
        let large_threshold: u64 = self.optional("LARGE_THRESHOLD")?.unwrap_or(250);
        if !(50..=250).contains(&large_threshold) {
            return Err(ConfigError::Invalid {
                name: "LARGE_THRESHOLD",
                value: large_threshold.to_string(),
                reason: "Discord accepts 50 to 250".into(),
            });
        }
        Ok(large_threshold)
    }

    /// The `STATE_*` variables. The flags are only read when `STATE_ENABLED`
    /// is on, so a bad value in one of them is no error while it is off.
    fn state(&self) -> Result<StateConfig, ConfigError> {
        let enabled = self.optional("STATE_ENABLED")?.unwrap_or(true);
        Ok(StateConfig {
            enabled,
            member: enabled && self.optional("STATE_MEMBER")?.unwrap_or(false),
            member_ttl: self.optional("STATE_MEMBER_TTL")?.unwrap_or(0),
            message: enabled && self.optional("STATE_MESSAGE")?.unwrap_or(false),
            message_ttl: self.optional("STATE_MESSAGE_TTL")?.unwrap_or(0),
            presence: enabled && self.optional("STATE_PRESENCE")?.unwrap_or(false),
            emoji: enabled && self.optional("STATE_EMOJI")?.unwrap_or(false),
            voice: enabled && self.optional("STATE_VOICE")?.unwrap_or(false),
            old: enabled && self.optional("STATE_OLD")?.unwrap_or(false),
        })
    }

    /// `AMQP_URL`, or a URL built from the `RABBIT_*` variables with their
    /// parts percent-encoded.
    fn amqp_url(&self) -> Result<String, ConfigError> {
        if let Some(url) = self.string("AMQP_URL") {
            return Ok(url);
        }
        let virtual_host = self.string("RABBIT_VHOST");
        let username = self.string("RABBIT_USERNAME");
        let password = self.string("RABBIT_PASSWORD");
        let host = self.required_string("RABBIT_HOST")?;
        let port = self.optional::<u16>("RABBIT_PORT")?.unwrap_or(5672);
        // heartbeat=30: LavinMQ's default (300 s) takes 10 minutes to notice a
        // dead connection.
        Ok(format!(
            "amqp://{}:{}@{host}:{port}/{}?heartbeat=30",
            encode(username.as_deref().unwrap_or("guest")),
            encode(password.as_deref().unwrap_or("guest")),
            encode(virtual_host.as_deref().unwrap_or("/")),
        ))
    }

    /// `REDIS_URL`, or a URL built from the `REDIS_*` variables with the
    /// credentials percent-encoded.
    fn redis_url(&self) -> Result<String, ConfigError> {
        if let Some(url) = self.string("REDIS_URL") {
            return Ok(url);
        }
        let username = self.string("REDIS_USERNAME");
        let password = self.string("REDIS_PASSWORD");
        let auth = match (username, password) {
            (None, None) => String::new(),
            (username, password) => format!(
                "{}:{}@",
                encode(&username.unwrap_or_default()),
                encode(&password.unwrap_or_default())
            ),
        };
        let host = self.required_string("REDIS_HOST")?;
        let port = self.optional::<u16>("REDIS_PORT")?.unwrap_or(6379);
        let db = self.optional::<u32>("REDIS_DB")?.unwrap_or(0);
        Ok(format!("redis://{auth}{host}:{port}/{db}"))
    }

    /// `PROMETHEUS_HOST` (default `0.0.0.0`) as an IP address, for binding.
    fn prometheus_host(&self) -> Result<String, ConfigError> {
        let configured = self.string("PROMETHEUS_HOST");
        let host = configured.as_deref().unwrap_or("0.0.0.0");
        match parse_host(host) {
            Some(ip) => Ok(ip.to_string()),
            None => Err(ConfigError::Invalid {
                name: "PROMETHEUS_HOST",
                value: host.to_owned(),
                reason: "must be an IP address or localhost".into(),
            }),
        }
    }
}

/// An IP address, or `localhost` (accepted by earlier versions' examples).
pub fn parse_host(value: &str) -> Option<std::net::IpAddr> {
    let value = value.trim();
    match value {
        "localhost" => Some(std::net::Ipv4Addr::LOCALHOST.into()),
        "ip6-localhost" => Some(std::net::Ipv6Addr::LOCALHOST.into()),
        _ => value.parse().ok(),
    }
}

/// Parse a value as JSON, falling back to treating it as a JSON string.
pub fn parse_value<T: DeserializeOwned>(value: &str) -> Option<T> {
    let value = value.trim();
    serde_json::from_str(value)
        .ok()
        .or_else(|| serde_json::from_value(serde_json::Value::String(value.to_owned())).ok())
}

/// Percent-encode a URL userinfo or path segment. Existing `%XX` escapes are
/// kept: earlier versions put credentials into the URL verbatim, so a
/// deployment may already have encoded them itself.
fn encode(value: &str) -> String {
    use std::fmt::Write as _;

    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    for (i, &byte) in bytes.iter().enumerate() {
        // `%XX`, an escape already there.
        let escape = byte == b'%'
            && bytes.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
            && bytes.get(i + 2).is_some_and(u8::is_ascii_hexdigit);
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') || escape {
            out.push(char::from(byte));
        } else {
            // Writing to a `String` cannot fail.
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{
        cell::RefCell,
        collections::{BTreeSet, HashMap},
    };

    const ENV_EXAMPLE: &str = include_str!("../.env.example");

    /// The variables of `.env.example`, which every deployment started from.
    pub(crate) fn example_env() -> HashMap<&'static str, String> {
        [
            ("RUST_LOG", "info"),
            ("BOT_TOKEN", "token"),
            ("SHARDS_START", "0"),
            ("SHARDS_END", "1"),
            ("SHARDS_TOTAL", "2"),
            ("SHARDS_CONCURRENCY", "1"),
            ("SHARDS_WAIT", "6"),
            ("CLUSTERS", "2"),
            ("DEFAULT_QUEUE", "true"),
            ("RESUME", "true"),
            ("INTENTS", "32767"),
            ("LARGE_THRESHOLD", "250"),
            ("STATUS", "online"),
            ("ACTIVITY_TYPE", "0"),
            ("ACTIVITY_NAME", "Testing"),
            ("LOG_CHANNEL", ""),
            ("LOG_GUILD_CHANNEL", ""),
            ("STATE_ENABLED", "true"),
            ("STATE_MEMBER", "true"),
            ("STATE_MEMBER_TTL", "60000"),
            ("STATE_MESSAGE", "true"),
            ("STATE_MESSAGE_TTL", "60000"),
            ("STATE_PRESENCE", "true"),
            ("STATE_EMOJI", "true"),
            ("STATE_VOICE", "true"),
            ("STATE_OLD", "false"),
            ("RABBIT_HOST", "127.0.0.1"),
            ("RABBIT_PORT", "5672"),
            ("RABBIT_USERNAME", "guest"),
            ("RABBIT_PASSWORD", "guest"),
            ("REDIS_HOST", "127.0.0.1"),
            ("REDIS_PORT", "6379"),
            ("PROMETHEUS_HOST", "127.0.0.1"),
            ("PROMETHEUS_PORT", "8005"),
        ]
        .into_iter()
        .map(|(k, v)| (k, v.to_owned()))
        .collect()
    }

    fn load(env: &HashMap<&'static str, String>) -> Result<Config, ConfigError> {
        Config::from_lookup(&|name| env.get(name).cloned())
    }

    /// `example_env()` with `changes` applied (an empty value means unset).
    fn with(changes: &[(&'static str, &str)]) -> Result<Config, ConfigError> {
        let mut env = example_env();
        env.extend(
            changes
                .iter()
                .map(|&(name, value)| (name, value.to_owned())),
        );
        load(&env)
    }

    /// The uncommented `NAME=value` lines of the shipped `.env.example`.
    fn shipped_env() -> HashMap<&'static str, String> {
        ENV_EXAMPLE
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .filter_map(|line| line.split_once('='))
            .map(|(k, v)| (k.trim(), v.trim().to_owned()))
            .collect()
    }

    fn assert_invalid(name: &str, changes: &[(&'static str, &str)]) {
        let result = with(changes);
        assert!(
            matches!(&result, Err(ConfigError::Invalid { name: n, .. }) if *n == name),
            "{changes:?} -> {result:?}"
        );
    }

    #[test]
    fn example_env_still_loads() {
        let config = load(&example_env()).unwrap();
        assert_eq!(
            config.shards,
            Some(ShardRange {
                start: 0,
                end: 1,
                total: 2
            })
        );
        assert_eq!(config.intents.bits(), 32767);
        assert_eq!(config.status, Status::Online);
        assert_eq!(config.activity_type, ActivityType::Playing);
        assert_eq!(config.log_channel, 0);
        assert_eq!(
            config.amqp_url,
            "amqp://guest:guest@127.0.0.1:5672/%2F?heartbeat=30"
        );
        assert_eq!(config.redis_url, "redis://127.0.0.1:6379/0");
        assert!(config.state.member && config.state.presence && !config.state.old);
        assert_eq!(config.state.member_ttl, 60_000);
        assert_eq!(config.prometheus_port, 8005);
    }

    #[test]
    fn obsolete_variables_are_reported() {
        let env = example_env();
        assert_eq!(
            Config::obsolete_variables(&|name| env.get(name).cloned()),
            vec!["CLUSTERS", "SHARDS_WAIT"]
        );
    }

    #[test]
    fn new_intents_are_accepted_and_unknown_bits_rejected() {
        let mut env = example_env();
        env.insert("INTENTS", ((1u64 << 24) | (1 << 25) | 1).to_string());
        assert!(load(&env).is_ok());

        env.insert("INTENTS", (1u64 << 40).to_string());
        assert!(matches!(
            load(&env),
            Err(ConfigError::Invalid {
                name: "INTENTS",
                ..
            })
        ));
    }

    #[test]
    fn large_threshold_is_validated() {
        let mut env = example_env();
        env.insert("LARGE_THRESHOLD", "49".into());
        assert!(matches!(
            load(&env),
            Err(ConfigError::Invalid {
                name: "LARGE_THRESHOLD",
                ..
            })
        ));
    }

    #[test]
    fn shard_range_is_validated() {
        let mut env = example_env();
        env.insert("SHARDS_END", "2".into());
        assert!(load(&env).is_err());

        let mut env = example_env();
        for name in ["SHARDS_START", "SHARDS_END", "SHARDS_TOTAL"] {
            env.remove(name);
        }
        assert_eq!(load(&env).unwrap().shards, None);

        let mut env = example_env();
        env.remove("SHARDS_TOTAL");
        assert_eq!(
            load(&env).unwrap_err(),
            ConfigError::Missing("SHARDS_TOTAL")
        );

        // A range that ends before it starts would make ShardRange::len
        // underflow; SHARDS_START alone can do that too (END defaults to the
        // last shard).
        for (start, end, total) in [("3", "1", "4"), ("2", "", "2"), ("0", "0", "0")] {
            assert_invalid(
                "SHARDS_END",
                &[
                    ("SHARDS_START", start),
                    ("SHARDS_END", end),
                    ("SHARDS_TOTAL", total),
                ],
            );
        }
    }

    /// One shard per process (e.g. per Swarm replica).
    #[test]
    fn a_process_may_run_a_single_shard() {
        let config = with(&[
            ("SHARDS_START", "1"),
            ("SHARDS_END", "1"),
            ("SHARDS_TOTAL", "2"),
        ])
        .unwrap();
        let range = config.shards.unwrap();
        assert_eq!(
            range,
            ShardRange {
                start: 1,
                end: 1,
                total: 2
            }
        );
        assert_eq!(range.len(), 1);
        assert_eq!(range.ids().collect::<Vec<_>>(), [1]);
        assert!(range.contains(1) && !range.contains(0) && !range.contains(2));

        let range = with(&[
            ("SHARDS_START", ""),
            ("SHARDS_END", ""),
            ("SHARDS_TOTAL", "1"),
        ])
        .unwrap()
        .shards
        .unwrap();
        assert_eq!((range.start, range.end, range.len()), (0, 0, 1));
    }

    #[test]
    fn shard_range_len_and_contains_agree_with_ids() {
        let range = ShardRange {
            start: 2,
            end: 5,
            total: 8,
        };
        assert_eq!(range.len(), 4);
        assert_eq!(range.ids().collect::<Vec<_>>(), [2, 3, 4, 5]);
        assert!(!range.contains(1) && range.contains(2) && range.contains(5));
        assert!(!range.contains(6) && !range.contains(u32::MAX));

        let widest = ShardRange {
            start: 0,
            end: u32::MAX - 1,
            total: u32::MAX,
        };
        assert_eq!(widest.len(), u32::MAX);
        assert!(widest.contains(0) && widest.contains(u32::MAX - 1));
    }

    #[test]
    fn pre_encoded_credentials_are_not_encoded_twice() {
        let mut env = example_env();
        env.insert("RABBIT_PASSWORD", "p%40ss".into());
        let config = load(&env).unwrap();
        assert_eq!(
            config.amqp_url,
            "amqp://guest:p%40ss@127.0.0.1:5672/%2F?heartbeat=30"
        );
        // A lone % is still escaped.
        env.insert("RABBIT_PASSWORD", "100%".into());
        assert!(load(&env).unwrap().amqp_url.contains(":100%25@"));
    }

    #[test]
    fn tuning_values_have_lower_bounds() {
        for (name, value) in [
            ("SHARD_SILENCE_SECS", "5"),
            ("CACHE_TIMEOUT_MS", "0"),
            ("WORKERS", "0"),
            ("PROMETHEUS_HOST", "metrics.internal"),
        ] {
            let mut env = example_env();
            env.insert(name, value.into());
            assert!(
                matches!(load(&env), Err(ConfigError::Invalid { name: n, .. }) if n == name),
                "{name}={value}"
            );
        }
    }

    /// The documented limits are inclusive on both sides.
    #[test]
    fn tuning_bounds_are_inclusive() {
        for (name, accepted, rejected) in [
            ("CACHE_TIMEOUT_MS", "100", "99"),
            ("SHARD_SILENCE_SECS", "120", "119"),
            ("WORKERS", "1", "0"),
            ("WORKERS", "1024", "1025"),
            ("LARGE_THRESHOLD", "50", "49"),
            ("LARGE_THRESHOLD", "250", "251"),
        ] {
            let result = with(&[(name, accepted)]);
            assert!(result.is_ok(), "{name}={accepted}: {result:?}");
            assert_invalid(name, &[(name, rejected)]);
        }
        let config = with(&[
            ("CACHE_TIMEOUT_MS", "100"),
            ("SHARD_SILENCE_SECS", "120"),
            ("WORKERS", "1024"),
            ("LARGE_THRESHOLD", "50"),
        ])
        .unwrap();
        assert_eq!(config.cache_timeout, Duration::from_millis(100));
        assert_eq!(config.shard_silence, Duration::from_secs(120));
        assert_eq!((config.workers, config.large_threshold), (1024, 50));
    }

    /// The metrics server binds PROMETHEUS_HOST, so it is resolved here; the
    /// loopback names that /etc/hosts usually has are accepted too.
    #[test]
    fn prometheus_host_is_an_ip_address_or_a_loopback_name() {
        for (value, expected) in [
            ("localhost", "127.0.0.1"),
            ("ip6-localhost", "::1"),
            (" 10.0.0.5 ", "10.0.0.5"),
            ("::", "::"),
        ] {
            let config = with(&[("PROMETHEUS_HOST", value)]).unwrap();
            assert_eq!(config.prometheus_host, expected, "{value:?}");
        }
        assert_eq!(
            with(&[("PROMETHEUS_HOST", "")]).unwrap().prometheus_host,
            "0.0.0.0"
        );
        assert_invalid("PROMETHEUS_HOST", &[("PROMETHEUS_HOST", "10.0.0.256")]);
    }

    #[test]
    fn shipped_env_example_loads() {
        let mut env = shipped_env();
        env.insert("BOT_TOKEN", "token".into());
        let config = load(&env).unwrap();
        assert_eq!(
            config.shards,
            Some(ShardRange {
                start: 0,
                end: 1,
                total: 2
            })
        );
        assert_eq!(config.workers, 32);
    }

    #[test]
    fn discord_endpoints_can_be_overridden() {
        let mut env = example_env();
        let config = load(&env).unwrap();
        assert_eq!(config.discord_api_proxy, None);
        assert_eq!(config.discord_gateway_url, None);

        env.insert("DISCORD_API_PROXY", "http://proxy:3000/".into());
        env.insert("DISCORD_GATEWAY_URL", "ws://gateway:8080/".into());
        let config = load(&env).unwrap();
        assert_eq!(
            config.discord_api_proxy,
            Some(DiscordProxy {
                address: "proxy:3000".into(),
                use_http: true
            })
        );
        assert_eq!(
            config.discord_gateway_url.as_deref(),
            Some("ws://gateway:8080")
        );

        env.insert("DISCORD_API_PROXY", "https://proxy".into());
        assert!(!load(&env).unwrap().discord_api_proxy.unwrap().use_http);

        for (name, value) in [
            ("DISCORD_API_PROXY", "proxy:3000"),
            ("DISCORD_API_PROXY", "ftp://proxy"),
            ("DISCORD_API_PROXY", "http://proxy/path"),
            ("DISCORD_GATEWAY_URL", "http://gateway"),
        ] {
            let mut env = example_env();
            env.insert(name, value.into());
            assert!(
                matches!(load(&env), Err(ConfigError::Invalid { name: n, .. }) if n == name),
                "{name}={value}"
            );
        }
    }

    /// twilight appends `/?v=10&encoding=json&compress=zstd-stream` to the
    /// gateway URL and panics (aborting the process) if that does not parse.
    #[test]
    fn gateway_url_is_a_uri_twilight_can_connect_to() {
        for (value, expected) in [
            ("ws://gateway:8080", "ws://gateway:8080"),
            ("wss://gateway.discord.gg/", "wss://gateway.discord.gg"),
            // Docker and Compose do not trim values (dotenvy does).
            ("ws://gateway:8080 ", "ws://gateway:8080"),
            (" \tws://127.0.0.1:9000//\n", "ws://127.0.0.1:9000"),
            ("ws://proxy:8080/gateway/", "ws://proxy:8080/gateway"),
            ("ws://[::1]:7878/a/b", "ws://[::1]:7878/a/b"),
        ] {
            let config = with(&[("DISCORD_GATEWAY_URL", value)]).unwrap();
            let url = config.discord_gateway_url.unwrap();
            assert_eq!(url, expected, "{value:?}");
            let uri: hyper::Uri = format!("{url}/?v=10&encoding=json&compress=zstd-stream")
                .parse()
                .unwrap();
            assert_eq!(uri.query(), Some("v=10&encoding=json&compress=zstd-stream"));
        }
        for value in [
            "ws://",
            "wss://",
            "ws:///",
            "ws:",
            "ws://gate way:8080",
            "ws://gateway:8080 x",
            "ws://gatewäy",
            "ws://:8080",
            // http::Uri accepts these ports; the client would use port 80.
            "ws://gateway:99999",
            "ws://gateway:port",
            "ws://gateway:",
            "ws://user:pass@gateway",
            "WSS://gateway.discord.gg",
            "http://gateway",
            "gateway:8080",
            // Discord's documented URL: twilight adds its own query.
            "wss://gateway.discord.gg/?v=10&encoding=json",
            "ws://gateway?x",
            "ws://gateway/#fragment",
        ] {
            assert_invalid("DISCORD_GATEWAY_URL", &[("DISCORD_GATEWAY_URL", value)]);
        }
        // Parses on its own, but not once twilight appends its query
        // (http::Uri's length limit), which would panic in the connect.
        let long = format!("ws://gw/{}", "a".repeat(65_500 - "ws://gw/".len()));
        assert!(long.parse::<hyper::Uri>().is_ok());
        assert_invalid(
            "DISCORD_GATEWAY_URL",
            &[("DISCORD_GATEWAY_URL", long.as_str())],
        );
        assert_eq!(
            with(&[("DISCORD_GATEWAY_URL", " ")])
                .unwrap()
                .discord_gateway_url,
            None
        );
    }

    /// twilight-http requests `{scheme}://{address}/api/v10/...`.
    #[test]
    fn api_proxy_is_a_host_and_port() {
        for (value, address, use_http) in [
            ("http://proxy:3000", "proxy:3000", true),
            (" https://proxy/ ", "proxy", false),
            ("http://[::1]:3000", "[::1]:3000", true),
        ] {
            let proxy = with(&[("DISCORD_API_PROXY", value)])
                .unwrap()
                .discord_api_proxy
                .unwrap();
            assert_eq!(
                proxy,
                DiscordProxy {
                    address: address.into(),
                    use_http
                },
                "{value:?}"
            );
        }
        for value in [
            "http://",
            "http:///",
            "http://:3000",
            "http://pro xy:3000",
            "http://proxy:3000\tx",
            "http://proxy:3000?x=1",
            "http://proxy:3000#x",
            "http://user:pass@proxy:3000",
            "http://@proxy",
            "http://proxy:99999",
            "http://proxy:",
            "http://proxy/api",
        ] {
            assert_invalid("DISCORD_API_PROXY", &[("DISCORD_API_PROXY", value)]);
        }
    }

    /// Every variable `Config::from_lookup` asks for, on variants of the
    /// example that between them take every branch: URLs or their parts,
    /// state on and off, every optional value set and the SHARDS_* unset.
    fn variables_read() -> BTreeSet<String> {
        let variants: [&[(&'static str, &str)]; 4] = [
            &[],
            &[
                ("AMQP_URL", "amqp://mq"),
                ("REDIS_URL", "redis://cache"),
                ("STATE_ENABLED", "false"),
            ],
            &[
                ("RABBIT_VHOST", "vhost"),
                ("REDIS_USERNAME", "user"),
                ("REDIS_PASSWORD", "pass"),
                ("REDIS_DB", "1"),
                ("ACTIVITY_TYPE", "1"),
                ("ACTIVITY_STATE", "state"),
                ("ACTIVITY_URL", "https://twitch.tv/discord"),
                ("LOG_CHANNEL", "1"),
                ("LOG_GUILD_CHANNEL", "2"),
                ("DISCORD_API_PROXY", "http://proxy:3000"),
                ("DISCORD_GATEWAY_URL", "ws://gateway:8080"),
            ],
            &[
                ("SHARDS_START", ""),
                ("SHARDS_END", ""),
                ("SHARDS_TOTAL", ""),
            ],
        ];
        let seen = RefCell::new(BTreeSet::new());
        for variant in variants {
            let mut env = shipped_env();
            env.insert("BOT_TOKEN", "token".into());
            env.extend(
                variant
                    .iter()
                    .map(|&(name, value)| (name, value.to_owned())),
            );
            // A failed load stops at the first error and asks for less.
            let config = Config::from_lookup(&|name| {
                seen.borrow_mut().insert(name.to_owned());
                env.get(name).cloned()
            });
            assert!(config.is_ok(), "{variant:?}: {config:?}");
        }
        seen.into_inner()
    }

    /// The `NAME=value` and `# NAME=value` lines of `.env.example`.
    fn env_example_names() -> BTreeSet<&'static str> {
        ENV_EXAMPLE
            .lines()
            .filter_map(|line| line.trim_start_matches(['#', ' ']).split_once('='))
            .map(|(name, _)| name)
            .filter(|name| {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
            })
            .collect()
    }

    /// Whether `text` contains `name` as a whole word.
    fn mentions(text: &str, name: &str) -> bool {
        let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
        text.match_indices(name).any(|(at, _)| {
            !text[..at].ends_with(word) && !text[at + name.len()..].starts_with(word)
        })
    }

    /// `.env.example` is where operators look for settings (README points
    /// there), so a new variable must be documented in it.
    #[test]
    fn every_variable_the_config_reads_is_in_env_example() {
        let undocumented: Vec<String> = variables_read()
            .into_iter()
            .filter(|name| !mentions(ENV_EXAMPLE, name))
            .filter(|name| !OBSOLETE_VARIABLES.contains(&name.as_str()))
            .collect();
        assert!(
            undocumented.is_empty(),
            "read by Config::from_lookup but not in .env.example: {undocumented:?}"
        );
    }

    /// A variable that is no longer read leaves .env.example, or goes into
    /// OBSOLETE_VARIABLES so that deployments still setting it are told.
    #[test]
    fn every_variable_in_env_example_is_read() {
        // Read by tracing-subscriber's EnvFilter in main.rs.
        let external = ["RUST_LOG"];
        let read = variables_read();
        let names = env_example_names();
        // Commented-out entries count too.
        assert!(names.contains("DISCORD_GATEWAY_URL") && names.contains("BOT_TOKEN"));
        let dead: Vec<&str> = names
            .into_iter()
            .filter(|name| !read.contains(*name))
            .filter(|name| !OBSOLETE_VARIABLES.contains(name) && !external.contains(name))
            .collect();
        assert!(dead.is_empty(), "in .env.example but never read: {dead:?}");
    }

    #[test]
    fn credentials_are_escaped() {
        let mut env = example_env();
        env.insert("RABBIT_PASSWORD", "p@ss/w:rd".into());
        env.insert("REDIS_PASSWORD", "s#cret".into());
        let config = load(&env).unwrap();
        assert_eq!(
            config.amqp_url,
            "amqp://guest:p%40ss%2Fw%3Ard@127.0.0.1:5672/%2F?heartbeat=30"
        );
        assert_eq!(config.redis_url, "redis://:s%23cret@127.0.0.1:6379/0");
    }

    #[test]
    fn explicit_urls_win() {
        let mut env = example_env();
        env.insert("AMQP_URL", "amqp://u:p@mq:5672/prod".into());
        env.insert("REDIS_URL", "redis://cache:6380/2".into());
        let config = load(&env).unwrap();
        assert_eq!(config.amqp_url, "amqp://u:p@mq:5672/prod");
        assert_eq!(config.redis_url, "redis://cache:6380/2");
    }

    #[test]
    fn disabled_state_disables_everything() {
        let state = with(&[("STATE_ENABLED", "false"), ("STATE_OLD", "true")])
            .unwrap()
            .state;
        // The TTLs are kept: they only matter while the flags are on.
        assert_eq!(
            state,
            StateConfig {
                enabled: false,
                member_ttl: 60_000,
                message_ttl: 60_000,
                ..StateConfig::default()
            }
        );
        let state = load(&example_env()).unwrap().state;
        assert_eq!(
            state,
            StateConfig {
                enabled: true,
                member: true,
                member_ttl: 60_000,
                message: true,
                message_ttl: 60_000,
                presence: true,
                emoji: true,
                voice: true,
                old: false,
            }
        );
    }

    #[test]
    fn bad_values_are_errors_not_panics() {
        let mut env = example_env();
        env.insert("RESUME", "maybe".into());
        assert!(matches!(
            load(&env),
            Err(ConfigError::Invalid { name: "RESUME", .. })
        ));

        let mut env = example_env();
        env.remove("BOT_TOKEN");
        assert_eq!(load(&env).unwrap_err(), ConfigError::Missing("BOT_TOKEN"));
    }

    #[test]
    fn parse_value_matches_previous_rules() {
        assert_eq!(parse_value::<u64>("42"), Some(42));
        assert_eq!(parse_value::<bool>("true"), Some(true));
        assert_eq!(
            parse_value::<String>("hello world"),
            Some("hello world".into())
        );
        assert_eq!(parse_value::<Status>("dnd"), Some(Status::DoNotDisturb));
        assert_eq!(parse_value::<u64>(""), None);
    }
}

#[cfg(test)]
mod authority_tests {
    use super::*;

    /// Both callers parse `scheme://...`, which `http::Uri` only accepts with
    /// an authority, so no configured value gets here without one. The check
    /// still rejects such a URI instead of relying on that.
    #[test]
    fn a_uri_without_an_authority_is_not_a_host_and_port() {
        for text in ["/api/v10/gateway/bot", "*"] {
            let uri: hyper::Uri = text.parse().unwrap();
            assert!(uri.authority().is_none(), "{text}");
            assert!(!is_host_and_port(&uri), "{text}");
        }
        let uri: hyper::Uri = "ws://gateway:8080".parse().unwrap();
        assert!(is_host_and_port(&uri));
    }
}
