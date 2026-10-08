//! Domain services of the mini-chat gear.
//!
//! One [`Service`] value carries every dependency; the operations are split
//! by area into the submodules (`impl Service` blocks).

pub mod attachments;
pub mod chats;
pub mod cleanup;
pub mod context;
pub mod finalize;
pub mod messages;
pub mod models;
pub mod quota;
pub mod stream;
pub mod summary;
pub mod thumbnail;
pub mod turns;
pub mod workers;

#[allow(unused_imports)]
use sea_orm::{EntityTrait as _, QueryFilter as _};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use mini_chat_sdk::PolicySnapshot;
use sea_orm::{ColumnTrait, Condition};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::{SecureEntityExt, TxConfig};
use toolkit_db::{Db, DbTx};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::config::MiniChatConfig;
use crate::domain::authz::{Authorizer, tenant_scope};
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::ports::PolicyProvider;
use crate::infra::llm::client::LlmClient;
use crate::infra::llm::provider::ProviderRegistry;
use crate::infra::llm::storage::StorageClient;
use crate::infra::outbox::OutboxEnqueuer;
use crate::infra::plugins::audit_gateway::AuditGateway;
use crate::infra::storage::entity::chat;

/// The mini-chat domain service.
pub struct Service {
    pub cfg: Arc<MiniChatConfig>,
    pub db: Db,
    pub authz: Authorizer,
    pub policy: Arc<dyn PolicyProvider>,
    pub audit: Option<Arc<AuditGateway>>,
    pub providers: Arc<ProviderRegistry>,
    pub llm: LlmClient,
    pub storage: StorageClient,
    pub outbox: Arc<OutboxEnqueuer>,
    pub upload_slots: Arc<Semaphore>,
    /// Cancelled on gear stop (background indexing tasks, workers).
    pub shutdown: CancellationToken,
    pub metrics: Arc<crate::infra::metrics::Metrics>,
}

/// Attempts of a contended transaction.
const TX_ATTEMPTS: u32 = 8;

/// SQLite opens transactions deferred: a transaction that reads before it
/// writes fails with `SQLITE_BUSY_SNAPSHOT` when another writer commits in
/// between. A no-op write as the first statement takes the write lock up
/// front (waiting on `busy_timeout`).
async fn take_write_lock(tx: &DbTx<'_>) -> DomainResult<()> {
    use sea_orm::sea_query::Expr;
    use toolkit_db::secure::SecureUpdateExt;
    chat::Entity::update_many()
        .col_expr(
            chat::Column::IsTemporary,
            Expr::col(chat::Column::IsTemporary),
        )
        .filter(Condition::all().add(sea_orm::ExprTrait::eq(Expr::val(1), 0)))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(tx)
        .await?;
    Ok(())
}

/// Boxed future of a transaction body.
pub type TxFuture<'a, T> = Pin<Box<dyn Future<Output = DomainResult<T>> + Send + 'a>>;

impl Service {
    /// Run `body` in a transaction, retrying on SQLite / Postgres contention.
    /// The body may run more than once.
    ///
    /// # Errors
    /// The body's error, or a database error.
    pub async fn tx<T, F>(&self, mut body: F) -> DomainResult<T>
    where
        T: Send + 'static,
        F: for<'a> FnMut(&'a DbTx<'a>) -> TxFuture<'a, T> + Send,
    {
        let sqlite = self.db.backend() == sea_orm::DbBackend::Sqlite;
        self.db
            .transaction_with_retry_max(
                TxConfig::default(),
                TX_ATTEMPTS,
                DomainError::db_err,
                move |tx| {
                    let fut = body(tx);
                    Box::pin(async move {
                        if sqlite {
                            take_write_lock(tx).await?;
                        }
                        fut.await
                    })
                },
            )
            .await
    }

    /// Current policy snapshot for the caller.
    ///
    /// # Errors
    /// `PolicyResolution` when the plugin fails.
    pub async fn snapshot(&self, user_id: Uuid) -> DomainResult<PolicySnapshot> {
        self.policy.current_snapshot(user_id).await
    }

    /// Authorize a chat action and load the (non-deleted) chat with the
    /// owner-scoped query. Returns the chat and the tenant scope for its
    /// child tables.
    ///
    /// # Errors
    /// 403 / 503 from the PDP, 404 when the chat is missing, deleted or
    /// foreign.
    pub async fn authorized_chat(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
    ) -> DomainResult<(chat::Model, AccessScope)> {
        let scope = self.authz.chat_scope(ctx, action, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = chat::Entity::find()
            .secure()
            .scope_with(&scope)
            .filter(
                Condition::all()
                    .add(chat::Column::Id.eq(chat_id))
                    .add(chat::Column::DeletedAt.is_null()),
            )
            .one(&conn)
            .await?
            .ok_or(DomainError::ChatNotFound { id: chat_id })?;
        let child = tenant_scope(chat.tenant_id);
        Ok((chat, child))
    }
}

impl Service {
    /// Count a booked reserve (one per period).
    pub(crate) fn record_reserve(&self) {
        for period in ["daily", "monthly"] {
            self.metrics
                .quota_reserve
                .add(1, &crate::infra::metrics::labels(&[("period", period)]));
        }
    }
}

/// Composite provider `user` value: tenant and user UUIDs in simple form.
#[must_use]
pub fn provider_user(tenant_id: Uuid, user_id: Uuid) -> String {
    format!("{}{}", tenant_id.as_simple(), user_id.as_simple())
}
