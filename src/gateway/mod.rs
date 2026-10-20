//! Discord gateway: shard creation, per-shard runners and ordered workers.

pub mod payload;
pub mod registry;
pub mod runner;
pub mod seq;
pub mod sessions;
pub mod worker;

use crate::{
    config::{Config, ShardRange},
    models::{ApiResult, SessionInfo},
};

use std::{collections::HashMap, time::Duration};
use tracing::{info, warn};
use twilight_gateway::{queue::InMemoryQueue, ConfigBuilder, Session, Shard};
use twilight_http::Client;
use twilight_model::gateway::{
    payload::outgoing::update_presence::UpdatePresencePayload,
    presence::{Activity, ActivityType},
};

/// Shards of this process plus what they were created from.
pub struct Shards {
    pub shards: Vec<Shard<InMemoryQueue>>,
    pub range: ShardRange,
    pub resumed: HashMap<u32, SessionInfo>,
}

/// The bot's presence. Bots may set name, state, type and url; a custom
/// status (type 4) is displayed from `state`.
pub fn presence(config: &Config) -> ApiResult<UpdatePresencePayload> {
    let state = config.activity_state.clone().or_else(|| {
        (config.activity_type == ActivityType::Custom).then(|| config.activity_name.clone())
    });
    let activity = Activity {
        application_id: None,
        assets: None,
        buttons: Vec::new(),
        created_at: None,
        details: None,
        emoji: None,
        flags: None,
        id: None,
        instance: None,
        kind: config.activity_type,
        name: config.activity_name.clone(),
        party: None,
        secrets: None,
        state,
        timestamps: None,
        url: config.activity_url.clone(),
    };
    UpdatePresencePayload::new(vec![activity], false, None, config.status)
        .map_err(|err| format!("invalid presence: {err}").into())
}

/// Identify concurrency: Discord's value, lowered by SHARDS_CONCURRENCY if set.
pub fn concurrency(discord: u16, configured: Option<u16>) -> u16 {
    match configured {
        Some(configured) if configured > 0 => configured.min(discord.max(1)),
        _ => discord.max(1),
    }
}

/// Create this process' shards, resuming saved sessions where possible.
pub async fn create(
    config: &Config,
    http: &Client,
    saved: HashMap<u32, SessionInfo>,
) -> ApiResult<Shards> {
    let info = http.gateway().authed().await?.model().await?;
    let limit = info.session_start_limit;

    let range = config.shards.unwrap_or(ShardRange {
        start: 0,
        end: info.shards.saturating_sub(1),
        total: info.shards.max(1),
    });
    if range.total < info.shards {
        warn!(
            "SHARDS_TOTAL is {} but Discord recommends {} shards; shards over 2500 guilds are disconnected with 4011",
            range.total, info.shards
        );
    }

    let resumed: HashMap<u32, SessionInfo> = if config.resume {
        saved
            .into_iter()
            .filter(|(shard, session)| range.contains(*shard) && session.is_resumable())
            .collect()
    } else {
        HashMap::new()
    };

    let identifies = range.len() as usize - resumed.len();
    if (limit.remaining as usize) < identifies {
        warn!(
            "Only {} of {} daily identifies left but {} shards need one; identifying will wait {:?} for the limit to reset",
            limit.remaining,
            limit.total,
            identifies,
            Duration::from_millis(limit.reset_after)
        );
    }

    let max_concurrency = concurrency(limit.max_concurrency, config.shards_concurrency);
    info!(
        "Identify concurrency {} (Discord allows {}), {} identifies left today",
        max_concurrency, limit.max_concurrency, limit.remaining
    );
    let queue = InMemoryQueue::new(
        max_concurrency,
        limit.remaining.min(limit.total),
        Duration::from_millis(limit.reset_after),
        limit.total,
    );

    let mut base = ConfigBuilder::new(config.bot_token.clone(), config.intents)
        .presence(presence(config)?)
        .large_threshold(config.large_threshold)
        .queue(queue);
    if let Some(url) = &config.discord_gateway_url {
        base = base.proxy_url(url.clone());
    }
    let base = base.build();

    let sessions = resumed.clone();
    let shards = twilight_gateway::create_iterator(
        range.start..range.end + 1,
        range.total,
        base,
        move |id, builder| match sessions.get(&id.number()) {
            Some(session) => {
                let builder =
                    builder.session(Session::new(session.sequence, session.session_id.clone()));
                match &session.resume_url {
                    Some(url) => builder.resume_url(url.clone()).build(),
                    None => builder.build(),
                }
            }
            None => builder.build(),
        },
    )
    .collect();

    Ok(Shards {
        shards,
        range,
        resumed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrency_never_exceeds_discord() {
        assert_eq!(concurrency(16, None), 16);
        assert_eq!(concurrency(16, Some(1)), 1);
        assert_eq!(concurrency(1, Some(16)), 1);
        assert_eq!(concurrency(0, None), 1);
        assert_eq!(concurrency(16, Some(0)), 16);
    }

    #[test]
    fn custom_status_uses_state() {
        let mut env = crate::config::tests::example_env();
        env.insert("ACTIVITY_TYPE", "4".into());
        env.insert("ACTIVITY_NAME", "Counting servers".into());
        let config = Config::from_lookup(&|name| env.get(name).cloned()).unwrap();
        let presence = serde_json::to_value(presence(&config).unwrap()).unwrap();
        let activity = &presence["activities"][0];
        assert_eq!(activity["type"], 4);
        assert_eq!(activity["state"], "Counting servers");
        assert_eq!(presence["status"], "online");
    }

    #[test]
    fn playing_status_has_no_state() {
        let env = crate::config::tests::example_env();
        let config = Config::from_lookup(&|name| env.get(name).cloned()).unwrap();
        let presence = serde_json::to_value(presence(&config).unwrap()).unwrap();
        assert_eq!(presence["activities"][0]["name"], "Testing");
        assert!(presence["activities"][0].get("state").is_none());
    }
}
