use crate::{
    gateway::presence::{
        ActivityAssets, ActivityButton, ActivityEmoji, ActivityFlags, ActivityParty,
        ActivitySecrets, ActivityTimestamps, ActivityType,
    },
    id::{marker::ApplicationMarker, Id},
};
use serde::{Deserialize, Serialize};

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

        fn visit_some<D: serde::Deserializer<'de>>(
            self,
            de: D,
        ) -> Result<Self::Value, D::Error> {
            de.deserialize_any(NullableIdVisitor)
        }

        fn visit_u64<E: Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Id::new_checked(v))
        }

        fn visit_i64<E: Error>(self, v: i64) -> Result<Self::Value, E> {
            if v <= 0 {
                Ok(None)
            } else {
                Ok(Id::new_checked(v as u64))
            }
        }

        fn visit_str<E: Error>(self, v: &str) -> Result<Self::Value, E> {
            if v.is_empty() || v == "0" {
                return Ok(None);
            }
            v.parse::<u64>()
                .map(Id::new_checked)
                .map_err(|_| E::custom(format!("invalid snowflake string: {}", v)))
        }

        fn visit_string<E: Error>(self, v: String) -> Result<Self::Value, E> {
            self.visit_str(&v)
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
    // Introduced with custom statuses.
    pub created_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emoji: Option<ActivityEmoji>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flags: Option<ActivityFlags>,
    // Introduced with custom statuses.
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
}
