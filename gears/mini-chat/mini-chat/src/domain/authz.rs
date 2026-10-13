//! Authorization PEP: turns PDP decisions into `AccessScope`s for the secure ORM.

use authz_resolver_sdk::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use mini_chat_sdk::gts::{CHAT_RESOURCE_TYPE, MODEL_RESOURCE_TYPE, USER_QUOTA_RESOURCE_TYPE};
use toolkit_security::{AccessScope, SecurityContext, pep_properties};
use uuid::Uuid;

use crate::domain::error::DomainError;

const CHAT: ResourceType = ResourceType::from_static(
    CHAT_RESOURCE_TYPE,
    &[
        pep_properties::OWNER_TENANT_ID,
        pep_properties::OWNER_ID,
        pep_properties::RESOURCE_ID,
    ],
);
/// Permission-only resource: the PDP returns no row constraints.
const MODEL: ResourceType = ResourceType::from_static(MODEL_RESOURCE_TYPE, &[]);
const USER_QUOTA: ResourceType = ResourceType::from_static(
    USER_QUOTA_RESOURCE_TYPE,
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID],
);

/// Actions on the chat resource, sent to the PDP by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatAction {
    Create,
    List,
    Read,
    Update,
    Delete,
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
}

impl ChatAction {
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

/// Fail-closed mapping: a decision or compilation failure is a 403, an evaluation failure
/// (PDP unreachable, timeout) a 503. The cause is only logged.
fn map_enforcer_err(err: EnforcerError) -> DomainError {
    match err {
        EnforcerError::Denied { .. } | EnforcerError::CompileFailed(_) => DomainError::AccessDenied,
        EnforcerError::EvaluationFailed(cause) => {
            tracing::error!(error = %cause, "authorization evaluation failed");
            DomainError::AuthzUnavailable
        }
    }
}

/// Policy enforcement point for chats, models and user quota.
#[derive(Clone)]
pub struct Authz {
    enforcer: PolicyEnforcer,
}

impl Authz {
    #[must_use]
    pub fn new(enforcer: PolicyEnforcer) -> Self {
        Self { enforcer }
    }

    /// Scope for `action` on chats, always narrowed to the calling user (defence in depth: the
    /// static PDP only constrains the tenant). `Create` also advertises the owning tenant.
    ///
    /// # Errors
    /// `AccessDenied` when the PDP denies or its constraints cannot be compiled,
    /// `AuthzUnavailable` when the PDP cannot evaluate the request.
    pub async fn chat_scope(
        &self,
        ctx: &SecurityContext,
        action: ChatAction,
        chat_id: Option<Uuid>,
    ) -> Result<AccessScope, DomainError> {
        let mut request = AccessRequest::new().require_constraints(true);
        if action == ChatAction::Create {
            request =
                request.resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id());
        }
        let scope = self
            .enforcer
            .access_scope_with(ctx, &CHAT, action.as_str(), chat_id, &request)
            .await
            .map_err(map_enforcer_err)?;
        Ok(scope.ensure_owner(ctx.subject_id()))
    }

    /// Scope for creating a chat.
    ///
    /// # Errors
    /// See [`Self::chat_scope`].
    pub async fn create_scope(&self, ctx: &SecurityContext) -> Result<AccessScope, DomainError> {
        self.chat_scope(ctx, ChatAction::Create, None).await
    }

    /// Permission-only check on the model resource (`list` / `read`).
    ///
    /// # Errors
    /// `ModelAccessDenied` when the PDP denies, `AuthzUnavailable` when it fails.
    pub async fn model_permission(
        &self,
        ctx: &SecurityContext,
        action: &str,
    ) -> Result<(), DomainError> {
        self.enforcer
            .access_scope_with(
                ctx,
                &MODEL,
                action,
                None,
                &AccessRequest::new().require_constraints(false),
            )
            .await
            .map_err(|err| match map_enforcer_err(err) {
                DomainError::AccessDenied => DomainError::ModelAccessDenied,
                other => other,
            })?;
        Ok(())
    }

    /// Scope for reading the caller's own quota.
    ///
    /// # Errors
    /// See [`Self::chat_scope`].
    pub async fn quota_scope(&self, ctx: &SecurityContext) -> Result<AccessScope, DomainError> {
        let scope = self
            .enforcer
            .access_scope_with(ctx, &USER_QUOTA, "read", None, &AccessRequest::new())
            .await
            .map_err(map_enforcer_err)?;
        Ok(scope.ensure_owner(ctx.subject_id()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use authz_resolver_sdk::PolicyEnforcer;
    use toolkit_security::{SecurityContext, pep_properties};
    use uuid::Uuid;

    use super::{Authz, ChatAction};
    use crate::domain::error::DomainError;
    use crate::test_support::pdp::{FakePdp, PdpMode};

    fn ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .expect("ctx")
    }

    fn authz(mode: PdpMode) -> (Authz, Arc<FakePdp>) {
        let pdp = Arc::new(FakePdp::new(mode));
        (Authz::new(PolicyEnforcer::new(pdp.clone())), pdp)
    }

    #[tokio::test]
    async fn owner_constraint_added() {
        let (authz, _) = authz(PdpMode::TenantConstraint);
        let ctx = ctx();
        let scope = authz
            .chat_scope(&ctx, ChatAction::Read, Some(Uuid::new_v4()))
            .await
            .expect("scope");
        assert_eq!(
            scope.all_uuid_values_for(pep_properties::OWNER_ID),
            &[ctx.subject_id()]
        );
        assert!(scope.contains_uuid(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id()));
    }

    #[tokio::test]
    async fn deny_is_403() {
        let (authz, _) = authz(PdpMode::Deny);
        let err = authz
            .chat_scope(&ctx(), ChatAction::List, None)
            .await
            .expect_err("denied");
        assert!(matches!(err, DomainError::AccessDenied), "{err:?}");
        let err = authz.quota_scope(&ctx()).await.expect_err("denied");
        assert!(matches!(err, DomainError::AccessDenied), "{err:?}");
        let err = authz
            .model_permission(&ctx(), "list")
            .await
            .expect_err("denied");
        assert!(matches!(err, DomainError::ModelAccessDenied), "{err:?}");
    }

    #[tokio::test]
    async fn pdp_failure_is_503() {
        let (authz, _) = authz(PdpMode::Fail);
        let err = authz
            .chat_scope(&ctx(), ChatAction::Read, None)
            .await
            .expect_err("failed");
        assert!(matches!(err, DomainError::AuthzUnavailable), "{err:?}");
        let err = authz.create_scope(&ctx()).await.expect_err("failed");
        assert!(matches!(err, DomainError::AuthzUnavailable), "{err:?}");
    }

    #[tokio::test]
    async fn create_scope_sends_owner_tenant_and_requires_constraints() {
        let (authz, pdp) = authz(PdpMode::TenantConstraint);
        let ctx = ctx();
        let scope = authz.create_scope(&ctx).await.expect("scope");
        assert!(scope.contains_uuid(pep_properties::OWNER_ID, ctx.subject_id()));

        let requests = pdp.requests();
        assert_eq!(requests.len(), 1);
        let req = &requests[0];
        assert_eq!(req.action.name, "create");
        assert_eq!(
            req.resource.resource_type,
            mini_chat_sdk::gts::CHAT_RESOURCE_TYPE
        );
        assert_eq!(
            req.resource.properties.get(pep_properties::OWNER_TENANT_ID),
            Some(&serde_json::json!(ctx.subject_tenant_id()))
        );
        assert!(req.context.require_constraints);
    }

    #[tokio::test]
    async fn quota_scope_is_owner_scoped_and_model_permission_is_permission_only() {
        let (authz, pdp) = authz(PdpMode::TenantConstraint);
        let ctx = ctx();
        let scope = authz.quota_scope(&ctx).await.expect("quota scope");
        assert!(scope.contains_uuid(pep_properties::OWNER_ID, ctx.subject_id()));
        authz.model_permission(&ctx, "read").await.expect("model");

        let requests = pdp.requests();
        assert_eq!(requests[0].action.name, "read");
        assert_eq!(
            requests[0].resource.resource_type,
            mini_chat_sdk::gts::USER_QUOTA_RESOURCE_TYPE
        );
        assert_eq!(
            requests[1].resource.resource_type,
            mini_chat_sdk::gts::MODEL_RESOURCE_TYPE
        );
        assert!(!requests[1].context.require_constraints);
        assert!(requests[1].context.supported_properties.is_empty());
    }

    #[test]
    fn chat_action_names_are_stable() {
        assert_eq!(ChatAction::SendMessage.as_str(), "send_message");
        assert_eq!(ChatAction::UploadAttachment.as_str(), "upload_attachment");
        assert_eq!(ChatAction::DeleteReaction.as_str(), "delete_reaction");
    }
}
