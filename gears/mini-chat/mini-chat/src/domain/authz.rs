//! Authorization (PEP), DESIGN section 3.8.
//!
//! Mini Chat is a Policy Enforcement Point: every data-access operation asks the `AuthZ` resolver
//! (PDP) through the [`PolicyEnforcer`] and applies the compiled [`AccessScope`] to its queries.
//! Chat content is owner-only, so every chat-scoped scope additionally carries an owner predicate
//! for the subject (defence in depth: a foreign chat stays invisible even if the PDP returned only
//! a tenant predicate). Authorization fails closed.

use authz_resolver_sdk::PolicyEnforcer;
use authz_resolver_sdk::pep::{AccessRequest, EnforcerError, ResourceType};
use toolkit_gts::gts_id;
use toolkit_macros::domain_model;
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
use tracing::{debug, error, warn};
use uuid::Uuid;

use super::error::{DomainError, ProtectedResource};

/// GTS type of the authorized chat resource.
pub const CHAT_RESOURCE_TYPE: &str = gts_id!("cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~");

/// GTS type of the authorized model resource (permission-only, no constraint scoping).
pub const MODEL_RESOURCE_TYPE: &str =
    gts_id!("cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~");

/// GTS type of the authorized user-quota resource.
pub const USER_QUOTA_RESOURCE_TYPE: &str =
    gts_id!("cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~");

/// The chat resource: `chats.tenant_id` as `owner_tenant_id`, `chats.user_id` as `owner_id`,
/// `chats.id` as `id`. Sub-resources inherit the decision of their parent chat.
pub const CHAT: ResourceType = ResourceType::from_static(
    CHAT_RESOURCE_TYPE,
    &[
        pep_properties::OWNER_TENANT_ID,
        pep_properties::OWNER_ID,
        pep_properties::RESOURCE_ID,
    ],
);

/// The model resource: a pure permission check, the PDP is not asked for constraints.
pub const MODEL: ResourceType = ResourceType::from_static(MODEL_RESOURCE_TYPE, &[]);

/// The user-quota resource (`quota_usage`): tenant and owner constraints.
pub const USER_QUOTA: ResourceType = ResourceType::from_static(
    USER_QUOTA_RESOURCE_TYPE,
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID],
);

/// Actions evaluated by the PDP.
pub mod actions {
    /// Create a chat.
    pub const CREATE: &str = "create";
    /// List chats, models or the quota status.
    pub const LIST: &str = "list";
    /// Read a chat, a model or the quota status.
    pub const READ: &str = "read";
    /// Update the chat title.
    pub const UPDATE: &str = "update";
    /// Delete a chat.
    pub const DELETE: &str = "delete";
    /// List the messages of a chat.
    pub const LIST_MESSAGES: &str = "list_messages";
    /// Send a message.
    pub const SEND_MESSAGE: &str = "send_message";
    /// Upload an attachment.
    pub const UPLOAD_ATTACHMENT: &str = "upload_attachment";
    /// Read an attachment.
    pub const READ_ATTACHMENT: &str = "read_attachment";
    /// Delete an attachment.
    pub const DELETE_ATTACHMENT: &str = "delete_attachment";
    /// Read a turn.
    pub const READ_TURN: &str = "read_turn";
    /// Retry the last turn.
    pub const RETRY_TURN: &str = "retry_turn";
    /// Edit the last turn.
    pub const EDIT_TURN: &str = "edit_turn";
    /// Delete the last turn.
    pub const DELETE_TURN: &str = "delete_turn";
    /// Set a reaction.
    pub const SET_REACTION: &str = "set_reaction";
    /// Remove a reaction.
    pub const DELETE_REACTION: &str = "delete_reaction";
}

/// The Policy Enforcement Point of the gear.
#[domain_model]
#[derive(Clone)]
pub struct Pep {
    enforcer: PolicyEnforcer,
}

impl Pep {
    /// Creates the PEP over a policy enforcer.
    #[must_use]
    pub fn new(enforcer: PolicyEnforcer) -> Self {
        Self { enforcer }
    }

    /// The scope for creating a chat: the PDP gets the owner properties from the security
    /// context and must return constraints; the insert is validated against the returned scope.
    ///
    /// # Errors
    /// `AuthzDenied` when access is refused or no usable constraints came back,
    /// `AuthzUnavailable` when the PDP could not evaluate the request.
    pub async fn create_chat_scope(
        &self,
        ctx: &SecurityContext,
    ) -> Result<AccessScope, DomainError> {
        let request = AccessRequest::new()
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .resource_property(pep_properties::OWNER_ID, ctx.subject_id());
        let scope = self
            .enforcer
            .access_scope_with(ctx, &CHAT, actions::CREATE, None, &request)
            .await
            .map_err(|err| map_enforcer_error(&err, ProtectedResource::Chat))?;
        owner_scoped(ctx, &scope, ProtectedResource::Chat)
    }

    /// The scope for a chat-level action. `chat_id` is `None` for list operations.
    ///
    /// # Errors
    /// `AuthzDenied` when access is refused or no usable constraints came back,
    /// `AuthzUnavailable` when the PDP could not evaluate the request.
    pub async fn chat_scope(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Option<Uuid>,
    ) -> Result<AccessScope, DomainError> {
        let scope = self
            .enforcer
            .access_scope(ctx, &CHAT, action, chat_id)
            .await
            .map_err(|err| map_enforcer_error(&err, ProtectedResource::Chat))?;
        owner_scoped(ctx, &scope, ProtectedResource::Chat)
    }

    /// A pure permission check on the model catalog: the PDP is asked for a decision only
    /// (`require_constraints = false`).
    ///
    /// # Errors
    /// `AuthzDenied` when access is refused, `AuthzUnavailable` when the PDP could not evaluate
    /// the request.
    pub async fn require_model_access(
        &self,
        ctx: &SecurityContext,
        action: &str,
    ) -> Result<(), DomainError> {
        let request = AccessRequest::new().require_constraints(false);
        self.enforcer
            .access_scope_with(ctx, &MODEL, action, None, &request)
            .await
            .map(|_| ())
            .map_err(|err| map_enforcer_error(&err, ProtectedResource::Model))
    }

    /// The scope for reading the caller's quota status.
    ///
    /// # Errors
    /// `AuthzDenied` when access is refused or no usable constraints came back,
    /// `AuthzUnavailable` when the PDP could not evaluate the request.
    pub async fn user_quota_scope(
        &self,
        ctx: &SecurityContext,
    ) -> Result<AccessScope, DomainError> {
        let scope = self
            .enforcer
            .access_scope(ctx, &USER_QUOTA, actions::READ, None)
            .await
            .map_err(|err| map_enforcer_error(&err, ProtectedResource::UserQuota))?;
        owner_scoped(ctx, &scope, ProtectedResource::UserQuota)
    }
}

/// Adds the subject as owner (defence in depth) and refuses a scope that cannot match anything.
fn owner_scoped(
    ctx: &SecurityContext,
    scope: &AccessScope,
    resource: ProtectedResource,
) -> Result<AccessScope, DomainError> {
    let scoped = scope.ensure_owner(ctx.subject_id());
    if scoped.is_deny_all() {
        debug!("authorization scope has no constraint matching the subject");
        return Err(DomainError::AuthzDenied { resource });
    }
    Ok(scoped)
}

/// Fail-closed mapping of enforcer errors: a denial or a constraint compile failure is a 403, a
/// PDP evaluation failure is a retryable 503 (still no access). The cause is only logged.
fn map_enforcer_error(err: &EnforcerError, resource: ProtectedResource) -> DomainError {
    log_enforcer_error(err);
    match err {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => {
            DomainError::AuthzDenied { resource }
        }
        EnforcerError::EvaluationFailed(_) => DomainError::AuthzUnavailable,
    }
}

fn log_enforcer_error(err: &EnforcerError) {
    match err {
        EnforcerError::Denied { .. } => log_denied(),
        EnforcerError::CompileFailed(cause) => log_compile_failure(cause),
        EnforcerError::EvaluationFailed(cause) => log_evaluation_failure(cause),
    }
}

fn log_denied() {
    debug!("authorization denied by the PDP");
}

fn log_compile_failure(cause: &dyn std::fmt::Display) {
    warn!(error = %cause, "authorization constraints could not be compiled");
}

fn log_evaluation_failure(cause: &dyn std::fmt::Display) {
    error!(error = %cause, "authorization evaluation failed");
}
