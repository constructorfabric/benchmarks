//! The mini-chat domain service container (one instance per gear).

use std::sync::{Arc, OnceLock};

use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use toolkit_db::secure::SecureEntityExt;
use toolkit_db::{DBProvider, Db};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::audit::AuditGateway;
use super::authz::{Authz, child_scope};
use super::error::DomainError;
use super::policy::PolicyGateway;
use crate::config::MiniChatConfig;
use crate::infra::llm::LlmGateway;
use crate::infra::metrics::Metrics;
use crate::infra::outbox::OutboxSlot;
use crate::infra::storage::entity::chat;

/// Shared state of the gear: configuration, DB, PEP, gateways.
pub struct MiniChat {
    pub cfg: Arc<MiniChatConfig>,
    pub db: DBProvider<DomainError>,
    pub raw_db: Db,
    pub authz: Authz,
    pub policy: Arc<PolicyGateway>,
    pub audit: Arc<AuditGateway>,
    pub outbox: Arc<OutboxSlot>,
    pub llm: Arc<LlmGateway>,
    pub metrics: Arc<Metrics>,
    pub upload_permits: Arc<Semaphore>,
    /// Gear shutdown token (background tasks).
    pub shutdown: CancellationToken,
    /// S2S security context for background/system work (set at start).
    pub system_ctx: OnceLock<SecurityContext>,
    /// Client hub (plugin and client resolution).
    pub hub: Arc<toolkit::ClientHub>,
}

/// An authorized, live chat plus the scopes to reach it and its children.
#[derive(Debug, Clone)]
pub struct ChatAccess {
    pub chat: chat::Model,
    pub scope: AccessScope,
    pub child_scope: AccessScope,
}

/// Attempts of a transaction on retryable contention.
const TX_ATTEMPTS: u32 = 8;

impl MiniChat {
    /// Run a transaction, retrying the whole body on retryable contention
    /// (SQLite `BUSY`/`BUSY_SNAPSHOT`, PostgreSQL serialization failures).
    ///
    /// # Errors
    /// The body's error, or the last contention error.
    pub async fn tx<T, F>(&self, f: F) -> Result<T, DomainError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(
                &'a toolkit_db::DbTx<'a>,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, DomainError>> + Send + 'a>>
            + Send
            + Clone,
    {
        let mut delay = std::time::Duration::from_millis(5);
        let mut attempt = 1;
        loop {
            match self.db.transaction(f.clone()).await {
                Err(e) if e.is_contention() && attempt < TX_ATTEMPTS => {
                    tracing::debug!(error = %e, attempt, "transaction contention; retrying");
                    tokio::time::sleep(delay).await;
                    delay = std::cmp::min(delay * 2, std::time::Duration::from_millis(250));
                    attempt += 1;
                }
                r => return r,
            }
        }
    }

    /// Load a live (not soft-deleted) chat under `action`, or `ChatNotFound`.
    ///
    /// # Errors
    /// PDP errors, `ChatNotFound`, DB errors.
    pub async fn load_chat(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
    ) -> Result<ChatAccess, DomainError> {
        let scope = self.authz.chat_scope(ctx, action, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = chat::Entity::find()
            .filter(chat::Column::Id.eq(chat_id))
            .filter(chat::Column::DeletedAt.is_null())
            .secure()
            .scope_with(&scope)
            .one(&conn)
            .await?
            .ok_or(DomainError::ChatNotFound(chat_id))?;
        let child_scope = child_scope(&scope, chat.tenant_id);
        Ok(ChatAccess { chat, scope, child_scope })
    }

    /// System identity for background work of a tenant: the tenant and the
    /// platform default subject id.
    #[must_use]
    pub fn system_ctx_for(&self, tenant_id: Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(toolkit_security::constants::DEFAULT_SUBJECT_ID)
            .subject_tenant_id(tenant_id)
            .token_scopes(vec!["*".to_owned()])
            .build()
            .unwrap_or_else(|_| self.system_ctx.get().cloned().unwrap_or_else(SecurityContext::anonymous))
    }
}
