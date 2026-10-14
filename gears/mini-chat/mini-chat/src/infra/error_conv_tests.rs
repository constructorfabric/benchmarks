//! Storage and outbox error classification (algorithm `error-mapping`, steps 7 and 8, and
//! `outbox-queues` step 7).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use sea_orm::DbErr;
use toolkit_db::DbError;
use toolkit_db::outbox::OutboxError;
use toolkit_db::secure::ScopeError;

use super::outbox_error_for;
use crate::domain::error::{DomainError, EnqueueSite};

#[test]
fn a_unique_violation_becomes_unique_violation_with_the_conflict_code() {
    let err = ScopeError::Db(DbErr::Custom(
        "UNIQUE constraint failed: chats.id".to_owned(),
    ));
    match DomainError::from(err) {
        DomainError::UniqueViolation { code } => assert_eq!(code, "unique_violation"),
        other => panic!("expected UniqueViolation, got {other:?}"),
    }
}

#[test]
fn a_postgres_unique_violation_message_is_detected_too() {
    let err = ScopeError::Db(DbErr::Custom(
        "duplicate key value violates unique constraint \"chats_pkey\"".to_owned(),
    ));
    assert!(matches!(
        DomainError::from(err),
        DomainError::UniqueViolation { .. }
    ));
}

#[test]
fn any_other_database_failure_becomes_internal_and_keeps_driver_text_out_of_display() {
    let err = ScopeError::Db(DbErr::Custom(
        "connection reset by peer (10.0.0.5)".to_owned(),
    ));
    let mapped = DomainError::from(err);
    assert!(matches!(mapped, DomainError::Internal { .. }), "{mapped:?}");
    assert!(!mapped.to_string().contains("10.0.0.5"));
}

#[test]
fn a_toolkit_db_error_becomes_internal() {
    let mapped = DomainError::from(DbError::InvalidConfig("bad pool".to_owned()));
    assert!(matches!(mapped, DomainError::Internal { .. }), "{mapped:?}");
}

#[test]
fn a_payload_too_large_outbox_error_carries_the_call_site() {
    let sites = [
        EnqueueSite::ChatDelete,
        EnqueueSite::AttachmentDelete,
        EnqueueSite::TurnRetry,
        EnqueueSite::TurnEdit,
        EnqueueSite::TurnDelete,
    ];
    for site in sites {
        let err = OutboxError::PayloadTooLarge {
            size: 70_000,
            max: 65_536,
        };
        match outbox_error_for(err, site) {
            DomainError::OutboxPayloadTooLarge { site: got } => assert_eq!(got, site),
            other => panic!("expected OutboxPayloadTooLarge, got {other:?}"),
        }
    }
}

#[test]
fn other_outbox_errors_become_internal() {
    let err = OutboxError::QueueNotRegistered("mini-chat.audit".to_owned());
    let mapped = outbox_error_for(err, EnqueueSite::TurnRetry);
    assert!(matches!(mapped, DomainError::Internal { .. }), "{mapped:?}");
}
