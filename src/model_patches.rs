//! Tests for the patches in vendor/twilight-model (see its PATCHES.md).

#[cfg(test)]
mod tests {
    use serde::{de::DeserializeOwned, Serialize};
    use serde_json::{json, Value};
    use twilight_model::{
        channel::{
            forum::{DefaultReaction, ForumTag},
            message::EmojiReactionType,
            ChannelFlags,
        },
        gateway::presence::{Activity, ActivityEmoji},
        guild::{
            invite::WelcomeScreenChannel, onboarding::OnboardingPromptEmoji, MemberFlags,
            Permissions,
        },
        poll::PartialPollMediaEmoji,
        user::User,
    };

    fn activity(application_id: &str) -> Result<Activity, serde_json::Error> {
        serde_json::from_str(&format!(
            r#"{{"name":"x","type":0,"created_at":1,"application_id":{application_id}}}"#
        ))
    }

    #[test]
    fn application_id_zero_and_empty_mean_none() {
        for value in ["0", r#""0""#, r#""""#, "null", "-1"] {
            assert_eq!(activity(value).unwrap().application_id, None, "{value}");
        }
        assert_eq!(
            activity("123").unwrap().application_id.map(|id| id.get()),
            Some(123)
        );
        assert_eq!(
            activity(r#""367827983903490050""#)
                .unwrap()
                .application_id
                .map(|id| id.get()),
            Some(367_827_983_903_490_050)
        );
        assert!(activity(r#""abc""#).is_err());

        let missing: Activity =
            serde_json::from_str(r#"{"name":"x","type":0,"created_at":null}"#).unwrap();
        assert_eq!(missing.application_id, None);
        assert!(serde_json::to_value(&missing)
            .unwrap()
            .get("application_id")
            .is_none());
    }

    #[test]
    fn channel_flags_keep_obfuscated_and_unknown_bits() {
        let flags: ChannelFlags = serde_json::from_value(json!(131_072u64 | (1u64 << 40))).unwrap();
        assert!(flags.contains(ChannelFlags::CHANNEL_OBFUSCATED));
        assert_eq!(
            serde_json::to_value(flags).unwrap(),
            json!(131_072u64 | (1u64 << 40))
        );
    }

    #[test]
    fn permissions_keep_new_bits() {
        // PIN_MESSAGES (1 << 51) plus a bit twilight does not know yet.
        let raw = ((1u64 << 51) | (1 << 60)).to_string();
        let perms: Permissions = serde_json::from_value(json!(raw)).unwrap();
        assert!(perms.contains(Permissions::PIN_MESSAGES));
        assert_eq!(serde_json::to_value(perms).unwrap(), json!(raw));
    }

    #[test]
    fn member_flags_keep_unknown_bits() {
        let flags: MemberFlags = serde_json::from_value(json!(1u64 << 30)).unwrap();
        assert_eq!(serde_json::to_value(flags).unwrap(), json!(1u64 << 30));
    }

    #[test]
    fn every_cached_flags_type_keeps_unknown_bits() {
        use twilight_model::{
            application::monetization::SkuFlags,
            channel::{message::MessageFlags, AttachmentFlags},
            gateway::presence::ActivityFlags,
            guild::{RoleFlags, SystemChannelFlags},
            oauth::ApplicationFlags,
            user::UserFlags,
        };
        fn round_trip<T: serde::de::DeserializeOwned + serde::Serialize>(
            bits: u64,
        ) -> serde_json::Value {
            serde_json::to_value(serde_json::from_value::<T>(json!(bits)).unwrap()).unwrap()
        }
        let unknown = 1u64 << 45;
        assert_eq!(
            round_trip::<MessageFlags>(unknown | 4096),
            json!(unknown | 4096)
        );
        assert_eq!(round_trip::<AttachmentFlags>(unknown), json!(unknown));
        assert_eq!(round_trip::<ActivityFlags>(unknown), json!(unknown));
        assert_eq!(round_trip::<RoleFlags>(unknown), json!(unknown));
        assert_eq!(round_trip::<SystemChannelFlags>(unknown), json!(unknown));
        assert_eq!(round_trip::<ApplicationFlags>(unknown), json!(unknown));
        assert_eq!(round_trip::<UserFlags>(unknown), json!(unknown));
        assert_eq!(round_trip::<SkuFlags>(unknown), json!(unknown));
    }

    #[test]
    fn migrated_discriminator_is_zero() {
        let user: User = serde_json::from_value(json!({
            "id": "1", "username": "new", "global_name": "New", "discriminator": "0",
            "avatar": null
        }))
        .unwrap();
        let value = serde_json::to_value(&user).unwrap();
        assert_eq!(value["discriminator"], "0");
        assert_eq!(value["global_name"], "New");

        let legacy: User = serde_json::from_value(json!({
            "id": "1", "username": "old", "discriminator": "0042", "avatar": null
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(&legacy).unwrap()["discriminator"],
            "0042"
        );
    }

    /// Parse `json` with its emoji id set to `id` (or left out), and the id
    /// read back by `get`.
    fn emoji_id<T: DeserializeOwned + Serialize>(
        mut json: Value,
        field: &str,
        id: Option<Value>,
        get: fn(&T) -> Option<u64>,
    ) -> Result<(Option<u64>, Value), serde_json::Error> {
        match id {
            Some(id) => json[field] = id,
            None => {
                json.as_object_mut().unwrap().remove(field);
            }
        }
        let value: T = serde_json::from_value(json)?;
        Ok((get(&value), serde_json::to_value(&value).unwrap()))
    }

    /// Unicode emojis only have a name. Discord sends their id as null, leaves
    /// it out, or (some guilds) sends `0` / `"0"`; any of them used to fail the
    /// whole GUILD_CREATE / MESSAGE_CREATE the emoji was in.
    fn check_emoji_id<T: DeserializeOwned + Serialize>(
        json: Value,
        field: &str,
        get: fn(&T) -> Option<u64>,
    ) {
        let name = json
            .as_object()
            .unwrap()
            .keys()
            .find(|k| k.contains("name"))
            .cloned();
        for id in [
            None,
            Some(json!(null)),
            Some(json!("0")),
            Some(json!(0)),
            Some(json!("")),
        ] {
            let (parsed, stored) = emoji_id::<T>(json.clone(), field, id.clone(), get)
                .unwrap_or_else(|err| panic!("{field}={id:?}: {err}"));
            assert_eq!(parsed, None, "{field}={id:?}");
            // Stored as Discord documents a unicode emoji: a null id, the name kept.
            assert_eq!(stored[field], json!(null), "{field}={id:?}");
            if let Some(name) = &name {
                assert_eq!(stored[name], json[name], "{field}={id:?}");
            }
        }
        for id in [
            json!("367827983903490050"),
            json!(367_827_983_903_490_050u64),
        ] {
            let (parsed, stored) = emoji_id::<T>(json.clone(), field, Some(id.clone()), get)
                .unwrap_or_else(|err| panic!("{field}={id}: {err}"));
            assert_eq!(parsed, Some(367_827_983_903_490_050), "{field}={id}");
            assert_eq!(stored[field], json!("367827983903490050"), "{field}={id}");
        }
        assert!(emoji_id::<T>(json, field, Some(json!("abc")), get).is_err());
    }

    #[test]
    fn unicode_emojis_parse_with_a_null_missing_or_zero_id() {
        check_emoji_id::<ForumTag>(
            json!({"id": "1", "name": "tag", "moderated": false, "emoji_id": null, "emoji_name": "💩"}),
            "emoji_id",
            |tag| tag.emoji_id.map(|id| id.get()),
        );
        check_emoji_id::<DefaultReaction>(
            json!({"emoji_id": null, "emoji_name": "💩"}),
            "emoji_id",
            |reaction| reaction.emoji_id.map(|id| id.get()),
        );
        check_emoji_id::<WelcomeScreenChannel>(
            json!({"channel_id": "1", "description": "d", "emoji_id": null, "emoji_name": "💩"}),
            "emoji_id",
            |channel| channel.emoji_id.map(|id| id.get()),
        );
        check_emoji_id::<OnboardingPromptEmoji>(
            json!({"id": null, "name": "💩", "animated": false}),
            "id",
            |emoji| emoji.id.map(|id| id.get()),
        );
        check_emoji_id::<PartialPollMediaEmoji>(
            json!({"id": null, "name": "💩", "animated": false}),
            "id",
            |emoji| emoji.id.map(|id| id.get()),
        );
    }

    #[test]
    fn custom_status_emojis_keep_their_id_as_a_string() {
        let emoji = |json: Value| serde_json::from_value::<ActivityEmoji>(json).unwrap();
        let unicode = emoji(json!({"name": "💩"}));
        assert_eq!(unicode.id, None);
        assert_eq!(
            serde_json::to_value(&unicode).unwrap(),
            json!({"name": "💩"})
        );
        assert_eq!(emoji(json!({"name": "💩", "id": null})).id, None);
        for id in [json!("123"), json!(123)] {
            let custom = emoji(json!({"name": "x", "id": id, "animated": true}));
            assert_eq!(custom.id.as_deref(), Some("123"), "{id}");
            assert_eq!(
                serde_json::to_value(&custom).unwrap(),
                json!({"name": "x", "id": "123", "animated": true})
            );
        }
        // Kept as Discord sent it, as before.
        assert_eq!(
            emoji(json!({"name": "💩", "id": "0"})).id.as_deref(),
            Some("0")
        );
        assert_eq!(
            emoji(json!({"name": "💩", "id": 0})).id.as_deref(),
            Some("0")
        );
    }

    #[test]
    fn unicode_reactions_and_component_emojis_parse() {
        for json in [
            json!({"name": "💩"}),
            json!({"id": null, "name": "💩"}),
            json!({"id": "0", "name": "💩"}),
            json!({"id": 0, "name": "💩", "animated": false}),
        ] {
            let emoji: EmojiReactionType = serde_json::from_value(json.clone()).unwrap();
            assert_eq!(
                emoji,
                EmojiReactionType::Unicode {
                    name: "💩".into()
                },
                "{json}"
            );
            assert_eq!(serde_json::to_value(&emoji).unwrap(), json!({"name": "💩"}));
        }
        let custom: EmojiReactionType =
            serde_json::from_value(json!({"id": "123", "name": "x", "animated": true})).unwrap();
        assert!(matches!(custom, EmojiReactionType::Custom { id, .. } if id.get() == 123));
    }
}
