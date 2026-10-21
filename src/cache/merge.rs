//! Pure JSON merges for cached objects.

use serde_json::{Map, Value};

/// Guild fields that are stored under their own keys (and kept as empty
/// arrays in the guild blob by GUILD_CREATE). Threads are cached under their
/// own keys too but also stay in the blob, as in earlier versions.
pub const SPLIT_GUILD_FIELDS: [&str; 6] = [
    "channels",
    "roles",
    "emojis",
    "members",
    "presences",
    "voice_states",
];

/// Apply a GUILD_UPDATE to the cached guild blob.
///
/// GUILD_UPDATE carries a plain guild object without the GUILD_CREATE extra
/// fields (`member_count`, `joined_at`, `large`, `threads`, ...), so it is
/// merged into the cached blob instead of replacing it. Fields that GUILD_CREATE
/// splits out into their own keys are never copied back into the blob.
pub fn merge_guild_update(cached: Option<Value>, update: Value) -> Value {
    let Value::Object(update) = update else {
        return cached.unwrap_or(update);
    };

    let mut merged = match cached {
        // A READY / outage stub carries nothing worth keeping.
        Some(Value::Object(cached)) if cached.contains_key("name") => cached,
        _ => SPLIT_GUILD_FIELDS
            .iter()
            .chain(&["threads"])
            .map(|field| ((*field).to_owned(), Value::Array(Vec::new())))
            .collect(),
    };

    for (key, value) in update {
        if !SPLIT_GUILD_FIELDS.contains(&key.as_str()) {
            merged.insert(key, value);
        }
    }
    // Blobs written by older versions' GUILD_UPDATE carry stale inline lists.
    for field in SPLIT_GUILD_FIELDS {
        merged.insert(field.into(), Value::Array(Vec::new()));
    }
    // GUILD_UPDATE is only sent for available guilds.
    merged.insert("unavailable".into(), Value::Bool(false));

    Value::Object(merged)
}

/// GUILD_DELETE with `unavailable: true` is an outage, not a removal: keep the
/// cached guild and its children, only flag it.
pub fn mark_unavailable(cached: Option<Value>, guild_id: u64) -> Value {
    match cached {
        Some(Value::Object(mut guild)) => {
            guild.insert("unavailable".into(), Value::Bool(true));
            Value::Object(guild)
        }
        _ => {
            let mut guild = Map::new();
            guild.insert("id".into(), Value::String(guild_id.to_string()));
            guild.insert("unavailable".into(), Value::Bool(true));
            Value::Object(guild)
        }
    }
}

/// Adjust the cached `member_count` after GUILD_MEMBER_ADD / REMOVE.
/// Returns `None` when the blob has no count to adjust.
pub fn adjust_member_count(cached: Value, delta: i64) -> Option<Value> {
    let Value::Object(mut guild) = cached else {
        return None;
    };
    let count = guild.get("member_count")?.as_u64()?;
    let count = if delta.is_negative() {
        count.saturating_sub(delta.unsigned_abs())
    } else {
        count.saturating_add(delta as u64)
    };
    guild.insert("member_count".into(), Value::from(count));
    Some(Value::Object(guild))
}

/// Whether a cached guild value is only an unavailable-guild stub.
pub fn is_stub(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|guild| !guild.contains_key("name"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cached_guild() -> Value {
        json!({
            "id": "1", "name": "a", "icon": null, "member_count": 500,
            "joined_at": "2020-01-01T00:00:00.000000+00:00", "large": true,
            "roles": [], "emojis": [], "channels": [], "threads": [{"id": "9"}],
        })
    }

    #[test]
    fn guild_update_keeps_create_only_fields() {
        let update = json!({
            "id": "1", "name": "b", "icon": "abc",
            "roles": [{"id": "2"}], "emojis": [{"id": "3"}],
        });
        let merged = merge_guild_update(Some(cached_guild()), update);

        assert_eq!(merged["name"], "b");
        assert_eq!(merged["icon"], "abc");
        assert_eq!(merged["member_count"], 500);
        assert_eq!(merged["joined_at"], "2020-01-01T00:00:00.000000+00:00");
        assert_eq!(merged["large"], true);
        assert_eq!(merged["roles"], json!([]));
        assert_eq!(merged["emojis"], json!([]));
        assert_eq!(merged["threads"], json!([{"id": "9"}]));
    }

    #[test]
    fn guild_update_without_cache_strips_split_fields() {
        let merged = merge_guild_update(
            None,
            json!({"id": "1", "name": "b", "roles": [{"id": "2"}]}),
        );
        assert_eq!(merged["name"], "b");
        assert_eq!(merged["roles"], json!([]));
        assert_eq!(merged["members"], json!([]));
    }

    #[test]
    fn guild_update_replaces_a_stub() {
        let stub = json!({"id": "1", "unavailable": true});
        let merged = merge_guild_update(Some(stub), json!({"id": "1", "name": "b"}));
        assert_eq!(merged["name"], "b");
        assert_eq!(merged["unavailable"], false);
        assert_eq!(merged["roles"], json!([]));
        assert_eq!(merged["threads"], json!([]));
    }

    #[test]
    fn outage_keeps_the_guild() {
        let marked = mark_unavailable(Some(cached_guild()), 1);
        assert_eq!(marked["unavailable"], true);
        assert_eq!(marked["name"], "a");
        assert_eq!(marked["member_count"], 500);

        assert_eq!(
            mark_unavailable(None, 7),
            json!({"id": "7", "unavailable": true})
        );
    }

    #[test]
    fn member_count_moves_and_never_underflows() {
        let up = adjust_member_count(cached_guild(), 2).unwrap();
        assert_eq!(up["member_count"], 502);
        let down = adjust_member_count(json!({"member_count": 1}), -3).unwrap();
        assert_eq!(down["member_count"], 0);
        assert!(adjust_member_count(json!({"id": "1"}), 1).is_none());
        assert!(adjust_member_count(json!([1]), 1).is_none());
    }

    #[test]
    fn stubs_are_detected() {
        assert!(is_stub(&json!({"id": "1", "unavailable": true})));
        assert!(!is_stub(&cached_guild()));
    }
}
