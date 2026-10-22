//! Discord gateway: shard creation, the identify queue, per-shard runners,
//! their guild member requests and ordered workers.

pub mod members;
pub mod payload;
pub mod queue;
pub mod registry;
pub mod runner;
pub mod seq;
pub mod sessions;
pub mod worker;

use crate::{
    config::{Config, ShardRange},
    models::{ApiResult, SessionInfo},
};

use std::{collections::HashMap, hash::BuildHasher, time::Duration};
use tracing::{info, warn};
use twilight_gateway::{queue::InMemoryQueue, ConfigBuilder, Session, Shard};
use twilight_http::Client;
use twilight_model::gateway::{
    payload::outgoing::update_presence::UpdatePresencePayload,
    presence::{Activity, ActivityType},
};

/// Shards of this process plus what they were created from.
pub struct Shards<Q = InMemoryQueue> {
    /// One shard per id of `range`, in order. None is connected yet: a shard
    /// connects once it is first polled.
    pub shards: Vec<Shard<Q>>,
    /// The shards of this process: `SHARDS_START..=SHARDS_END` of
    /// `SHARDS_TOTAL`, or every shard of Discord's recommended count when
    /// those are unset.
    pub range: ShardRange,
    /// The saved sessions the shards resume, by shard id: those of `range`
    /// that are still resumable, or none with `RESUME=false`.
    pub resumed: HashMap<u32, SessionInfo>,
}

/// The bot's presence. Bots may set name, state, type and url; a custom
/// status (type 4) is displayed from `state`.
///
/// # Errors
///
/// When twilight rejects the payload, which it does only for an empty list of
/// activities (this one always has one).
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

/// Identify concurrency: Discord's value, lowered by `SHARDS_CONCURRENCY` if set.
///
/// Discord allows one identify per 5 s in each bucket `shard % max_concurrency`,
/// and `InMemoryQueue` releases one shard per 5 s in each bucket `shard % c`.
/// Only a divisor of Discord's value keeps the shards of one Discord bucket
/// in one queue bucket: with 16 and 3, shards 0 and 16 share Discord's bucket
/// 0 but would identify together from queue buckets 0 and 1, and Discord
/// answers that with an invalid session. So a configured value is lowered to
/// the largest divisor not above it.
pub fn concurrency(discord: u16, configured: Option<u16>) -> u16 {
    let discord = discord.max(1);
    let wanted = match configured {
        Some(configured) if configured > 0 => configured.min(discord),
        _ => discord,
    };
    (1..=wanted)
        .rev()
        .find(|&c| discord.is_multiple_of(c))
        .unwrap_or(1)
}

/// Create this process' shards, resuming saved sessions where possible. Their
/// identifies only take turns within this process.
///
/// # Errors
///
/// When Discord's gateway information (`GET /gateway/bot`) cannot be fetched
/// or decoded, or the presence is invalid (see [`presence`]).
pub async fn create<S: BuildHasher>(
    config: &Config,
    http: &Client,
    saved: HashMap<u32, SessionInfo, S>,
) -> ApiResult<Shards> {
    create_with_queue(config, http, saved, |local, _| local).await
}

/// [`create`] with the identify queue that `queue` builds from the local one
/// (this process' bucket spacing and daily limit, from Discord's session
/// start limit) and Discord's `max_concurrency`, for example a
/// [`queue::RedisQueue`] that also takes turns with other processes.
///
/// # Errors
///
/// As [`create`]: when Discord's gateway information cannot be fetched or
/// decoded, or the presence is invalid.
pub async fn create_with_queue<Q: Clone, S: BuildHasher>(
    config: &Config,
    http: &Client,
    saved: HashMap<u32, SessionInfo, S>,
    queue: impl FnOnce(InMemoryQueue, u16) -> Q,
) -> ApiResult<Shards<Q>> {
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
    if let Some(configured) = config
        .shards_concurrency
        .filter(|&configured| max_concurrency < configured.min(limit.max_concurrency))
    {
        warn!(
            "SHARDS_CONCURRENCY={} does not divide Discord's max_concurrency {}; using {} so that shards Discord rate-limits together never identify together",
            configured, limit.max_concurrency, max_concurrency
        );
    }
    info!(
        "Identify concurrency {} (Discord allows {}), {} identifies left today",
        max_concurrency, limit.max_concurrency, limit.remaining
    );
    let local = InMemoryQueue::new(
        max_concurrency,
        limit.remaining.min(limit.total),
        Duration::from_millis(limit.reset_after),
        limit.total,
    );

    let mut base = ConfigBuilder::new(config.bot_token.clone(), config.intents)
        .presence(presence(config)?)
        .large_threshold(config.large_threshold)
        .queue(queue(local, limit.max_concurrency));
    if let Some(url) = &config.discord_gateway_url {
        base = base.proxy_url(url.clone());
    }
    let base = base.build();

    let sessions = resumed.clone();
    #[allow(
        clippy::range_plus_one,
        reason = "create_iterator takes an ExactSizeIterator, which RangeInclusive<u32> is not"
    )]
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

    /// Every Discord bucket (`shard % discord`) maps into a single queue
    /// bucket (`shard % c`) exactly when `c` divides `discord`.
    #[test]
    fn concurrency_is_the_largest_divisor_of_discords_not_above_the_configured_value() {
        for discord in [1u16, 16, 32, 64, 128] {
            assert_eq!(concurrency(discord, None), discord);
            for configured in 0..=200u16 {
                let c = concurrency(discord, Some(configured));
                let wanted = if configured == 0 {
                    discord
                } else {
                    configured.min(discord)
                };
                assert!(
                    c >= 1 && c <= discord && c <= wanted && discord.is_multiple_of(c),
                    "{discord}/{configured} -> {c}"
                );
                assert!(
                    (c + 1..=wanted).all(|larger| !discord.is_multiple_of(larger)),
                    "{discord}/{configured} -> {c}, a larger divisor fits"
                );
            }
        }
        assert_eq!(concurrency(16, Some(3)), 2);
        assert_eq!(concurrency(16, Some(12)), 8);
        assert_eq!(concurrency(12, Some(5)), 4);
        assert_eq!(concurrency(7, Some(6)), 1);
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
