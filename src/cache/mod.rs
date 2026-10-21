//! Redis state cache.
//!
//! The key layout and the JSON stored under each key are a contract with the
//! consumers of the cache and are kept compatible with earlier versions (see
//! [`crate::constants`] and [`keys`]). Each gateway event is applied with at
//! most a few reads followed by one atomic write ([`batch::Batch`]). Events of
//! one guild are applied in order by the same worker (see
//! [`crate::gateway::worker`]), which makes the read-modify-write updates here
//! safe.
//!
//! READY is the exception: it commits the bot user and the guild stubs in one
//! transaction, then removes each stale guild of the shard in a transaction
//! of its own, which keeps every transaction the size of one guild. A failure
//! part way leaves the remaining stale guilds to the shard's next READY, which
//! finds them the same way.

pub mod batch;
pub mod jobs;
pub mod keys;
pub mod merge;

use self::{batch::Batch, keys::plan};
use crate::{
    config::StateConfig,
    constants::{
        channel_key, emoji_key, guild_key, member_key, message_key, presence_key, role_key,
        thread_key, voice_key, BOT_USER_KEY, EMOJI_KEY, GUILD_KEY,
    },
    models::ApiResult,
};

use redis::aio::ConnectionLike;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::HashSet;
use twilight_model::{
    channel::Channel,
    gateway::{
        event::Event,
        payload::incoming::{GuildCreate, Ready},
        presence::{Presence, Status, UserOrId},
    },
    guild::{Guild, Member},
    id::{
        marker::{ChannelMarker, GuildMarker, UserMarker},
        Id,
    },
    voice::VoiceState,
};

/// Inputs of one cache update besides the event itself.
#[derive(Clone, Copy, Debug)]
pub struct UpdateContext<'a> {
    pub state: &'a StateConfig,
    pub bot_id: Option<Id<UserMarker>>,
    pub now_ms: i64,
}

/// What a cache update observed.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    /// The previous cached value, when `STATE_OLD` is enabled.
    pub old: Option<Value>,
    /// For GUILD_CREATE / GUILD_DELETE: the guild as it was cached before.
    pub previous_guild: Option<Value>,
    /// For GUILD_CREATE: whether the guild was already known (cached in full,
    /// or as an unavailable stub from READY or an outage).
    pub guild_known: bool,
}

/// A member as stored in Redis. twilight removed `guild_id` from `Member`, but
/// cached members have always carried it.
#[derive(Serialize)]
pub struct CachedMember<'a> {
    pub guild_id: Id<GuildMarker>,
    #[serde(flatten)]
    pub member: &'a Member,
}

// ---------------------------------------------------------------------------
// Reads

/// GET a logical key and parse it as JSON.
pub async fn get<C: ConnectionLike + Send>(conn: &mut C, key: &str) -> ApiResult<Option<Value>> {
    let raw: Option<String> = redis::cmd("GET")
        .arg(plan(key).stored)
        .query_async(conn)
        .await?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
}

/// MGET logical keys (always MGET, even for one key) and parse them as JSON.
pub async fn get_all<C: ConnectionLike + Send>(
    conn: &mut C,
    keys: &[String],
) -> ApiResult<Vec<Option<Value>>> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let stored: Vec<String> = keys.iter().map(|key| plan(key).stored).collect();
    let raw: Vec<Option<String>> = redis::cmd("MGET").arg(stored).query_async(conn).await?;
    raw.into_iter()
        .map(|raw| {
            raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
                .transpose()
        })
        .collect()
}

pub async fn members<C: ConnectionLike + Send>(conn: &mut C, set: &str) -> ApiResult<Vec<String>> {
    Ok(redis::cmd("SMEMBERS").arg(set).query_async(conn).await?)
}

pub async fn exists<C: ConnectionLike + Send>(conn: &mut C, key: &str) -> ApiResult<bool> {
    let count: u64 = redis::cmd("EXISTS")
        .arg(plan(key).stored)
        .query_async(conn)
        .await?;
    Ok(count > 0)
}

// ---------------------------------------------------------------------------
// Writes shared by several events

/// Plan the deletion of a channel and every message cached under it.
async fn clear_channel<C: ConnectionLike + Send>(
    conn: &mut C,
    batch: &mut Batch,
    logical_key: &str,
    channel_id: u64,
) -> ApiResult<()> {
    let index = keys::channel_index(channel_id);
    for message in members(conn, &index).await? {
        batch.del(&message);
    }
    batch.del_raw(index);
    batch.del(logical_key);
    Ok(())
}

/// Plan the removal of a guild and everything cached under it. Returns the
/// cached guild.
///
/// Channels in `keep` are about to be written again (GUILD_CREATE), so their
/// cached messages survive.
async fn clear_guild<C: ConnectionLike + Send>(
    conn: &mut C,
    batch: &mut Batch,
    guild_id: Id<GuildMarker>,
    keep: &HashSet<u64>,
) -> ApiResult<Option<Value>> {
    let key = guild_key(guild_id);
    let index = keys::guild_index(guild_id);
    let previous = get(conn, &key).await?;

    for child in members(conn, &index).await? {
        if let Some(thread_id) = keys::thread_id_of(&child) {
            if !keep.contains(&thread_id) {
                clear_channel(conn, batch, &child, thread_id).await?;
            }
            continue;
        }
        // twilight never produces id 0; a `channel:0` would only be deleted,
        // without looking up its messages.
        match keys::channel_id_of(&child).and_then(Id::new_checked) {
            Some(channel_id) if keep.contains(&channel_id.get()) => {}
            Some(channel_id) => {
                // Stored channel keys lose their guild; re-add it so the
                // channel is also removed from guild_keys:<guild>.
                let logical = channel_key(Some(guild_id), channel_id);
                clear_channel(conn, batch, &logical, channel_id.get()).await?;
            }
            None => batch.del(&child),
        }
    }
    batch.del_raw(index);
    batch.del_raw(keys::thread_index(guild_id));
    batch.del(&key);

    Ok(previous)
}

/// Cached threads of a guild, optionally only those under `parents`.
async fn cached_threads<C: ConnectionLike + Send>(
    conn: &mut C,
    guild_id: Id<GuildMarker>,
    parents: Option<&HashSet<u64>>,
) -> ApiResult<Vec<Id<ChannelMarker>>> {
    let threads = members(conn, &keys::thread_index(guild_id)).await?;
    let thread_id = |key: &str| keys::thread_id_of(key).and_then(Id::new_checked);
    let Some(parents) = parents else {
        return Ok(threads.iter().filter_map(|key| thread_id(key)).collect());
    };
    let values = get_all(conn, &threads).await?;
    Ok(threads
        .iter()
        .zip(values)
        .filter_map(|(key, value)| {
            let parent = value?.get("parent_id")?.as_str()?.parse::<u64>().ok()?;
            parents.contains(&parent).then(|| thread_id(key))?
        })
        .collect())
}

/// Plan the deletion of threads (and their messages).
async fn clear_threads<C: ConnectionLike + Send>(
    conn: &mut C,
    batch: &mut Batch,
    guild_id: Id<GuildMarker>,
    threads: impl IntoIterator<Item = Id<ChannelMarker>>,
) -> ApiResult<()> {
    for thread in threads {
        clear_channel(conn, batch, &thread_key(guild_id, thread), thread.get()).await?;
    }
    Ok(())
}

/// How recent `joined_at` must be for a GUILD_MEMBER_ADD to count as a join.
pub const RECENT_JOIN_MS: i64 = 15 * 60 * 1000;

/// Whether a GUILD_MEMBER_ADD is a new join rather than a re-sent event for
/// an existing member (Discord does that; the re-sent event carries the
/// original `joined_at`).
pub fn joined_recently(joined_at: Option<twilight_model::util::Timestamp>, now_ms: i64) -> bool {
    joined_at.is_some_and(|at| now_ms - at.as_micros() / 1000 < RECENT_JOIN_MS)
}

fn set_member(
    batch: &mut Batch,
    ctx: &UpdateContext<'_>,
    guild_id: Id<GuildMarker>,
    member: &Member,
) -> ApiResult<()> {
    let key = member_key(guild_id, member.user.id);
    batch.set(&key, &CachedMember { guild_id, member })?;
    if Some(member.user.id) == ctx.bot_id {
        batch.persist(&key);
    } else {
        batch.expire(&key, ctx.state.member_ttl, ctx.now_ms);
    }
    Ok(())
}

fn wants_member(ctx: &UpdateContext<'_>, user_id: Id<UserMarker>) -> bool {
    ctx.state.member || Some(user_id) == ctx.bot_id
}

fn presence_user_id(presence: &Presence) -> Id<UserMarker> {
    match &presence.user {
        UserOrId::User(user) => user.id,
        UserOrId::UserId { id } => *id,
    }
}

fn set_channel(
    batch: &mut Batch,
    guild_id: Option<Id<GuildMarker>>,
    channel: &Channel,
) -> ApiResult<()> {
    let guild_id = channel.guild_id.or(guild_id);
    let key = channel_or_thread_key(guild_id, channel);
    if channel.guild_id.is_none() && guild_id.is_some() {
        let mut channel = channel.clone();
        channel.guild_id = guild_id;
        batch.set(&key, &channel)
    } else {
        batch.set(&key, channel)
    }
}

/// Where a channel is stored: threads under `thread:`, everything else under
/// `channel:`.
fn channel_or_thread_key(guild_id: Option<Id<GuildMarker>>, channel: &Channel) -> String {
    match guild_id {
        Some(guild_id) if channel.kind.is_thread() => thread_key(guild_id, channel.id),
        _ => channel_key(guild_id, channel.id),
    }
}

fn old_if(ctx: &UpdateContext<'_>, value: Option<Value>) -> Option<Value> {
    if ctx.state.old {
        value
    } else {
        None
    }
}

/// Overlay `update` onto `cached`. Keys in `keep_if_null` are only copied when
/// the update has a non-null value for them.
fn overlay(cached: Option<Value>, update: Value, keep_if_null: &[&str]) -> Value {
    let (Some(Value::Object(mut cached)), Value::Object(update)) = (cached, update.clone()) else {
        return update;
    };
    for (key, value) in update {
        if value.is_null() && keep_if_null.contains(&key.as_str()) && cached.contains_key(&key) {
            continue;
        }
        cached.insert(key, value);
    }
    Value::Object(cached)
}

/// A presence user that is only `{"id": ...}` must not replace a cached full user.
fn merge_presence(cached: Option<Value>, update: Value) -> Value {
    let partial_user = update
        .get("user")
        .and_then(Value::as_object)
        .is_some_and(|user| user.len() <= 1);
    let cached_user = cached.as_ref().and_then(|c| c.get("user")).cloned();
    let mut merged = update;
    if partial_user {
        if let (Some(user), Some(object)) = (cached_user, merged.as_object_mut()) {
            object.insert("user".into(), user);
        }
    }
    merged
}

// ---------------------------------------------------------------------------
// Event handling

/// Apply one gateway event to the cache.
///
/// GUILD_CREATE's lists are moved out of `event` into their own keys, so only
/// the guild's scalar fields are left in it afterwards.
///
/// With the cache disabled (`STATE_ENABLED=false`) nothing is read or written.
/// The callers skip it then too, but a new call site must not be able to
/// write to a Redis that other services expect to be left alone.
pub async fn update<C: ConnectionLike + Send>(
    conn: &mut C,
    event: &mut Event,
    ctx: &UpdateContext<'_>,
) -> ApiResult<Outcome> {
    let state = ctx.state;
    if !state.enabled {
        return Ok(Outcome::default());
    }
    let mut batch = Batch::new();
    let mut outcome = Outcome::default();

    match event {
        Event::ChannelCreate(data) => {
            set_channel(&mut batch, None, &data.0)?;
        }
        Event::ChannelUpdate(data) => {
            let key = channel_key(data.guild_id, data.id);
            if state.old {
                outcome.old = get(conn, &key).await?;
            }
            set_channel(&mut batch, None, &data.0)?;
        }
        Event::ChannelDelete(data) => {
            let key = channel_key(data.guild_id, data.id);
            if state.old {
                outcome.old = get(conn, &key).await?;
            }
            clear_channel(conn, &mut batch, &key, data.id.get()).await?;
            if let Some(guild_id) = data.guild_id {
                let parent: HashSet<u64> = [data.id.get()].into_iter().collect();
                let threads = cached_threads(conn, guild_id, Some(&parent)).await?;
                clear_threads(conn, &mut batch, guild_id, threads).await?;
            }
        }
        Event::ChannelPinsUpdate(data) => {
            let mut key = channel_key(data.guild_id, data.channel_id);
            let mut cached = get(conn, &key).await?;
            if let (None, Some(guild_id)) = (&cached, data.guild_id) {
                key = thread_key(guild_id, data.channel_id);
                cached = get(conn, &key).await?;
            }
            if let Some(Value::Object(mut channel)) = cached {
                channel.insert(
                    "last_pin_timestamp".into(),
                    serde_json::to_value(data.last_pin_timestamp)?,
                );
                batch.set(&key, &channel)?;
            }
        }
        Event::ThreadCreate(data) => {
            set_channel(&mut batch, None, &data.0)?;
        }
        Event::ThreadUpdate(data) => {
            let key = channel_or_thread_key(data.guild_id, &data.0);
            if state.old {
                outcome.old = get(conn, &key).await?;
            }
            // Like GUILD_CREATE, the cache only holds active threads.
            if data
                .thread_metadata
                .as_ref()
                .is_some_and(|meta| meta.archived)
            {
                clear_channel(conn, &mut batch, &key, data.id.get()).await?;
            } else {
                set_channel(&mut batch, None, &data.0)?;
            }
        }
        Event::ThreadDelete(data) => {
            let key = thread_key(data.guild_id, data.id);
            if state.old {
                outcome.old = get(conn, &key).await?;
            }
            clear_channel(conn, &mut batch, &key, data.id.get()).await?;
        }
        Event::ThreadListSync(data) => {
            // All active threads of `channel_ids` (or of the whole guild when
            // it is empty); cached threads that are not listed are gone.
            let parents: HashSet<u64> = data.channel_ids.iter().map(|id| id.get()).collect();
            let listed: HashSet<Id<ChannelMarker>> = data.threads.iter().map(|t| t.id).collect();
            let scope = (!parents.is_empty()).then_some(&parents);
            let cached = cached_threads(conn, data.guild_id, scope).await?;
            let gone = cached.into_iter().filter(|id| !listed.contains(id));
            clear_threads(conn, &mut batch, data.guild_id, gone).await?;
            for thread in &data.threads {
                set_channel(&mut batch, Some(data.guild_id), thread)?;
            }
        }
        Event::GuildCreate(data) => match data.as_mut() {
            GuildCreate::Available(guild) => {
                let keep: HashSet<u64> = guild
                    .channels
                    .iter()
                    .chain(&guild.threads)
                    .map(|channel| channel.id.get())
                    .collect();
                let previous = clear_guild(conn, &mut batch, guild.id, &keep).await?;
                outcome.guild_known = previous.is_some();
                outcome.old = old_if(ctx, previous.clone());
                outcome.previous_guild = previous;
                write_guild(&mut batch, ctx, guild)?;
            }
            GuildCreate::Unavailable(guild) => {
                let key = guild_key(guild.id);
                let previous = get(conn, &key).await?;
                outcome.guild_known = previous.is_some();
                outcome.old = old_if(ctx, previous.clone());
                batch.set(
                    &key,
                    &merge::mark_unavailable(previous.clone(), guild.id.get()),
                )?;
                outcome.previous_guild = previous;
            }
        },
        Event::GuildDelete(data) => {
            if data.unavailable == Some(true) {
                let key = guild_key(data.id);
                let previous = get(conn, &key).await?;
                outcome.old = old_if(ctx, previous.clone());
                batch.set(
                    &key,
                    &merge::mark_unavailable(previous.clone(), data.id.get()),
                )?;
                outcome.previous_guild = previous;
            } else {
                let previous = clear_guild(conn, &mut batch, data.id, &HashSet::new()).await?;
                outcome.old = old_if(ctx, previous.clone());
                outcome.previous_guild = previous;
            }
        }
        Event::UnavailableGuild(data) => {
            let key = guild_key(data.id);
            let previous = get(conn, &key).await?;
            outcome.old = old_if(ctx, previous.clone());
            batch.set(&key, &merge::mark_unavailable(previous, data.id.get()))?;
        }
        Event::GuildUpdate(data) => {
            let key = guild_key(data.id);
            let previous = get(conn, &key).await?;
            outcome.old = old_if(ctx, previous.clone());
            let merged = merge::merge_guild_update(previous, serde_json::to_value(&data.0)?);
            batch.set(&key, &merged)?;
            for role in &data.roles {
                batch.set(&role_key(data.id, role.id), role)?;
            }
            if state.emoji {
                for emoji in &data.emojis {
                    batch.set(&emoji_key(data.id, emoji.id), emoji)?;
                }
            }
        }
        Event::GuildEmojisUpdate(data) => {
            if state.emoji {
                let cached: Vec<String> = members(conn, &keys::guild_index(data.guild_id))
                    .await?
                    .into_iter()
                    .filter(|key| keys::has_prefix(key, EMOJI_KEY))
                    .collect();
                let keep: HashSet<String> = data
                    .emojis
                    .iter()
                    .map(|emoji| emoji_key(data.guild_id, emoji.id))
                    .collect();
                if state.old {
                    let values = get_all(conn, &cached).await?;
                    outcome.old = Some(Value::Array(values.into_iter().flatten().collect()));
                }
                for key in cached.iter().filter(|key| !keep.contains(*key)) {
                    batch.del(key);
                }
                for emoji in &data.emojis {
                    batch.set(&emoji_key(data.guild_id, emoji.id), emoji)?;
                }
            }
        }
        Event::GuildStickersUpdate(data) => {
            let key = guild_key(data.guild_id);
            if let Some(Value::Object(mut guild)) = get(conn, &key).await? {
                if state.old {
                    outcome.old = guild.get("stickers").cloned();
                }
                guild.insert("stickers".into(), serde_json::to_value(&data.stickers)?);
                batch.set(&key, &guild)?;
            }
        }
        Event::MemberAdd(data) => {
            let user_id = data.member.user.id;
            let key = member_key(data.guild_id, user_id);
            // Discord may resend MEMBER_ADD for existing members; only count
            // new ones when that can be checked.
            let is_new = joined_recently(data.member.joined_at, ctx.now_ms)
                && (!state.member || !exists(conn, &key).await?);
            if wants_member(ctx, user_id) {
                set_member(&mut batch, ctx, data.guild_id, &data.member)?;
            }
            if is_new {
                adjust_member_count(conn, &mut batch, data.guild_id, 1).await?;
            }
        }
        Event::MemberRemove(data) => {
            if state.member {
                let key = member_key(data.guild_id, data.user.id);
                if state.old {
                    outcome.old = get(conn, &key).await?;
                }
                batch.del(&key);
            }
            if state.presence {
                batch.del(&presence_key(data.guild_id, data.user.id));
            }
            adjust_member_count(conn, &mut batch, data.guild_id, -1).await?;
        }
        Event::MemberUpdate(data) => {
            if wants_member(ctx, data.user.id) {
                let key = member_key(data.guild_id, data.user.id);
                let previous = get(conn, &key).await?;
                outcome.old = old_if(ctx, previous.clone());
                let mut update = serde_json::to_value(data.as_ref())?;
                if previous.is_none() {
                    // Not cached: insert it, filling fields MemberUpdate lacks.
                    if let Value::Object(object) = &mut update {
                        for (field, default) in [("deaf", false), ("mute", false)] {
                            if object.get(field).is_none_or(Value::is_null) {
                                object.insert(field.into(), Value::Bool(default));
                            }
                        }
                        if object.get("flags").is_none_or(Value::is_null) {
                            object.insert("flags".into(), Value::from(0));
                        }
                    }
                }
                let merged = overlay(previous, update, &["deaf", "mute", "flags", "joined_at"]);
                batch.set(&key, &merged)?;
                if Some(data.user.id) == ctx.bot_id {
                    batch.persist(&key);
                } else {
                    batch.expire(&key, state.member_ttl, ctx.now_ms);
                }
            }
        }
        Event::MemberChunk(data) => {
            if state.member {
                for member in &data.members {
                    set_member(&mut batch, ctx, data.guild_id, member)?;
                }
            } else if let Some(bot) = data.members.iter().find(|m| Some(m.user.id) == ctx.bot_id) {
                set_member(&mut batch, ctx, data.guild_id, bot)?;
            }
            if state.presence {
                for presence in &data.presences {
                    let key = presence_key(data.guild_id, presence_user_id(presence));
                    if presence.status == Status::Offline {
                        batch.del(&key);
                    } else {
                        let mut presence = presence.clone();
                        presence.guild_id = data.guild_id;
                        batch.set(&key, &presence)?;
                    }
                }
            }
        }
        Event::MessageCreate(data) => {
            if state.message {
                let key = message_key(data.channel_id, data.id);
                batch.set(&key, &data.0)?;
                batch.expire(&key, state.message_ttl, ctx.now_ms);
            }
        }
        Event::MessageUpdate(data) => {
            if state.message {
                let key = message_key(data.channel_id, data.id);
                if let Some(previous) = get(conn, &key).await? {
                    // MESSAGE_UPDATE is a full message now, but its `tts` is
                    // always false.
                    let mut update = serde_json::to_value(&data.0)?;
                    if let (Some(tts), Value::Object(object)) = (previous.get("tts"), &mut update) {
                        object.insert("tts".into(), tts.clone());
                    }
                    outcome.old = old_if(ctx, Some(previous.clone()));
                    batch.set(&key, &overlay(Some(previous), update, &[]))?;
                    batch.expire(&key, state.message_ttl, ctx.now_ms);
                }
            }
        }
        Event::MessageDelete(data) => {
            if state.message {
                let key = message_key(data.channel_id, data.id);
                if state.old {
                    outcome.old = get(conn, &key).await?;
                }
                batch.del(&key);
            }
        }
        Event::MessageDeleteBulk(data) => {
            if state.message {
                let message_keys: Vec<String> = data
                    .ids
                    .iter()
                    .map(|id| message_key(data.channel_id, *id))
                    .collect();
                if state.old {
                    let values = get_all(conn, &message_keys).await?;
                    outcome.old = Some(Value::Array(values.into_iter().flatten().collect()));
                }
                for key in &message_keys {
                    batch.del(key);
                }
            }
        }
        Event::PresenceUpdate(data) => {
            if state.presence {
                let key = presence_key(data.guild_id, presence_user_id(&data.0));
                let previous = if state.old || matches!(data.user, UserOrId::UserId { .. }) {
                    get(conn, &key).await?
                } else {
                    None
                };
                if data.status == Status::Offline {
                    batch.del(&key);
                } else {
                    let update = serde_json::to_value(&data.0)?;
                    batch.set(&key, &merge_presence(previous.clone(), update))?;
                }
                outcome.old = old_if(ctx, previous);
            }
        }
        Event::Ready(data) => {
            ready(conn, data, ctx).await?;
        }
        Event::RoleCreate(data) => {
            batch.set(&role_key(data.guild_id, data.role.id), &data.role)?;
        }
        Event::RoleUpdate(data) => {
            let key = role_key(data.guild_id, data.role.id);
            if state.old {
                outcome.old = get(conn, &key).await?;
            }
            batch.set(&key, &data.role)?;
        }
        Event::RoleDelete(data) => {
            let key = role_key(data.guild_id, data.role_id);
            if state.old {
                outcome.old = get(conn, &key).await?;
            }
            batch.del(&key);
        }
        Event::UserUpdate(data) => {
            if state.old {
                outcome.old = get(conn, BOT_USER_KEY).await?;
            }
            batch.set(BOT_USER_KEY, &data.0)?;
        }
        Event::VoiceStateUpdate(data) if state.voice => {
            if let Some(guild_id) = data.guild_id {
                let key = voice_key(guild_id, data.user_id);
                if state.old {
                    outcome.old = get(conn, &key).await?;
                }
                match data.channel_id {
                    Some(_) => batch.set(&key, &voice_value(&data.0, guild_id)?)?,
                    None => batch.del(&key),
                }
            }
        }
        _ => {}
    }

    batch.commit(conn).await?;
    Ok(outcome)
}

/// Plan all keys of an available GUILD_CREATE. The lists that are stored
/// under their own keys are taken out of `guild`, which leaves exactly the
/// guild blob behind (with empty lists, as consumers expect).
fn write_guild(batch: &mut Batch, ctx: &UpdateContext<'_>, guild: &mut Guild) -> ApiResult<()> {
    let state = ctx.state;
    let guild_id = guild.id;

    // Threads get their own keys, which the thread events keep current. As in
    // earlier versions, the blob also keeps this list of them: a snapshot as
    // of GUILD_CREATE, for compatibility.
    let channels = std::mem::take(&mut guild.channels);
    for channel in channels.iter().chain(guild.threads.iter()) {
        set_channel(batch, Some(guild_id), channel)?;
    }
    for role in std::mem::take(&mut guild.roles) {
        batch.set(&role_key(guild_id, role.id), &role)?;
    }
    let emojis = std::mem::take(&mut guild.emojis);
    if state.emoji {
        for emoji in &emojis {
            batch.set(&emoji_key(guild_id, emoji.id), emoji)?;
        }
    }
    let voice_states = std::mem::take(&mut guild.voice_states);
    if state.voice {
        for voice in voice_states {
            batch.set(
                &voice_key(guild_id, voice.user_id),
                &voice_value(&voice, guild_id)?,
            )?;
        }
    }
    for member in std::mem::take(&mut guild.members)
        .iter()
        .filter(|m| wants_member(ctx, m.user.id))
    {
        set_member(batch, ctx, guild_id, member)?;
    }
    let presences = std::mem::take(&mut guild.presences);
    if state.presence {
        for mut presence in presences
            .into_iter()
            .filter(|p| p.status != Status::Offline)
        {
            presence.guild_id = guild_id;
            batch.set(
                &presence_key(guild_id, presence_user_id(&presence)),
                &presence,
            )?;
        }
    }

    let mut blob = serde_json::to_value(&*guild)?;
    if let Value::Object(object) = &mut blob {
        // Earlier versions always stored `unavailable`; twilight 0.17 omits it.
        if object.get("unavailable").is_none_or(Value::is_null) {
            object.insert("unavailable".into(), Value::Bool(false));
        }
    }
    batch.set(&guild_key(guild_id), &blob)?;
    Ok(())
}

/// A voice state as stored: with `guild_id`, also on its nested member
/// (twilight removed `Member::guild_id`, earlier versions stored it).
fn voice_value(voice: &VoiceState, guild_id: Id<GuildMarker>) -> ApiResult<Value> {
    let mut value = serde_json::to_value(voice)?;
    let guild = Value::String(guild_id.to_string());
    if let Value::Object(object) = &mut value {
        object.insert("guild_id".into(), guild.clone());
        if let Some(Value::Object(member)) = object.get_mut("member") {
            member.insert("guild_id".into(), guild);
        }
    }
    Ok(value)
}

async fn adjust_member_count<C: ConnectionLike + Send>(
    conn: &mut C,
    batch: &mut Batch,
    guild_id: Id<GuildMarker>,
    delta: i64,
) -> ApiResult<()> {
    let key = guild_key(guild_id);
    if let Some(guild) = get(conn, &key).await? {
        if let Some(adjusted) = merge::adjust_member_count(guild, delta) {
            batch.set(&key, &adjusted)?;
        }
    }
    Ok(())
}

/// READY: store the bot user, add stubs for guilds that are not cached yet,
/// and drop cached guilds of this shard that READY no longer lists.
async fn ready<C: ConnectionLike + Send>(
    conn: &mut C,
    data: &Ready,
    _ctx: &UpdateContext<'_>,
) -> ApiResult<()> {
    let mut pipe = redis::pipe();
    pipe.atomic();
    pipe.cmd("SET")
        .arg(BOT_USER_KEY)
        .arg(serde_json::to_string(&data.user)?)
        .ignore();
    let guild_index = keys::type_index(GUILD_KEY);
    for guild in &data.guilds {
        let key = guild_key(guild.id);
        let mut stub = Map::new();
        stub.insert("id".into(), Value::String(guild.id.to_string()));
        stub.insert("unavailable".into(), Value::Bool(true));
        // NX: keep a fully cached guild until its GUILD_CREATE replaces it.
        pipe.cmd("SET")
            .arg(&key)
            .arg(serde_json::to_string(&stub)?)
            .arg("NX")
            .ignore();
        pipe.cmd("SADD").arg(&guild_index).arg(&key).ignore();
    }
    pipe.query_async::<()>(conn).await?;

    if let Some(shard) = data.shard {
        let listed: HashSet<u64> = data.guilds.iter().map(|g| g.id.get()).collect();
        let cached = members(conn, &guild_index)
            .await?
            .into_iter()
            .filter_map(|key| keys::guild_id_of(&key));
        let stale = keys::stale_guilds(cached, &listed, shard.number(), shard.total());
        for guild_id in stale.into_iter().filter_map(Id::new_checked) {
            let mut batch = Batch::new();
            clear_guild(conn, &mut batch, guild_id, &HashSet::new()).await?;
            batch.commit(conn).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn joins_are_recognised_by_a_recent_joined_at() {
        use twilight_model::util::Timestamp;
        let now_ms = 1_800_000_000_000i64;
        let at = |ms: i64| Timestamp::from_micros(ms * 1000).ok();
        assert!(joined_recently(at(now_ms - 1_000), now_ms));
        assert!(!joined_recently(at(now_ms - RECENT_JOIN_MS - 1), now_ms));
        assert!(!joined_recently(None, now_ms), "guests have no joined_at");

        // Spelled out in minutes: the bounds above follow the constant, so
        // they cannot tell a window of seconds or of an hour from this one.
        let minutes_ago = |minutes: i64| at(now_ms - minutes * 60 * 1000);
        assert!(joined_recently(minutes_ago(14), now_ms), "a join");
        assert!(!joined_recently(minutes_ago(16), now_ms), "a re-sent add");
    }

    #[test]
    fn overlay_keeps_selected_fields_on_null() {
        let cached = json!({"deaf": true, "nick": "a", "flags": 2, "roles": ["1"]});
        let update = json!({"deaf": null, "nick": null, "flags": null, "roles": ["2"]});
        let merged = overlay(Some(cached), update, &["deaf", "flags"]);
        assert_eq!(
            merged,
            json!({"deaf": true, "nick": null, "flags": 2, "roles": ["2"]})
        );
    }

    #[test]
    fn overlay_without_cache_is_the_update() {
        let update = json!({"a": 1});
        assert_eq!(overlay(None, update.clone(), &["a"]), update);
    }

    #[test]
    fn partial_presence_user_keeps_cached_user() {
        let cached = json!({"user": {"id": "1", "username": "u"}, "status": "online"});
        let update = json!({"user": {"id": "1"}, "status": "idle"});
        let merged = merge_presence(Some(cached), update);
        assert_eq!(merged["user"]["username"], "u");
        assert_eq!(merged["status"], "idle");

        let full = json!({"user": {"id": "1", "username": "new"}, "status": "dnd"});
        assert_eq!(merge_presence(None, full.clone()), full);
    }
}
