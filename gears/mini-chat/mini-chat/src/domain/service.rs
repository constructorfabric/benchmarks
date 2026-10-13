//! The gear's domain service: shared dependencies and common helpers.

use std::sync::Arc;

use mini_chat_sdk::{ModelCatalogEntry, PolicySnapshot};
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::{DBRunner, SecureEntityExt};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::config::MiniChatConfig;
use crate::domain::authz::Authorizer;
use crate::domain::error::DomainError;
use crate::infra::db::MiniChatDb;
use crate::infra::db::entity::{chats, messages};
use crate::infra::llm::client::LlmClient;
use crate::infra::llm::resolver::ProviderResolver;
use crate::infra::llm::storage::StorageClient;
use crate::infra::outbox::OutboxEnqueuer;
use crate::infra::policy_gateway::PolicyProvider;

/// Domain service (PEP + orchestration). Cheap to clone through `Arc`.
pub struct MiniChat {
    pub cfg: Arc<MiniChatConfig>,
    pub db: Arc<MiniChatDb>,
    pub authz: Arc<dyn Authorizer>,
    pub policy: Arc<dyn PolicyProvider>,
    pub llm: LlmClient,
    pub storage: StorageClient,
    pub resolver: Arc<ProviderResolver>,
    pub outbox: OutboxEnqueuer,
    pub upload_slots: Arc<Semaphore>,
    pub shutdown: CancellationToken,
}

/// Owner-scoped, non-deleted chat filter.
#[must_use]
pub fn owned_chat_cond(ctx: &SecurityContext, chat_id: Uuid) -> Condition {
    Condition::all()
        .add(chats::Column::Id.eq(chat_id))
        .add(chats::Column::TenantId.eq(ctx.subject_tenant_id()))
        .add(chats::Column::UserId.eq(ctx.subject_id()))
        .add(chats::Column::DeletedAt.is_null())
}

impl MiniChat {
    /// Load an owner-scoped, non-deleted chat (404 otherwise).
    ///
    /// # Errors
    /// `ChatNotFound` or database failure.
    pub async fn load_chat(
        &self,
        runner: &impl DBRunner,
        scope: &AccessScope,
        ctx: &SecurityContext,
        chat_id: Uuid,
    ) -> Result<chats::Model, DomainError> {
        chats::Entity::find()
            .filter(owned_chat_cond(ctx, chat_id))
            .secure()
            .scope_with(scope)
            .one(runner)
            .await?
            .ok_or_else(|| DomainError::ChatNotFound(chat_id.to_string()))
    }

    /// Authorize `action` on the chat and load it.
    ///
    /// # Errors
    /// 403 / 503 / 404.
    pub async fn authorize_chat(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
    ) -> Result<(AccessScope, chats::Model), DomainError> {
        let scope = self.authz.chat_scope(ctx, action, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = self.load_chat(&conn, &scope, ctx, chat_id).await?;
        Ok((scope, chat))
    }

    /// Current policy snapshot for the subject.
    ///
    /// # Errors
    /// Policy plugin failure (500).
    pub async fn snapshot(&self, ctx: &SecurityContext) -> Result<PolicySnapshot, DomainError> {
        self.policy.current_snapshot(ctx.subject_id()).await
    }

    /// The chat's model, resolved without the enabled filter; a model missing
    /// from the catalog is `INVALID_MODEL`.
    ///
    /// # Errors
    /// `InvalidModel` or plugin failure.
    pub fn chat_model(snapshot: &PolicySnapshot, model_id: &str) -> Result<ModelCatalogEntry, DomainError> {
        snapshot
            .find(model_id)
            .cloned()
            .ok_or_else(|| DomainError::InvalidModel(format!("model '{model_id}' is not in the catalog")))
    }

    /// Non-deleted message count of a chat.
    ///
    /// # Errors
    /// Database failure.
    pub async fn message_count(&self, runner: &impl DBRunner, tenant: Uuid, chat_id: Uuid) -> Result<i64, DomainError> {
        let n = messages::Entity::find()
            .filter(
                Condition::all()
                    .add(messages::Column::ChatId.eq(chat_id))
                    .add(messages::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(tenant))
            .count(runner)
            .await?;
        Ok(i64::try_from(n).unwrap_or(i64::MAX))
    }
}
