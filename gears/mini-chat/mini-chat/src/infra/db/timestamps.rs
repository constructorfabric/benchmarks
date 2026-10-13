//! Comparable timestamps on `SQLite` for `OData` filters, ordering and cursors.
//!
//! `SQLite` stores timestamps as TEXT and compares them as text. The ORM writes
//! `time::OffsetDateTime` as RFC 3339 with trailing subsecond zeros trimmed and
//! a `Z` suffix (`…:30Z`, `…:30.5Z`, `…:30.25Z`). These strings do not sort
//! chronologically (`"30.5Z" < "30Z"`). `OData` filter literals are bound as
//! chrono values (`…:30+00:00`), which never equal the stored text. PostgreSQL
//! (`TIMESTAMPTZ`) has neither problem.
//!
//! The fix is in two parts:
//!
//! - **Storage.** On `SQLite`, triggers rewrite every listed timestamp column
//!   to a fixed-width form ([`sqlite_text`], `YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ`)
//!   after insert and update. Fixed-width UTC text sorts chronologically and
//!   still decodes as RFC 3339. The triggers come from [`sqlite_triggers_sql`],
//!   for the columns in [`SORTABLE_TIMESTAMP_COLUMNS`].
//! - **Binding.** `OData` mappers are generic over a [`Dialect`]. Under
//!   [`SqliteDialect`], filter literals ([`filter_value`]) and cursor values
//!   ([`cursor_kind`] / [`cursor_value`]) are bound as the same fixed-width
//!   text. Under [`PgDialect`] they keep their native timestamp types. Pick the
//!   dialect at run time with [`is_sqlite`].
//!
//! To make another timestamp field filterable: add `(table, column)` to
//! [`SORTABLE_TIMESTAMP_COLUMNS`] (with a migration if the schema is already
//! deployed). Then use [`filter_value`], [`cursor_kind`] and [`cursor_value`]
//! for that field in the mapper.

use sea_orm::DbBackend;
use time::OffsetDateTime;
use toolkit_odata::filter::{FieldKind, ODataValue};

/// Timestamp columns kept in the fixed-width text form on `SQLite`.
pub const SORTABLE_TIMESTAMP_COLUMNS: &[(&str, &str)] =
    &[("chats", "updated_at"), ("messages", "created_at")];

/// Length of the fixed-width text form (`YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ`).
pub const SQLITE_TEXT_LEN: usize = 30;

/// Fixed-width UTC text of `t` (`YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ`, years
/// 0..=9999).
#[must_use]
pub fn sqlite_text(t: OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:09}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.nanosecond()
    )
}

/// SQL turning an RFC 3339 UTC text `expr` (`…SS[.f{1,9}]Z`) into the
/// fixed-width form.
fn sqlite_canonical_expr(expr: &str) -> String {
    format!(
        "substr({expr}, 1, 19) || '.' || substr((CASE WHEN substr({expr}, 20, 1) = '.' \
         THEN substr({expr}, 21, length({expr}) - 21) ELSE '' END) || '000000000', 1, 9) || 'Z'"
    )
}

/// `SQLite` statements that rewrite `table.column` to the fixed-width form:
/// a backfill `UPDATE` plus `AFTER INSERT` / `AFTER UPDATE OF column`
/// triggers. Only UTC (`…Z`) values that are not already fixed-width are
/// rewritten. Rows are matched by `id`.
#[must_use]
pub fn sqlite_triggers_sql(table: &str, column: &str) -> Vec<String> {
    let needs_fix = |v: &str| format!("{v} LIKE '%Z' AND length({v}) <> {SQLITE_TEXT_LEN}");
    let rewrite = |v: &str| sqlite_canonical_expr(v);
    let new_col = format!("NEW.{column}");
    vec![
        format!(
            "UPDATE {table} SET {column} = {} WHERE {}",
            rewrite(column),
            needs_fix(column)
        ),
        format!(
            "CREATE TRIGGER IF NOT EXISTS {table}_{column}_fixed_width_ins \
             AFTER INSERT ON {table} WHEN {} \
             BEGIN UPDATE {table} SET {column} = {} WHERE id = NEW.id; END",
            needs_fix(&new_col),
            rewrite(&new_col)
        ),
        format!(
            "CREATE TRIGGER IF NOT EXISTS {table}_{column}_fixed_width_upd \
             AFTER UPDATE OF {column} ON {table} WHEN {} \
             BEGIN UPDATE {table} SET {column} = {} WHERE id = NEW.id; END",
            needs_fix(&new_col),
            rewrite(&new_col)
        ),
    ]
}

/// Statements dropping the triggers of [`sqlite_triggers_sql`].
#[must_use]
pub fn sqlite_drop_triggers_sql(table: &str, column: &str) -> Vec<String> {
    ["ins", "upd"]
        .iter()
        .map(|s| format!("DROP TRIGGER IF EXISTS {table}_{column}_fixed_width_{s}"))
        .collect()
}

/// Timestamp columns the background workers compare with a cutoff (orphan
/// watchdog, upload reaper), kept in the fixed-width form on `SQLite` by
/// migration `m20261005_000003_sqlite_worker_timestamps`.
pub const WORKER_TIMESTAMP_COLUMNS: &[(&str, &str)] = &[
    ("chat_turns", "started_at"),
    ("chat_turns", "last_progress_at"),
    ("attachments", "updated_at"),
];

/// `t` bound for comparison with a fixed-width column (see
/// [`WORKER_TIMESTAMP_COLUMNS`]): the fixed-width text on `SQLite`, a native
/// timestamp otherwise.
#[must_use]
pub fn comparable(backend: DbBackend, t: OffsetDateTime) -> sea_orm::Value {
    if is_sqlite(backend) {
        sea_orm::Value::String(Some(sqlite_text(t)))
    } else {
        sea_orm::Value::TimeDateTimeWithTimeZone(Some(t))
    }
}

/// SQL dialect marker used by the `OData` mappers.
pub trait Dialect: Send + Sync + 'static {
    /// Whether timestamps are compared as fixed-width text (`SQLite`).
    const TEXT_TIMESTAMPS: bool;
}

/// `SQLite`: timestamps are bound as fixed-width text.
#[derive(Debug, Clone, Copy, Default)]
pub struct SqliteDialect;

/// PostgreSQL: timestamps are bound as native timestamps.
#[derive(Debug, Clone, Copy, Default)]
pub struct PgDialect;

impl Dialect for SqliteDialect {
    const TEXT_TIMESTAMPS: bool = true;
}

impl Dialect for PgDialect {
    const TEXT_TIMESTAMPS: bool = false;
}

/// Whether `backend` needs [`SqliteDialect`].
#[must_use]
pub fn is_sqlite(backend: DbBackend) -> bool {
    matches!(backend, DbBackend::Sqlite)
}

/// `OData` filter literal for a timestamp field (use in
/// `FieldToColumn::map_value`). Under `SQLite`, a datetime literal becomes
/// the fixed-width text. Other values are unchanged.
#[must_use]
pub fn filter_value<D: Dialect>(value: &ODataValue) -> ODataValue {
    match value {
        ODataValue::DateTime(dt) if D::TEXT_TIMESTAMPS => {
            ODataValue::String(dt.format("%Y-%m-%dT%H:%M:%S%.9fZ").to_string())
        }
        other => other.clone(),
    }
}

/// Cursor codec kind of a timestamp field (for `ODataFieldMapping::cursor_kind`).
#[must_use]
pub fn cursor_kind<D: Dialect>() -> FieldKind {
    if D::TEXT_TIMESTAMPS {
        FieldKind::String
    } else {
        FieldKind::DateTimeUtc
    }
}

/// Cursor value of a timestamp (for `ODataFieldMapping::extract_cursor_value`).
#[must_use]
pub fn cursor_value<D: Dialect>(t: OffsetDateTime) -> sea_orm::Value {
    if D::TEXT_TIMESTAMPS {
        sea_orm::Value::String(Some(sqlite_text(t)))
    } else {
        sea_orm::Value::TimeDateTimeWithTimeZone(Some(t))
    }
}

#[cfg(test)]
#[path = "timestamps_tests.rs"]
mod tests;
