//! Domain service: orchestration and PEP for every mini-chat operation.

pub mod attachments;
pub mod chats;
pub mod cleanup;
pub mod finalize;
pub mod messages;
pub mod models;
pub mod reactions;
pub mod stream;
pub mod summary;
pub mod turns;

use std::sync::Arc;

use authz_resolver_sdk::PolicyEnforcer;
use mini_chat_sdk::{ModelCatalogEntry, PolicySnapshot};
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::{DBRunner, DbTx, SecureEntityExt, TxConfig};
use toolkit_db::DBProvider;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::MiniChatConfig;
use crate::domain::authz::{ChatScopes, chat_scope};
use crate::domain::error::{DomainError, DomainResult, Res};
use crate::domain::ports::{AuditPort, PolicyPort};
use crate::infra::db::entities::chats as chat_entity;
use crate::infra::llm::LlmGateway;
use crate::infra::outbox::OutboxEnqueuer;

/// Attempt budget of [`MiniChatService::tx`] (first try included).
const TX_RETRY_ATTEMPTS: u32 = 12;

/// Shared state of the domain service.
pub struct MiniChatService {
    pub db: Arc<DBProvider<DomainError>>,
    pub enforcer: PolicyEnforcer,
    pub policy: Arc<dyn PolicyPort>,
    pub audit: Arc<dyn AuditPort>,
    pub llm: Arc<LlmGateway>,
    pub outbox: Arc<OutboxEnqueuer>,
    pub cfg: Arc<MiniChatConfig>,
    pub upload_slots: Arc<tokio::sync::Semaphore>,
    pub shutdown: CancellationToken,
}

/// Current time truncated to microseconds (PostgreSQL precision).
#[must_use]
pub fn now() -> OffsetDateTime {
    let t = OffsetDateTime::now_utc();
    let micros = t.microsecond();
    t.replace_microsecond(micros).unwrap_or(t)
}

impl MiniChatService {
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        enforcer: PolicyEnforcer,
        policy: Arc<dyn PolicyPort>,
        audit: Arc<dyn AuditPort>,
        llm: Arc<LlmGateway>,
        outbox: Arc<OutboxEnqueuer>,
        cfg: Arc<MiniChatConfig>,
    ) -> Arc<Self> {
        let slots = usize::from(cfg.rag.max_concurrent_uploads);
        Arc::new(Self {
            db,
            enforcer,
            policy,
            audit,
            llm,
            outbox,
            cfg,
            upload_slots: Arc::new(tokio::sync::Semaphore::new(slots)),
            shutdown: CancellationToken::new(),
        })
    }

    /// Run `body` in a transaction, retrying on lock contention
    /// (`SQLite` BUSY / `BUSY_SNAPSHOT`, `PostgreSQL` serialization failures).
    ///
    /// The budget is above the workspace default: on `SQLite` every deferred
    /// read-then-write transaction competes with the outbox workers' claims
    /// for the single write lock, and a lost race fails immediately instead of
    /// waiting on `busy_timeout`.
    ///
    /// # Errors
    /// The body's error, or the database error after the retry budget.
    pub(crate) async fn tx<T, F>(&self, body: F) -> DomainResult<T>
    where
        T: Send + 'static,
        F: for<'a> FnMut(&'a DbTx<'a>) -> std::pin::Pin<Box<dyn std::future::Future<Output = DomainResult<T>> + Send + 'a>> + Send,
    {
        self.db.db().transaction_with_retry_max(TxConfig::default(), TX_RETRY_ATTEMPTS, DomainError::db_err, body).await
    }

    pub(crate) async fn scopes(&self, ctx: &SecurityContext, action: &str, chat_id: Option<Uuid>) -> DomainResult<ChatScopes> {
        chat_scope(&self.enforcer, ctx, action, chat_id).await
    }

    /// Load a non-deleted chat visible under the owner scope.
    pub(crate) async fn load_chat(&self, runner: &impl DBRunner, scopes: &ChatScopes, chat_id: Uuid) -> DomainResult<chat_entity::Model> {
        chat_entity::Entity::find()
            .filter(Condition::all().add(chat_entity::Column::Id.eq(chat_id)).add(chat_entity::Column::DeletedAt.is_null()))
            .secure()
            .scope_with(&scopes.owner)
            .one(runner)
            .await?
            .ok_or(DomainError::NotFound(Res::Chat))
    }

    pub(crate) async fn snapshot(&self, ctx: &SecurityContext) -> DomainResult<Arc<PolicySnapshot>> {
        self.policy.current_snapshot(ctx.subject_id()).await
    }

    /// Resolve the chat's model without the enabled filter; a model removed
    /// from the catalog is `INVALID_MODEL`.
    pub(crate) fn chat_model(snap: &PolicySnapshot, chat: &chat_entity::Model) -> DomainResult<ModelCatalogEntry> {
        snap.find(&chat.model).cloned().ok_or_else(DomainError::invalid_model)
    }
}

