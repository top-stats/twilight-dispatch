//! Redis state cache.
//!
//! The key layout and the JSON stored under each key are a contract with the
//! consumers of the cache and are kept compatible with earlier versions (see
//! [`crate::constants`] and [`keys`]). Each gateway event is applied with a
//! few reads (removing a guild or a channel also reads the message index of
//! every removed channel and thread, and `GUILD_EMOJIS_UPDATE` reads the
//! guild's whole key index), followed by one atomic write ([`batch::Batch`],
//! or one script for an edit of a previous session, see [`update_stale`]).
//! Events of one guild are applied in order by the same worker (see
//! [`crate::gateway::worker`]), which makes the read-modify-write updates here
//! safe.
//!
//! READY is the exception: it commits the bot user and the guild stubs in one
//! transaction, then removes each stale guild of the shard in a transaction
//! of its own, which keeps every transaction the size of one guild. A failure
//! part way leaves the remaining stale guilds to the shard's next READY, which
//! finds them the same way.
//!
//! A `GUILD_CREATE` of a cached guild (Discord sends one for every guild after
//! a new session) replaces what is cached under the guild, except the messages
//! of the channels it still lists and, with `STATE_MEMBER`, the members: it
//! lists only some members of a large guild. [`MEMBER_RESYNC_GRACE_MS`] says
//! when the others expire. Removing a guild (`GUILD_DELETE`, READY no longer
//! listing it) removes its members.

pub mod batch;
pub mod jobs;
pub mod keys;
pub mod merge;

use self::{batch::Batch, keys::stored_key};
use crate::{
    config::StateConfig,
    constants::{
        channel_key, emoji_key, guild_key, member_key, message_key, presence_key, role_key,
        thread_key, voice_key, BOT_USER_KEY, EMOJI_KEY, EXPIRY_ZSET, GUILD_KEY, MEMBER_KEY,
    },
    models::ApiResult,
};

use redis::{aio::ConnectionLike, Script};
use serde::Serialize;
use serde_json::Value;
use std::{collections::HashSet, sync::LazyLock};
use twilight_model::{
    channel::Channel,
    gateway::{
        event::Event,
        payload::incoming::{GuildCreate, MessageUpdate, Ready},
        presence::{Status, UserOrId},
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
    /// What to cache (the `STATE_*` settings).
    pub state: &'a StateConfig,
    /// The bot's own user id, `None` when it is not known. The bot's member is
    /// cached even with `STATE_MEMBER=false` and never expires.
    pub bot_id: Option<Id<UserMarker>>,
    /// The current time in unix milliseconds: the base of expiry deadlines
    /// and of the recent-join check ([`joined_recently`]).
    pub now_ms: i64,
}

/// What a cache update observed.
#[derive(Debug, Default, PartialEq)]
pub struct Outcome {
    /// The previous cached value, when `STATE_OLD` is enabled. For
    /// `GUILD_EMOJIS_UPDATE` and `MESSAGE_DELETE_BULK` it is an array of the
    /// cached values that were found; for `GUILD_STICKERS_UPDATE` it is the
    /// guild's previous `stickers`.
    pub old: Option<Value>,
    /// For `GUILD_CREATE` / `GUILD_DELETE`: the guild as it was cached before.
    pub previous_guild: Option<Value>,
    /// For `GUILD_CREATE`: whether the guild was already known (cached in
    /// full, or as an unavailable stub from READY or an outage).
    pub guild_known: bool,
}

/// A member as stored in Redis. twilight removed `guild_id` from `Member`, but
/// cached members have always carried it.
#[derive(Serialize)]
pub struct CachedMember<'a> {
    /// The guild the member belongs to.
    pub guild_id: Id<GuildMarker>,
    /// The member, serialized next to `guild_id` (flattened).
    #[serde(flatten)]
    pub member: &'a Member,
}

// ---------------------------------------------------------------------------
// Reads

/// GET a logical key and parse it as JSON. `None` when the key is not cached.
///
/// # Errors
///
/// Fails when the Redis command fails or the cached value is not JSON.
pub async fn get<C: ConnectionLike + Send>(conn: &mut C, key: &str) -> ApiResult<Option<Value>> {
    let raw: Option<String> = redis::cmd("GET")
        .arg(stored_key(key))
        .query_async(conn)
        .await?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
}

/// MGET logical keys (always MGET, even for one key) and parse them as JSON,
/// in the order of `keys` (`None` for a key that is not cached). No keys send
/// no command.
///
/// # Errors
///
/// Fails when the Redis command fails or a cached value is not JSON.
pub async fn get_all<C: ConnectionLike + Send>(
    conn: &mut C,
    keys: &[String],
) -> ApiResult<Vec<Option<Value>>> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    let stored: Vec<String> = keys.iter().map(|key| stored_key(key)).collect();
    let raw: Vec<Option<String>> = redis::cmd("MGET").arg(stored).query_async(conn).await?;
    raw.into_iter()
        .map(|raw| {
            raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
                .transpose()
        })
        .collect()
}

/// The members of a set (SMEMBERS), in no particular order. `set` is used
/// verbatim: index sets are not planned.
///
/// # Errors
///
/// Fails when the Redis command fails.
pub async fn members<C: ConnectionLike + Send>(conn: &mut C, set: &str) -> ApiResult<Vec<String>> {
    Ok(redis::cmd("SMEMBERS").arg(set).query_async(conn).await?)
}

/// The ids of the cached guilds of shard `shard` of `shard_total`.
///
/// # Errors
///
/// Fails when the Redis command fails.
pub async fn shard_guilds<C: ConnectionLike + Send>(
    conn: &mut C,
    shard: u32,
    shard_total: u32,
) -> ApiResult<Vec<u64>> {
    let cached = members(conn, &keys::type_index(GUILD_KEY)).await?;
    Ok(cached
        .into_iter()
        .filter_map(|key| keys::guild_id_of(&key))
        .filter(|id| keys::shard_for_guild(*id, shard_total) == shard)
        .collect())
}

/// Whether a logical key is cached.
///
/// # Errors
///
/// Fails when the Redis command fails.
pub async fn exists<C: ConnectionLike + Send>(conn: &mut C, key: &str) -> ApiResult<bool> {
    let count: u64 = redis::cmd("EXISTS")
        .arg(stored_key(key))
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

/// What [`clear_guild`] does with the member keys of the guild.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MemberKeys {
    /// Delete them with everything else: the guild is gone.
    Delete,
    /// Leave them cached, with their values and expiry deadlines, for a
    /// `GUILD_CREATE` re-sync ([`keep_members`]).
    Keep,
}

/// Plan the removal of a guild and everything cached under it. Returns the
/// cached guild and the stored keys of the members it kept.
///
/// Channels in `keep` are about to be written again (`GUILD_CREATE`), so
/// their cached messages survive. With [`MemberKeys::Keep`] the members
/// survive too, and stay indexed: `guild_keys:<guild>` is not deleted as a
/// whole but loses only the keys that are removed.
async fn clear_guild<C: ConnectionLike + Send>(
    conn: &mut C,
    batch: &mut Batch,
    guild_id: Id<GuildMarker>,
    keep: &HashSet<u64>,
    member_keys: MemberKeys,
) -> ApiResult<(Option<Value>, Vec<String>)> {
    let key = guild_key(guild_id);
    let index = keys::guild_index(guild_id);
    let previous = get(conn, &key).await?;
    let mut kept_members = Vec::new();

    for child in members(conn, &index).await? {
        if member_keys == MemberKeys::Keep && keys::has_prefix(&child, MEMBER_KEY) {
            kept_members.push(child);
            continue;
        }
        let cleared = if let Some(thread_id) = keys::thread_id_of(&child) {
            if keep.contains(&thread_id) {
                false
            } else {
                clear_channel(conn, batch, &child, thread_id).await?;
                true
            }
        } else {
            // twilight never produces id 0; a `channel:0` would only be
            // deleted, without looking up its messages.
            match keys::channel_id_of(&child).and_then(Id::<ChannelMarker>::new_checked) {
                Some(channel_id) if keep.contains(&channel_id.get()) => false,
                Some(channel_id) => {
                    clear_channel(conn, batch, &child, channel_id.get()).await?;
                    true
                }
                None => {
                    batch.del(&child);
                    true
                }
            }
        };
        // A stored channel key has no guild segment, so deleting it does not
        // take it out of `guild_keys:<guild>`.
        if cleared && member_keys == MemberKeys::Keep {
            batch.unindex_raw(&index, &child);
        }
    }
    if member_keys == MemberKeys::Delete {
        batch.del_raw(index);
    }
    batch.del_raw(keys::thread_index(guild_id));
    batch.del(&key);

    Ok((previous, kept_members))
}

/// Plan the removal of a guild that is gone (`GUILD_DELETE` without
/// `unavailable`, or READY no longer lists it) and of everything cached under
/// it, members included. Returns the cached guild.
async fn remove_guild<C: ConnectionLike + Send>(
    conn: &mut C,
    batch: &mut Batch,
    guild_id: Id<GuildMarker>,
) -> ApiResult<Option<Value>> {
    let keep = HashSet::new();
    let (previous, _) = clear_guild(conn, batch, guild_id, &keep, MemberKeys::Delete).await?;
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

/// How recent `joined_at` must be for a `GUILD_MEMBER_ADD` to count as a join,
/// in milliseconds.
pub const RECENT_JOIN_MS: i64 = 15 * 60 * 1000;

/// Whether a `GUILD_MEMBER_ADD` is a new join rather than a re-sent event for
/// an existing member (Discord does that; the re-sent event carries the
/// original `joined_at`). `now_ms` is in unix milliseconds.
pub fn joined_recently(joined_at: Option<twilight_model::util::Timestamp>, now_ms: i64) -> bool {
    joined_at.is_some_and(|at| now_ms - at.as_micros() / 1000 < RECENT_JOIN_MS)
}

/// How long a member that a `GUILD_CREATE` re-sync does not list stays
/// cached with `STATE_MEMBER_TTL=0`, in milliseconds: 24 hours.
///
/// Discord sends `GUILD_CREATE` again for every guild after a new session,
/// and it lists only some members of a large guild: none that are offline
/// above `LARGE_THRESHOLD` members, and only the bot and the members in voice
/// channels over 75k members. With `STATE_MEMBER` the members cached before
/// stay cached and indexed; those it does not list get this deadline in
/// [`EXPIRY_ZSET`], unless they have one already (another re-sync does not
/// postpone it). A `GUILD_CREATE` that lists every member removes the others
/// at once instead. Writing a member again (`GUILD_MEMBERS_CHUNK`,
/// `GUILD_MEMBER_ADD`, `GUILD_MEMBER_UPDATE`) drops its deadline. The grace
/// outlives the bot's hourly re-chunk of every guild, so the members still in
/// the guild are confirmed before it ends, while those who left while the bot
/// was disconnected (their `GUILD_MEMBER_REMOVE` was missed) are removed by
/// [`jobs::expire_due`] when it does. With a TTL the kept members keep the
/// deadline they had, and one without a deadline gets the TTL.
pub const MEMBER_RESYNC_GRACE_MS: u64 = 24 * 60 * 60 * 1000;

fn set_member(
    batch: &mut Batch,
    ctx: &UpdateContext<'_>,
    guild_id: Id<GuildMarker>,
    member: &Member,
) -> ApiResult<()> {
    let key = member_key(guild_id, member.user.id);
    batch.set(&key, &CachedMember { guild_id, member })?;
    set_member_expiry(batch, ctx, &key, member.user.id);
    Ok(())
}

/// Plan the expiry of a member that was just written: `STATE_MEMBER_TTL`
/// from now. With a TTL of 0, and for the bot's own member, it gets none, and
/// a deadline set before (a re-sync's grace, see [`keep_members`]) is dropped.
fn set_member_expiry(
    batch: &mut Batch,
    ctx: &UpdateContext<'_>,
    key: &str,
    user_id: Id<UserMarker>,
) {
    if ctx.state.member_ttl == 0 || Some(user_id) == ctx.bot_id {
        batch.persist(key);
    } else {
        batch.expire(key, ctx.state.member_ttl, ctx.now_ms);
    }
}

fn wants_member(ctx: &UpdateContext<'_>, user_id: Id<UserMarker>) -> bool {
    ctx.state.member || Some(user_id) == ctx.bot_id
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

/// A copy of the previous cached value for [`Outcome::old`], only when
/// `STATE_OLD` is enabled (so nothing is cloned otherwise).
fn old_if(ctx: &UpdateContext<'_>, value: Option<&Value>) -> Option<Value> {
    if ctx.state.old {
        value.cloned()
    } else {
        None
    }
}

/// Overlay `update` onto `cached`. Keys in `keep_if_null` are only copied when
/// the update has a non-null value for them. Unless both are objects the
/// result is `update`.
fn overlay(cached: Option<Value>, update: Value, keep_if_null: &[&str]) -> Value {
    let Value::Object(update) = update else {
        return update;
    };
    let Some(Value::Object(mut cached)) = cached else {
        return Value::Object(update);
    };
    for (key, value) in update {
        if value.is_null() && keep_if_null.contains(&key.as_str()) && cached.contains_key(&key) {
            continue;
        }
        cached.insert(key, value);
    }
    Value::Object(cached)
}

/// Message lists that twilight leaves out when they are empty
/// (`skip_serializing_if = "Vec::is_empty"`) and that an edit can empty.
/// `MESSAGE_UPDATE` carries the full message, so a list it leaves out is empty
/// now and must not keep its cached value. `reactions`, `mention_channels`
/// and `message_snapshots` are left out the same way but keep their cached
/// value: an edit does not reliably carry them.
const EMPTIED_BY_EDITS: [&str; 1] = ["components"];

/// A cached message with a `MESSAGE_UPDATE` applied. `MESSAGE_UPDATE` is a
/// full message now, so every field it carries replaces the cached one, except
/// `tts`: it is always false in an update, so the cached one is kept. The lists
/// in `EMPTIED_BY_EDITS` that the update leaves out are dropped.
fn edited_message(mut previous: Value, data: &MessageUpdate) -> ApiResult<Value> {
    let mut update = serde_json::to_value(&data.0)?;
    if let (Some(tts), Value::Object(object)) = (previous.get("tts"), &mut update) {
        object.insert("tts".into(), tts.clone());
    }
    if let (Value::Object(cached), Value::Object(object)) = (&mut previous, &update) {
        for list in EMPTIED_BY_EDITS {
            if !object.contains_key(list) {
                cached.remove(list);
            }
        }
    }
    Ok(overlay(Some(previous), update, &[]))
}

/// A presence user that is only `{"id": ...}` must not replace a cached full user.
fn merge_presence(cached: Option<Value>, mut update: Value) -> Value {
    let partial_user = update
        .get("user")
        .and_then(Value::as_object)
        .is_some_and(|user| user.len() <= 1);
    if partial_user {
        if let (Some(Value::Object(mut cached)), Some(object)) = (cached, update.as_object_mut()) {
            if let Some(user) = cached.remove("user") {
                object.insert("user".into(), user);
            }
        }
    }
    update
}

// ---------------------------------------------------------------------------
// Event handling

/// Apply one gateway event to the cache.
///
/// `GUILD_CREATE`'s lists that are stored under their own keys
/// ([`merge::SPLIT_GUILD_FIELDS`]) are taken out of `event`, also when their
/// `STATE_*` switch is off; `threads` and the guild's other fields stay in it.
///
/// With the cache disabled (`STATE_ENABLED=false`) nothing is read or written
/// by this function (the background jobs in [`jobs`] still run). The callers
/// skip it then too, but a new call site must not be able to write to a Redis
/// that other services expect to be left alone.
///
/// # Errors
///
/// Fails when a Redis command fails or a value cannot be converted to or from
/// JSON. The reads come first, so a failure before the write leaves the cache
/// as it was; READY, which writes several transactions, keeps those it
/// committed.
#[allow(
    clippy::too_many_lines,
    reason = "a flat match with one arm per event kind"
)]
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
                // A re-sync lists only some members of a large guild.
                let member_keys = if state.member {
                    MemberKeys::Keep
                } else {
                    MemberKeys::Delete
                };
                let (previous, kept_members) =
                    clear_guild(conn, &mut batch, guild.id, &keep, member_keys).await?;
                outcome.guild_known = previous.is_some();
                outcome.old = old_if(ctx, previous.as_ref());
                outcome.previous_guild = previous;
                keep_members(&mut batch, ctx, guild, &kept_members);
                write_guild(&mut batch, ctx, guild)?;
            }
            GuildCreate::Unavailable(guild) => {
                let key = guild_key(guild.id);
                let previous = get(conn, &key).await?;
                outcome.guild_known = previous.is_some();
                outcome.old = old_if(ctx, previous.as_ref());
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
                outcome.old = old_if(ctx, previous.as_ref());
                batch.set(
                    &key,
                    &merge::mark_unavailable(previous.clone(), data.id.get()),
                )?;
                outcome.previous_guild = previous;
            } else {
                let previous = remove_guild(conn, &mut batch, data.id).await?;
                outcome.old = old_if(ctx, previous.as_ref());
                outcome.previous_guild = previous;
            }
        }
        Event::UnavailableGuild(data) => {
            let key = guild_key(data.id);
            let previous = get(conn, &key).await?;
            outcome.old = old_if(ctx, previous.as_ref());
            batch.set(&key, &merge::mark_unavailable(previous, data.id.get()))?;
        }
        Event::GuildUpdate(data) => {
            let key = guild_key(data.id);
            let previous = get(conn, &key).await?;
            outcome.old = old_if(ctx, previous.as_ref());
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
                outcome.old = old_if(ctx, previous.as_ref());
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
                set_member_expiry(&mut batch, ctx, &key, data.user.id);
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
                let guild_id = data.guild_id;
                for presence in &mut data.presences {
                    let key = presence_key(guild_id, presence.user.id());
                    if presence.status == Status::Offline {
                        batch.del(&key);
                    } else {
                        // twilight's deserializer sets it already; a chunk
                        // built in code may not.
                        presence.guild_id = guild_id;
                        batch.set(&key, &*presence)?;
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
                    outcome.old = old_if(ctx, Some(&previous));
                    batch.set(&key, &edited_message(previous, data)?)?;
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
                let key = presence_key(data.guild_id, data.user.id());
                let offline = data.status == Status::Offline;
                // The cached user only completes a partial one that is stored.
                let partial = matches!(data.user, UserOrId::UserId { .. });
                let previous = if state.old || (partial && !offline) {
                    get(conn, &key).await?
                } else {
                    None
                };
                outcome.old = old_if(ctx, previous.as_ref());
                if offline {
                    batch.del(&key);
                } else {
                    let update = serde_json::to_value(&data.0)?;
                    batch.set(&key, &merge_presence(previous, update))?;
                }
            }
        }
        Event::Ready(data) => {
            ready(conn, data).await?;
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

/// SET a key and refresh its expiry, only if the key exists. Index sets are
/// left alone: an existing key is indexed already, and removals delete a
/// key, its index entries and its expiry in one transaction. Running before
/// one, the script changes nothing the removal leaves behind; running after
/// it, the script writes nothing.
///
/// KEYS: the stored key, the expiry zset. ARGV: the value, then the expiry
/// deadline (left out for a TTL of 0).
const SET_IF_CACHED_SCRIPT: &str = r"
if redis.call('EXISTS', KEYS[1]) == 0 then
  return 0
end
redis.call('SET', KEYS[1], ARGV[1])
if ARGV[2] then
  redis.call('ZADD', KEYS[2], ARGV[2], KEYS[1])
end
return 1
";

static SET_IF_CACHED: LazyLock<Script> = LazyLock::new(|| Script::new(SET_IF_CACHED_SCRIPT));

/// Apply an event that a previous session of its shard dispatched, after
/// READY of the current session reconciled the cache. Only for the events
/// `worker::only_removes` lets through.
///
/// READY's reconcile runs in the shard runner, at the same time as the
/// workers, and can remove a guild between the read and the write of an
/// update. Removals bring nothing back, so they are applied by [`update`].
/// A `MESSAGE_UPDATE` has to read the message to merge the edit into it, so
/// its write only goes ahead if the message is still cached when it runs
/// (`SET_IF_CACHED_SCRIPT`). It then stores what [`update`] stores (value
/// and expiry), but never undoes a removal; `old` is only set when the edit
/// was written, as [`update`] only sets it for a cached message.
///
/// # Errors
///
/// As [`update`]: fails when a Redis command or the script fails, or a value
/// cannot be converted to or from JSON.
pub async fn update_stale<C: ConnectionLike + Send>(
    conn: &mut C,
    event: &mut Event,
    ctx: &UpdateContext<'_>,
) -> ApiResult<Outcome> {
    let Event::MessageUpdate(data) = event else {
        return update(conn, event, ctx).await;
    };
    let state = ctx.state;
    let mut outcome = Outcome::default();
    // As in `update`: with the cache disabled nothing is read or written.
    if !state.enabled || !state.message {
        return Ok(outcome);
    }
    let key = message_key(data.channel_id, data.id);
    let Some(previous) = get(conn, &key).await? else {
        return Ok(outcome);
    };
    let old = old_if(ctx, Some(&previous));
    let edited = serde_json::to_string(&edited_message(previous, data)?)?;
    let mut invocation = SET_IF_CACHED.key(stored_key(&key));
    invocation.key(EXPIRY_ZSET).arg(edited);
    if let Some(deadline) = batch::deadline(state.message_ttl, ctx.now_ms) {
        invocation.arg(deadline);
    }
    let written: bool = invocation.invoke_async(conn).await?;
    if written {
        outcome.old = old;
    }
    Ok(outcome)
}

/// Keep the members that [`clear_guild`] left cached and indexed for a
/// `GUILD_CREATE` re-sync (`kept`, stored keys): give those `guild` does not
/// list a deadline unless they have one already: with `STATE_MEMBER_TTL=0`
/// the grace ([`MEMBER_RESYNC_GRACE_MS`]), with a TTL the TTL. So with a TTL
/// they keep the deadline they had.
///
/// When `guild` lists every member (as many as its `member_count`, which
/// Discord does below `LARGE_THRESHOLD` members), the ones it does not list
/// left while no session received their `GUILD_MEMBER_REMOVE`: they are
/// removed at once, as a re-sync always did.
///
/// A key that the expiry job removed after [`clear_guild`] read the index
/// only gets a deadline here, which the job drops when it is due. Called
/// before [`write_guild`] takes the members out of `guild`; the ones it lists
/// are written again there.
fn keep_members(batch: &mut Batch, ctx: &UpdateContext<'_>, guild: &Guild, kept: &[String]) {
    if kept.is_empty() {
        return;
    }
    let ttl = if ctx.state.member_ttl == 0 {
        MEMBER_RESYNC_GRACE_MS
    } else {
        ctx.state.member_ttl
    };
    // The bot's own member never expires, listed or not.
    let listed: HashSet<String> = guild
        .members
        .iter()
        .map(|member| member.user.id)
        .chain(ctx.bot_id)
        .map(|user_id| member_key(guild.id, user_id))
        .collect();
    // Every member listed: the members it does not list have left.
    let listed_len = u64::try_from(guild.members.len()).unwrap_or(u64::MAX);
    let complete = guild.member_count.is_some_and(|count| listed_len >= count);
    for key in kept.iter().filter(|key| !listed.contains(*key)) {
        if complete {
            batch.del(key);
        } else {
            batch.expire_if_unset(key, ttl, ctx.now_ms);
        }
    }
}

/// Plan all keys of an available `GUILD_CREATE`. The lists that are stored
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
            batch.set(&presence_key(guild_id, presence.user.id()), &presence)?;
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
async fn ready<C: ConnectionLike + Send>(conn: &mut C, data: &Ready) -> ApiResult<()> {
    let mut pipe = redis::pipe();
    pipe.atomic();
    pipe.cmd("SET")
        .arg(BOT_USER_KEY)
        .arg(serde_json::to_string(&data.user)?)
        .ignore();
    let guild_index = keys::type_index(GUILD_KEY);
    for guild in &data.guilds {
        let key = guild_key(guild.id);
        let stub = merge::mark_unavailable(None, guild.id.get());
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
            remove_guild(conn, &mut batch, guild_id).await?;
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

#[cfg(test)]
mod edit_tests {
    use super::*;
    use serde_json::json;

    /// A `MESSAGE_UPDATE`, which carries the full message 1 of channel 3.
    fn edit_of(content: &str, components: Value) -> MessageUpdate {
        let message = json!({
            "id": "1", "channel_id": "3", "type": 0, "content": content,
            "author": {
                "id": "2", "username": "author", "global_name": "AUTHOR",
                "discriminator": "0", "avatar": null, "bot": false, "public_flags": 0,
            },
            "timestamp": "2024-01-01T00:00:00.000000+00:00", "edited_timestamp": null,
            "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
            "attachments": [], "embeds": [], "pinned": false, "components": components,
        });
        serde_json::from_str(&message.to_string()).unwrap()
    }

    /// One action row with one button.
    fn buttons(custom_id: &str) -> Value {
        json!([{"type": 1, "components": [{"type": 2, "style": 1, "custom_id": custom_id}]}])
    }

    /// The `custom_id` of a message's first button, `None` without one.
    fn button(message: &Value) -> Option<&str> {
        message["components"][0]["components"][0]["custom_id"].as_str()
    }

    #[test]
    fn an_edit_without_components_drops_the_cached_ones() {
        let mut cached = serde_json::to_value(edit_of("pick", buttons("a")).0).unwrap();
        cached["tts"] = json!(true);
        assert_eq!(button(&cached), Some("a"));

        // The update's buttons replace the cached ones.
        let kept = edited_message(cached.clone(), &edit_of("again", buttons("b"))).unwrap();
        assert_eq!(button(&kept), Some("b"));

        // No buttons left: twilight leaves the empty list out of the update.
        let edited = edited_message(cached, &edit_of("done", json!([]))).unwrap();
        assert!(edited.get("components").is_none(), "{edited}");
        assert_eq!(edited["content"], "done");
        assert_eq!(edited["tts"], true, "an update's tts is always false");

        // A message without buttons is edited without the key.
        let plain = serde_json::to_value(edit_of("plain", json!([])).0).unwrap();
        assert!(plain.get("components").is_none(), "{plain}");
        let edited = edited_message(plain, &edit_of("plain!", json!([]))).unwrap();
        assert!(edited.get("components").is_none(), "{edited}");
    }

    #[test]
    fn overlay_onto_or_of_a_non_object_is_the_update() {
        let object = json!({"a": 1});
        assert_eq!(overlay(Some(json!([1])), object.clone(), &["a"]), object);
        assert_eq!(overlay(Some(object), json!("b"), &["a"]), json!("b"));
    }

    #[test]
    fn a_partial_presence_user_without_a_cached_one_stays_partial() {
        let update = json!({"user": {"id": "1"}, "status": "idle"});
        for cached in [json!({"status": "online"}), json!("online")] {
            assert_eq!(merge_presence(Some(cached), update.clone()), update);
        }
    }
}
