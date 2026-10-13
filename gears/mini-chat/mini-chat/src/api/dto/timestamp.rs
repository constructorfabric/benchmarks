//! RFC 3339 serialization of API timestamps at microsecond precision.
//!
//! Stored timestamps carry one extra nanosecond (`infra::db::ts::normalize`, for a fixed
//! nine-digit text width in `SQLite`) and `PostgreSQL` drops it on write, so the in-memory value of
//! a create response and the value read back later may differ by 1 ns. Every DTO timestamp is
//! serialized through this module instead of `time::serde::rfc3339`: truncated to microseconds
//! (the precision of the store), so clients see neither the artefact nor the difference.
//! Cursors are built from the stored rows, not from these values; `$filter` literals are
//! normalized back to the stored shape, so a truncated value filters like the stored one.

use serde::{Deserializer, Serializer};
use time::OffsetDateTime;

/// `t` truncated to whole microseconds.
#[must_use]
pub fn to_api(t: OffsetDateTime) -> OffsetDateTime {
    t.replace_nanosecond(t.nanosecond() - t.nanosecond() % 1_000)
        .unwrap_or(t)
}

/// Serializes `t` as RFC 3339 truncated to microseconds.
///
/// # Errors
/// When the value cannot be formatted as RFC 3339 (year outside 0..=9999).
pub fn serialize<S: Serializer>(t: &OffsetDateTime, s: S) -> Result<S::Ok, S::Error> {
    time::serde::rfc3339::serialize(&to_api(*t), s)
}

/// Parses an RFC 3339 timestamp.
///
/// # Errors
/// When the input is not RFC 3339.
pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<OffsetDateTime, D::Error> {
    time::serde::rfc3339::deserialize(d)
}

/// [`serialize`] / [`deserialize`] for `Option<OffsetDateTime>`.
pub mod option {
    use serde::{Deserializer, Serializer};
    use time::OffsetDateTime;

    /// Serializes `t` like [`super::serialize`], `None` as `null`.
    ///
    /// # Errors
    /// See [`super::serialize`].
    #[allow(clippy::ref_option)] // the signature serde's `with` expects
    pub fn serialize<S: Serializer>(t: &Option<OffsetDateTime>, s: S) -> Result<S::Ok, S::Error> {
        time::serde::rfc3339::option::serialize(&t.map(super::to_api), s)
    }

    /// Parses an optional RFC 3339 timestamp.
    ///
    /// # Errors
    /// When the input is neither `null` nor RFC 3339.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<OffsetDateTime>, D::Error> {
        time::serde::rfc3339::option::deserialize(d)
    }
}

#[cfg(test)]
mod tests {
    use serde::Serialize;
    use time::macros::datetime;

    use crate::infra::db::ts::normalize;

    #[derive(Serialize)]
    struct Dto {
        #[serde(with = "super")]
        at: time::OffsetDateTime,
        #[serde(with = "super::option")]
        maybe: Option<time::OffsetDateTime>,
    }

    fn json(at: time::OffsetDateTime) -> serde_json::Value {
        serde_json::to_value(Dto {
            at,
            maybe: Some(at),
        })
        .unwrap()
    }

    #[test]
    fn stored_and_postgres_read_back_values_serialize_identically() {
        let raw = datetime!(2026-10-04 12:00:00.123_456_789 UTC);
        // In memory after `normalize` (+1 ns) vs. read back from PostgreSQL (µs).
        let in_memory = normalize(raw);
        let from_pg = datetime!(2026-10-04 12:00:00.123_456 UTC);
        assert_eq!(json(in_memory), json(from_pg));
        assert_eq!(json(in_memory)["at"], "2026-10-04T12:00:00.123456Z");
        assert_eq!(json(in_memory)["maybe"], "2026-10-04T12:00:00.123456Z");
        let none = serde_json::to_value(Dto {
            at: from_pg,
            maybe: None,
        })
        .unwrap();
        assert_eq!(none["maybe"], serde_json::Value::Null);
    }

    #[test]
    fn whole_seconds_have_no_fraction() {
        let at = normalize(datetime!(2026-10-04 12:00:00 UTC));
        assert_eq!(json(at)["at"], "2026-10-04T12:00:00Z");
    }
}
