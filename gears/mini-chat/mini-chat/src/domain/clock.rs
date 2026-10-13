//! Single source of the timestamps the gear writes.

use time::OffsetDateTime;

/// The current UTC time truncated to microseconds.
///
/// `PostgreSQL` stores `TIMESTAMPTZ` with microsecond precision while the
/// responses and payloads of the gear carry nanoseconds, so every timestamp the
/// gear generates (create, rename, delete, reaction) is truncated here, once:
/// the stored value, the response value and an `OData` filter or cursor built
/// from either agree on both engines.
#[must_use]
pub fn now_micros() -> OffsetDateTime {
    OffsetDateTime::now_utc().truncate_to_microsecond()
}
