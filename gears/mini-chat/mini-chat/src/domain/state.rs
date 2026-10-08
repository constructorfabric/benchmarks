//! Shared service state built in `init` and used by the REST handlers,
//! outbox handlers and background workers.

use std::sync::Arc;

use authz_resolver_sdk::{AccessRequest, PolicyEnforcer, ResourceType};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use toolkit_db::DBProvider;
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
use uuid::Uuid;

use crate::config::MiniChatConfig;
use crate::domain::errors::{DomainError, DomainResult, map_enforcer_error};
use crate::infra::llm::client::LlmClient;
use crate::infra::llm::resolver::ProviderResolver;
use crate::infra::outbox::OutboxDispatch;
use crate::infra::policy::{AuditGateway, PolicyGateway};
use crate::infra::storage::StorageClient;

pub const CHAT_TYPE: &str = "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~";
pub const MODEL_TYPE: &str = "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~";
pub const USER_QUOTA_TYPE: &str = "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~";

pub const CHAT_RESOURCE: ResourceType = ResourceType::from_static(
    CHAT_TYPE,
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID, pep_properties::RESOURCE_ID],
);
pub const MODEL_RESOURCE: ResourceType = ResourceType::from_static(MODEL_TYPE, &[]);
pub const USER_QUOTA_RESOURCE: ResourceType = ResourceType::from_static(
    USER_QUOTA_TYPE,
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID],
);

pub type Db = DBProvider<DomainError>;

pub struct AppState {
    pub cfg: Arc<MiniChatConfig>,
    pub db: Arc<Db>,
    pub enforcer: PolicyEnforcer,
    pub policy: Arc<PolicyGateway>,
    pub audit: Arc<AuditGateway>,
    pub outbox: Arc<OutboxDispatch>,
    pub resolver: Arc<ProviderResolver>,
    pub llm: Arc<LlmClient>,
    pub storage: Arc<StorageClient>,
    pub upload_slots: Arc<Semaphore>,
    pub shutdown: CancellationToken,
}

/// Scopes for one authorized chat operation.
#[derive(Debug, Clone)]
pub struct ChatScopes {
    /// Tenant + owner scope for `chats`.
    pub chat: AccessScope,
    /// Tenant-only scope for child tables (filtered by chat id).
    pub tenant: AccessScope,
}

/// `true` for transient lock/contention failures worth retrying.
#[must_use]
pub fn is_contention(e: &DomainError) -> bool {
    match e {
        DomainError::Internal(m) => {
            let m = m.to_ascii_lowercase();
            m.contains("database is locked")
                || m.contains("(code: 5)")
                || m.contains("(code: 517)")
                || m.contains("sqlite_busy")
                || m.contains("deadlock")
                || m.contains("could not serialize")
        }
        _ => false,
    }
}

impl AppState {
    /// Run a write transaction, retrying on lock contention (SQLite BUSY,
    /// PostgreSQL serialization failures). The body must be idempotent.
    ///
    /// # Errors
    /// The body's error, or the last contention error.
    pub async fn write_tx<T, F>(&self, f: F) -> DomainResult<T>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(
                &'a toolkit_db::DbTx<'a>,
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = DomainResult<T>> + Send + 'a>>
            + Send
            + Clone,
    {
        let mut delay = std::time::Duration::from_millis(5);
        let mut attempt = 0;
        loop {
            match self.db.transaction(f.clone()).await {
                Err(e) if is_contention(&e) && attempt < 10 => {
                    attempt += 1;
                    let jitter = u64::from(uuid::Uuid::new_v4().as_bytes()[0] % 10);
                    tokio::time::sleep(delay + std::time::Duration::from_millis(jitter)).await;
                    delay = (delay * 2).min(std::time::Duration::from_millis(200));
                }
                r => return r,
            }
        }
    }

    /// Evaluate a chat action (owner-only) and return the compiled scopes.
    /// The owner predicate is added even when the PDP returns only a tenant
    /// constraint (defence in depth).
    ///
    /// # Errors
    /// 403 on denial / compile failure, 503 on PDP failure.
    pub async fn chat_scope(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Option<Uuid>,
    ) -> DomainResult<ChatScopes> {
        let req = AccessRequest::new();
        let req = if action == "create" {
            req.resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
                .resource_property(pep_properties::OWNER_ID, ctx.subject_id())
        } else {
            req
        };
        let scope = self
            .enforcer
            .access_scope_with(ctx, &CHAT_RESOURCE, action, chat_id, &req)
            .await
            .map_err(|e| map_enforcer_error(&e))?;
        Ok(Self::scopes_from(&scope, ctx))
    }

    fn scopes_from(scope: &AccessScope, ctx: &SecurityContext) -> ChatScopes {
        let owned = if scope.is_unconstrained() {
            AccessScope::for_tenant(ctx.subject_tenant_id()).ensure_owner(ctx.subject_id())
        } else {
            scope.ensure_owner(ctx.subject_id())
        };
        let tenant = owned.tenant_only();
        ChatScopes { chat: owned, tenant }
    }

    /// Permission-only check for the models API.
    ///
    /// # Errors
    /// 403 / 503 as for chats.
    pub async fn model_check(&self, ctx: &SecurityContext, action: &str) -> DomainResult<()> {
        self.enforcer
            .access_scope_with(
                ctx,
                &MODEL_RESOURCE,
                action,
                None,
                &AccessRequest::new().require_constraints(false),
            )
            .await
            .map_err(|e| map_enforcer_error(&e))?;
        Ok(())
    }

    /// Authorization of the quota status API (`USER_QUOTA`, `read`).
    ///
    /// # Errors
    /// 403 / 503 as for chats.
    pub async fn quota_check(&self, ctx: &SecurityContext) -> DomainResult<()> {
        self.enforcer
            .access_scope(ctx, &USER_QUOTA_RESOURCE, "read", None)
            .await
            .map_err(|e| map_enforcer_error(&e))?;
        Ok(())
    }
}
