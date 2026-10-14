//! Policy Enforcement Point: the per-operation authorization matrix and the PEP flow.
//!
//! The matrix is normative in DESIGN 3.8 `Per-Operation Authorization Matrix`. The PEP
//! builds the PDP request from the matrix, compiles the constraints into an `AccessScope`,
//! adds the owner predicate to every Chat and `UserQuota` scope and fails closed.

use std::sync::Arc;
use std::time::Duration;

use authz_resolver_sdk::models::TenantMode;
use authz_resolver_sdk::pep::{AccessRequest, ResourceType};
use authz_resolver_sdk::{AuthZResolverApi, EnforcerError, PolicyEnforcer};
use toolkit_gts::gts_id;
use toolkit_macros::domain_model;
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Subject type of every PDP request: the calling end user.
const SUBJECT_USER_TYPE: &str = gts_id!("cf.core.security.subject_user.v1~");

/// GTS resource types sent to the PDP.
const CHAT_TYPE: &str = gts_id!("cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~");
const MODEL_TYPE: &str = gts_id!("cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~");
const USER_QUOTA_TYPE: &str =
    gts_id!("cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~");

/// Properties the PEP compiles for a chat (`chats.tenant_id`, `chats.user_id`, `chats.id`).
const CHAT_PROPERTIES: &[&str] = &[
    pep_properties::OWNER_TENANT_ID,
    pep_properties::OWNER_ID,
    pep_properties::RESOURCE_ID,
];
/// Properties the PEP compiles for the quota status (`quota_usage.tenant_id`, `.user_id`).
const USER_QUOTA_PROPERTIES: &[&str] = &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID];
/// The Models API is decision-only: no property is compiled.
const MODEL_PROPERTIES: &[&str] = &[];

/// Per-call PDP deadline of the production PEP. It stays clearly below the 30 s request
/// timeout of the api-gateway, so a hung PDP is refused by the PEP as a 503
/// `service_unavailable` with `Retry-After: 5` before the gateway answers 504.
pub const PDP_DEADLINE: Duration = Duration::from_secs(10);

/// The 19 authorized operations of the public API.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    CreateChat,
    ListChats,
    GetChat,
    DeleteChat,
    UpdateChat,
    ListMessages,
    SendMessage,
    UploadAttachment,
    ReadAttachment,
    DeleteAttachment,
    ReadTurn,
    RetryTurn,
    EditTurn,
    DeleteTurn,
    SetReaction,
    DeleteReaction,
    ListModels,
    GetModel,
    GetQuotaStatus,
}

/// The resource types a PEP evaluation targets.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthzResource {
    /// `gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~`
    Chat,
    /// `gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~`
    Model,
    /// `gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~`
    UserQuota,
}

/// What the matrix says about one operation.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperationDescriptor {
    /// The PDP action name, for example `list_messages`.
    pub action: &'static str,
    pub resource: AuthzResource,
    /// Whether the chat id from the path is sent as `resource.id`.
    pub sends_resource_id: bool,
    pub require_constraints: bool,
}

impl OperationDescriptor {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-return
    const fn new(
        action: &'static str,
        resource: AuthzResource,
        sends_resource_id: bool,
        require_constraints: bool,
    ) -> Self {
        Self {
            action,
            resource,
            sends_resource_id,
            require_constraints,
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-return
}

impl Operation {
    /// Every operation, in the order of the DESIGN matrix.
    pub const ALL: [Self; 19] = [
        Self::CreateChat,
        Self::ListChats,
        Self::GetChat,
        Self::DeleteChat,
        Self::UpdateChat,
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
        Self::ListModels,
        Self::GetModel,
        Self::GetQuotaStatus,
    ];

    /// The matrix row of this operation.
    #[must_use]
    pub fn descriptor(self) -> OperationDescriptor {
        use AuthzResource::{Chat, Model, UserQuota};
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-match
        match self {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-create
            Self::CreateChat => OperationDescriptor::new("create", Chat, false, true),
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-create
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-list
            Self::ListChats => OperationDescriptor::new("list", Chat, false, true),
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-list
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-read
            Self::GetChat => OperationDescriptor::new("read", Chat, true, true),
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-read
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-delete
            Self::DeleteChat => OperationDescriptor::new("delete", Chat, true, true),
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-delete
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-update
            Self::UpdateChat => OperationDescriptor::new("update", Chat, true, true),
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-update
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-list-messages
            Self::ListMessages => OperationDescriptor::new("list_messages", Chat, true, true),
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-list-messages
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-send-message
            Self::SendMessage => OperationDescriptor::new("send_message", Chat, true, true),
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-send-message
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-upload
            Self::UploadAttachment => {
                OperationDescriptor::new("upload_attachment", Chat, true, true)
            }
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-upload
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-read-attachment
            Self::ReadAttachment => OperationDescriptor::new("read_attachment", Chat, true, true),
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-read-attachment
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-delete-attachment
            Self::DeleteAttachment => {
                OperationDescriptor::new("delete_attachment", Chat, true, true)
            }
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-delete-attachment
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-read-turn
            Self::ReadTurn => OperationDescriptor::new("read_turn", Chat, true, true),
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-read-turn
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-retry
            Self::RetryTurn => OperationDescriptor::new("retry_turn", Chat, true, true),
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-retry
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-edit
            Self::EditTurn => OperationDescriptor::new("edit_turn", Chat, true, true),
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-edit
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-delete-turn
            Self::DeleteTurn => OperationDescriptor::new("delete_turn", Chat, true, true),
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-delete-turn
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-set-reaction
            Self::SetReaction => OperationDescriptor::new("set_reaction", Chat, true, true),
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-set-reaction
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-delete-reaction
            Self::DeleteReaction => OperationDescriptor::new("delete_reaction", Chat, true, true),
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-delete-reaction
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-models-list
            Self::ListModels => OperationDescriptor::new("list", Model, false, false),
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-models-list
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-models-read
            Self::GetModel => OperationDescriptor::new("read", Model, false, false),
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-models-read
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-quota-status
            Self::GetQuotaStatus => OperationDescriptor::new("read", UserQuota, false, true),
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-quota-status
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-match
    }

    /// The HTTP method and path template (without the URL prefix) of this operation.
    #[must_use]
    pub const fn route(self) -> (&'static str, &'static str) {
        match self {
            Self::CreateChat => ("POST", "/v1/chats"),
            Self::ListChats => ("GET", "/v1/chats"),
            Self::GetChat => ("GET", "/v1/chats/{id}"),
            Self::DeleteChat => ("DELETE", "/v1/chats/{id}"),
            Self::UpdateChat => ("PATCH", "/v1/chats/{id}"),
            Self::ListMessages => ("GET", "/v1/chats/{id}/messages"),
            Self::SendMessage => ("POST", "/v1/chats/{id}/messages:stream"),
            Self::UploadAttachment => ("POST", "/v1/chats/{id}/attachments"),
            Self::ReadAttachment => ("GET", "/v1/chats/{id}/attachments/{attachment_id}"),
            Self::DeleteAttachment => ("DELETE", "/v1/chats/{id}/attachments/{attachment_id}"),
            Self::ReadTurn => ("GET", "/v1/chats/{id}/turns/{request_id}"),
            Self::RetryTurn => ("POST", "/v1/chats/{id}/turns/{request_id}/retry"),
            Self::EditTurn => ("PATCH", "/v1/chats/{id}/turns/{request_id}"),
            Self::DeleteTurn => ("DELETE", "/v1/chats/{id}/turns/{request_id}"),
            Self::SetReaction => ("PUT", "/v1/chats/{id}/messages/{msg_id}/reaction"),
            Self::DeleteReaction => ("DELETE", "/v1/chats/{id}/messages/{msg_id}/reaction"),
            Self::ListModels => ("GET", "/v1/models"),
            Self::GetModel => ("GET", "/v1/models/{id}"),
            Self::GetQuotaStatus => ("GET", "/v1/quota/status"),
        }
    }

    /// Resolve an HTTP method and a path template (without the URL prefix, with the
    /// DESIGN parameter names, for example `/v1/chats/{id}/messages:stream`) to its
    /// operation. A route without an authorization entry (for example
    /// `POST /v1/chats/{id}:temporary`) resolves to `None`.
    #[must_use]
    pub fn lookup(method: &str, path_template: &str) -> Option<Self> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-default
        // no authorization entry means `None`, so the route is not registered
        Self::ALL
            .into_iter()
            .find(|operation| operation.route() == (method, path_template))
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-default
    }
}

impl AuthzResource {
    /// The GTS resource type id sent to the PDP.
    #[must_use]
    pub fn type_id(self) -> &'static str {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-resource-types
        match self {
            Self::Chat => CHAT_TYPE,
            Self::Model => MODEL_TYPE,
            Self::UserQuota => USER_QUOTA_TYPE,
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-authz-matrix:p1:inst-matrix-resource-types
    }

    /// The PEP resource type with its supported properties (`owner_tenant_id`,
    /// `owner_id`, `id` for Chat; `owner_tenant_id`, `owner_id` for `UserQuota`).
    #[must_use]
    pub fn resource_type(self) -> ResourceType {
        let properties = match self {
            Self::Chat => CHAT_PROPERTIES,
            Self::Model => MODEL_PROPERTIES,
            Self::UserQuota => USER_QUOTA_PROPERTIES,
        };
        ResourceType::from_static(self.type_id(), properties)
    }
}

/// Outcome of an authorization that was not refused.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub enum Authorization {
    /// A permission-only decision (the Models API): no `AccessScope`.
    DecisionOnly,
    /// The compiled scope, with the owner predicate added for Chat and `UserQuota`.
    Scoped(AccessScope),
}

impl Authorization {
    /// The scope of a `Scoped` outcome.
    #[must_use]
    pub fn scope(&self) -> Option<&AccessScope> {
        match self {
            Self::DecisionOnly => None,
            Self::Scoped(scope) => Some(scope),
        }
    }
}

/// The PEP of the gear: builds the PDP request from the matrix, compiles the
/// constraints, adds the owner predicate and fails closed.
#[domain_model]
pub struct PepService {
    enforcer: PolicyEnforcer,
}

impl PepService {
    /// Create the PEP over a policy enforcer.
    #[must_use]
    pub fn new(enforcer: PolicyEnforcer) -> Self {
        Self { enforcer }
    }

    /// Create the production PEP over the PDP client, bounded by [`PDP_DEADLINE`].
    #[must_use]
    pub fn with_pdp(authz: Arc<dyn AuthZResolverApi>) -> Self {
        Self::new(PolicyEnforcer::new(authz).with_deadline(PDP_DEADLINE))
    }

    /// Evaluate authorization for `operation`.
    ///
    /// # Errors
    ///
    /// - [`DomainError::AuthzDenied`] for a PDP deny, a decision without usable
    ///   constraints and a constraint compile failure;
    /// - [`DomainError::AuthzUnavailable`] when the PDP cannot evaluate the request.
    pub async fn authorize(
        &self,
        ctx: &SecurityContext,
        operation: Operation,
        chat_id: Option<Uuid>,
    ) -> Result<Authorization, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-resolve-operation
        let descriptor = operation.descriptor();
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-resolve-operation
        let resource_type = descriptor.resource.resource_type();

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-build-request
        let subject = subject_user_context(ctx)?;
        let resource_id = if descriptor.sends_resource_id {
            chat_id
        } else {
            None
        };
        let mut request = AccessRequest::new();
        if operation == Operation::CreateChat {
            request = request
                .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
                .resource_property(pep_properties::OWNER_ID, ctx.subject_id());
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-build-request

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-build-context
        // `capabilities` stay empty (the enforcer default) and `supported_properties` come from
        // the resource type; `token_scopes` are copied from the subject context
        let request = request
            .tenant_mode(TenantMode::RootOnly)
            .context_tenant_id(ctx.subject_tenant_id())
            .require_constraints(descriptor.require_constraints);
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-build-context

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-try
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-call-pdp
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-authorize
        let outcome = self
            .enforcer
            .access_scope_with(
                &subject,
                &resource_type,
                descriptor.action,
                resource_id,
                &request,
            )
            .await;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-authorize
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-call-pdp
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-try
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-authz-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-return-refusal
        let scope = refuse_or_scope(outcome)?;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-return-refusal
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-authz-refused

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-permission-only
        if !descriptor.require_constraints {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-return-permission-only
            return Ok(Authorization::DecisionOnly);
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-return-permission-only
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-permission-only

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-owner-predicate
        let scope = match descriptor.resource {
            AuthzResource::Chat | AuthzResource::UserQuota => scope.ensure_owner(ctx.subject_id()),
            AuthzResource::Model => scope,
        };
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-owner-predicate
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-return-scope
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-return-scope
        Ok(Authorization::Scoped(scope))
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-request-admission:p1:inst-admission-return-scope
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-return-scope
    }
}

/// The security context sent to the PDP: the caller's identity, tenant, token scopes and bearer
/// token, with the subject type `gts.cf.core.security.subject_user.v1~` whatever the
/// authentication plugin supplied.
fn subject_user_context(ctx: &SecurityContext) -> Result<SecurityContext, DomainError> {
    let mut builder = SecurityContext::builder()
        .subject_id(ctx.subject_id())
        .subject_tenant_id(ctx.subject_tenant_id())
        .subject_type(SUBJECT_USER_TYPE);
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-token-scopes
    // the scopes are forwarded as they are; the PDP alone enforces them
    builder = builder.token_scopes(ctx.token_scopes().to_vec());
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-token-scopes
    if let Some(token) = ctx.bearer_token() {
        builder = builder.bearer_token(token.clone());
    }
    builder
        .build()
        .map_err(|e| DomainError::internal(format!("cannot build the PDP subject: {e}")))
}

/// Fail closed: every enforcer failure is a refusal, and only a PDP that cannot evaluate is
/// reported as unavailable.
fn refuse_or_scope(
    outcome: Result<AccessScope, EnforcerError>,
) -> Result<AccessScope, DomainError> {
    match outcome {
        Ok(scope) => Ok(scope),
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-catch
        Err(EnforcerError::EvaluationFailed(cause)) => {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-return-503
            tracing::error!(error = %cause, "PDP evaluation failed; access stays refused");
            Err(DomainError::AuthzUnavailable {
                cause: format!("{cause}"),
            })
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-return-503
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-catch
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-denied
        Err(EnforcerError::Denied { .. }) => {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-return-403-denied
            // the deny reason details are never exposed
            Err(DomainError::AuthzDenied)
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-return-403-denied
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-denied
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-compile
        Err(EnforcerError::CompileFailed(cause)) => {
            // a decision without usable constraints, an unknown predicate or property, or an
            // unadvertised predicate kind: the scope cannot be compiled, so access is refused
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-no-constraints
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-compile-failed
            tracing::warn!(error = %cause, "PDP constraints could not be compiled; refusing");
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-return-403-no-constraints
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-return-403-compile
            Err(DomainError::AuthzDenied)
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-return-403-compile
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-return-403-no-constraints
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-compile-failed
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-no-constraints
        } // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-pep-authorize:p1:inst-pep-compile
    }
}

#[cfg(test)]
#[path = "authz_tests.rs"]
mod authz_tests;
