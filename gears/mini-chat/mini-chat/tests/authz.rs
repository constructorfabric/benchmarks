#![allow(clippy::unwrap_used, clippy::expect_used)]

//! PEP wrapper (`domain::authz::Pep`) against the fake PDP.

mod common;

use common::{PdpMode, TestApp, chat_row};
use mini_chat::domain::authz;
use mini_chat::domain::error::DomainError;
use mini_chat::infra::db::repos::ChatRepo;
use toolkit_security::{SecurityContext, pep_properties};
use uuid::Uuid;

const CHAT_TYPE: &str = "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~";
const QUOTA_TYPE: &str = "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~";

fn ctx(user: Uuid, tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(user)
        .subject_tenant_id(tenant)
        .build()
        .unwrap()
}

#[tokio::test]
async fn chat_scope_hides_other_users_chats_when_pdp_returns_tenant_only() {
    let app = TestApp::builder().build().await;
    let tenant = Uuid::new_v4();
    let (alice, bob) = (Uuid::new_v4(), Uuid::new_v4());
    let conn = app.db.conn().unwrap();
    let owner_scope = common::tenant_scope(tenant, bob);
    let bobs = ChatRepo
        .insert(&conn, &owner_scope, chat_row(tenant, bob))
        .await
        .unwrap();
    let alices = ChatRepo
        .insert(
            &conn,
            &common::tenant_scope(tenant, alice),
            chat_row(tenant, alice),
        )
        .await
        .unwrap();

    let scope = app
        .services
        .pep
        .chat_scope(&ctx(alice, tenant), authz::READ, Some(bobs.id))
        .await
        .expect("allowed");

    assert!(
        ChatRepo
            .find_by_id(&conn, &scope, bobs.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        ChatRepo
            .find_by_id(&conn, &scope, alices.id)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn chat_scope_request_shape_for_read() {
    let app = TestApp::builder().build().await;
    let (user, tenant, chat) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());

    app.services
        .pep
        .chat_scope(&ctx(user, tenant), authz::LIST_MESSAGES, Some(chat))
        .await
        .unwrap();

    let req = app.pdp.last_request();
    assert_eq!(req.resource.resource_type, CHAT_TYPE);
    assert_eq!(req.action.name, "list_messages");
    assert_eq!(req.resource.id, Some(chat));
    assert!(req.context.require_constraints);
    assert!(req.context.capabilities.is_empty());
    assert_eq!(
        req.context.supported_properties,
        ["owner_tenant_id", "owner_id", "id"]
    );
    let tc = req.context.tenant_context.expect("tenant context");
    assert_eq!(tc.root_id, Some(tenant));
    assert_eq!(tc.mode, authz_resolver_sdk::TenantMode::RootOnly);
    assert!(req.resource.properties.is_empty());
}

#[tokio::test]
async fn chat_scope_create_passes_owner_properties() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = (Uuid::new_v4(), Uuid::new_v4());

    let scope = app
        .services
        .pep
        .chat_scope(&ctx(user, tenant), authz::CREATE, None)
        .await
        .unwrap();

    let req = app.pdp.last_request();
    assert_eq!(req.action.name, "create");
    assert!(req.resource.id.is_none());
    assert_eq!(
        req.resource.properties[pep_properties::OWNER_TENANT_ID],
        tenant.to_string()
    );
    assert_eq!(
        req.resource.properties[pep_properties::OWNER_ID],
        user.to_string()
    );
    assert_eq!(scope.all_uuid_values_for(pep_properties::OWNER_ID), [user]);
    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        [tenant]
    );
}

#[tokio::test]
async fn deny_maps_to_authz_denied_and_failure_to_unavailable() {
    let app = TestApp::builder().build().await;
    let c = ctx(Uuid::new_v4(), Uuid::new_v4());
    let pep = &app.services.pep;

    app.pdp.set_mode(PdpMode::Deny);
    assert_eq!(
        pep.chat_scope(&c, authz::READ, Some(Uuid::new_v4())).await,
        Err(DomainError::AuthzDenied)
    );
    assert_eq!(
        pep.model_access(&c, authz::LIST).await,
        Err(DomainError::AuthzDenied)
    );
    assert_eq!(pep.quota_scope(&c).await, Err(DomainError::AuthzDenied));

    app.pdp.set_mode(PdpMode::Fail);
    assert_eq!(
        pep.chat_scope(&c, authz::READ, Some(Uuid::new_v4())).await,
        Err(DomainError::AuthzUnavailable)
    );
    assert_eq!(
        pep.model_access(&c, authz::LIST).await,
        Err(DomainError::AuthzUnavailable)
    );
    assert_eq!(
        pep.quota_scope(&c).await,
        Err(DomainError::AuthzUnavailable)
    );
}

#[tokio::test]
async fn quota_scope_is_tenant_and_owner_of_subject() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = (Uuid::new_v4(), Uuid::new_v4());

    let scope = app
        .services
        .pep
        .quota_scope(&ctx(user, tenant))
        .await
        .unwrap();

    let req = app.pdp.last_request();
    assert_eq!(req.resource.resource_type, QUOTA_TYPE);
    assert_eq!(req.action.name, "read");
    assert!(req.context.require_constraints);
    assert_eq!(
        req.context.supported_properties,
        ["owner_tenant_id", "owner_id"]
    );
    assert_eq!(scope.all_uuid_values_for(pep_properties::OWNER_ID), [user]);
    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        [tenant]
    );
}

#[test]
fn action_names_are_snake_case() {
    assert_eq!(
        [
            authz::CREATE,
            authz::LIST,
            authz::READ,
            authz::UPDATE,
            authz::DELETE,
            authz::LIST_MESSAGES,
            authz::SEND_MESSAGE,
            authz::UPLOAD_ATTACHMENT,
            authz::READ_ATTACHMENT,
            authz::DELETE_ATTACHMENT,
            authz::READ_TURN,
            authz::RETRY_TURN,
            authz::EDIT_TURN,
            authz::DELETE_TURN,
            authz::SET_REACTION,
            authz::DELETE_REACTION,
        ],
        [
            "create",
            "list",
            "read",
            "update",
            "delete",
            "list_messages",
            "send_message",
            "upload_attachment",
            "read_attachment",
            "delete_attachment",
            "read_turn",
            "retry_turn",
            "edit_turn",
            "delete_turn",
            "set_reaction",
            "delete_reaction",
        ]
    );
    assert_eq!(authz::CHAT.name(), CHAT_TYPE);
    assert_eq!(
        authz::MODEL.name(),
        "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~"
    );
    assert_eq!(authz::USER_QUOTA.name(), QUOTA_TYPE);
}
