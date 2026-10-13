//! Domain services. One `MiniChatService` holds all dependencies; operations
//! are grouped by area in the submodules.

pub mod attachments;
pub mod background;
pub mod chats;
pub mod finalize;
pub mod messages;
pub mod misc;
pub mod stream;
pub mod turns;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use toolkit_db::outbox::Wake;
use toolkit_db::{DBProvider, DbTx};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::authz::Authz;
use super::error::DomainError;
use super::quota::QuotaService;
use crate::config::MiniChatConfig;
use crate::infra::audit::AuditGateway;
use crate::infra::db::entities::chats as chat_entity;
use crate::infra::db::repo;
use crate::infra::llm::LlmClient;
use crate::infra::llm::storage::StorageClient;
use crate::infra::metrics::Metrics;
use crate::infra::outbox::OutboxEnqueuer;
use crate::infra::policy::PolicyGateway;

/// Database provider typed with the domain error.
pub type Db = Arc<DBProvider<DomainError>>;

/// Dependencies of the service.
pub struct ServiceDeps {
    pub db: Db,
    pub cfg: Arc<MiniChatConfig>,
    pub authz: Authz,
    pub policy: Arc<PolicyGateway>,
    pub audit: Arc<AuditGateway>,
    pub llm: Arc<LlmClient>,
    pub storage: Arc<StorageClient>,
    pub outbox: Arc<OutboxEnqueuer>,
    pub metrics: Arc<Metrics>,
}

/// The mini-chat domain service.
pub struct MiniChatService {
    pub(crate) db: Db,
    pub(crate) cfg: Arc<MiniChatConfig>,
    pub(crate) authz: Authz,
    pub(crate) policy: Arc<PolicyGateway>,
    pub(crate) audit: Arc<AuditGateway>,
    pub(crate) quota: Arc<QuotaService>,
    pub(crate) llm: Arc<LlmClient>,
    pub(crate) storage: Arc<StorageClient>,
    pub(crate) outbox: Arc<OutboxEnqueuer>,
    pub(crate) metrics: Arc<Metrics>,
    pub(crate) upload_slots: Arc<Semaphore>,
    pub(crate) shutdown: CancellationToken,
    pub(crate) timings: Timings,
}

/// Internal waits of the attachment indexing protocol (DESIGN values by
/// default; tests shorten them).
#[derive(Debug, Clone, Copy)]
pub struct Timings {
    /// Upload request waits this long for indexing before answering `uploaded`.
    pub sync_index_deadline: std::time::Duration,
    /// One background indexing poll round.
    pub background_round: std::time::Duration,
    /// Background indexing gives up after this long.
    pub background_index_limit: std::time::Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            sync_index_deadline: attachments::SYNC_INDEX_DEADLINE,
            background_round: attachments::BACKGROUND_ROUND,
            background_index_limit: attachments::BACKGROUND_INDEX_LIMIT,
        }
    }
}

/// Boxed transaction closure result.
pub type TxFuture<'a, T> = Pin<Box<dyn Future<Output = Result<(T, Wake), DomainError>> + Send + 'a>>;

impl MiniChatService {
    #[must_use]
    pub fn new(d: ServiceDeps) -> Self {
        let quota = Arc::new(QuotaService::new(
            d.cfg.quota.clone(),
            d.cfg.streaming.max_output_tokens,
            d.cfg.estimation_budgets.minimal_generation_floor,
        ));
        let slots = usize::from(d.cfg.rag.max_concurrent_uploads);
        crate::infra::db::odata::set_text_timestamp_cursors(
            d.db.db().backend() == sea_orm::DbBackend::Sqlite,
        );
        Self {
            db: d.db,
            cfg: d.cfg,
            authz: d.authz,
            policy: d.policy,
            audit: d.audit,
            quota,
            llm: d.llm,
            storage: d.storage,
            outbox: d.outbox,
            metrics: d.metrics,
            upload_slots: Arc::new(Semaphore::new(slots)),
            shutdown: CancellationToken::new(),
            timings: Timings::default(),
        }
    }

    /// Overrides the indexing waits (tests).
    #[must_use]
    pub fn with_timings(mut self, timings: Timings) -> Self {
        self.timings = timings;
        self
    }

    /// Cancellation token of background work spawned by the service.
    #[must_use]
    pub fn shutdown_token(&self) -> &CancellationToken {
        &self.shutdown
    }

    #[must_use]
    pub fn config(&self) -> &MiniChatConfig {
        &self.cfg
    }

    #[must_use]
    pub fn db(&self) -> &Db {
        &self.db
    }

    #[must_use]
    pub fn outbox(&self) -> &Arc<OutboxEnqueuer> {
        &self.outbox
    }

    #[must_use]
    pub fn policy(&self) -> &Arc<PolicyGateway> {
        &self.policy
    }

    #[must_use]
    pub fn audit(&self) -> &Arc<AuditGateway> {
        &self.audit
    }

    #[must_use]
    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.metrics
    }

    #[must_use]
    pub fn llm(&self) -> &Arc<LlmClient> {
        &self.llm
    }

    #[must_use]
    pub fn storage(&self) -> &Arc<StorageClient> {
        &self.storage
    }

    #[must_use]
    pub fn quota(&self) -> &Arc<QuotaService> {
        &self.quota
    }

    /// Runs a transaction whose closure returns a value and a `Wake`; fires
    /// the wake after commit.
    ///
    /// # Errors
    /// Errors of the closure or the commit.
    pub(crate) async fn transact<T, F>(&self, f: F) -> Result<T, DomainError>
    where
        F: for<'a> FnOnce(&'a DbTx<'a>) -> TxFuture<'a, T> + Send + 'static,
        T: Send + 'static,
    {
        let lock = self.db.db().backend() == sea_orm::DbBackend::Sqlite;
        let (value, wake) = self
            .db
            .transaction(move |tx| {
                Box::pin(async move {
                    if lock {
                        repo::acquire_write_lock(tx).await?;
                    }
                    f(tx).await
                })
            })
            .await?;
        wake.fire();
        Ok(value)
    }

    /// Loads a non-deleted chat in the scope (404 otherwise).
    ///
    /// # Errors
    /// `ChatNotFound` or database errors.
    pub(crate) async fn load_chat(&self, scope: &AccessScope, chat_id: Uuid) -> Result<chat_entity::Model, DomainError> {
        let conn = self.db.conn()?;
        repo::chats::find(&conn, scope, chat_id)
            .await?
            .ok_or(DomainError::ChatNotFound)
    }

    /// Authorizes a chat operation and loads the chat.
    ///
    /// # Errors
    /// 403/503 from the PEP, 404 for a hidden chat.
    pub(crate) async fn authorized_chat(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
    ) -> Result<(AccessScope, chat_entity::Model), DomainError> {
        let scope = self.authz.chat_scope(ctx, action, Some(chat_id)).await?;
        let chat = self.load_chat(&scope, chat_id).await?;
        Ok((scope, chat))
    }
}
