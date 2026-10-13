//! Policy enforcement point (PEP) of the gear (DESIGN section 3.8).
//!
//! Every data access or catalog read is authorized here first.
// @cpt-dod:cpt-cf-mini-chat-dod-gear-foundation-authz-pep:p1

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use authz_resolver_sdk::AuthZResolverApi;
use authz_resolver_sdk::pep::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use toolkit_security::{
    AccessScope, PlatformSecurityContext, ScopeConstraint, ScopeFilter, SecurityContext,
    pep_properties,
};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Chat resource type.
pub const CHAT_RESOURCE_TYPE: &str = "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~";
/// Model resource type (decision only).
pub const MODEL_RESOURCE_TYPE: &str = "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~";
/// User quota resource type.
pub const USER_QUOTA_RESOURCE_TYPE: &str =
    "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~";

/// Bound of one PDP evaluation.
pub const PDP_EVALUATION_TIMEOUT: Duration = Duration::from_secs(5);

/// Properties every mini-chat resource declares to the PDP: `owner_tenant_id`
/// (`chats.tenant_id`), `owner_id` (`chats.user_id`) and `id` (`chats.id`).
const SUPPORTED_PROPERTIES: &[&str] = &[
    pep_properties::OWNER_TENANT_ID,
    pep_properties::OWNER_ID,
    pep_properties::RESOURCE_ID,
];

/// Properties of the user quota resource: tenant and owner only, it has no
/// `id` property (DESIGN section 3.8).
const USER_QUOTA_SUPPORTED_PROPERTIES: &[&str] =
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID];

const CHAT_RESOURCE: ResourceType =
    ResourceType::from_static(CHAT_RESOURCE_TYPE, SUPPORTED_PROPERTIES);
const MODEL_RESOURCE: ResourceType =
    ResourceType::from_static(MODEL_RESOURCE_TYPE, SUPPORTED_PROPERTIES);
const USER_QUOTA_RESOURCE: ResourceType =
    ResourceType::from_static(USER_QUOTA_RESOURCE_TYPE, USER_QUOTA_SUPPORTED_PROPERTIES);

/// Headroom of the enforcer's own deadline over the service bound, so that the
/// service bound always fires first and is the one reported as `timeout`.
const ENFORCER_DEADLINE_HEADROOM: Duration = Duration::from_millis(250);

/// Actions on the chat resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatAction {
    /// `create`
    Create,
    /// `list`
    List,
    /// `read`
    Read,
    /// `update`
    Update,
    /// `delete`
    Delete,
    /// `list_messages`
    ListMessages,
    /// `send_message`
    SendMessage,
    /// `upload_attachment`
    UploadAttachment,
    /// `read_attachment`
    ReadAttachment,
    /// `delete_attachment`
    DeleteAttachment,
    /// `read_turn`
    ReadTurn,
    /// `retry_turn`
    RetryTurn,
    /// `edit_turn`
    EditTurn,
    /// `delete_turn`
    DeleteTurn,
    /// `set_reaction`
    SetReaction,
    /// `delete_reaction`
    DeleteReaction,
}

impl ChatAction {
    /// Action name sent to the PDP.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::List => "list",
            Self::Read => "read",
            Self::Update => "update",
            Self::Delete => "delete",
            Self::ListMessages => "list_messages",
            Self::SendMessage => "send_message",
            Self::UploadAttachment => "upload_attachment",
            Self::ReadAttachment => "read_attachment",
            Self::DeleteAttachment => "delete_attachment",
            Self::ReadTurn => "read_turn",
            Self::RetryTurn => "retry_turn",
            Self::EditTurn => "edit_turn",
            Self::DeleteTurn => "delete_turn",
            Self::SetReaction => "set_reaction",
            Self::DeleteReaction => "delete_reaction",
        }
    }
}

/// Actions on the model resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelAction {
    /// `list`
    List,
    /// `read`
    Read,
}

impl ModelAction {
    /// Action name sent to the PDP.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::List => "list",
            Self::Read => "read",
        }
    }
}

/// Domain-layer PEP service (fail-closed).
///
/// Every outcome other than an explicit allow with a usable scope is an error:
/// a deny, an empty or uncompilable constraint list and an empty scope are
/// [`DomainError::AuthzDenied`]; an unreachable, failing or slow PDP is
/// [`DomainError::PdpUnavailable`].
pub struct AuthzService {
    enforcer: PolicyEnforcer,
    authz: Arc<dyn AuthZResolverApi>,
    timeout: Duration,
}

impl AuthzService {
    /// Creates the service with the default `PDP_EVALUATION_TIMEOUT`.
    #[must_use]
    pub fn new(authz: Arc<dyn AuthZResolverApi>) -> Self {
        Self::with_timeout(authz, PDP_EVALUATION_TIMEOUT)
    }

    /// Creates the service with a custom PDP evaluation bound.
    #[must_use]
    pub fn with_timeout(authz: Arc<dyn AuthZResolverApi>, timeout: Duration) -> Self {
        Self {
            enforcer: PolicyEnforcer::new(Arc::clone(&authz))
                .with_deadline(timeout.saturating_add(ENFORCER_DEADLINE_HEADROOM)),
            authz,
            timeout,
        }
    }

    /// Authorizes `action` on the chat resource and returns the compiled scope
    /// including the owner predicates. `chat_id` becomes `resource.id`.
    ///
    /// # Errors
    ///
    /// `AuthzDenied` on deny, empty constraints or compile failure;
    /// `PdpUnavailable` when the PDP fails or times out.
    pub async fn chat_scope(
        &self,
        ctx: &SecurityContext,
        action: ChatAction,
        chat_id: Option<Uuid>,
    ) -> Result<AccessScope, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-build-request
        let mut request = AccessRequest::new().require_constraints(true);
        if action == ChatAction::Create {
            request = request
                .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
                .resource_property(pep_properties::OWNER_ID, ctx.subject_id());
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-build-request

        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-authorize
        let scope = self
            .evaluate(ctx, &CHAT_RESOURCE, action.as_str(), chat_id, &request)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-authorize

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-if-chat-resource
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-add-owner-predicate
        let scope = restrict_to_tenant(
            &scope.ensure_owner(ctx.subject_id()),
            ctx.subject_tenant_id(),
        );
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-add-owner-predicate
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-if-chat-resource
        Self::non_empty(scope)
    }

    /// Authorizes `action` on the model resource (decision only).
    ///
    /// # Errors
    ///
    /// `AuthzDenied` on deny; `PdpUnavailable` when the PDP fails or times out.
    pub async fn model_decision(
        &self,
        ctx: &SecurityContext,
        action: ModelAction,
    ) -> Result<(), DomainError> {
        let request = AccessRequest::new().require_constraints(false);
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-authorize
        // Decision only: the PDP answer is checked before, and without, any
        // constraint compilation, so an allow that carries constraints this
        // resource cannot express is still an allow.
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-build-context
        let eval_request = self.enforcer.build_request_with(
            ctx,
            &MODEL_RESOURCE,
            action.as_str(),
            None,
            false,
            &request,
        );
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-build-context
        let decision = async {
            let response = self
                .authz
                .evaluate(PlatformSecurityContext::outbound_marker(), eval_request)
                .await?;
            if response.decision {
                Ok(())
            } else {
                Err(EnforcerError::Denied {
                    deny_reason: response.context.deny_reason,
                })
            }
        };
        self.bounded(&MODEL_RESOURCE, action.as_str(), decision)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-authorize
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-if-decision-only
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-return-decision-only
        Ok(())
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-return-decision-only
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-if-decision-only
    }

    /// Authorizes `read` on the user quota resource.
    ///
    /// # Errors
    ///
    /// `AuthzDenied` on deny, empty constraints or compile failure;
    /// `PdpUnavailable` when the PDP fails or times out.
    pub async fn user_quota_scope(
        &self,
        ctx: &SecurityContext,
    ) -> Result<AccessScope, DomainError> {
        let request = AccessRequest::new().require_constraints(true);
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-authorize
        let scope = self
            .evaluate(ctx, &USER_QUOTA_RESOURCE, "read", None, &request)
            .await?;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-gate:p1:inst-authorize

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-add-owner-predicate
        let scope = restrict_to_tenant(
            &scope.ensure_owner(ctx.subject_id()),
            ctx.subject_tenant_id(),
        );
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-add-owner-predicate
        Self::non_empty(scope)
    }

    /// Builds and evaluates the PDP request, bounded by the service timeout, and
    /// maps every failure to the fail-closed domain error.
    async fn evaluate(
        &self,
        ctx: &SecurityContext,
        resource: &ResourceType,
        action: &str,
        resource_id: Option<Uuid>,
        request: &AccessRequest,
    ) -> Result<AccessScope, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-build-context
        // The enforcer declares no `tenant_hierarchy`, `group_membership` or
        // `group_hierarchy` capability, the supported properties of the resource
        // descriptor, the token scopes of `ctx` and the `require_constraints`
        // flag of `request`.
        let evaluation =
            self.enforcer
                .access_scope_with(ctx, resource, action, resource_id, request);
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-build-context

        self.bounded(resource, action, evaluation).await
    }

    /// Bounds a PEP call by the service timeout and maps every failure to the
    /// fail-closed domain error.
    async fn bounded<T>(
        &self,
        resource: &ResourceType,
        action: &str,
        evaluation: impl Future<Output = Result<T, EnforcerError>>,
    ) -> Result<T, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-try-pdp
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-evaluate
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-for-each-constraint
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-compile-constraint
        let outcome = tokio::time::timeout(self.timeout, evaluation).await;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-compile-constraint
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-for-each-constraint
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-evaluate
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-try-pdp

        match outcome {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => Err(map_enforcer_error(&err, action)),
            Err(_elapsed) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-catch-pdp
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-return-pdp-unavailable
                tracing::error!(
                    action,
                    resource = resource.name(),
                    cause = "timeout",
                    timeout_ms = u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX),
                    "PDP evaluation timed out"
                );
                Err(DomainError::PdpUnavailable)
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-return-pdp-unavailable
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-catch-pdp
            }
        }
    }

    /// An empty scope admits nothing: fail closed with the denied error.
    fn non_empty(scope: AccessScope) -> Result<AccessScope, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-if-compile-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-return-compile-denied
        if scope.is_deny_all() {
            return Err(DomainError::AuthzDenied);
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-return-compile-denied
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-if-compile-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-return-scope
        Ok(scope)
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-return-scope
    }
}

/// Maps a PEP failure of the enforcer to the fail-closed domain error. Causes
/// are logged here and never reach the response.
fn map_enforcer_error(err: &EnforcerError, action: &str) -> DomainError {
    match err {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-if-denied
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-return-denied
        EnforcerError::Denied { .. } => {
            tracing::debug!(action, "PDP denied the request");
            DomainError::AuthzDenied
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-return-denied
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-if-denied
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-if-no-constraints
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-return-no-constraints
        EnforcerError::CompileFailed(cause) => compile_denied(action, cause),
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-return-no-constraints
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-if-no-constraints
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-catch-pdp
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-return-pdp-unavailable
        EnforcerError::EvaluationFailed(cause) => pdp_unavailable(action, cause),
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-return-pdp-unavailable
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authorize:p1:inst-catch-pdp
    }
}

fn compile_denied(action: &str, cause: &dyn std::fmt::Display) -> DomainError {
    tracing::warn!(action, %cause, "PDP constraints could not be compiled; denying");
    DomainError::AuthzDenied
}

fn pdp_unavailable(action: &str, cause: &dyn std::fmt::Display) -> DomainError {
    tracing::error!(action, %cause, "PDP evaluation failed");
    DomainError::PdpUnavailable
}

/// Intersects every constraint of `scope` with `tenant_id = tenant`: a
/// constraint that already names tenants must include `tenant` (and is narrowed
/// to it), a constraint without a tenant predicate gets one. Constraints whose
/// tenant predicate cannot be decided from its values (subtree predicates) are
/// dropped, so the result is never wider than the input.
fn restrict_to_tenant(scope: &AccessScope, tenant: Uuid) -> AccessScope {
    if scope.is_deny_all() {
        return AccessScope::deny_all();
    }
    let tenant_filter = ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, tenant);
    if scope.is_unconstrained() {
        return AccessScope::single(ScopeConstraint::new(vec![tenant_filter]));
    }
    let constraints = scope
        .constraints()
        .iter()
        .filter_map(|constraint| {
            let mut kept = Vec::with_capacity(constraint.filters().len() + 1);
            for filter in constraint.filters() {
                if filter.property() == pep_properties::OWNER_TENANT_ID {
                    if !filter.uuid_values().contains(&tenant) {
                        return None;
                    }
                } else {
                    kept.push(filter.clone());
                }
            }
            kept.push(tenant_filter.clone());
            ScopeConstraint::try_new(kept).ok()
        })
        .collect();
    AccessScope::from_constraints(constraints)
}

#[cfg(test)]
#[path = "authz_tests.rs"]
mod tests;
