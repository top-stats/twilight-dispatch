//! What a deployment gets for the variables it leaves unset, and what it is
//! told about the ones it gets wrong: the defaults, the error for each
//! required variable, the text operators see in the logs, obsolete variables
//! and the credentials a rejected Discord endpoint must not leak. Only
//! `Config::from_lookup` and `Config::obsolete_variables`: no services.

use std::{collections::HashMap, error::Error as _, time::Duration};
use twilight_dispatch::config::{self, Config, ConfigError, StateConfig};
use twilight_model::gateway::presence::{ActivityType, Status};

/// The variables a deployment must set (intents 513: guilds and guild
/// messages).
const MINIMAL: &[(&str, &str)] = &[
    ("BOT_TOKEN", "token"),
    ("INTENTS", "513"),
    ("RABBIT_HOST", "mq"),
    ("REDIS_HOST", "cache"),
];

/// Every other variable `Config::from_lookup` reads.
const OPTIONAL: &[&str] = &[
    "SHARDS_START",
    "SHARDS_END",
    "SHARDS_TOTAL",
    "SHARDS_CONCURRENCY",
    "DEFAULT_QUEUE",
    "AMQP_CONFIRMS",
    "RESUME",
    "LARGE_THRESHOLD",
    "STATUS",
    "ACTIVITY_TYPE",
    "ACTIVITY_NAME",
    "ACTIVITY_STATE",
    "ACTIVITY_URL",
    "LOG_CHANNEL",
    "LOG_GUILD_CHANNEL",
    "STATE_ENABLED",
    "STATE_MEMBER",
    "STATE_MEMBER_TTL",
    "STATE_MESSAGE",
    "STATE_MESSAGE_TTL",
    "STATE_PRESENCE",
    "STATE_EMOJI",
    "STATE_VOICE",
    "STATE_OLD",
    "AMQP_URL",
    "RABBIT_PORT",
    "RABBIT_USERNAME",
    "RABBIT_PASSWORD",
    "RABBIT_VHOST",
    "REDIS_URL",
    "REDIS_PORT",
    "REDIS_DB",
    "REDIS_USERNAME",
    "REDIS_PASSWORD",
    "PROMETHEUS_HOST",
    "PROMETHEUS_PORT",
    "WORKERS",
    "CACHE_TIMEOUT_MS",
    "SHARD_SILENCE_SECS",
    "DISCORD_API_PROXY",
    "DISCORD_GATEWAY_URL",
];

/// The configuration with exactly `vars` set (a later value wins).
fn load(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
    let env: HashMap<&str, &str> = vars.iter().copied().collect();
    Config::from_lookup(&|name: &str| env.get(name).copied().map(str::to_owned))
}

/// `MINIMAL` with `changes` applied.
fn with(changes: &[(&'static str, &'static str)]) -> Result<Config, ConfigError> {
    let mut vars = MINIMAL.to_vec();
    vars.extend_from_slice(changes);
    load(&vars)
}

/// `MINIMAL` without `name`.
fn without(name: &str) -> Result<Config, ConfigError> {
    let vars: Vec<(&str, &str)> = MINIMAL
        .iter()
        .copied()
        .filter(|&(variable, _)| variable != name)
        .collect();
    load(&vars)
}

/// The defaults are part of the contract: a changed one silently changes
/// every deployment that relies on it. Literals on purpose, not the
/// constants of the code.
#[test]
fn unset_variables_take_their_documented_defaults() {
    let config = load(MINIMAL).unwrap();
    assert_eq!(config.bot_token, "token");
    assert_eq!(config.shards, None);
    assert_eq!(config.shards_concurrency, None);
    assert!(config.default_queue);
    assert!(!config.amqp_confirms);
    assert!(config.resume);
    assert_eq!(config.intents.bits(), 513);
    assert_eq!(config.large_threshold, 250);
    assert_eq!(config.status, Status::Online);
    assert_eq!(config.activity_type, ActivityType::Playing);
    assert!(config.activity_name.is_empty());
    assert_eq!(config.activity_state, None);
    assert_eq!(config.activity_url, None);
    assert_eq!((config.log_channel, config.log_guild_channel), (0, 0));
    assert_eq!(
        config.state,
        StateConfig {
            enabled: true,
            ..StateConfig::default()
        }
    );
    assert_eq!(
        config.amqp_url,
        "amqp://guest:guest@mq:5672/%2F?heartbeat=30"
    );
    assert_eq!(config.redis_url, "redis://cache:6379/0");
    assert_eq!(config.prometheus_host, "0.0.0.0");
    assert_eq!(config.prometheus_port, 8005);
    // What `twilight-dispatch healthcheck` probes when the port is unset.
    assert_eq!(config.prometheus_port, config::DEFAULT_PROMETHEUS_PORT);
    assert_eq!(config.workers, 32);
    assert_eq!(config.cache_timeout, Duration::from_secs(10));
    assert_eq!(config.shard_silence, Duration::from_secs(180));
    assert_eq!(config.discord_api_proxy, None);
    assert_eq!(config.discord_gateway_url, None);
}

/// Blank values (empty or whitespace, as `NAME=` in a Compose file gives)
/// mean "unset" for every optional variable.
#[test]
fn blank_variables_are_unset() {
    let defaults = format!("{:?}", load(MINIMAL).unwrap());
    for blank in ["", " \t\r\n"] {
        let vars: Vec<(&str, &str)> = OPTIONAL.iter().map(|&name| (name, blank)).collect();
        let config = with(&vars).unwrap();
        assert_eq!(format!("{config:?}"), defaults, "{blank:?}");
    }
}

#[test]
fn each_required_variable_is_reported_when_unset_or_blank() {
    for name in ["BOT_TOKEN", "INTENTS", "RABBIT_HOST", "REDIS_HOST"] {
        assert_eq!(
            without(name).unwrap_err(),
            ConfigError::Missing(name),
            "{name}"
        );
        assert_eq!(
            with(&[(name, " \t")]).unwrap_err(),
            ConfigError::Missing(name),
            "{name}"
        );
    }
}

/// What operators see in the logs for a bad or a missing value.
#[test]
fn errors_name_the_variable_the_value_and_the_reason() {
    let err = with(&[("WORKERS", "0")]).unwrap_err();
    assert_eq!(
        err.to_string(),
        r#"invalid environment variable WORKERS="0": need 1 to 1024 workers"#
    );
    assert!(err.source().is_none());

    // (1 << 40) | GUILDS: only the bits of no known intent are named.
    let err = with(&[("INTENTS", "1099511627777")]).unwrap_err();
    assert_eq!(
        err.to_string(),
        concat!(
            r#"invalid environment variable INTENTS="1099511627777": "#,
            "unknown intent bits 0x10000000000"
        )
    );

    // The value is quoted and escaped.
    let err = with(&[("RESUME", r#"say "yes""#)]).unwrap_err();
    assert_eq!(
        err.to_string(),
        r#"invalid environment variable RESUME="say \"yes\"": cannot parse as bool"#
    );

    assert_eq!(
        without("REDIS_HOST").unwrap_err().to_string(),
        "missing environment variable REDIS_HOST"
    );
}

/// The obsolete variables set in a lookup that knows only `CLUSTERS` and
/// `SHARDS_WAIT`.
fn obsolete(clusters: Option<&str>, shards_wait: Option<&str>) -> Vec<&'static str> {
    Config::obsolete_variables(&|name: &str| match name {
        "CLUSTERS" => clusters.map(str::to_owned),
        "SHARDS_WAIT" => shards_wait.map(str::to_owned),
        _ => None,
    })
}

/// A blank obsolete variable is as good as unset, so it is not reported.
#[test]
fn blank_obsolete_variables_are_not_reported() {
    assert_eq!(obsolete(Some(" "), Some("6")), ["SHARDS_WAIT"]);
    assert_eq!(obsolete(Some(""), Some("6")), ["SHARDS_WAIT"]);
    assert_eq!(obsolete(Some("2"), Some("\t")), ["CLUSTERS"]);
    assert!(obsolete(None, None).is_empty());
}

/// Built URLs percent-encode every byte that is not unreserved, except a
/// complete `%XX` escape, also at the end of a value.
#[test]
fn built_urls_encode_credentials_byte_by_byte() {
    let config = with(&[
        ("RABBIT_USERNAME", "a b~"),
        ("RABBIT_PASSWORD", "%41%"),
        ("RABBIT_VHOST", "é/"),
        ("REDIS_PASSWORD", "%4"),
    ])
    .unwrap();
    assert_eq!(
        config.amqp_url,
        "amqp://a%20b~:%41%25@mq:5672/%C3%A9%2F?heartbeat=30"
    );
    assert_eq!(config.redis_url, "redis://:%254@cache:6379/0");
}

/// The `value` of the `Invalid` error that `MINIMAL` plus `name=value` is
/// rejected with.
fn rejected_value(name: &'static str, value: &'static str) -> String {
    match with(&[(name, value)]) {
        Err(ConfigError::Invalid {
            name: rejected,
            value: logged,
            ..
        }) => {
            assert_eq!(rejected, name);
            logged
        }
        other => panic!("{name}={value}: {other:?}"),
    }
}

/// `name=value` is rejected, and its error, as logged (`Display`) or
/// debugged, shows `logged` and none of the credentials (`user`, `hunt...`).
fn assert_redacted(name: &'static str, value: &'static str, logged: &str) {
    assert_eq!(rejected_value(name, value), logged, "{name}={value}");
    let err = with(&[(name, value)]).unwrap_err();
    let text = format!("{err} {err:?}");
    assert!(!text.contains("hunt"), "{text}");
    assert!(!text.contains("user"), "{text}");
}

/// The Discord endpoints reject credentials, and the rejected value is
/// logged: everything up to its last `@` (after the scheme) becomes `***`.
#[test]
fn rejected_discord_endpoints_do_not_log_their_credentials() {
    for (value, logged) in [
        ("http://user:hunter2@proxy:3000", "http://***@proxy:3000"),
        ("user:hunter2@proxy:3000", "***@proxy:3000"),
        ("https://us@er:hunter2@proxy", "https://***@proxy"),
    ] {
        assert_redacted("DISCORD_API_PROXY", value, logged);
    }
    for (value, logged) in [
        ("ws://user:hunter2@gateway", "ws://***@gateway"),
        // An unescaped `/` in the password ends the URL's authority.
        ("wss://user:hunt/er2@gw:1/p", "wss://***@gw:1/p"),
    ] {
        assert_redacted("DISCORD_GATEWAY_URL", value, logged);
    }
}

/// A rejected value without credentials is logged as set, and the values of
/// other variables are never redacted.
#[test]
fn other_rejected_values_are_logged_as_set() {
    for (name, value) in [
        ("DISCORD_API_PROXY", "ftp://proxy"),
        ("DISCORD_GATEWAY_URL", "http://gateway"),
        ("PROMETHEUS_HOST", "user@metrics"),
        ("WORKERS", "a@b"),
    ] {
        assert_eq!(rejected_value(name, value), value);
    }
}
