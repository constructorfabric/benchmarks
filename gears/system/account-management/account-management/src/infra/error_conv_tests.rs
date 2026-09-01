//! Tests for the infra-layer DB-error classification predicates.
//!
//! Exercises [`is_serialization_failure`], [`is_check_violation`], and
//! [`is_db_availability_error`] directly — these are the typed signals
//! that [`From<DomainError> for CanonicalError`] consumes at the boundary.
//! Boundary-mapping coverage (`DbErr` → `CanonicalError` category + HTTP
//! status) lives in `domain/error_tests.rs` and is intentionally kept
//! there alongside the mapping itself.

use super::{is_check_violation, is_db_availability_error, is_serialization_failure};
use sea_orm::{ConnAcquireErr, DbErr, RuntimeErr};
use toolkit_db::DbError;

#[test]
fn unclassified_db_err_is_not_serialization_failure() {
    let db_err = DbErr::Custom("nothing transient".into());
    assert!(!is_serialization_failure(&db_err));
}

#[test]
fn connection_acquire_timeout_is_db_availability_error() {
    let wrapped = DbError::Sea(DbErr::ConnectionAcquire(ConnAcquireErr::Timeout));
    assert!(is_db_availability_error(&wrapped));
}

#[test]
fn io_error_is_db_availability_error() {
    let io_err = std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset by peer");
    let wrapped = DbError::Io(io_err);
    assert!(is_db_availability_error(&wrapped));
}

#[test]
fn custom_db_err_is_not_db_availability_error() {
    let wrapped = DbError::Sea(DbErr::Custom("query failed".into()));
    assert!(!is_db_availability_error(&wrapped));
}

// ---- is_check_violation -------------------------------------------
//
// Coverage pins each documented match arm in `is_check_violation`.
// The two end-to-end shapes (Postgres human-readable + SQLite default
// "CHECK constraint failed") are exercised through the full
// `classify_db_err_to_domain` chain in `canonical_mapping_tests.rs`;
// the unit tests below additionally cover the proxy-stripped SQLite
// code-token variants so a future typed-discriminant refactor that
// drops one of the fallback arms fails here loudly rather than
// silently regressing the SQLite path back to HTTP 500.

#[test]
fn is_check_violation_matches_postgres_human_readable_text() {
    let db_err = DbErr::Exec(RuntimeErr::Internal(
        "new row for relation \"tenants\" violates check constraint \"ck_tenants_root_depth\""
            .into(),
    ));
    assert!(is_check_violation(&db_err));
}

#[test]
fn is_check_violation_matches_postgres_sqlstate_token() {
    // `23514` token alone, e.g. proxy that emits only SQLSTATE.
    let db_err = DbErr::Exec(RuntimeErr::Internal(
        "error returned from database: error with SQLSTATE 23514".into(),
    ));
    assert!(is_check_violation(&db_err));
}

#[test]
fn is_check_violation_matches_postgres_symbolic_name() {
    // `check_violation` is the symbolic SQLSTATE name.
    let db_err = DbErr::Exec(RuntimeErr::Internal(
        "ERROR: check_violation: row rejected".into(),
    ));
    assert!(is_check_violation(&db_err));
}

#[test]
fn is_check_violation_matches_sqlite_default_text() {
    let db_err = DbErr::Exec(RuntimeErr::Internal(
        "CHECK constraint failed: ck_conversion_requests_actor_invariant".into(),
    ));
    assert!(is_check_violation(&db_err));
}

#[test]
fn is_check_violation_matches_sqlite_symbolic_code() {
    // Some SQLite drivers emit the symbolic extended-error name only.
    let db_err = DbErr::Exec(RuntimeErr::Internal(
        "SQLITE_CONSTRAINT_CHECK: invariant rejected".into(),
    ));
    assert!(is_check_violation(&db_err));
}

#[test]
fn is_check_violation_matches_sqlite_code_token() {
    // `"code 275"` form (proxy strips the default text but keeps the code).
    let db_err = DbErr::Exec(RuntimeErr::Internal(
        "sqlite error: code 275 (extended)".into(),
    ));
    assert!(is_check_violation(&db_err));
}

#[test]
fn is_check_violation_matches_sqlite_paren_code() {
    // `"(275)"` form (proxy emits only the parenthesised code).
    let db_err = DbErr::Exec(RuntimeErr::Internal(
        "sqlite constraint (275): row rejected".into(),
    ));
    assert!(is_check_violation(&db_err));
}

#[test]
fn is_check_violation_rejects_unrelated_error() {
    let db_err = DbErr::Custom("connection reset".into());
    assert!(
        !is_check_violation(&db_err),
        "unrelated DbErr must not be classified as CHECK violation"
    );
}

#[test]
fn is_check_violation_rejects_unrelated_23514_substring_without_pg_context() {
    // Regression guard for the false-positive risk in the PG SQLSTATE
    // numeric arm: a `DbErr` whose Display contains the substring
    // `"23514"` outside a SQLSTATE / code shape (a retry counter, a
    // millisecond timeout, a row count) MUST NOT be classified as a
    // CHECK violation just because the digits coincide. The fallback
    // requires `"sqlstate"`, `"code"`, `"23514:"`, or `"(23514)"`
    // context tokens.
    let retry = DbErr::Custom("retry attempt 23514 of unbounded".into());
    assert!(!is_check_violation(&retry));
    let timeout = DbErr::Custom("operation timed out (23514 ms)".into());
    assert!(!is_check_violation(&timeout));
    let rows = DbErr::Custom("affected 23514 rows in batch".into());
    assert!(!is_check_violation(&rows));
}

#[test]
fn is_check_violation_rejects_unrelated_275_substring_without_sqlite_context() {
    // Regression guard for the false-positive risk in the numeric-
    // code fallback: a non-SQLite error whose Display contains the
    // substring `"275"` (a byte offset, a timeout in ms, a port
    // number, etc.) MUST NOT be classified as a CHECK violation just
    // because the digits coincide. The fallback is gated on
    // `"sqlite"` so the engine context is required for the match.
    let timeout = DbErr::Custom("operation timed out (275 ms) on host db-12".into());
    assert!(!is_check_violation(&timeout));
    let offset = DbErr::Custom("read failed at byte offset 275".into());
    assert!(!is_check_violation(&offset));
    let port = DbErr::Custom("connection refused on port (275)".into());
    assert!(!is_check_violation(&port));
}

#[test]
fn is_check_violation_rejects_unique_violation() {
    // Cross-classifier safety: a unique-constraint message must NOT
    // also match as a CHECK violation. `is_check_violation` looks
    // for the substring `"check constraint"` (with a space), not
    // `"unique constraint"`, so the two predicates stay disjoint.
    let db_err = DbErr::Exec(RuntimeErr::Internal(
        "duplicate key value violates unique constraint \"ux_tenants\"".into(),
    ));
    assert!(!is_check_violation(&db_err));
}
