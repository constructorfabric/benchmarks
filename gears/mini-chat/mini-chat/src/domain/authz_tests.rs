#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use authz_resolver_sdk::PolicyEnforcer;
use authz_resolver_sdk::models::TenantMode;
use toolkit_security::pep_properties;
use uuid::Uuid;

use super::{ChatAuthz, actions};
use crate::domain::error::DomainError;
use crate::testing::{MockPdp, PdpMode, TestUser};

fn authz(mode: PdpMode) -> (ChatAuthz, Arc<MockPdp>) {
    let pdp = Arc::new(MockPdp::new(mode));
    let enforcer =
        PolicyEnforcer::new(Arc::clone(&pdp) as Arc<dyn authz_resolver_sdk::AuthZResolverApi>);
    (ChatAuthz::new(enforcer), pdp)
}

#[tokio::test]
async fn chat_scope_narrows_tenant_permit_to_owner() {
    let (authz, pdp) = authz(PdpMode::Allow);
    let user = TestUser::A1;
    let chat = Uuid::new_v4();
    let scope = authz
        .chat_scope(&user.security_context(), actions::READ, Some(chat))
        .await
        .unwrap();
    assert!(scope.contains_uuid(pep_properties::OWNER_TENANT_ID, user.tenant_id));
    assert!(scope.contains_uuid(pep_properties::OWNER_ID, user.user_id));
    assert!(!scope.contains_uuid(pep_properties::OWNER_ID, TestUser::A2.user_id));

    let req = &pdp.requests()[0];
    assert_eq!(req.action.name, "read");
    assert_eq!(
        req.resource.resource_type,
        "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~"
    );
    assert_eq!(req.resource.id, Some(chat));
    assert!(req.context.require_constraints);
    assert_eq!(
        req.context.supported_properties,
        ["owner_tenant_id", "owner_id", "id"]
    );
    let tc = req.context.tenant_context.as_ref().unwrap();
    assert_eq!(tc.root_id, Some(user.tenant_id));
    assert_eq!(tc.mode, TenantMode::RootOnly);
    assert_eq!(req.context.token_scopes, ["*"]);
    assert_eq!(
        req.resource.properties[pep_properties::OWNER_TENANT_ID],
        user.tenant_id.to_string()
    );
    assert!(
        !req.resource
            .properties
            .contains_key(pep_properties::OWNER_ID)
    );
}

#[tokio::test]
async fn create_passes_owner_properties() {
    let (authz, pdp) = authz(PdpMode::Allow);
    let user = TestUser::B1;
    authz
        .chat_scope(&user.security_context(), actions::CREATE, None)
        .await
        .unwrap();
    let req = &pdp.requests()[0];
    assert_eq!(req.action.name, "create");
    assert_eq!(req.resource.id, None);
    assert_eq!(
        req.resource.properties[pep_properties::OWNER_ID],
        user.user_id.to_string()
    );
    assert_eq!(
        req.resource.properties[pep_properties::OWNER_TENANT_ID],
        user.tenant_id.to_string()
    );
}

#[tokio::test]
async fn deny_is_permission_denied_and_failure_is_unavailable() {
    let ctx = TestUser::A1.security_context();
    let (authz, pdp) = authz(PdpMode::Deny);
    for res in [
        authz.chat_scope(&ctx, actions::LIST, None).await.map(drop),
        authz.model_permission(&ctx, actions::LIST).await,
        authz.quota_scope(&ctx).await.map(drop),
    ] {
        assert!(matches!(res, Err(DomainError::PermissionDenied)), "{res:?}");
    }
    pdp.set_mode(PdpMode::Fail);
    for res in [
        authz.chat_scope(&ctx, actions::LIST, None).await.map(drop),
        authz.model_permission(&ctx, actions::READ).await,
        authz.quota_scope(&ctx).await.map(drop),
    ] {
        assert!(matches!(res, Err(DomainError::AuthzUnavailable)), "{res:?}");
    }
}

#[tokio::test]
async fn model_permission_does_not_require_constraints() {
    let (authz, pdp) = authz(PdpMode::Allow);
    authz
        .model_permission(&TestUser::A1.security_context(), actions::LIST)
        .await
        .unwrap();
    let req = &pdp.requests()[0];
    assert_eq!(
        req.resource.resource_type,
        "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~"
    );
    assert!(!req.context.require_constraints);
}

#[tokio::test]
async fn quota_scope_targets_user_quota_and_owner() {
    let (authz, pdp) = authz(PdpMode::Allow);
    let user = TestUser::A2;
    let scope = authz.quota_scope(&user.security_context()).await.unwrap();
    assert!(scope.contains_uuid(pep_properties::OWNER_ID, user.user_id));
    let req = &pdp.requests()[0];
    assert_eq!(req.action.name, "read");
    assert_eq!(
        req.resource.resource_type,
        "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~"
    );
    assert_eq!(
        req.context.supported_properties,
        ["owner_tenant_id", "owner_id"]
    );
}
