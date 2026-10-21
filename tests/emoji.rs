#![recursion_limit = "256"]
//! Unicode emojis are only a name. Discord sends their id as null, leaves it
//! out, or (in some guilds) sends `0` / `"0"`, and one such emoji used to make
//! the whole event fail to parse: a GUILD_CREATE with a forum tag like that
//! was never cached. These tests go through `runner::prepare`, as the shard
//! runner does, and check what ends up in the cache.

mod common;

use common::*;
use serde_json::{json, Value};
use twilight_dispatch::{
    cache::{self, UpdateContext},
    config::StateConfig,
    gateway::runner::{parse_event, prepare, Prepared},
};

const GUILD: u64 = 1000;
const FORUM: u64 = 1010;

/// A forum channel whose tags and default reaction use every shape of
/// unicode emoji id Discord sends, next to a custom emoji.
fn forum() -> Value {
    let mut forum = channel(FORUM, Some(GUILD), "forum");
    forum["type"] = json!(15);
    forum["default_forum_layout"] = json!(0);
    forum["default_sort_order"] = json!(null);
    forum["default_thread_rate_limit_per_user"] = json!(0);
    forum["default_reaction_emoji"] = json!({"emoji_id": "0", "emoji_name": "💩"});
    forum["available_tags"] = json!([
        {"id": "1", "name": "zero", "moderated": false, "emoji_id": "0", "emoji_name": "💩"},
        {"id": "2", "name": "int", "moderated": false, "emoji_id": 0, "emoji_name": "🔥"},
        {"id": "3", "name": "null", "moderated": false, "emoji_id": null, "emoji_name": "✅"},
        {"id": "4", "name": "none", "moderated": false},
        {"id": "5", "name": "custom", "moderated": true, "emoji_id": "1002", "emoji_name": null},
    ]);
    forum
}

/// The tags as they are stored: unicode emojis with a null id.
fn stored_tags() -> Value {
    json!([
        {"id": "1", "name": "zero", "moderated": false, "emoji_id": null, "emoji_name": "💩"},
        {"id": "2", "name": "int", "moderated": false, "emoji_id": null, "emoji_name": "🔥"},
        {"id": "3", "name": "null", "moderated": false, "emoji_id": null, "emoji_name": "✅"},
        {"id": "4", "name": "none", "moderated": false, "emoji_id": null, "emoji_name": null},
        {"id": "5", "name": "custom", "moderated": true, "emoji_id": "1002", "emoji_name": null},
    ])
}

fn custom_status(emoji: Value) -> Value {
    json!({"type": 4, "name": "Custom Status", "state": "hi", "created_at": 1, "emoji": emoji})
}

fn presence_with(user_id: u64, emoji: Value) -> Value {
    let mut presence = presence(GUILD, user_id, "online");
    presence["activities"] = json!([custom_status(emoji)]);
    presence
}

fn guild_with_unicode_emojis() -> Value {
    let mut guild = guild(GUILD, "emoji");
    guild["channels"].as_array_mut().unwrap().push(forum());
    guild["presences"] = json!([
        presence_with(10, json!({"name": "💩"})),
        presence_with(
            BOT_ID,
            json!({"name": "pog", "id": "1002", "animated": false})
        ),
    ]);
    guild
}

/// A message with a unicode reaction, a poll with unicode answer emojis and
/// a button with a unicode emoji.
fn message_with_unicode_emojis(id: u64) -> Value {
    let mut message = message(id, 1003, Some(GUILD), "vote");
    message["reactions"] = json!([{
        "count": 1, "me": false, "me_burst": false, "burst_colors": [],
        "count_details": {"burst": 0, "normal": 1},
        "emoji": {"id": null, "name": "💩"},
    }]);
    message["poll"] = json!({
        "question": {"text": "?"},
        "answers": [
            {"answer_id": 1, "poll_media": {"text": "a", "emoji": {"name": "💩"}}},
            {"answer_id": 2, "poll_media": {"text": "b", "emoji": {"id": "0", "name": "🔥"}}},
            {"answer_id": 3, "poll_media": {"text": "c", "emoji": {"id": null, "name": "✅"}}},
        ],
        "expiry": "2024-01-02T00:00:00.000000+00:00",
        "allow_multiselect": false, "layout_type": 1,
    });
    message["components"] = json!([{
        "type": 1,
        "components": [{"type": 2, "style": 1, "custom_id": "b", "emoji": {"name": "💩"}}],
    }]);
    message
}

/// Parse a dispatch the way the shard runner does and apply it to the cache.
async fn dispatch(
    conn: &mut redis::aio::ConnectionManager,
    state: &StateConfig,
    kind: &str,
    d: Value,
) {
    let Prepared::Dispatch {
        event, parse_error, ..
    } = prepare(&raw(kind, 1, d), state)
    else {
        panic!("{kind} is not a dispatch");
    };
    assert_eq!(parse_error, None, "{kind}");
    let mut event = event.unwrap_or_else(|| panic!("{kind} was not parsed for the cache"));
    let ctx = UpdateContext {
        state,
        bot_id: Some(bot_id()),
        now_ms: now_ms(),
    };
    cache::update(conn, &mut event, &ctx)
        .await
        .unwrap_or_else(|err| panic!("{kind} update failed: {err}"));
}

#[tokio::test]
async fn a_guild_with_unicode_forum_emojis_is_cached() {
    let Some(mut conn) = redis(7).await else {
        return;
    };
    let state = state_all();
    dispatch(
        &mut conn,
        &state,
        "GUILD_CREATE",
        guild_with_unicode_emojis(),
    )
    .await;

    assert!(exists(&mut conn, &format!("guild:{GUILD}")).await);
    let forum = get(&mut conn, &format!("channel:{FORUM}")).await.unwrap();
    assert_eq!(
        forum["default_reaction_emoji"],
        json!({"emoji_id": null, "emoji_name": "💩"})
    );
    assert_eq!(forum["available_tags"], stored_tags());

    let unicode = get(&mut conn, &format!("presence:{GUILD}:10"))
        .await
        .unwrap();
    assert_eq!(unicode["activities"][0]["emoji"], json!({"name": "💩"}));
    let custom = get(&mut conn, &format!("presence:{GUILD}:{BOT_ID}"))
        .await
        .unwrap();
    assert_eq!(
        custom["activities"][0]["emoji"],
        json!({"name": "pog", "id": "1002", "animated": false})
    );
}

#[tokio::test]
async fn forum_and_presence_updates_with_unicode_emojis_are_cached() {
    let Some(mut conn) = redis(8).await else {
        return;
    };
    let state = state_all();
    dispatch(&mut conn, &state, "GUILD_CREATE", guild(GUILD, "emoji")).await;

    dispatch(&mut conn, &state, "CHANNEL_CREATE", forum()).await;
    let mut update = forum();
    update["default_reaction_emoji"] = json!({"emoji_id": 0, "emoji_name": "🔥"});
    dispatch(&mut conn, &state, "CHANNEL_UPDATE", update).await;
    let stored = get(&mut conn, &format!("channel:{FORUM}")).await.unwrap();
    assert_eq!(
        stored["default_reaction_emoji"],
        json!({"emoji_id": null, "emoji_name": "🔥"})
    );
    assert_eq!(stored["available_tags"], stored_tags());

    // A forum post: a thread with tags of the forum.
    let mut post = thread(1011, GUILD, FORUM);
    post["applied_tags"] = json!(["1", "5"]);
    dispatch(&mut conn, &state, "THREAD_CREATE", post).await;
    let stored = get(&mut conn, &format!("thread:{GUILD}:1011"))
        .await
        .unwrap();
    assert_eq!(stored["applied_tags"], json!(["1", "5"]));

    dispatch(
        &mut conn,
        &state,
        "PRESENCE_UPDATE",
        presence_with(10, json!({"name": "💩", "id": null})),
    )
    .await;
    let stored = get(&mut conn, &format!("presence:{GUILD}:10"))
        .await
        .unwrap();
    assert_eq!(stored["activities"][0]["emoji"], json!({"name": "💩"}));
}

#[tokio::test]
async fn messages_with_unicode_emojis_are_cached() {
    let Some(mut conn) = redis(9).await else {
        return;
    };
    let state = state_all();
    dispatch(
        &mut conn,
        &state,
        "MESSAGE_CREATE",
        message_with_unicode_emojis(1),
    )
    .await;

    let stored = get(&mut conn, "message:1003:1").await.unwrap();
    assert_eq!(stored["reactions"][0]["emoji"], json!({"name": "💩"}));
    let answers: Vec<&Value> = stored["poll"]["answers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|answer| &answer["poll_media"]["emoji"])
        .collect();
    assert_eq!(
        answers,
        [
            &json!({"animated": false, "id": null, "name": "💩"}),
            &json!({"animated": false, "id": null, "name": "🔥"}),
            &json!({"animated": false, "id": null, "name": "✅"}),
        ]
    );
    assert_eq!(
        stored["components"][0]["components"][0]["emoji"],
        json!({"name": "💩"})
    );

    let mut update = message_with_unicode_emojis(1);
    update["content"] = json!("edited");
    dispatch(&mut conn, &state, "MESSAGE_UPDATE", update).await;
    let stored = get(&mut conn, "message:1003:1").await.unwrap();
    assert_eq!(stored["content"], "edited");
    assert_eq!(stored["reactions"][0]["emoji"], json!({"name": "💩"}));
}

/// Reaction events are published, not cached, so the runner does not parse
/// them; they must still parse, for consumers using twilight and for a cache
/// that adds reactions later.
#[test]
fn unicode_reaction_events_parse() {
    let reaction = json!({
        "user_id": "10", "channel_id": "1003", "message_id": "1", "guild_id": GUILD.to_string(),
        "emoji": {"id": null, "name": "💩"}, "burst": false, "type": 0,
    });
    for kind in [
        "MESSAGE_REACTION_ADD",
        "MESSAGE_REACTION_REMOVE",
        "MESSAGE_REACTION_REMOVE_EMOJI",
    ] {
        let json = raw(kind, 1, reaction.clone());
        assert!(
            matches!(parse_event(&json), Ok(Some(_))),
            "{kind}: {:?}",
            parse_event(&json).err()
        );
        assert!(matches!(
            prepare(&json, &state_all()),
            Prepared::Dispatch {
                parse_error: None,
                ..
            }
        ));
    }
}
