//! Policy enforcement point (DESIGN §3.8).
//!
//! Every chat operation evaluates the PDP for the chat resource and gets back
//! the compiled `AccessScope`. The gear always adds the owner predicate
//! itself (defence in depth): chat queries filter on `tenant_id` and
//! `user_id` of the subject in addition to the scope.

use std::sync::Arc;

use async_trait::async_trait;
use authz_resolver_sdk::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Actions on the chat resource.
pub mod actions {
    pub const CREATE: &str = "create";
    pub const LIST: &str = "list";
    pub const READ: &str = "read";
    pub const UPDATE: &str = "update";
    pub const DELETE: &str = "delete";
    pub const LIST_MESSAGES: &str = "list_messages";
    pub const SEND_MESSAGE: &str = "send_message";
    pub const UPLOAD_ATTACHMENT: &str = "upload_attachment";
    pub const READ_ATTACHMENT: &str = "read_attachment";
    pub const DELETE_ATTACHMENT: &str = "delete_attachment";
    pub const READ_TURN: &str = "read_turn";
    pub const RETRY_TURN: &str = "retry_turn";
    pub const EDIT_TURN: &str = "edit_turn";
    pub const DELETE_TURN: &str = "delete_turn";
    pub const SET_REACTION: &str = "set_reaction";
    pub const DELETE_REACTION: &str = "delete_reaction";
}

const CHAT_PROPS: &[&str] = &[
    pep_properties::OWNER_TENANT_ID,
    pep_properties::OWNER_ID,
    pep_properties::RESOURCE_ID,
];
const QUOTA_PROPS: &[&str] = &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID];

const CHAT_RESOURCE: ResourceType =
    ResourceType::from_static(mini_chat_sdk::gts::CHAT_RESOURCE_TYPE, CHAT_PROPS);
const MODEL_RESOURCE: ResourceType =
    ResourceType::from_static(mini_chat_sdk::gts::MODEL_RESOURCE_TYPE, &[]);
const QUOTA_RESOURCE: ResourceType =
    ResourceType::from_static(mini_chat_sdk::gts::USER_QUOTA_RESOURCE_TYPE, QUOTA_PROPS);

/// Authorization port used by the domain services.
#[async_trait]
pub trait Authorizer: Send + Sync {
    /// Evaluate `action` on the chat resource (`chat_id` for existing chats).
    ///
    /// # Errors
    /// 403 on denial / compile failure, 503 on PDP failure.
    async fn chat_scope(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Option<Uuid>,
    ) -> Result<AccessScope, DomainError>;

    /// Permission-only check on the model resource.
    ///
    /// # Errors
    /// 403 / 503 as above.
    async fn model_access(&self, ctx: &SecurityContext, action: &str) -> Result<(), DomainError>;

    /// Evaluate `read` on the user quota resource.
    ///
    /// # Errors
    /// 403 / 503 as above.
    async fn quota_scope(&self, ctx: &SecurityContext) -> Result<AccessScope, DomainError>;
}

fn map_enforcer_err(err: EnforcerError) -> DomainError {
    match err {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => {
            tracing::debug!(error = %err, "authorization denied");
            DomainError::AuthzDenied
        }
        EnforcerError::EvaluationFailed(e) => {
            tracing::error!(error = %e, "authorization evaluation failed");
            DomainError::AuthzUnavailable
        }
    }
}

/// Production authorizer backed by the platform PDP.
pub struct PdpAuthorizer {
    enforcer: PolicyEnforcer,
}

impl PdpAuthorizer {
    #[must_use]
    pub fn new(enforcer: PolicyEnforcer) -> Self {
        Self { enforcer }
    }
}

fn owner_request(ctx: &SecurityContext) -> AccessRequest {
    AccessRequest::new()
        .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
        .resource_property(pep_properties::OWNER_ID, ctx.subject_id())
        .require_constraints(true)
}

#[async_trait]
impl Authorizer for PdpAuthorizer {
    async fn chat_scope(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Option<Uuid>,
    ) -> Result<AccessScope, DomainError> {
        let request = owner_request(ctx);
        self.enforcer
            .access_scope_with(ctx, &CHAT_RESOURCE, action, chat_id, &request)
            .await
            .map_err(map_enforcer_err)
    }

    async fn model_access(&self, ctx: &SecurityContext, action: &str) -> Result<(), DomainError> {
        let request = AccessRequest::new().require_constraints(false);
        self.enforcer
            .access_scope_with(ctx, &MODEL_RESOURCE, action, None, &request)
            .await
            .map(|_| ())
            .map_err(map_enforcer_err)
    }

    async fn quota_scope(&self, ctx: &SecurityContext) -> Result<AccessScope, DomainError> {
        let request = owner_request(ctx);
        self.enforcer
            .access_scope_with(ctx, &QUOTA_RESOURCE, "read", None, &request)
            .await
            .map_err(map_enforcer_err)
    }
}

/// Shared handle.
pub type AuthorizerRef = Arc<dyn Authorizer>;
