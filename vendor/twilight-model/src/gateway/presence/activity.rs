use crate::{
    gateway::presence::{
        ActivityAssets, ActivityButton, ActivityEmoji, ActivityFlags, ActivityParty,
        ActivitySecrets, ActivityTimestamps, ActivityType,
    },
    id::{Id, marker::ApplicationMarker},
};
use serde::{Deserialize, Serialize};

/// Discord sometimes sends `application_id: 0` (or `"0"`) for activities
/// without an application, which a non-zero [`Id`] cannot represent. Treat
/// zero and empty values as absent. (twilight-dispatch patch)
fn deserialize_nullable_application_id<'de, D>(
    de: D,
) -> Result<Option<Id<ApplicationMarker>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{Error, Visitor};
    use std::fmt;

    struct NullableIdVisitor;

    impl<'de> Visitor<'de> for NullableIdVisitor {
        type Value = Option<Id<ApplicationMarker>>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a discord snowflake (string or integer), zero, or null")
        }

        fn visit_unit<E: Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_none<E: Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D: serde::Deserializer<'de>>(self, de: D) -> Result<Self::Value, D::Error> {
            de.deserialize_any(NullableIdVisitor)
        }

        fn visit_u64<E: Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Id::new_checked(v))
        }

        fn visit_i64<E: Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok(u64::try_from(v).ok().and_then(Id::new_checked))
        }

        fn visit_str<E: Error>(self, v: &str) -> Result<Self::Value, E> {
            if v.is_empty() || v == "0" {
                return Ok(None);
            }
            v.parse::<u64>()
                .map(Id::new_checked)
                .map_err(|_| E::custom(format!("invalid snowflake string: {v}")))
        }
    }

    de.deserialize_any(NullableIdVisitor)
}


#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct Activity {
    #[serde(
        default,
        deserialize_with = "deserialize_nullable_application_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub application_id: Option<Id<ApplicationMarker>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assets: Option<ActivityAssets>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buttons: Vec<ActivityButton>,
    /// Unix timestamp of when the activity was added to the user's session, in
    /// milliseconds.
    pub created_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emoji: Option<ActivityEmoji>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flags: Option<ActivityFlags>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<bool>,
    #[serde(default = "ActivityType::default", rename = "type")]
    pub kind: ActivityType,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub party: Option<ActivityParty>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secrets: Option<ActivitySecrets>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamps: Option<ActivityTimestamps>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[cfg(test)]
mod tests {
    // Custom activities is tested by the custom presence test.
    // The application_id patch is tested in twilight-dispatch (src/model_patches.rs).
}
