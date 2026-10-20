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

/// Timestamp stored in Redis as `YYYY-MM-DDTHH:MM:SS.fffffffff` (UTC, no
/// offset). The format is kept for compatibility with existing consumers.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FormattedDateTime(OffsetDateTime);

const FORMAT: &[BorrowedFormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond]");

impl FormattedDateTime {
    pub fn now() -> Self {
        Self(OffsetDateTime::now_utc())
    }

    pub const fn from_offset(value: OffsetDateTime) -> Self {
        Self(value)
    }

    pub fn unix_millis(self) -> i64 {
        (self.0.unix_timestamp_nanos() / 1_000_000) as i64
    }

    pub fn parse(value: &str) -> Option<Self> {
        time::PrimitiveDateTime::parse(value, FORMAT)
            .ok()
            .map(|dt| Self(dt.assume_utc()))
    }

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
    pub session_id: String,
    pub sequence: u64,
    /// READY's `resume_gateway_url`; Discord asks clients to resume there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_url: Option<String>,
}

impl SessionInfo {
    pub fn is_resumable(&self) -> bool {
        !self.session_id.is_empty() && self.sequence > 0
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct StatusInfo {
    pub shard: u32,
    pub status: String,
    pub latency: u64,
    pub last_ack: FormattedDateTime,
}

pub type ApiResult<T> = Result<T, ApiError>;

macro_rules! make_api_errors {
    ($($name:ident($ty:ty)),* $(,)?) => {
        #[derive(Debug)]
        pub enum ApiError {
            $($name($ty)),*
        }

        $(
            impl From<$ty> for ApiError {
                fn from(err: $ty) -> Self {
                    Self::$name(err)
                }
            }
        )*

        impl Display for ApiError {
            fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
                match self {
                    $(Self::$name(err) => write!(f, "{}: {}", stringify!($name), err)),*
                }
            }
        }
    }
}

make_api_errors! {
    Config(ConfigError),
    Json(serde_json::Error),
    Redis(redis::RedisError),
    Lapin(lapin::Error),
    Io(std::io::Error),
    TwilightHttp(twilight_http::Error),
    DeserializeBody(twilight_http::response::DeserializeBodyError),
    Prometheus(prometheus::Error),
    Other(String),
}

impl Error for ApiError {}

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
}
