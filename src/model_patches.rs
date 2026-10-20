//! Tests for the patches in vendor/twilight-model (see its PATCHES.md).

#[cfg(test)]
mod tests {
    use serde_json::json;
    use twilight_model::{
        channel::ChannelFlags,
        gateway::presence::Activity,
        guild::{MemberFlags, Permissions},
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
}
