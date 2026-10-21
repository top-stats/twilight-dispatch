//! Values the service keeps in Redis (timestamps, shard sessions and
//! statuses) and [`ApiError`], the error of its fallible operations.

use crate::config::ConfigError;

use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    ops::{Add, Sub},
};
use time::{
    format_description::BorrowedFormatItem, macros::format_description, Duration, OffsetDateTime,
};

/// Timestamp stored in Redis as `YYYY-MM-DDTHH:MM:SS.f` (UTC, no offset),
/// where the fraction is the shortest exact one of 1 to 9 digits (`.0` for
/// whole seconds). The format is kept for compatibility with existing
/// consumers.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FormattedDateTime(OffsetDateTime);

const FORMAT: &[BorrowedFormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond]");

impl FormattedDateTime {
    /// The current time.
    pub fn now() -> Self {
        Self(OffsetDateTime::now_utc())
    }

    /// Wraps a point in time; it is formatted in UTC whatever its offset.
    pub const fn from_offset(value: OffsetDateTime) -> Self {
        Self(value)
    }

    /// Milliseconds since the Unix epoch, rounded toward zero.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "OffsetDateTime years are within +-999,999 (about 3.2e16 ms), far inside i64"
    )]
    pub fn unix_millis(self) -> i64 {
        (self.0.unix_timestamp_nanos() / 1_000_000) as i64
    }

    /// Parse the stored format; `None` for anything else, such as a
    /// timestamp with an offset.
    pub fn parse(value: &str) -> Option<Self> {
        time::PrimitiveDateTime::parse(value, FORMAT)
            .ok()
            .map(|dt| Self(dt.assume_utc()))
    }

    /// The stored format, in UTC. Empty if the time cannot be formatted.
    pub fn format(self) -> String {
        self.0
            .to_offset(time::UtcOffset::UTC)
            .format(FORMAT)
            .unwrap_or_default()
    }
}

impl Sub<Duration> for FormattedDateTime {
    type Output = Self;

    fn sub(self, rhs: Duration) -> Self::Output {
        Self(self.0 - rhs)
    }
}

impl Sub<FormattedDateTime> for FormattedDateTime {
    type Output = Duration;

    fn sub(self, rhs: FormattedDateTime) -> Self::Output {
        self.0 - rhs.0
    }
}

impl Add<Duration> for FormattedDateTime {
    type Output = Self;

    fn add(self, rhs: Duration) -> Self::Output {
        Self(self.0 + rhs)
    }
}

impl Serialize for FormattedDateTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.format())
    }
}

impl<'de> Deserialize<'de> for FormattedDateTime {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).ok_or_else(|| D::Error::custom("not a valid formatted timestamp"))
    }
}

/// A shard session that can be resumed after a restart.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionInfo {
    /// The session id from READY.
    pub session_id: String,
    /// The sequence to resume from: every event up to it has been handled.
    pub sequence: u64,
    /// READY's `resume_gateway_url`; Discord asks clients to resume there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_url: Option<String>,
}

impl SessionInfo {
    /// Whether the session can be resumed: it has an id and at least one
    /// event was handled.
    pub fn is_resumable(&self) -> bool {
        !self.session_id.is_empty() && self.sequence > 0
    }
}

/// One shard's entry in the `gateway_statuses` list in Redis.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StatusInfo {
    /// The shard id.
    pub shard: u32,
    /// Its state, one of `gateway::registry::STATUS_LABELS`.
    pub status: String,
    /// The latest heartbeat latency in milliseconds, 0 before the first.
    pub latency: u64,
    /// When the last heartbeat ACK arrived (the time of the update before
    /// the first one).
    pub last_ack: FormattedDateTime,
}

/// The result of the service's fallible operations.
pub type ApiResult<T> = Result<T, ApiError>;

/// Defines [`ApiError`] with one variant and one `From` impl per wrapped
/// error type, and an `Other` variant for a plain message.
macro_rules! make_api_errors {
    ($($(#[$doc:meta])* $name:ident($ty:ty)),* $(,)?) => {
        /// An error of the service: one of its dependencies' errors, or a
        /// message. `Display` names the variant (`Redis: ...`).
        #[derive(Debug)]
        pub enum ApiError {
            $($(#[$doc])* $name($ty),)*
            /// Any other failure, described by a message.
            Other(String),
        }

        $(
            impl From<$ty> for ApiError {
                fn from(err: $ty) -> Self {
                    Self::$name(err)
                }
            }
        )*

        impl From<String> for ApiError {
            fn from(err: String) -> Self {
                Self::Other(err)
            }
        }

        impl Display for ApiError {
            fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
                match self {
                    $(Self::$name(err) => write!(f, "{}: {}", stringify!($name), err),)*
                    Self::Other(err) => write!(f, "Other: {err}"),
                }
            }
        }

        impl Error for ApiError {
            // The wrapped error's own source: its message is already part of
            // this error's, so an error chain shows each message once.
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                match self {
                    $(Self::$name(err) => Error::source(err),)*
                    Self::Other(_) => None,
                }
            }
        }
    };
}

make_api_errors! {
    /// Invalid or missing configuration.
    Config(ConfigError),
    /// JSON could not be serialized or deserialized.
    Json(serde_json::Error),
    /// A Redis command or connection failed.
    Redis(redis::RedisError),
    /// An AMQP connection, channel or publish failed.
    Lapin(lapin::Error),
    /// An I/O operation failed, such as binding the metrics listener.
    Io(std::io::Error),
    /// A Discord REST request failed.
    TwilightHttp(twilight_http::Error),
    /// A Discord REST response body could not be deserialized.
    DeserializeBody(twilight_http::response::DeserializeBodyError),
    /// Registering or encoding Prometheus metrics failed.
    Prometheus(prometheus::Error),
}

impl From<&str> for ApiError {
    fn from(err: &str) -> Self {
        Self::Other(err.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn timestamp_round_trips_to_the_nanosecond() {
        for value in [
            datetime!(2026-10-20 12:30:04.836086 UTC),
            datetime!(2026-10-20 12:30:04 UTC),
            datetime!(2026-10-20 12:30:04.123 UTC),
            datetime!(2026-10-20 12:30:04.999999999 UTC),
        ] {
            let ts = FormattedDateTime::from_offset(value);
            let json = serde_json::to_string(&ts).unwrap();
            let back: FormattedDateTime = serde_json::from_str(&json).unwrap();
            assert_eq!(back, ts, "{json}");
        }
    }

    #[test]
    fn timestamp_format_is_unchanged() {
        let ts = FormattedDateTime::from_offset(datetime!(2026-10-20 12:30:04.836086 UTC));
        assert_eq!(
            serde_json::to_string(&ts).unwrap(),
            r#""2026-10-20T12:30:04.836086""#
        );
        // Values written by older versions still parse.
        assert!(FormattedDateTime::parse("2026-10-20T12:30:04.836086").is_some());
        assert!(serde_json::from_str::<FormattedDateTime>(r#""garbage""#).is_err());
    }

    #[test]
    fn unix_millis() {
        let ts = FormattedDateTime::from_offset(datetime!(1970-01-01 00:00:01.5 UTC));
        assert_eq!(ts.unix_millis(), 1500);
    }

    #[test]
    fn sessions_without_resume_url_still_parse() {
        let old: SessionInfo =
            serde_json::from_str(r#"{"session_id":"abc","sequence":42}"#).unwrap();
        assert_eq!(old.resume_url, None);
        assert!(old.is_resumable());

        let new = SessionInfo {
            resume_url: Some("wss://gateway-us-east1-b.discord.gg".into()),
            ..old.clone()
        };
        let json = serde_json::to_string(&new).unwrap();
        assert_eq!(serde_json::from_str::<SessionInfo>(&json).unwrap(), new);
        assert_eq!(
            serde_json::to_string(&old).unwrap(),
            r#"{"session_id":"abc","sequence":42}"#
        );
    }

    #[test]
    fn empty_sessions_are_not_resumable() {
        let empty = SessionInfo {
            session_id: String::new(),
            sequence: 0,
            resume_url: None,
        };
        assert!(!empty.is_resumable());
    }

    #[test]
    fn a_session_needs_both_an_id_and_a_sequence() {
        let session = |id: &str, sequence| SessionInfo {
            session_id: id.to_owned(),
            sequence,
            resume_url: None,
        };
        assert!(session("abc", 1).is_resumable());
        assert!(!session("abc", 0).is_resumable(), "nothing received yet");
        assert!(!session("", 5).is_resumable(), "no session id");
    }
}
