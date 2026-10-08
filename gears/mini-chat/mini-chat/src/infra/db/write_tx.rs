//! Write transactions that take the database write lock up front.
//!
//! `SQLite` (WAL) starts transactions as readers; a transaction that reads and
//! then writes fails immediately with `SQLITE_BUSY` when another connection
//! committed in between (the busy handler is not consulted for a snapshot
//! upgrade). Issuing a no-op write as the first statement acquires the write
//! lock at `BEGIN` time, so concurrent writers queue on `busy_timeout` instead
//! (the `BEGIN IMMEDIATE` equivalent). On server databases the statement is a
//! cheap no-op.

use std::future::Future;
use std::pin::Pin;

use async_trait::async_trait;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use toolkit_db::DBProvider;
use toolkit_db::secure::{DbTx, SecureUpdateExt};
use toolkit_security::AccessScope;

use crate::domain::error::DomainError;
use crate::infra::db::entities::chat;

/// Acquires the write lock of the current transaction.
///
/// # Errors
/// Database failure.
pub async fn acquire_write_lock(tx: &DbTx<'_>) -> Result<(), DomainError> {
    chat::Entity::update_many()
        .col_expr(chat::Column::UpdatedAt, Expr::col(chat::Column::UpdatedAt))
        .filter(chat::Column::Id.eq(uuid::Uuid::nil()))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(tx)
        .await?;
    Ok(())
}

#[async_trait]
pub trait WriteTransaction {
    /// Like `transaction`, but the write lock is taken before `f` runs.
    async fn write_transaction<T, F>(&self, f: F) -> Result<T, DomainError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(
                &'a DbTx<'a>,
            )
                -> Pin<Box<dyn Future<Output = Result<T, DomainError>> + Send + 'a>>
            + Send
            + 'static;
}

#[async_trait]
impl WriteTransaction for DBProvider<DomainError> {
    async fn write_transaction<T, F>(&self, f: F) -> Result<T, DomainError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(
                &'a DbTx<'a>,
            )
                -> Pin<Box<dyn Future<Output = Result<T, DomainError>> + Send + 'a>>
            + Send
            + 'static,
    {
        self.transaction(move |tx| {
            Box::pin(async move {
                acquire_write_lock(tx).await?;
                f(tx).await
            })
        })
        .await
    }
}
