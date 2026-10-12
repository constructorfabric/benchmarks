//! Canonical persisted timestamp encoding.
//!
//! Every timestamp column mini-chat writes is declared as a UTC `DateTimeUtc`
//! (chrono) in its entity, never `time::OffsetDateTime`. The platform `OData`
//! layer binds `$filter` values and cursor values as chrono UTC; on `SQLite`
//! that is the text `YYYY-MM-DDTHH:MM:SS[.fff|.ffffff|.fffffffff]+00:00`. Storing
//! the very same encoding makes `eq`/`ne`/`in` exact and keeps plain text order
//! equal to chronological order (the `+` terminator sorts below every fraction
//! digit, and the fraction width is always a multiple of three). On `PostgreSQL`
//! the column is a real `TIMESTAMPTZ`, so the encoding is irrelevant there.
//!
//! The domain keeps `time::OffsetDateTime`; repositories convert at the
//! persistence boundary with [`to_db`] / [`from_db`] and must never bind a
//! `time` value to a timestamp column or a `$filter`/`$orderby` expression.

use sea_orm::prelude::DateTimeUtc;
use time::OffsetDateTime;

/// Domain timestamp to the canonical persisted value.
#[must_use]
pub fn to_db(value: OffsetDateTime) -> DateTimeUtc {
    let nanos = value.unix_timestamp_nanos();
    let secs = i64::try_from(nanos.div_euclid(1_000_000_000)).unwrap_or_default();
    let sub = u32::try_from(nanos.rem_euclid(1_000_000_000)).unwrap_or_default();
    DateTimeUtc::from_timestamp(secs, sub).unwrap_or_default()
}

/// Optional [`to_db`].
#[must_use]
pub fn to_db_opt(value: Option<OffsetDateTime>) -> Option<DateTimeUtc> {
    value.map(to_db)
}

/// Persisted value to the domain timestamp (UTC).
#[must_use]
pub fn from_db(value: DateTimeUtc) -> OffsetDateTime {
    let nanos =
        i128::from(value.timestamp()) * 1_000_000_000 + i128::from(value.timestamp_subsec_nanos());
    OffsetDateTime::from_unix_timestamp_nanos(nanos).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}
