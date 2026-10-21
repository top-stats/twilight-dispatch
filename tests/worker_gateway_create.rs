#![recursion_limit = "256"]

//! `gateway::create` against a fake Discord: which gateway URL the shards
//! connect to, and saved sessions in a map with any hasher. Nothing connects:
//! shards only do once they are polled.

#[path = "support/fake_discord.rs"]
mod fake_discord;

use fake_discord::FakeDiscord;
use std::{
    collections::HashMap,
    hash::{BuildHasherDefault, DefaultHasher},
};
use twilight_dispatch::{app, config::Config, gateway, models::SessionInfo};

/// Where Discord asked the saved session to resume.
const RESUME_URL: &str = "wss://resume-0.discord.gg";

/// The configuration for shards 0 and 1 of 2 against `fake`'s REST API, with
/// `DISCORD_GATEWAY_URL` set to `gateway_url` when given.
fn load_config(fake: &FakeDiscord, gateway_url: Option<&str>) -> Config {
    let [(api_name, api_url), (gateway_name, _)] = fake.env();
    let mut vars: HashMap<&'static str, String> = [
        ("BOT_TOKEN", "create-token"),
        ("SHARDS_START", "0"),
        ("SHARDS_END", "1"),
        ("SHARDS_TOTAL", "2"),
        ("INTENTS", "513"),
        ("REDIS_URL", "redis://127.0.0.1:6379"),
        ("AMQP_URL", "amqp://127.0.0.1:5672/%2f"),
    ]
    .into_iter()
    .map(|(name, value)| (name, value.to_owned()))
    .collect();
    vars.insert(api_name, api_url);
    if let Some(url) = gateway_url {
        vars.insert(gateway_name, url.to_owned());
    }
    Config::from_lookup(&|name| vars.get(name).cloned()).expect("valid config")
}

/// Production sets no `DISCORD_GATEWAY_URL`, so the shards connect to the URL
/// in Discord's gateway information without a proxy. Only a configured URL
/// is a proxy; an empty value counts as unset.
#[tokio::test]
async fn shards_use_a_gateway_proxy_only_when_one_is_configured() {
    // main() installs it; twilight's rustls connectors panic without one.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let fake = FakeDiscord::start().await;
    let proxy = fake.gateway.url();
    for (value, expected) in [
        (None, None),
        (Some(""), None),
        (Some(proxy.as_str()), Some(proxy.as_str())),
    ] {
        let config = load_config(&fake, value);
        let configured = config.discord_gateway_url.as_deref();
        assert_eq!(configured, expected, "{value:?}");
        let http = app::http_client(&config);
        let created = gateway::create(&config, &http, HashMap::new())
            .await
            .expect("create the shards");
        assert_eq!(created.shards.len(), 2);
        for shard in &created.shards {
            assert_eq!(shard.config().proxy_url(), expected, "{value:?}");
        }
    }
}

/// `create` takes the saved sessions in a map with any hasher, and resumes the
/// shards they belong to.
#[tokio::test]
async fn saved_sessions_may_come_in_a_map_with_any_hasher() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let fake = FakeDiscord::start().await;
    let config = load_config(&fake, None);
    let info = SessionInfo {
        session_id: "abc".to_owned(),
        sequence: 42,
        resume_url: Some(RESUME_URL.to_owned()),
    };
    let mut saved = HashMap::with_hasher(BuildHasherDefault::<DefaultHasher>::default());
    saved.insert(0, info.clone());
    let http = app::http_client(&config);
    let created = gateway::create(&config, &http, saved)
        .await
        .expect("create the shards");

    assert_eq!(created.resumed, HashMap::from([(0, info)]));
    let session = created.shards[0].session().expect("shard 0 resumes");
    assert_eq!((session.id(), session.sequence()), ("abc", 42));
    assert_eq!(created.shards[0].resume_url(), Some(RESUME_URL));
    assert!(created.shards[1].session().is_none(), "it identifies");
}
