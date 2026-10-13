use authz_resolver_sdk::EnforcerError;
use authz_resolver_sdk::pep::ConstraintCompileError;
use toolkit_canonical_errors::CanonicalError;
use toolkit_db::secure::ScopeError;

use super::DomainError;

#[test]
fn enforcer_denied_maps_to_authz_denied() {
    let e = EnforcerError::Denied { deny_reason: None };
    assert_eq!(DomainError::from(e), DomainError::AuthzDenied);
}

#[test]
fn enforcer_compile_failed_maps_to_authz_denied() {
    let e = EnforcerError::CompileFailed(ConstraintCompileError::ConstraintsRequiredButAbsent);
    assert_eq!(DomainError::from(e), DomainError::AuthzDenied);
}

#[test]
fn enforcer_evaluation_failed_maps_to_authz_unavailable() {
    let e = EnforcerError::EvaluationFailed(CanonicalError::internal("pdp down").create());
    assert_eq!(DomainError::from(e), DomainError::AuthzUnavailable);
}

#[test]
fn scope_unique_violation_is_classified() {
    let e = ScopeError::Db(sea_orm::DbErr::Custom(
        "UNIQUE constraint failed: chat_turns.chat_id, chat_turns.request_id".to_owned(),
    ));
    let d = DomainError::from(e);
    assert!(matches!(d, DomainError::UniqueViolation(_)), "{d:?}");
    assert!(d.is_unique_violation());
}

#[test]
fn scope_other_db_error_keeps_the_driver_error() {
    let e = ScopeError::Db(sea_orm::DbErr::Custom("disk I/O error".to_owned()));
    let d = DomainError::from(e);
    assert!(matches!(d, DomainError::Db(_)), "{d:?}");
    assert!(!d.is_unique_violation());
    assert_eq!(
        d.db_err().unwrap().to_string(),
        "Custom Error: disk I/O error"
    );
}

#[test]
fn contention_error_stays_classifiable() {
    let busy = "error returned from database: (code: 517) database is locked";
    for d in [
        DomainError::from(ScopeError::Db(sea_orm::DbErr::Custom(busy.to_owned()))),
        DomainError::from(toolkit_db::DbError::Sea(sea_orm::DbErr::Custom(
            busy.to_owned(),
        ))),
        DomainError::from(sea_orm::DbErr::Custom(busy.to_owned())),
    ] {
        let db = d.db_err().expect("driver error preserved");
        assert!(
            toolkit_db::contention::is_retryable_contention(sea_orm::DbBackend::Sqlite, db),
            "{d:?}"
        );
    }
}

#[test]
fn non_db_scope_errors_are_database_without_driver_error() {
    let d = DomainError::from(ScopeError::Invalid("bad scope"));
    assert!(matches!(d, DomainError::Database(_)), "{d:?}");
    assert!(d.db_err().is_none());
}

#[test]
fn db_error_unique_violation_is_classified() {
    let e = toolkit_db::DbError::Sea(sea_orm::DbErr::Custom(
        "UNIQUE constraint failed: chats.id".to_owned(),
    ));
    assert!(DomainError::from(e).is_unique_violation());
}

#[test]
fn odata_db_error_is_database_and_client_errors_are_invalid_query() {
    let d = DomainError::from(toolkit_odata::Error::Db("locked".to_owned()));
    assert_eq!(d, DomainError::Database("locked".to_owned()));
    let d = DomainError::from(toolkit_odata::Error::InvalidFilter(
        "unknown field".to_owned(),
    ));
    assert!(matches!(d, DomainError::InvalidQuery(_)), "{d:?}");
    let d = DomainError::from(toolkit_odata::Error::InvalidCursor);
    assert!(matches!(d, DomainError::InvalidQuery(_)), "{d:?}");
}
