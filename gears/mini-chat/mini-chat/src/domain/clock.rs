//! Application clock: UTC, truncated to microseconds so stored values round-trip
//! identically on `PostgreSQL` and `SQLite`.
//!
//! Every stored timestamp is a `chrono::DateTime<Utc>`: on `SQLite` it is written
//! as RFC 3339 text with `+00:00` (the shape the toolkit `OData` layer binds for
//! `$filter` literals and cursors), so equality and ordering compare like for like.

use chrono::{DateTime, Timelike, Utc};

/// Current UTC time truncated to whole microseconds.
#[must_use]
pub fn now_utc() -> DateTime<Utc> {
    truncate_to_micros(Utc::now())
}

/// `t` with its sub-microsecond part dropped.
#[must_use]
pub fn truncate_to_micros(t: DateTime<Utc>) -> DateTime<Utc> {
    let nanos = t.nanosecond();
    t.with_nanosecond(nanos - nanos % 1_000).unwrap_or(t)
}
