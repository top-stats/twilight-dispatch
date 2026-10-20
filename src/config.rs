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
    /// Upper bound for identify concurrency; Discord's value is used if lower.
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
        })
    }

    /// Variables that older versions required but that no longer do anything.
    pub fn obsolete_variables(lookup: Lookup<'_>) -> Vec<&'static str> {
        ["CLUSTERS", "SHARDS_WAIT"]
            .into_iter()
            .filter(|name| lookup(name).is_some_and(|v| !v.trim().is_empty()))
            .collect()
    }
}

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
    use std::collections::HashMap;

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

    #[test]
    fn shipped_env_example_loads() {
        let env: HashMap<&'static str, String> = include_str!("../.env.example")
            .lines()
            .filter(|line| !line.trim_start().starts_with('#'))
            .filter_map(|line| line.split_once('='))
            .map(|(k, v)| {
                (
                    &*Box::leak(k.trim().to_owned().into_boxed_str()),
                    v.trim().to_owned(),
                )
            })
            .chain([("BOT_TOKEN", "token".to_owned())])
            .collect();
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
        let mut env = example_env();
        env.insert("STATE_ENABLED", "false".into());
        let state = load(&env).unwrap().state;
        assert!(!state.member && !state.message && !state.presence && !state.old);
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
