//! Policy Enforcement Point of the gear (DESIGN section 3.8 "Authorization
//! (PEP)").
//!
//! [`AuthzService`] wraps the platform [`PolicyEnforcer`]: it builds the
//! evaluation request for one of the three mini-chat resource types, sends it
//! to the PDP and compiles the returned constraints into an [`AccessScope`]
//! that the Secure ORM applies as SQL `WHERE` clauses.
//!
//! - **Chat** ([`CHAT_RESOURCE`]) — every chat-content operation of the
//!   per-operation matrix ([`Action`]); messages, attachments and turns are
//!   authorized through their parent chat (`resource.id` = chat id).
//! - **Model** ([`MODEL_RESOURCE`]) — permission-only check
//!   (`require_constraints = false`), the catalog is not tenant data.
//! - **`UserQuota`** ([`USER_QUOTA_RESOURCE`]) — `read`, tenant + owner
//!   constraints.
//!
//! Every request carries `tenant_context = {mode: root_only, root_id:
//! subject tenant}` and the caller's `token_scopes`; no capabilities are
//! advertised in P1.
//!
//! **Defence in depth.** Every chat and user-quota scope is confined to the
//! subject: each constraint additionally gets `owner_tenant_id = subject
//! tenant` and `owner_id = subject id`, so a foreign row stays invisible even
//! when the PDP returns only a tenant predicate. Constraints naming another
//! tenant or owner are dropped; a scope with no constraint left is a denial.
//!
//! **Fail-closed mapping** (DESIGN "Fail-Closed Behavior"):
//!
//! | PEP outcome | Domain error |
//! |---|---|
//! | `decision: false` | [`DomainError::PermissionDenied`] (deny details are never exposed) |
//! | constraint compile failure, missing/empty constraints when required, unknown predicates or properties | [`DomainError::PermissionDenied`] |
//! | PDP unreachable / timeout / evaluation failure | [`DomainError::PdpUnavailable`] (503, `Retry-After: 5`); cause logged at `error` |

use std::sync::Arc;

use authz_resolver_sdk::pep::{
    AccessRequest, ConstraintCompileError, EnforcerError, PolicyEnforcer, ResourceType,
};
use authz_resolver_sdk::{AuthZResolverApi, TenantMode};
use mini_chat_sdk::resource_types;
use toolkit_security::{
    AccessScope, ScopeConstraint, ScopeFilter, SecurityContext, pep_properties,
};
use uuid::Uuid;

use crate::domain::error::{DomainError, ResourceKind};

// ── Resource types ───────────────────────────────────────────────────────

/// Properties of a chat the PEP can compile (`owner_tenant_id` →
/// `chats.tenant_id`, `owner_id` → `chats.user_id`, `id` → `chats.id`).
pub const CHAT_PROPERTIES: &[&str] = &[
    pep_properties::OWNER_TENANT_ID,
    pep_properties::OWNER_ID,
    pep_properties::RESOURCE_ID,
];

/// Properties of a user quota the PEP can compile (`owner_tenant_id` →
/// `quota_usage.tenant_id`, `owner_id` → `quota_usage.user_id`).
pub const USER_QUOTA_PROPERTIES: &[&str] =
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID];

/// PEP descriptor of the Chat resource.
pub const CHAT_RESOURCE: ResourceType =
    ResourceType::from_static(resource_types::CHAT_AUTHZ_RESOURCE_TYPE, CHAT_PROPERTIES);

/// PEP descriptor of the Model resource: permission-only, so no property can
/// be compiled and any returned constraint fails closed.
pub const MODEL_RESOURCE: ResourceType =
    ResourceType::from_static(resource_types::MODEL_AUTHZ_RESOURCE_TYPE, &[]);

/// PEP descriptor of the `UserQuota` resource (`GET /v1/quota/status`).
pub const USER_QUOTA_RESOURCE: ResourceType = ResourceType::from_static(
    resource_types::USER_QUOTA_AUTHZ_RESOURCE_TYPE,
    USER_QUOTA_PROPERTIES,
);

// ── Actions ──────────────────────────────────────────────────────────────

/// PDP action names of the per-operation authorization matrix.
///
/// Chat-scoped actions use the Chat resource; `List` / `Read` are also the
/// Models API actions and `Read` the `UserQuota` action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// `POST /v1/chats`.
    Create,
    /// `GET /v1/chats`, `GET /v1/models`.
    List,
    /// `GET /v1/chats/{id}`, `GET /v1/models/{id}`, `GET /v1/quota/status`.
    Read,
    /// `DELETE /v1/chats/{id}`.
    Delete,
    /// `PATCH /v1/chats/{id}`.
    Update,
    /// `GET /v1/chats/{id}/messages`.
    ListMessages,
    /// `POST /v1/chats/{id}/messages:stream`.
    SendMessage,
    /// `POST /v1/chats/{id}/attachments`.
    UploadAttachment,
    /// `GET /v1/chats/{id}/attachments/{attachment_id}`.
    ReadAttachment,
    /// `DELETE /v1/chats/{id}/attachments/{attachment_id}`.
    DeleteAttachment,
    /// `GET /v1/chats/{id}/turns/{request_id}`.
    ReadTurn,
    /// `POST /v1/chats/{id}/turns/{request_id}/retry`.
    RetryTurn,
    /// `PATCH /v1/chats/{id}/turns/{request_id}`.
    EditTurn,
    /// `DELETE /v1/chats/{id}/turns/{request_id}`.
    DeleteTurn,
    /// `PUT /v1/chats/{id}/messages/{msg_id}/reaction`.
    SetReaction,
    /// `DELETE /v1/chats/{id}/messages/{msg_id}/reaction`.
    DeleteReaction,
}

impl Action {
    /// Every action of the matrix.
    pub const ALL: [Self; 16] = [
        Self::Create,
        Self::List,
        Self::Read,
        Self::Delete,
        Self::Update,
        Self::ListMessages,
        Self::SendMessage,
        Self::UploadAttachment,
        Self::ReadAttachment,
        Self::DeleteAttachment,
        Self::ReadTurn,
        Self::RetryTurn,
        Self::EditTurn,
        Self::DeleteTurn,
        Self::SetReaction,
        Self::DeleteReaction,
    ];

    /// Wire action name (`action.name` of the evaluation request).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::List => "list",
            Self::Read => "read",
            Self::Delete => "delete",
            Self::Update => "update",
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

    /// Whether the action is defined for the Model resource (`list`, `read`).
    #[must_use]
    pub const fn is_model_action(self) -> bool {
        matches!(self, Self::List | Self::Read)
    }
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── PEP service ──────────────────────────────────────────────────────────

/// Mini-chat PEP: evaluates the PDP decision of one operation and returns the
/// compiled, subject-confined [`AccessScope`] or a fail-closed [`DomainError`].
///
/// Cheap to clone (the enforcer holds its client behind an `Arc`).
#[derive(Debug, Clone)]
pub struct AuthzService {
    enforcer: PolicyEnforcer,
}

impl AuthzService {
    /// Wraps a configured enforcer (e.g. `PolicyEnforcer::from_hub(hub)` when
    /// the `AuthZResolverApi` client is wired after `init`).
    #[must_use]
    pub const fn new(enforcer: PolicyEnforcer) -> Self {
        Self { enforcer }
    }

    /// Builds the PEP over a concrete PDP client.
    #[must_use]
    pub fn from_client(client: Arc<dyn AuthZResolverApi>) -> Self {
        Self::new(PolicyEnforcer::new(client))
    }

    /// Authorizes `action` on a chat and returns the scope to apply to the
    /// chat queries (or to the insert of `create`).
    ///
    /// `chat_id` is the `resource.id` (the parent chat for sub-resource
    /// actions; `None` for `create` and `list`). For `create` the request
    /// carries `resource.properties.owner_tenant_id` / `owner_id` of the
    /// subject. The returned scope always contains the subject's tenant and
    /// owner predicates.
    ///
    /// # Errors
    ///
    /// [`DomainError::PermissionDenied`] (chat) on denial, constraint compile
    /// failure or when no constraint remains after confinement to the
    /// subject; [`DomainError::PdpUnavailable`] when the PDP cannot evaluate.
    pub async fn authorize_chat(
        &self,
        ctx: &SecurityContext,
        action: Action,
        chat_id: Option<Uuid>,
    ) -> Result<AccessScope, DomainError> {
        let mut request = base_request(ctx).require_constraints(true);
        if action == Action::Create {
            request = request
                .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
                .resource_property(pep_properties::OWNER_ID, ctx.subject_id());
        }
        let scope = self
            .evaluate(
                ctx,
                &CHAT_RESOURCE,
                ResourceKind::Chat,
                action,
                chat_id,
                &request,
            )
            .await?;
        confine(&scope, ctx, ResourceKind::Chat, action)
    }

    /// Permission-only check of a Models API action (`list` or `read`,
    /// `require_constraints = false`).
    ///
    /// # Errors
    ///
    /// [`DomainError::PermissionDenied`] (model) on denial or when the PDP
    /// returns any constraint (the catalog cannot be row-filtered);
    /// [`DomainError::PdpUnavailable`] when the PDP cannot evaluate;
    /// [`DomainError::Internal`] for an action outside the Models API.
    pub async fn authorize_model(
        &self,
        ctx: &SecurityContext,
        action: Action,
    ) -> Result<(), DomainError> {
        if !action.is_model_action() {
            return Err(DomainError::internal(format!(
                "action `{action}` is not defined for the model resource"
            )));
        }
        let request = base_request(ctx).require_constraints(false);
        let scope = self
            .evaluate(
                ctx,
                &MODEL_RESOURCE,
                ResourceKind::Model,
                action,
                None,
                &request,
            )
            .await?;
        if scope.is_unconstrained() {
            Ok(())
        } else {
            tracing::warn!(
                action = action.as_str(),
                resource_type = MODEL_RESOURCE.name(),
                "PDP returned constraints for the permission-only model resource; access denied"
            );
            Err(DomainError::PermissionDenied {
                resource: ResourceKind::Model,
            })
        }
    }

    /// Authorizes `read` on the caller's user quota and returns the scope to
    /// apply to the quota queries (always confined to the subject's tenant
    /// and owner).
    ///
    /// Denials report the message resource, like every other quota error of
    /// the REST contract.
    ///
    /// # Errors
    ///
    /// [`DomainError::PermissionDenied`] on denial, compile failure or when no
    /// constraint remains after confinement; [`DomainError::PdpUnavailable`]
    /// when the PDP cannot evaluate.
    pub async fn authorize_user_quota(
        &self,
        ctx: &SecurityContext,
    ) -> Result<AccessScope, DomainError> {
        let request = base_request(ctx).require_constraints(true);
        let scope = self
            .evaluate(
                ctx,
                &USER_QUOTA_RESOURCE,
                ResourceKind::Message,
                Action::Read,
                None,
                &request,
            )
            .await?;
        confine(&scope, ctx, ResourceKind::Message, Action::Read)
    }

    async fn evaluate(
        &self,
        ctx: &SecurityContext,
        resource: &ResourceType,
        kind: ResourceKind,
        action: Action,
        resource_id: Option<Uuid>,
        request: &AccessRequest,
    ) -> Result<AccessScope, DomainError> {
        self.enforcer
            .access_scope_with(ctx, resource, action.as_str(), resource_id, request)
            .await
            .map_err(|err| map_enforcer_error(err, resource, kind, action))
    }
}

/// Request defaults shared by every mini-chat evaluation: the subject's tenant
/// as `root_only` context tenant (no hierarchy for chat content).
fn base_request(ctx: &SecurityContext) -> AccessRequest {
    AccessRequest::new()
        .context_tenant_id(ctx.subject_tenant_id())
        .tenant_mode(TenantMode::RootOnly)
}

/// Maps a PEP failure to the fail-closed domain error; deny details and
/// diagnostics stay server-side.
fn map_enforcer_error(
    err: EnforcerError,
    resource: &ResourceType,
    kind: ResourceKind,
    action: Action,
) -> DomainError {
    match err {
        EnforcerError::Denied { deny_reason } => {
            tracing::debug!(
                action = action.as_str(),
                resource_type = resource.name(),
                deny_code = deny_reason.as_ref().map(|r| r.error_code.as_str()),
                "PDP denied access"
            );
            DomainError::PermissionDenied { resource: kind }
        }
        EnforcerError::CompileFailed(cause) => {
            log_compile_failure(&cause, resource, action);
            DomainError::PermissionDenied { resource: kind }
        }
        EnforcerError::EvaluationFailed(cause) => {
            tracing::error!(
                action = action.as_str(),
                resource_type = resource.name(),
                error = %cause,
                "PDP evaluation failed; access refused"
            );
            DomainError::PdpUnavailable
        }
    }
}

/// A missing constraint set is a policy outcome (`warn`); any other compile
/// failure is a PDP contract violation (`error`).
fn log_compile_failure(cause: &ConstraintCompileError, resource: &ResourceType, action: Action) {
    if matches!(cause, ConstraintCompileError::ConstraintsRequiredButAbsent) {
        tracing::warn!(
            action = action.as_str(),
            resource_type = resource.name(),
            "PDP allowed without constraints although constraints are required; access denied"
        );
    } else {
        tracing::error!(
            action = action.as_str(),
            resource_type = resource.name(),
            error = %cause,
            "PDP constraints failed compilation (PDP contract violation); access denied"
        );
    }
}

/// Confines `scope` to the subject (defence in depth) and fails closed when
/// nothing of the subject's own data remains reachable.
fn confine(
    scope: &AccessScope,
    ctx: &SecurityContext,
    kind: ResourceKind,
    action: Action,
) -> Result<AccessScope, DomainError> {
    let confined = confine_to_subject(scope, ctx.subject_tenant_id(), ctx.subject_id());
    if confined.is_deny_all() {
        tracing::warn!(
            action = action.as_str(),
            resource = kind.name(),
            "PDP constraints grant no access to the subject's own data; access denied"
        );
        return Err(DomainError::PermissionDenied { resource: kind });
    }
    Ok(confined)
}

/// Returns `scope` with `owner_tenant_id = tenant_id` and `owner_id =
/// owner_id` in every constraint (intersection semantics: constraints naming
/// another tenant or owner are dropped; an unconstrained scope becomes the
/// tenant + owner constraint; deny-all stays deny-all).
#[must_use]
pub fn confine_to_subject(scope: &AccessScope, tenant_id: Uuid, owner_id: Uuid) -> AccessScope {
    ensure_eq(
        &ensure_eq(scope, pep_properties::OWNER_ID, owner_id),
        pep_properties::OWNER_TENANT_ID,
        tenant_id,
    )
}

/// Intersects every constraint of `scope` with `property = value`.
fn ensure_eq(scope: &AccessScope, property: &str, value: Uuid) -> AccessScope {
    if scope.is_deny_all() {
        return AccessScope::deny_all();
    }
    let required = ScopeFilter::eq(property, value);
    if scope.is_unconstrained() {
        return AccessScope::single(ScopeConstraint::new(vec![required]));
    }
    let constraints = scope
        .constraints()
        .iter()
        .filter_map(|constraint| {
            let (on_property, mut others): (Vec<&ScopeFilter>, Vec<&ScopeFilter>) = constraint
                .filters()
                .iter()
                .partition(|filter| filter.property() == property);
            // Subquery filters expose no in-memory values and are dropped here
            // too (fail-closed): P1 advertises no hierarchy or group capability.
            let admits_value = on_property
                .iter()
                .all(|filter| filter.values().iter().any(|v| v.as_uuid() == Some(value)));
            if !admits_value {
                return None;
            }
            others.push(&required);
            Some(ScopeConstraint::new(others.into_iter().cloned().collect()))
        })
        .collect();
    AccessScope::from_constraints(constraints)
}

#[cfg(test)]
#[path = "authz_tests.rs"]
mod tests;
