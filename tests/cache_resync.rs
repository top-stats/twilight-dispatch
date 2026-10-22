#![recursion_limit = "256"]

//! Cached members across a `GUILD_CREATE` re-sync, against a real Redis (set
//! TEST_REDIS_URL). After a new session Discord sends every guild's
//! GUILD_CREATE again, and for a guild over 75k members it lists only the bot
//! and the members in voice channels. The members cached before stay; with
//! STATE_MEMBER_TTL=0 those it does not list expire after a grace of 24 hours
//! unless a members chunk or a member event writes them again; a GUILD_CREATE
//! that lists every member removes the others at once. Removals
//! (leaving the guild, READY's reconcile) still remove every member. Each
//! test uses its own database index.

mod common;

use common::*;
use redis::aio::ConnectionManager;
use serde_json::{json, Value};
use twilight_dispatch::{
    cache::{self, jobs, Outcome, UpdateContext, MEMBER_RESYNC_GRACE_MS},
    config::StateConfig,
    constants::EXPIRY_ZSET,
};

const G: u64 = 1000;
/// The clock of every update here: 2026-10-20T12:00:00Z.
const NOW: i64 = 1_792_497_600_000;
/// A `joined_at` one minute before [`NOW`] (a new join).
const JUST_JOINED: &str = "2026-10-20T11:59:00.000000+00:00";
/// The grace, spelled out: a bound computed from the constant could not tell
/// a wrong unit.
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// The grace deadline of a re-sync at [`NOW`].
const DUE: i64 = NOW + DAY_MS;
/// The member TTL of `common::state_all()`.
const TTL: i64 = 60_000;
/// The `member_count` of a re-synced guild: far more than its GUILD_CREATE
/// lists.
const LARGE: u64 = 180_000;

// ---------------------------------------------------------------------------
// Helpers

/// The production settings: members cached without a TTL.
fn production() -> StateConfig {
    StateConfig {
        member_ttl: 0,
        message: false,
        ..state_all()
    }
}

/// Apply an event as of `now_ms`.
async fn apply_at(
    conn: &mut ConnectionManager,
    state: &StateConfig,
    now_ms: i64,
    kind: &str,
    d: Value,
) -> Outcome {
    let mut event = event(kind, d);
    let ctx = UpdateContext {
        state,
        bot_id: Some(bot_id()),
        now_ms,
    };
    cache::update(conn, &mut event, &ctx)
        .await
        .unwrap_or_else(|err| panic!("{kind} update failed: {err}"))
}

/// A GUILD_MEMBERS_CHUNK of guild `G` with `users`, all online.
fn chunk(users: &[u64]) -> Value {
    let mut members = Vec::new();
    let mut presences = Vec::new();
    for user in users {
        members.push(member(*user, &[]));
        presences.push(presence(G, *user, "online"));
    }
    json!({
        "guild_id": G.to_string(), "members": members, "presences": presences,
        "chunk_index": 0, "chunk_count": 1, "not_found": [],
    })
}

/// A member event's `d` in guild `G`.
fn in_guild(mut d: Value) -> Value {
    d["guild_id"] = json!(G.to_string());
    d
}

/// Guild `G` as its first GUILD_CREATE caches it (with the bot and member
/// 10), then a members chunk with `users`.
async fn seed(conn: &mut ConnectionManager, state: &StateConfig, now_ms: i64, users: &[u64]) {
    apply_at(conn, state, now_ms, "GUILD_CREATE", guild(G, "g")).await;
    apply_at(conn, state, now_ms, "GUILD_MEMBERS_CHUNK", chunk(users)).await;
}

/// A new session: READY keeps the cached guild, then its GUILD_CREATE lists
/// only the bot and member 10 (in voice) of its [`LARGE`] members, as for a
/// guild over 75k members.
async fn resync(conn: &mut ConnectionManager, state: &StateConfig, now_ms: i64) -> Outcome {
    apply_at(conn, state, now_ms, "READY", ready([0, 1], &[G])).await;
    let mut d = guild(G, "g");
    d["member_count"] = json!(LARGE);
    apply_at(conn, state, now_ms, "GUILD_CREATE", d).await
}

fn member_key(user: u64) -> String {
    format!("member:{G}:{user}")
}

/// The expiry deadline of a stored key, `None` without one.
async fn deadline(conn: &mut ConnectionManager, key: &str) -> Option<i64> {
    let score: Option<f64> = redis::cmd("ZSCORE")
        .arg(EXPIRY_ZSET)
        .arg(key)
        .query_async(conn)
        .await
        .unwrap();
    score.map(|score| score as i64)
}

/// The member keys an index set lists, sorted.
async fn indexed(conn: &mut ConnectionManager, set: &str) -> Vec<String> {
    let mut listed = members(conn, set).await;
    listed.retain(|key| key.starts_with("member:"));
    listed
}

/// Every key of the database, sorted.
async fn keys(conn: &mut ConnectionManager) -> Vec<String> {
    let mut keys: Vec<String> = redis::cmd("KEYS").arg("*").query_async(conn).await.unwrap();
    keys.sort();
    keys
}

/// The cached guild's `member_count`.
async fn member_count(conn: &mut ConnectionManager) -> Option<u64> {
    let guild = get(conn, "guild:1000").await?;
    guild["member_count"].as_u64()
}

// ---------------------------------------------------------------------------
// Tests

#[test]
fn the_grace_is_a_day() {
    assert_eq!(MEMBER_RESYNC_GRACE_MS, 86_400_000, "24 hours");
}

/// The production path after a new session. Members the GUILD_CREATE does
/// not list stay cached and indexed with a deadline a day later; the ones it
/// lists are written again and get none. The rest of the guild is replaced as
/// before, and the outcome is what it was.
#[tokio::test]
async fn a_resync_keeps_the_members_it_does_not_list() {
    let Some(mut conn) = redis(1).await else {
        return;
    };
    let state = production();
    seed(&mut conn, &state, NOW, &[10, 20, 21]).await;
    let cached = get(&mut conn, &member_key(20)).await;
    assert!(cached.is_some());
    for user in [10, 20, 21, BOT_ID] {
        let key = member_key(user);
        assert_eq!(deadline(&mut conn, &key).await, None, "{key}");
    }

    let outcome = resync(&mut conn, &state, NOW).await;
    assert!(outcome.guild_known, "a re-sync is not a join");
    assert_eq!(outcome.previous_guild.as_ref().unwrap()["name"], "g");
    assert_eq!(outcome.old, outcome.previous_guild);

    let all = [
        "member:1000:10",
        "member:1000:20",
        "member:1000:21",
        "member:1000:900",
    ];
    assert_eq!(indexed(&mut conn, "guild_keys:1000").await, all);
    assert_eq!(indexed(&mut conn, "member_keys").await, all);
    assert_eq!(get(&mut conn, &member_key(20)).await, cached);
    for user in [20, 21] {
        let key = member_key(user);
        assert_eq!(deadline(&mut conn, &key).await, Some(DUE), "{key}");
    }
    for user in [10, BOT_ID] {
        let key = member_key(user);
        assert_eq!(deadline(&mut conn, &key).await, None, "{key}");
    }

    // The chunk's presences are replaced by the payload's, as before.
    assert!(!exists(&mut conn, "presence:1000:20").await);
    assert!(exists(&mut conn, "presence:1000:10").await);
    assert_eq!(member_count(&mut conn).await, Some(LARGE));
}

/// A member written again after the re-sync is confirmed: a members chunk,
/// GUILD_MEMBER_UPDATE and GUILD_MEMBER_ADD drop its deadline. MEMBER_ADD
/// still only counts a recent join of a member that is not cached.
#[tokio::test]
async fn members_written_again_lose_the_grace() {
    let Some(mut conn) = redis(2).await else {
        return;
    };
    let state = production();
    seed(&mut conn, &state, NOW, &[20, 21, 22, 23]).await;
    resync(&mut conn, &state, NOW).await;
    for user in [20, 21, 22, 23] {
        let key = member_key(user);
        assert_eq!(deadline(&mut conn, &key).await, Some(DUE), "{key}");
    }

    let mut update = in_guild(member(21, &[]));
    update["nick"] = json!("nick");
    let mut rejoined = in_guild(member(22, &[]));
    rejoined["joined_at"] = json!(JUST_JOINED);
    let events = [
        ("GUILD_MEMBERS_CHUNK", chunk(&[20])),
        ("GUILD_MEMBER_UPDATE", update),
        ("GUILD_MEMBER_ADD", rejoined),
    ];
    for (kind, d) in events {
        apply_at(&mut conn, &state, NOW, kind, d).await;
    }
    for user in [20, 21, 22] {
        let key = member_key(user);
        assert_eq!(deadline(&mut conn, &key).await, None, "{key}");
    }
    let unconfirmed = member_key(23);
    assert_eq!(deadline(&mut conn, &unconfirmed).await, Some(DUE));
    let updated = get(&mut conn, &member_key(21)).await.unwrap();
    assert_eq!(updated["nick"], "nick");

    // Member 22 was cached, so its add is no join; member 30's is.
    assert_eq!(member_count(&mut conn).await, Some(LARGE));
    let mut joined = in_guild(member(30, &[]));
    joined["joined_at"] = json!(JUST_JOINED);
    apply_at(&mut conn, &state, NOW, "GUILD_MEMBER_ADD", joined).await;
    assert_eq!(member_count(&mut conn).await, Some(LARGE + 1));
    assert_eq!(deadline(&mut conn, &member_key(30)).await, None);
}

/// Another re-sync before the grace ends does not postpone it: a member that
/// nothing confirmed since the first one still goes a day after it.
#[tokio::test]
async fn a_second_resync_keeps_the_first_deadline() {
    let Some(mut conn) = redis(3).await else {
        return;
    };
    let state = production();
    seed(&mut conn, &state, NOW, &[20]).await;
    resync(&mut conn, &state, NOW).await;
    resync(&mut conn, &state, NOW + 3_600_000).await;

    let key = member_key(20);
    assert_eq!(deadline(&mut conn, &key).await, Some(DUE));
    let listed = indexed(&mut conn, "guild_keys:1000").await;
    assert!(listed.contains(&key), "{listed:?}");
}

/// The expiry job removes a member whose grace ended together with its index
/// entries, and nothing else of the guild.
#[tokio::test]
async fn the_expiry_job_removes_members_whose_grace_ended() {
    let Some(mut conn) = redis(4).await else {
        return;
    };
    let state = production();
    seed(&mut conn, &state, NOW, &[20]).await;
    resync(&mut conn, &state, NOW).await;
    let before = keys(&mut conn).await;

    assert_eq!(jobs::expire_due(&mut conn, DUE - 1).await.unwrap(), 0);
    assert_eq!(keys(&mut conn).await, before);
    assert_eq!(jobs::expire_due(&mut conn, DUE).await.unwrap(), 1);

    let key = member_key(20);
    assert!(!exists(&mut conn, &key).await);
    assert_eq!(deadline(&mut conn, &key).await, None);
    let left = ["member:1000:10", "member:1000:900"];
    assert_eq!(indexed(&mut conn, "guild_keys:1000").await, left);
    assert_eq!(indexed(&mut conn, "member_keys").await, left);
    // Its deadline was the only one, so the sorted set is gone too.
    let mut expected = before;
    expected.retain(|stored| *stored != key && *stored != EXPIRY_ZSET);
    assert_eq!(keys(&mut conn).await, expected);
}

/// With a member TTL the members a re-sync does not list keep the deadline
/// they had, and get no grace; the listed ones get a new one. A kept member
/// without a deadline (cached while the TTL was 0, or removed by the expiry
/// job while the re-sync ran and indexed again) gets the TTL, so it cannot
/// stay cached, or indexed, for good.
#[tokio::test]
async fn with_a_member_ttl_kept_members_keep_their_deadline() {
    let Some(mut conn) = redis(5).await else {
        return;
    };
    let state = state_all();
    seed(&mut conn, &state, NOW, &[10, 20]).await;
    let d = in_guild(member(30, &[]));
    apply_at(&mut conn, &production(), NOW, "GUILD_MEMBER_UPDATE", d).await;
    let (listed, kept, forever) = (member_key(10), member_key(20), member_key(30));
    assert_eq!(deadline(&mut conn, &kept).await, Some(NOW + TTL));
    assert_eq!(deadline(&mut conn, &forever).await, None);

    let later = NOW + 30_000;
    resync(&mut conn, &state, later).await;
    assert_eq!(deadline(&mut conn, &kept).await, Some(NOW + TTL));
    assert_eq!(deadline(&mut conn, &forever).await, Some(later + TTL));
    assert_eq!(deadline(&mut conn, &listed).await, Some(later + TTL));
    assert_eq!(deadline(&mut conn, &member_key(BOT_ID)).await, None);
    let all = [
        "member:1000:10",
        "member:1000:20",
        "member:1000:30",
        "member:1000:900",
    ];
    assert_eq!(indexed(&mut conn, "guild_keys:1000").await, all);
    assert_eq!(indexed(&mut conn, "member_keys").await, all);
}

/// Leaving the guild (GUILD_DELETE without `unavailable`) still removes every
/// member, those a re-sync kept with a grace deadline included.
#[tokio::test]
async fn leaving_the_guild_still_removes_every_member() {
    let Some(mut conn) = redis(6).await else {
        return;
    };
    let state = production();
    seed(&mut conn, &state, NOW, &[20, 21]).await;
    resync(&mut conn, &state, NOW).await;
    assert!(deadline(&mut conn, &member_key(20)).await.is_some());

    let d = json!({"id": G.to_string()});
    let outcome = apply_at(&mut conn, &state, NOW, "GUILD_DELETE", d).await;
    assert_eq!(outcome.previous_guild.unwrap()["name"], "g");
    // Only READY's bot user is left: no member, index entry or deadline.
    assert_eq!(keys(&mut conn).await, ["bot_user"]);
}

/// READY's reconcile still removes every member of a guild it no longer
/// lists (the bot left it while it was disconnected).
#[tokio::test]
async fn ready_still_removes_the_members_of_a_stale_guild() {
    let Some(mut conn) = redis(7).await else {
        return;
    };
    let state = production();
    seed(&mut conn, &state, NOW, &[20, 21]).await;
    resync(&mut conn, &state, NOW).await;
    assert!(deadline(&mut conn, &member_key(20)).await.is_some());

    apply_at(&mut conn, &state, NOW, "READY", ready([0, 1], &[])).await;
    assert_eq!(keys(&mut conn).await, ["bot_user"]);
}

/// Without STATE_MEMBER only the bot's member is cached, so a re-sync clears
/// the guild's members as before (also one cached by a run with
/// STATE_MEMBER) and writes the bot's again.
#[tokio::test]
async fn without_state_member_a_resync_clears_members_as_before() {
    let Some(mut conn) = redis(8).await else {
        return;
    };
    seed(&mut conn, &production(), NOW, &[20]).await;
    let bot_only = StateConfig {
        member: false,
        ..production()
    };
    resync(&mut conn, &bot_only, NOW).await;

    let bot = ["member:1000:900"];
    assert_eq!(indexed(&mut conn, "guild_keys:1000").await, bot);
    assert_eq!(indexed(&mut conn, "member_keys").await, bot);
    assert!(!exists(&mut conn, &member_key(20)).await);
    assert_eq!(deadline(&mut conn, &member_key(BOT_ID)).await, None);
}

/// A GUILD_CREATE that lists every member (its `member_count`, as Discord
/// does below `LARGE_THRESHOLD`) shows that the members it does not list left
/// while no session received their GUILD_MEMBER_REMOVE: they are removed at
/// once, with their index entries, and get no grace.
#[tokio::test]
async fn a_resync_that_lists_every_member_removes_the_others() {
    let Some(mut conn) = redis(9).await else {
        return;
    };
    let state = production();
    seed(&mut conn, &state, NOW, &[20]).await;
    apply_at(&mut conn, &state, NOW, "READY", ready([0, 1], &[G])).await;
    // `guild` lists the bot and member 10, and its member_count is 2.
    apply_at(&mut conn, &state, NOW, "GUILD_CREATE", guild(G, "g")).await;

    let left = ["member:1000:10", "member:1000:900"];
    assert_eq!(indexed(&mut conn, "guild_keys:1000").await, left);
    assert_eq!(indexed(&mut conn, "member_keys").await, left);
    assert!(!exists(&mut conn, &member_key(20)).await);
    assert_eq!(deadline(&mut conn, &member_key(20)).await, None);
    assert_eq!(member_count(&mut conn).await, Some(2));
}
