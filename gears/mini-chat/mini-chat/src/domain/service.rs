//! Shared service state and helpers.

use std::sync::Arc;

use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use toolkit_db::secure::{DBRunner, SecureEntityExt};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::config::MiniChatConfig;
use crate::domain::audit::AuditGateway;
use crate::domain::authz::Authz;
use crate::domain::error::DomainError;
use crate::domain::policy::PolicyGateway;
use crate::infra::db::entities::chats;
use crate::infra::llm::client::LlmClient;
use crate::infra::metrics::Metrics;
use crate::infra::outbox::Enqueuer;

/// Domain services (one instance per gear).
pub struct Svc {
    /// Configuration.
    pub cfg: Arc<MiniChatConfig>,
    /// Database.
    pub db: Arc<DBProvider<DomainError>>,
    /// PEP.
    pub authz: Authz,
    /// Model policy.
    pub policy: Arc<PolicyGateway>,
    /// Audit.
    pub audit: Arc<AuditGateway>,
    /// LLM / storage.
    pub llm: Arc<LlmClient>,
    /// Outbox.
    pub outbox: Arc<Enqueuer>,
    /// Upload concurrency limit.
    pub upload_sem: Arc<Semaphore>,
    /// Metrics.
    pub metrics: Arc<Metrics>,
    /// Gear-level cancellation (background tasks).
    pub shutdown: CancellationToken,
}

/// Loads a non-deleted chat under a scope.
///
/// # Errors
/// `ChatNotFound` when missing, foreign or deleted.
pub async fn load_chat(
    runner: &impl DBRunner,
    scope: &AccessScope,
    chat_id: Uuid,
) -> Result<chats::Model, DomainError> {
    chats::Entity::find()
        .filter(
            Condition::all()
                .add(chats::Column::Id.eq(chat_id))
                .add(chats::Column::DeletedAt.is_null()),
        )
        .secure()
        .scope_with(scope)
        .one(runner)
        .await?
        .ok_or(DomainError::ChatNotFound)
}

impl Svc {
    /// Authorizes a chat-scoped action and loads the chat.
    ///
    /// # Errors
    /// 403/503 from the PDP, `ChatNotFound`.
    pub async fn authorized_chat(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
    ) -> Result<(AccessScope, chats::Model), DomainError> {
        let scope = self.authz.chat_scope(ctx, action, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = load_chat(&conn, &scope, chat_id).await?;
        Ok((scope, chat))
    }
}

/// Scope of rows belonging to an authorized chat.
#[must_use]
pub fn child_scope(chat: &chats::Model) -> AccessScope {
    AccessScope::for_tenant(chat.tenant_id)
}
