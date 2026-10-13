#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 3: the infra mapping of unique-constraint violations
//! (`SQLITE_CONSTRAINT_UNIQUE` / `SQLITE_CONSTRAINT_PRIMARYKEY`, PostgreSQL
//! `23505`) to the typed `DomainError::UniqueViolation`, so that the reserve
//! transaction can resolve a race instead of reporting a database error.

use sea_orm::DbErr;
use toolkit_db::DbError;
use toolkit_db::secure::ScopeError;
use uuid::Uuid;

use super::*;
use crate::infra::db::test_support::{TestDb, chat_row, message_row};

const TENANT: Uuid = Uuid::from_u128(0x7E);
const USER: Uuid = Uuid::from_u128(0x55);

/// A genuine duplicate-key error of the SQLite driver.
async fn driver_error(kind: &str) -> DbErr {
    let db = TestDb::migrated().await;
    let chat = Uuid::from_u128(1);
    db.insert("chats", chat_row(chat, TENANT, USER))
        .await
        .unwrap();
    let first =
        message_row(Uuid::from_u128(10), TENANT, chat).set("request_id", Uuid::from_u128(20));
    db.insert("messages", first).await.unwrap();
    let duplicate = match kind {
        // The primary key.
        "primary" => message_row(Uuid::from_u128(10), TENANT, chat),
        // The unique index (chat_id, request_id, role).
        _ => message_row(Uuid::from_u128(11), TENANT, chat).set("request_id", Uuid::from_u128(20)),
    };
    db.insert("messages", duplicate).await.unwrap_err()
}

#[tokio::test]
async fn a_sqlite_unique_index_violation_is_a_unique_violation() {
    let error = driver_error("unique").await;
    assert!(
        matches!(
            scope_error(&ScopeError::Db(error)),
            DomainError::UniqueViolation
        ),
        "the UNIQUE index case"
    );
}

#[tokio::test]
async fn a_sqlite_primary_key_violation_is_a_unique_violation() {
    let error = driver_error("primary").await;
    assert!(
        matches!(
            scope_error(&ScopeError::Db(error)),
            DomainError::UniqueViolation
        ),
        "the PRIMARY KEY case"
    );
}

#[tokio::test]
async fn a_unique_violation_of_the_database_provider_is_a_unique_violation_too() {
    let error = driver_error("unique").await;
    assert!(matches!(
        DomainError::from(DbError::Sea(error)),
        DomainError::UniqueViolation
    ));
}

#[test]
fn the_message_forms_of_both_engines_are_recognized_when_the_typed_shape_is_lost() {
    for message in [
        "error returned from database: (code: 2067) UNIQUE constraint failed: messages.chat_id, messages.request_id",
        "error returned from database: (code: 1555) UNIQUE constraint failed: chat_turns.id",
        "error returned from database: duplicate key value violates unique constraint \"uq_chat_turns_one_running\"",
        "error returned from database: SQLSTATE 23505 unique_violation",
    ] {
        assert!(
            matches!(
                scope_error(&ScopeError::Db(DbErr::Custom(message.to_owned()))),
                DomainError::UniqueViolation
            ),
            "{message}"
        );
    }
}

#[test]
fn the_constraint_name_is_never_inspected() {
    // Whatever index fired, the answer is the same typed error.
    for name in [
        "messages",
        "chat_turns",
        "uq_chat_turns_one_running",
        "anything_else",
    ] {
        let message = format!("UNIQUE constraint failed: {name}.x");
        assert!(matches!(
            scope_error(&ScopeError::Db(DbErr::Custom(message))),
            DomainError::UniqueViolation
        ));
    }
}

#[test]
fn other_database_errors_stay_database_errors() {
    for message in [
        "error returned from database: (code: 787) FOREIGN KEY constraint failed",
        "connection reset by peer",
        "database is locked",
        "error returned from database: could not serialize access due to concurrent update",
    ] {
        assert!(
            matches!(
                scope_error(&ScopeError::Db(DbErr::Custom(message.to_owned()))),
                DomainError::Database(_)
            ),
            "{message}"
        );
    }
    assert!(matches!(
        scope_error(&ScopeError::Invalid("scope")),
        DomainError::Database(_)
    ));
}

#[test]
fn a_lock_conflict_text_is_kept_so_that_the_retry_can_recognize_it() {
    let DomainError::Database(text) = scope_error(&ScopeError::Db(DbErr::Custom(
        "database is locked".to_owned(),
    ))) else {
        panic!("a database error expected");
    };
    assert!(text.contains("database is locked"));
}
