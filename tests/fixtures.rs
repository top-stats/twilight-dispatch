#![recursion_limit = "256"]

//! The fixtures used by the integration tests parse with the twilight 0.17
//! models (no services needed).

mod common;

use common::*;
use serde_json::json;
use twilight_dispatch::gateway::runner::{prepare, Prepared};
use twilight_model::gateway::{event::Event, payload::incoming::GuildCreate};

/// GUILD_CREATE is an untagged enum in twilight 0.17, so its own error never
/// says what failed; the parse error must, or a guild that is not cached
/// cannot be diagnosed from the log.
#[test]
fn guild_create_parse_errors_name_the_failing_value() {
    let mut guild = guild(1000, "g");
    guild["channels"][0]["parent_id"] = json!("0");
    guild["presences"][0]["activities"] = json!([{
        "type": 4, "name": "Custom Status", "created_at": 1, "emoji": {"id": "1", "name": null},
    }]);
    for (d, inner) in [
        (
            guild.clone(),
            "invalid value: integer `0`, expected non zero u64",
        ),
        (
            {
                let mut guild = guild.clone();
                guild["channels"][0]["parent_id"] = json!(null);
                guild
            },
            "invalid type: null, expected a string",
        ),
    ] {
        let Prepared::Dispatch { parse_error, .. } =
            prepare(&raw("GUILD_CREATE", 1, d), &state_all())
        else {
            panic!("not a dispatch");
        };
        let parse_error = parse_error.expect("does not parse");
        assert!(
            parse_error.contains("untagged enum GuildCreate"),
            "{parse_error}"
        );
        assert!(parse_error.contains(inner), "{parse_error}");
    }

    // Other events keep their own error.
    let mut channel = channel(1003, Some(1000), "general");
    channel["parent_id"] = json!(0);
    let Prepared::Dispatch { parse_error, .. } =
        prepare(&raw("CHANNEL_UPDATE", 1, channel), &state_all())
    else {
        panic!("not a dispatch");
    };
    assert!(parse_error
        .unwrap()
        .starts_with("invalid value: integer `0`, expected non zero u64"));
}

#[test]
fn fixtures_parse() {
    match event("GUILD_CREATE", guild(1000, "g")) {
        Event::GuildCreate(data) => match data.as_ref() {
            GuildCreate::Available(guild) => {
                assert_eq!(guild.channels.len(), 2);
                assert_eq!(guild.threads.len(), 1);
                assert_eq!(guild.members.len(), 2);
                assert_eq!(guild.presences.len(), 2);
            }
            GuildCreate::Unavailable(_) => panic!("expected an available guild"),
        },
        other => panic!("unexpected {other:?}"),
    }

    assert!(matches!(
        event("GUILD_CREATE", json!({"id": "5", "unavailable": true})),
        Event::GuildCreate(data) if matches!(data.as_ref(), GuildCreate::Unavailable(_))
    ));
    assert!(matches!(
        event("READY", ready([0, 1], &[1, 2])),
        Event::Ready(_)
    ));

    let mut add = member(20, &[]);
    add["guild_id"] = json!("1000");
    assert!(matches!(
        event("GUILD_MEMBER_ADD", add),
        Event::MemberAdd(_)
    ));

    let mut update = member(20, &[]);
    update["guild_id"] = json!("1000");
    update["joined_at"] = json!(null);
    assert!(matches!(
        event("GUILD_MEMBER_UPDATE", update),
        Event::MemberUpdate(_)
    ));

    assert!(matches!(
        event("PRESENCE_UPDATE", presence(1000, 10, "idle")),
        Event::PresenceUpdate(_)
    ));
    assert!(matches!(
        event(
            "VOICE_STATE_UPDATE",
            voice_state(Some(1000), 10, Some(1003))
        ),
        Event::VoiceStateUpdate(_)
    ));
    assert!(matches!(
        event("MESSAGE_CREATE", message(1, 1003, Some(1000), "hi")),
        Event::MessageCreate(_)
    ));
    assert!(matches!(
        event("MESSAGE_UPDATE", message(1, 1003, Some(1000), "edited")),
        Event::MessageUpdate(_)
    ));
    assert!(matches!(
        event("CHANNEL_CREATE", channel(7, Some(1000), "new")),
        Event::ChannelCreate(_)
    ));
    assert!(matches!(
        event("THREAD_CREATE", thread(8, 1000, 1003)),
        Event::ThreadCreate(_)
    ));
    assert!(matches!(
        event(
            "GUILD_ROLE_CREATE",
            json!({"guild_id": "1000", "role": role(9, "new")})
        ),
        Event::RoleCreate(_)
    ));
    assert!(matches!(
        event(
            "GUILD_EMOJIS_UPDATE",
            json!({"guild_id": "1000", "emojis": [emoji(3, "x")]})
        ),
        Event::GuildEmojisUpdate(_)
    ));

    let mut partial = guild(1000, "renamed");
    for field in [
        "joined_at",
        "large",
        "member_count",
        "channels",
        "threads",
        "members",
        "presences",
        "voice_states",
        "unavailable",
    ] {
        partial.as_object_mut().unwrap().remove(field);
    }
    assert!(matches!(
        event("GUILD_UPDATE", partial),
        Event::GuildUpdate(_)
    ));
}

#[test]
fn presence_with_application_id_zero_still_parses() {
    // Discord sends this for some activities; twilight 0.17 alone rejects it
    // and with it the whole GUILD_CREATE.
    let mut guild = guild(2000, "g");
    guild["presences"][0]["activities"] = json!([{
        "name": "Game", "type": 0, "created_at": 1, "application_id": 0,
    }]);
    assert!(matches!(
        event("GUILD_CREATE", guild),
        Event::GuildCreate(_)
    ));
}
