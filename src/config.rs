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

#[derive(Clone, Debug)]
pub struct Config {
    pub bot_token: String,
    /// `None` means "use Discord's recommended shard count, all shards".
    pub shards: Option<ShardRange>,
    /// Upper bound for identify concurrency; Discord's value is used if lower,
    /// and a value that does not divide it is lowered further (see
    /// `gateway::concurrency`).
    pub shards_concurrency: Option<u16>,
    pub default_queue: bool,
    pub resume: bool,
    pub intents: Intents,
    pub large_threshold: u64,
    pub status: Status,
    pub activity_type: ActivityType,
    pub activity_name: String,
    pub activity_state: Option<String>,
    pub activity_url: Option<String>,
    pub log_channel: u64,
    pub log_guild_channel: u64,
    pub state: StateConfig,
    pub amqp_url: String,
    pub redis_url: String,
    pub prometheus_host: String,
    pub prometheus_port: u16,
    /// Number of ordered workers that cache and publish events.
    pub workers: usize,
    /// Time limit for one cache update before the event is published without it.
    pub cache_timeout: Duration,
    /// A shard that has received nothing for this long is reported unhealthy.
    pub shard_silence: Duration,
    /// Send Discord REST requests to this proxy instead of discord.com
    /// (e.g. twilight-http-proxy), as `http://host:port` or `https://host:port`.
    pub discord_api_proxy: Option<DiscordProxy>,
    /// Connect shards to this gateway URL instead of wss://gateway.discord.gg
    /// (e.g. a gateway proxy). Resumes still go to Discord's resume URL.
    /// Trimmed and without a trailing `/`, as twilight appends `/?v=10&...`.
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

/// The part of the configuration the state cache needs.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StateConfig {
    pub enabled: bool,
    pub member: bool,
    pub member_ttl: u64,
    pub message: bool,
    pub message_ttl: u64,
    pub presence: bool,
    pub emoji: bool,
    pub voice: bool,
    pub old: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShardRange {
    pub start: u32,
    pub end: u32,
    pub total: u32,
}

impl ShardRange {
    pub fn ids(self) -> std::ops::RangeInclusive<u32> {
        self.start..=self.end
    }

    #[allow(clippy::len_without_is_empty)] // a range always has at least one shard
    pub fn len(self) -> u32 {
        self.end - self.start + 1
    }

    pub fn contains(self, shard: u32) -> bool {
        self.ids().contains(&shard)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum ConfigError {
    Missing(&'static str),
    Invalid {
        name: &'static str,
        value: String,
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

type Lookup<'a> = &'a dyn Fn(&str) -> Option<String>;

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(&|name| std::env::var(name).ok())
    }

    pub fn from_lookup(lookup: Lookup<'_>) -> Result<Self, ConfigError> {
        let env = Env(lookup);

        let shards = match (
            env.optional::<u32>("SHARDS_START")?,
            env.optional::<u32>("SHARDS_END")?,
            env.optional::<u32>("SHARDS_TOTAL")?,
        ) {
            (None, None, None) => None,
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
                Some(ShardRange { start, end, total })
            }
            _ => return Err(ConfigError::Missing("SHARDS_TOTAL")),
        };

        let intents_bits: u64 = env.required("INTENTS")?;
        let intents = Intents::from_bits(intents_bits).ok_or_else(|| ConfigError::Invalid {
            name: "INTENTS",
            value: intents_bits.to_string(),
            reason: format!(
                "unknown intent bits {:#x}",
                intents_bits & !Intents::all().bits()
            ),
        })?;

        let large_threshold: u64 = env.optional("LARGE_THRESHOLD")?.unwrap_or(250);
        if !(50..=250).contains(&large_threshold) {
            return Err(ConfigError::Invalid {
                name: "LARGE_THRESHOLD",
                value: large_threshold.to_string(),
                reason: "Discord accepts 50 to 250".into(),
            });
        }

        let state_enabled = env.optional("STATE_ENABLED")?.unwrap_or(true);
        let state = StateConfig {
            enabled: state_enabled,
            member: state_enabled && env.optional("STATE_MEMBER")?.unwrap_or(false),
            member_ttl: env.optional("STATE_MEMBER_TTL")?.unwrap_or(0),
            message: state_enabled && env.optional("STATE_MESSAGE")?.unwrap_or(false),
            message_ttl: env.optional("STATE_MESSAGE_TTL")?.unwrap_or(0),
            presence: state_enabled && env.optional("STATE_PRESENCE")?.unwrap_or(false),
            emoji: state_enabled && env.optional("STATE_EMOJI")?.unwrap_or(false),
            voice: state_enabled && env.optional("STATE_VOICE")?.unwrap_or(false),
            old: state_enabled && env.optional("STATE_OLD")?.unwrap_or(false),
        };

        let amqp_url = match env.string("AMQP_URL") {
            Some(url) => url,
            None => {
                let vhost = env.string("RABBIT_VHOST").unwrap_or_else(|| "/".into());
                // heartbeat=30: LavinMQ's default (300 s) takes 10 minutes to
                // notice a dead connection.
                format!(
                    "amqp://{}:{}@{}:{}/{}?heartbeat=30",
                    encode(
                        &env.string("RABBIT_USERNAME")
                            .unwrap_or_else(|| "guest".into())
                    ),
                    encode(
                        &env.string("RABBIT_PASSWORD")
                            .unwrap_or_else(|| "guest".into())
                    ),
                    env.string("RABBIT_HOST")
                        .ok_or(ConfigError::Missing("RABBIT_HOST"))?,
                    env.optional::<u16>("RABBIT_PORT")?.unwrap_or(5672),
                    encode(&vhost),
                )
            }
        };

        let redis_url = match env.string("REDIS_URL") {
            Some(url) => url,
            None => {
                let auth = match (env.string("REDIS_USERNAME"), env.string("REDIS_PASSWORD")) {
                    (None, None) => String::new(),
                    (user, pass) => format!(
                        "{}:{}@",
                        encode(&user.unwrap_or_default()),
                        encode(&pass.unwrap_or_default())
                    ),
                };
                format!(
                    "redis://{auth}{}:{}/{}",
                    env.string("REDIS_HOST")
                        .ok_or(ConfigError::Missing("REDIS_HOST"))?,
                    env.optional::<u16>("REDIS_PORT")?.unwrap_or(6379),
                    env.optional::<u32>("REDIS_DB")?.unwrap_or(0),
                )
            }
        };

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
        let prometheus_host = env
            .string("PROMETHEUS_HOST")
            .unwrap_or_else(|| "0.0.0.0".into());
        let prometheus_host = match parse_host(&prometheus_host) {
            Some(ip) => ip.to_string(),
            None => {
                return Err(ConfigError::Invalid {
                    name: "PROMETHEUS_HOST",
                    value: prometheus_host,
                    reason: "must be an IP address or localhost".into(),
                })
            }
        };

        Ok(Self {
            bot_token: env
                .string("BOT_TOKEN")
                .ok_or(ConfigError::Missing("BOT_TOKEN"))?,
            shards,
            shards_concurrency: env.optional("SHARDS_CONCURRENCY")?,
            default_queue: env.optional("DEFAULT_QUEUE")?.unwrap_or(true),
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
            prometheus_port: env.optional("PROMETHEUS_PORT")?.unwrap_or(8005),
            workers,
            cache_timeout: Duration::from_millis(cache_timeout_ms),
            shard_silence: Duration::from_secs(shard_silence_secs),
            discord_api_proxy: match env.string("DISCORD_API_PROXY") {
                Some(value) => Some(DiscordProxy::parse(&value).ok_or_else(|| {
                    ConfigError::Invalid {
                        name: "DISCORD_API_PROXY",
                        value: value.clone(),
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
                        value: value.clone(),
                        reason: "expected ws:// or wss:// with a host, an optional port and \
                                 path, and no query, fragment or credentials"
                            .into(),
                    }
                })?),
                None => None,
            },
        })
    }

    /// Variables that older versions required but that no longer do anything.
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

struct Env<'a>(Lookup<'a>);

impl Env<'_> {
    fn string(&self, name: &str) -> Option<String> {
        (self.0)(name).filter(|value| !value.trim().is_empty())
    }

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

    fn required<T: DeserializeOwned>(&self, name: &'static str) -> Result<T, ConfigError> {
        self.optional(name)?.ok_or(ConfigError::Missing(name))
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
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        let escape = byte == b'%'
            && bytes.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
            && bytes.get(i + 2).is_some_and(u8::is_ascii_hexdigit);
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') || escape {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
        i += 1;
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
