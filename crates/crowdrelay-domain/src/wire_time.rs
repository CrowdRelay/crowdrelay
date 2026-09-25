//! Timestamps and dates that leave as text but still read what was stored.
//!
//! [`crate::iso_date`] and `time::serde::rfc3339` are right for write-only
//! types. A type that also deserializes its own JSON back — an idempotent
//! response replayed from `idempotency_keys`, a pass or coupon result stored
//! and served again — has rows already written in serde's tuple form,
//! `[2026, 268, 7, 0, 0, 0, 0, 0, 0]`. Switching such a type to plain
//! `rfc3339` would make every stored row unreadable.
//!
//! These modules write RFC 3339 (`YYYY-MM-DD` for a date) and read either
//! shape. Clients get text they can parse: the Signal app declares a pass's
//! `redeemed_at` as `Option<String>` and failed to decode the tuple, and the
//! Virya site declares every one of these as a string. Stored tuples keep
//! reading.
//!
//! Use `#[serde(with = "crowdrelay_domain::wire_time")]` (or `::option`) on an
//! `OffsetDateTime`, and `wire_time::date` (or `::date_option`) on a `Date`.

use serde::{Deserialize, Deserializer, Serializer};
use time::{Date, OffsetDateTime};

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredTime {
    Text(String),
    Tuple(OffsetDateTime),
}

impl StoredTime {
    fn into_time<E: serde::de::Error>(self) -> Result<OffsetDateTime, E> {
        match self {
            Self::Tuple(value) => Ok(value),
            Self::Text(text) => {
                OffsetDateTime::parse(&text, &time::format_description::well_known::Rfc3339)
                    .map_err(E::custom)
            }
        }
    }
}

/// Writes RFC 3339.
///
/// # Errors
/// When the value has no RFC 3339 form (a year outside 0–9999).
pub fn serialize<S: Serializer>(value: &OffsetDateTime, serializer: S) -> Result<S::Ok, S::Error> {
    time::serde::rfc3339::serialize(value, serializer)
}

/// Reads RFC 3339 text or the stored tuple.
///
/// # Errors
/// When the input is neither.
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<OffsetDateTime, D::Error> {
    StoredTime::deserialize(deserializer)?.into_time()
}

pub mod option {
    use super::{Deserialize, Deserializer, OffsetDateTime, Serializer, StoredTime};

    /// Writes RFC 3339, or `null`.
    ///
    /// # Errors
    /// When the value has no RFC 3339 form.
    pub fn serialize<S: Serializer>(
        value: &Option<OffsetDateTime>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        time::serde::rfc3339::option::serialize(value, serializer)
    }

    /// Reads RFC 3339 text, the stored tuple, or `null`.
    ///
    /// # Errors
    /// When the input is none of those.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<OffsetDateTime>, D::Error> {
        Option::<StoredTime>::deserialize(deserializer)?
            .map(StoredTime::into_time)
            .transpose()
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StoredDate {
    Text(String),
    Tuple(Date),
}

impl StoredDate {
    fn into_date<E: serde::de::Error>(self) -> Result<Date, E> {
        match self {
            Self::Tuple(value) => Ok(value),
            Self::Text(text) => Date::parse(
                &text,
                time::macros::format_description!("[year]-[month]-[day]"),
            )
            .map_err(E::custom),
        }
    }
}

pub mod date {
    use super::{Date, Deserialize, Deserializer, Serializer, StoredDate};

    /// Writes `YYYY-MM-DD`.
    ///
    /// # Errors
    /// When the serializer refuses a string.
    pub fn serialize<S: Serializer>(value: &Date, serializer: S) -> Result<S::Ok, S::Error> {
        crate::iso_date::serialize(value, serializer)
    }

    /// Reads `YYYY-MM-DD` or the stored tuple.
    ///
    /// # Errors
    /// When the input is neither.
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Date, D::Error> {
        StoredDate::deserialize(deserializer)?.into_date()
    }
}

pub mod date_option {
    use super::{Date, Deserialize, Deserializer, Serializer, StoredDate};

    /// Writes `YYYY-MM-DD`, or `null`.
    ///
    /// # Errors
    /// When the serializer refuses a string.
    pub fn serialize<S: Serializer>(
        value: &Option<Date>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        crate::iso_date::option::serialize(value, serializer)
    }

    /// Reads `YYYY-MM-DD`, the stored tuple, or `null`.
    ///
    /// # Errors
    /// When the input is none of those.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Date>, D::Error> {
        Option::<StoredDate>::deserialize(deserializer)?
            .map(StoredDate::into_date)
            .transpose()
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use time::macros::{date, datetime};

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Row {
        #[serde(with = "super")]
        at: time::OffsetDateTime,
        #[serde(with = "super::option")]
        maybe: Option<time::OffsetDateTime>,
        #[serde(with = "super::date")]
        day: time::Date,
        #[serde(with = "super::date_option")]
        maybe_day: Option<time::Date>,
    }

    fn row() -> Row {
        Row {
            at: datetime!(2026-09-25 07:00 UTC),
            maybe: Some(datetime!(2026-09-25 07:00 UTC)),
            day: date!(2026 - 09 - 25),
            maybe_day: None,
        }
    }

    #[test]
    fn it_writes_text() {
        assert_eq!(
            serde_json::to_string(&row()).expect("serializes"),
            r#"{"at":"2026-09-25T07:00:00Z","maybe":"2026-09-25T07:00:00Z","day":"2026-09-25","maybe_day":null}"#
        );
    }

    #[test]
    fn it_reads_text_and_the_stored_tuple() {
        let text = r#"{"at":"2026-09-25T07:00:00Z","maybe":"2026-09-25T07:00:00Z","day":"2026-09-25","maybe_day":null}"#;
        let tuple = r#"{"at":[2026,268,7,0,0,0,0,0,0],"maybe":[2026,268,7,0,0,0,0,0,0],"day":[2026,268],"maybe_day":null}"#;
        assert_eq!(serde_json::from_str::<Row>(text).expect("text"), row());
        assert_eq!(serde_json::from_str::<Row>(tuple).expect("tuple"), row());
    }
}
