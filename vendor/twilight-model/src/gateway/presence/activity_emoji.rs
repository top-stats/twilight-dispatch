use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ActivityEmoji {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub animated: Option<bool>,
    pub name: String,
    // A snowflake as a string or an integer, kept as a string.
    // (twilight-dispatch patch)
    #[serde(
        default,
        deserialize_with = "deserialize_id",
        skip_serializing_if = "Option::is_none"
    )]
    pub id: Option<String>,
}

/// A string or integer id as a string. (twilight-dispatch patch)
fn deserialize_id<'de, D>(de: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{Error, Visitor};
    use std::fmt;

    struct IdVisitor;

    impl<'de> Visitor<'de> for IdVisitor {
        type Value = Option<String>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("an emoji id (string or integer) or null")
        }

        fn visit_unit<E: Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_none<E: Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D: serde::Deserializer<'de>>(self, de: D) -> Result<Self::Value, D::Error> {
            de.deserialize_any(IdVisitor)
        }

        fn visit_u64<E: Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_i64<E: Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_str<E: Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(Some(v.to_owned()))
        }

        fn visit_string<E: Error>(self, v: String) -> Result<Self::Value, E> {
            Ok(Some(v))
        }
    }

    de.deserialize_option(IdVisitor)
}

#[cfg(test)]
mod tests {
    use super::ActivityEmoji;
    use serde_test::Token;

    #[test]
    fn activity_emoji() {
        let value = ActivityEmoji {
            animated: Some(false),
            name: "a".to_owned(),
            id: None,
        };

        serde_test::assert_tokens(
            &value,
            &[
                Token::Struct {
                    name: "ActivityEmoji",
                    len: 2,
                },
                Token::Str("animated"),
                Token::Some,
                Token::Bool(false),
                Token::Str("name"),
                Token::Str("a"),
                Token::StructEnd,
            ],
        );
    }

    #[test]
    fn activity_emoji_complete() {
        let value = ActivityEmoji {
            animated: Some(false),
            name: "a".to_owned(),
            id: Some("123".to_owned()),
        };

        serde_test::assert_tokens(
            &value,
            &[
                Token::Struct {
                    name: "ActivityEmoji",
                    len: 3,
                },
                Token::Str("animated"),
                Token::Some,
                Token::Bool(false),
                Token::Str("name"),
                Token::Str("a"),
                Token::Str("id"),
                Token::Some,
                Token::Str("123"),
                Token::StructEnd,
            ],
        );
    }
}
