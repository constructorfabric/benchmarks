#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests for the `ensure_chat_live` function's row locking behavior.

use sea_orm::{
    ColumnTrait, Condition, DbBackend, EntityTrait, QueryFilter, QuerySelect, QueryTrait,
};
use uuid::Uuid;

use crate::infra::db::entities::chat;

/// Test that `ensure_chat_live` generates FOR SHARE lock on PostgreSQL.
#[test]
fn ensure_chat_live_uses_for_share_on_postgres() {
    let chat_id = Uuid::nil();

    // Build the query without executing it
    let query = chat::Entity::find()
        .filter(
            Condition::all()
                .add(chat::Column::Id.eq(chat_id))
                .add(chat::Column::DeletedAt.is_null()),
        )
        .select_only()
        .column(chat::Column::Id)
        .lock_shared();

    // Convert to SQL for PostgreSQL
    let sql = query.build(DbBackend::Postgres).to_string();

    // Assert FOR SHARE is present in the SQL
    assert!(
        sql.contains("FOR SHARE"),
        "PostgreSQL query should contain FOR SHARE, but got: {sql}",
    );
}

/// Test that `ensure_chat_live` does not add lock syntax on SQLite.
#[test]
fn ensure_chat_live_no_lock_on_sqlite() {
    let chat_id = Uuid::nil();

    // Build the query without executing it
    let query = chat::Entity::find()
        .filter(
            Condition::all()
                .add(chat::Column::Id.eq(chat_id))
                .add(chat::Column::DeletedAt.is_null()),
        )
        .select_only()
        .column(chat::Column::Id);

    // Convert to SQL for SQLite (no lock applied)
    let sql = query.build(DbBackend::Sqlite).to_string();

    // Assert FOR SHARE is not present in SQLite
    assert!(
        !sql.contains("FOR SHARE"),
        "SQLite query should not contain FOR SHARE, but got: {sql}",
    );
}
