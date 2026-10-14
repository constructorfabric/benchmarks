//! Authorization tests: the operation matrix (DESIGN 3.8) and the PEP flow against a mock PDP.
//!
//! Slice S1, algorithms `authz-matrix` and `pep-authorize`, definition of done `authz-pep`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::time::Duration;

use authz_resolver_sdk::PolicyEnforcer;
use authz_resolver_sdk::models::TenantMode;
use serde_json::json;
use toolkit_security::{SecurityContext, pep_properties};
use uuid::Uuid;

use super::{
    Authorization, AuthzResource, Operation, OperationDescriptor, PDP_DEADLINE, PepService,
};
use crate::domain::error::DomainError;
use crate::test_support::{
    Pdp, RecordingPdp, SUBJECT_USER_TYPE, TENANT_A, USER_A1, USER_A2, ctx, ctx_with, enforcer,
};

const CHAT_TYPE: &str = "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~";
const MODEL_TYPE: &str = "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~";
const QUOTA_TYPE: &str = "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~";

fn chat_id() -> Uuid {
    Uuid::from_u128(0xC4A7)
}

fn user() -> SecurityContext {
    ctx_with(USER_A1, TENANT_A, Some(SUBJECT_USER_TYPE), &["*"])
}

fn new_pep(behavior: Pdp) -> (PepService, std::sync::Arc<RecordingPdp>) {
    let (enforcer, pdp) = enforcer(behavior);
    (PepService::new(enforcer), pdp)
}

fn chat_id_for(op: Operation) -> Option<Uuid> {
    op.descriptor().sends_resource_id.then(chat_id)
}

/// `(method, path template, operation, action, resource, sends resource.id, require_constraints)`
type MatrixRow = (
    &'static str,
    &'static str,
    Operation,
    &'static str,
    AuthzResource,
    bool,
    bool,
);

fn matrix() -> Vec<MatrixRow> {
    use AuthzResource::{Chat, Model, UserQuota};
    use Operation::*;
    vec![
        ("POST", "/v1/chats", CreateChat, "create", Chat, false, true),
        ("GET", "/v1/chats", ListChats, "list", Chat, false, true),
        ("GET", "/v1/chats/{id}", GetChat, "read", Chat, true, true),
        (
            "DELETE",
            "/v1/chats/{id}",
            DeleteChat,
            "delete",
            Chat,
            true,
            true,
        ),
        (
            "PATCH",
            "/v1/chats/{id}",
            UpdateChat,
            "update",
            Chat,
            true,
            true,
        ),
        (
            "GET",
            "/v1/chats/{id}/messages",
            ListMessages,
            "list_messages",
            Chat,
            true,
            true,
        ),
        (
            "POST",
            "/v1/chats/{id}/messages:stream",
            SendMessage,
            "send_message",
            Chat,
            true,
            true,
        ),
        (
            "POST",
            "/v1/chats/{id}/attachments",
            UploadAttachment,
            "upload_attachment",
            Chat,
            true,
            true,
        ),
        (
            "GET",
            "/v1/chats/{id}/attachments/{attachment_id}",
            ReadAttachment,
            "read_attachment",
            Chat,
            true,
            true,
        ),
        (
            "DELETE",
            "/v1/chats/{id}/attachments/{attachment_id}",
            DeleteAttachment,
            "delete_attachment",
            Chat,
            true,
            true,
        ),
        (
            "GET",
            "/v1/chats/{id}/turns/{request_id}",
            ReadTurn,
            "read_turn",
            Chat,
            true,
            true,
        ),
        (
            "POST",
            "/v1/chats/{id}/turns/{request_id}/retry",
            RetryTurn,
            "retry_turn",
            Chat,
            true,
            true,
        ),
        (
            "PATCH",
            "/v1/chats/{id}/turns/{request_id}",
            EditTurn,
            "edit_turn",
            Chat,
            true,
            true,
        ),
        (
            "DELETE",
            "/v1/chats/{id}/turns/{request_id}",
            DeleteTurn,
            "delete_turn",
            Chat,
            true,
            true,
        ),
        (
            "PUT",
            "/v1/chats/{id}/messages/{msg_id}/reaction",
            SetReaction,
            "set_reaction",
            Chat,
            true,
            true,
        ),
        (
            "DELETE",
            "/v1/chats/{id}/messages/{msg_id}/reaction",
            DeleteReaction,
            "delete_reaction",
            Chat,
            true,
            true,
        ),
        ("GET", "/v1/models", ListModels, "list", Model, false, false),
        (
            "GET",
            "/v1/models/{id}",
            GetModel,
            "read",
            Model,
            false,
            false,
        ),
        (
            "GET",
            "/v1/quota/status",
            GetQuotaStatus,
            "read",
            UserQuota,
            false,
            true,
        ),
    ]
}

// ── authorization matrix ─────────────────────────────────────────────────────

#[test]
fn the_matrix_covers_all_19_operations_exactly_once() {
    assert_eq!(matrix().len(), 19);
    assert_eq!(Operation::ALL.len(), 19);
    let distinct: BTreeSet<String> = Operation::ALL.iter().map(|op| format!("{op:?}")).collect();
    assert_eq!(distinct.len(), 19);
    for op in Operation::ALL {
        assert_eq!(
            matrix().iter().filter(|row| row.2 == op).count(),
            1,
            "{op:?}"
        );
    }
}

#[test]
fn every_operation_has_the_exact_action_resource_id_presence_and_constraint_flag() {
    for (method, template, op, action, resource, sends_id, require) in matrix() {
        let descriptor = op.descriptor();
        let expected = OperationDescriptor {
            action,
            resource,
            sends_resource_id: sends_id,
            require_constraints: require,
        };
        assert_eq!(descriptor, expected, "{method} {template}");
    }
}

#[test]
fn a_method_and_path_template_resolve_to_their_operation() {
    for (method, template, op, ..) in matrix() {
        assert_eq!(
            Operation::lookup(method, template),
            Some(op),
            "{method} {template}"
        );
    }
}

#[test]
fn routes_without_an_authorization_entry_resolve_to_nothing() {
    let unknown = [
        ("POST", "/v1/chats/{id}:temporary"),
        ("GET", "/v1/mcp-servers"),
        ("POST", "/v1/mcp-servers"),
        ("GET", "/v1/admin/roles/{role}/mcp-servers"),
        ("GET", "/v1/chats/{id}/mcp-tools/effective"),
        ("POST", "/v1/mcp-connections:complete"),
        ("PUT", "/v1/chats"),
        ("GET", "/v1/chats/{id}/messages:stream"),
        ("POST", "/v1/models"),
        ("GET", "/v2/chats"),
        ("GET", "/mini-chat/v1/chats"),
    ];
    for (method, template) in unknown {
        assert_eq!(
            Operation::lookup(method, template),
            None,
            "{method} {template}"
        );
    }
}

#[test]
fn messages_stream_is_authorized_with_send_message_only() {
    assert_eq!(Operation::SendMessage.descriptor().action, "send_message");
    assert_ne!(Operation::SendMessage.descriptor().action, "read");
}

#[test]
fn resource_types_and_supported_properties_follow_the_design() {
    assert_eq!(AuthzResource::Chat.type_id(), CHAT_TYPE);
    assert_eq!(AuthzResource::Model.type_id(), MODEL_TYPE);
    assert_eq!(AuthzResource::UserQuota.type_id(), QUOTA_TYPE);
    for resource in [
        AuthzResource::Chat,
        AuthzResource::Model,
        AuthzResource::UserQuota,
    ] {
        assert_eq!(resource.resource_type().name(), resource.type_id());
    }
    let chat: BTreeSet<&str> = AuthzResource::Chat
        .resource_type()
        .supported_properties()
        .iter()
        .copied()
        .collect();
    assert_eq!(chat, BTreeSet::from(["owner_tenant_id", "owner_id", "id"]));
    let quota: BTreeSet<&str> = AuthzResource::UserQuota
        .resource_type()
        .supported_properties()
        .iter()
        .copied()
        .collect();
    assert_eq!(quota, BTreeSet::from(["owner_tenant_id", "owner_id"]));
}

// ── the PDP request ──────────────────────────────────────────────────────────

#[tokio::test]
async fn the_request_declares_subject_tenant_context_capabilities_and_supported_properties() {
    let (pep, pdp) = new_pep(Pdp::TenantAndOwner);
    pep.authorize(&user(), Operation::ListChats, None)
        .await
        .unwrap();
    let request = pdp.only_request();

    assert_eq!(request.subject.id, USER_A1);
    assert_eq!(
        request.subject.subject_type.as_deref(),
        Some(SUBJECT_USER_TYPE)
    );
    assert_eq!(
        request.subject.properties["tenant_id"],
        json!(TENANT_A.to_string())
    );
    assert_eq!(request.action.name, "list");
    assert_eq!(request.resource.resource_type, CHAT_TYPE);
    assert_eq!(request.resource.id, None);

    let tenant_context = request.context.tenant_context.expect("tenant_context");
    assert_eq!(tenant_context.mode, TenantMode::RootOnly);
    assert_eq!(tenant_context.root_id, Some(TENANT_A));
    assert!(request.context.require_constraints);
    assert!(request.context.capabilities.is_empty());
    let supported: BTreeSet<&str> = request
        .context
        .supported_properties
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(
        supported,
        BTreeSet::from(["owner_tenant_id", "owner_id", "id"])
    );
}

#[tokio::test]
async fn the_subject_type_is_user_even_when_the_security_context_has_none() {
    let (pep, pdp) = new_pep(Pdp::TenantAndOwner);
    pep.authorize(&ctx(USER_A1, TENANT_A), Operation::ListChats, None)
        .await
        .unwrap();
    assert_eq!(
        pdp.only_request().subject.subject_type.as_deref(),
        Some(SUBJECT_USER_TYPE)
    );
}

#[tokio::test]
async fn the_chat_id_is_sent_as_resource_id_only_when_the_matrix_says_so() {
    for op in Operation::ALL {
        let (pep, pdp) = new_pep(Pdp::TenantAndOwner);
        pep.authorize(&user(), op, chat_id_for(op)).await.unwrap();
        let request = pdp.only_request();
        let expected = op.descriptor().sends_resource_id.then(chat_id);
        assert_eq!(request.resource.id, expected, "{op:?}");
        assert_eq!(request.action.name, op.descriptor().action, "{op:?}");
        assert_eq!(
            request.resource.resource_type,
            op.descriptor().resource.type_id(),
            "{op:?}"
        );
        assert_eq!(
            request.context.require_constraints,
            op.descriptor().require_constraints,
            "{op:?}"
        );
    }
}

#[tokio::test]
async fn create_passes_the_owner_properties_from_the_security_context() {
    let (pep, pdp) = new_pep(Pdp::TenantAndOwner);
    pep.authorize(&user(), Operation::CreateChat, None)
        .await
        .unwrap();
    let request = pdp.only_request();
    assert_eq!(
        request.resource.properties["owner_tenant_id"],
        json!(TENANT_A.to_string())
    );
    assert_eq!(
        request.resource.properties["owner_id"],
        json!(USER_A1.to_string())
    );
    assert!(request.context.require_constraints);
}

#[tokio::test]
async fn only_create_carries_resource_properties() {
    for op in Operation::ALL
        .into_iter()
        .filter(|op| *op != Operation::CreateChat)
    {
        let (pep, pdp) = new_pep(Pdp::TenantAndOwner);
        pep.authorize(&user(), op, chat_id_for(op)).await.unwrap();
        assert!(pdp.only_request().resource.properties.is_empty(), "{op:?}");
    }
}

#[tokio::test]
async fn token_scopes_are_forwarded_and_enforcement_is_left_to_the_pdp() {
    let read_only = ctx_with(
        USER_A1,
        TENANT_A,
        Some(SUBJECT_USER_TYPE),
        &["ai:mini_chat:read"],
    );
    let (pep, pdp) = new_pep(Pdp::TenantAndOwner);
    // the PEP does not enforce scopes: a write operation under a read scope still reaches the PDP
    pep.authorize(&read_only, Operation::CreateChat, None)
        .await
        .expect("PDP decides");
    assert_eq!(
        pdp.only_request().context.token_scopes,
        vec!["ai:mini_chat:read".to_owned()]
    );

    let (pep, pdp) = new_pep(Pdp::TenantAndOwner);
    pep.authorize(&user(), Operation::ListChats, None)
        .await
        .unwrap();
    assert_eq!(
        pdp.only_request().context.token_scopes,
        vec!["*".to_owned()]
    );
}

#[tokio::test]
async fn the_user_quota_request_does_not_advertise_the_id_property() {
    let (pep, pdp) = new_pep(Pdp::TenantAndOwner);
    pep.authorize(&user(), Operation::GetQuotaStatus, None)
        .await
        .unwrap();
    let request = pdp.only_request();
    let supported: BTreeSet<&str> = request
        .context
        .supported_properties
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(supported, BTreeSet::from(["owner_tenant_id", "owner_id"]));
    assert_eq!(request.resource.resource_type, QUOTA_TYPE);
}

#[tokio::test]
async fn streaming_and_turn_mutations_evaluate_the_pdp_exactly_once() {
    for op in [
        Operation::SendMessage,
        Operation::RetryTurn,
        Operation::EditTurn,
    ] {
        let (pep, pdp) = new_pep(Pdp::TenantAndOwner);
        pep.authorize(&user(), op, Some(chat_id())).await.unwrap();
        let requests = pdp.requests();
        assert_eq!(requests.len(), 1, "{op:?}");
        assert_eq!(requests[0].action.name, op.descriptor().action);
    }
}

// ── decision-only (Models API) ───────────────────────────────────────────────

#[tokio::test]
async fn the_models_operations_are_decision_only() {
    for op in [Operation::ListModels, Operation::GetModel] {
        for behavior in [Pdp::NoConstraints, Pdp::TenantOnly, Pdp::TenantAndOwner] {
            let (pep, pdp) = new_pep(behavior);
            let outcome = pep.authorize(&user(), op, None).await.unwrap();
            assert_eq!(outcome, Authorization::DecisionOnly, "{op:?} {behavior:?}");
            assert!(outcome.scope().is_none());
            let request = pdp.only_request();
            assert!(!request.context.require_constraints);
            assert_eq!(request.resource.resource_type, MODEL_TYPE);
            assert_eq!(request.resource.id, None);
        }
    }
}

#[tokio::test]
async fn a_decision_only_operation_is_still_refused_on_deny_and_on_failure() {
    let (pep, _) = new_pep(Pdp::Deny);
    let err = pep
        .authorize(&user(), Operation::ListModels, None)
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::AuthzDenied), "{err:?}");

    let (pep, _) = new_pep(Pdp::Unreachable);
    let err = pep
        .authorize(&user(), Operation::GetModel, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, DomainError::AuthzUnavailable { .. }),
        "{err:?}"
    );
}

// ── fail closed ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_pdp_deny_is_refused_with_and_without_a_resource_id() {
    for (op, id) in [
        (Operation::GetChat, Some(chat_id())),
        (Operation::ListChats, None),
    ] {
        let (pep, _) = new_pep(Pdp::Deny);
        let err = pep.authorize(&user(), op, id).await.unwrap_err();
        assert!(matches!(err, DomainError::AuthzDenied), "{op:?}: {err:?}");
    }
}

#[tokio::test]
async fn a_decision_without_constraints_is_refused_when_constraints_are_required() {
    for op in [
        Operation::CreateChat,
        Operation::ListChats,
        Operation::GetChat,
        Operation::GetQuotaStatus,
    ] {
        let (pep, _) = new_pep(Pdp::NoConstraints);
        let err = pep
            .authorize(&user(), op, chat_id_for(op))
            .await
            .unwrap_err();
        assert!(matches!(err, DomainError::AuthzDenied), "{op:?}: {err:?}");
    }
}

#[tokio::test]
async fn a_predicate_on_an_unknown_property_fails_the_constraint_and_the_request() {
    let (pep, _) = new_pep(Pdp::UnknownProperty);
    let err = pep
        .authorize(&user(), Operation::ListChats, None)
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::AuthzDenied), "{err:?}");
}

#[tokio::test]
async fn a_predicate_of_an_unadvertised_kind_is_a_compile_failure_and_refused() {
    let (pep, _) = new_pep(Pdp::UnadvertisedPredicate);
    let err = pep
        .authorize(&user(), Operation::ListChats, None)
        .await
        .unwrap_err();
    assert!(matches!(err, DomainError::AuthzDenied), "{err:?}");
}

#[tokio::test]
async fn one_failed_constraint_does_not_fail_the_set_while_another_is_valid() {
    let (pep, _) = new_pep(Pdp::OneBadOneGood);
    let outcome = pep
        .authorize(&user(), Operation::ListChats, None)
        .await
        .unwrap();
    let scope = outcome.scope().expect("scope");
    assert!(scope.contains_uuid(pep_properties::OWNER_TENANT_ID, TENANT_A));
    assert!(scope.contains_uuid(pep_properties::OWNER_ID, USER_A1));
}

#[tokio::test]
async fn an_unreachable_pdp_is_a_503_class_failure_not_a_denial() {
    for op in [
        Operation::CreateChat,
        Operation::GetChat,
        Operation::GetQuotaStatus,
    ] {
        let (pep, _) = new_pep(Pdp::Unreachable);
        let err = pep
            .authorize(&user(), op, chat_id_for(op))
            .await
            .unwrap_err();
        assert!(
            matches!(err, DomainError::AuthzUnavailable { .. }),
            "{op:?}: {err:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_pdp_that_never_answers_hits_the_deadline_and_is_a_503_class_failure() {
    let pdp = RecordingPdp::new(Pdp::Hang);
    let enforcer = PolicyEnforcer::new(pdp).with_deadline(Duration::from_secs(1));
    let pep = PepService::new(enforcer);
    let err = pep
        .authorize(&user(), Operation::ListChats, None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, DomainError::AuthzUnavailable { .. }),
        "{err:?}"
    );
}

#[test]
fn the_production_pdp_deadline_is_below_the_gateway_request_timeout() {
    // the api-gateway request timeout is 30 s: the PEP must refuse first
    assert!(PDP_DEADLINE < Duration::from_secs(30), "{PDP_DEADLINE:?}");
    assert!(PDP_DEADLINE >= Duration::from_secs(1), "{PDP_DEADLINE:?}");
}

#[tokio::test(start_paused = true)]
async fn the_production_pep_wiring_maps_a_pdp_over_the_deadline_to_503_with_retry_after_5() {
    let pdp = RecordingPdp::new(Pdp::Hang);
    let pep = PepService::with_pdp(pdp);
    let started = tokio::time::Instant::now();
    let err = pep
        .authorize(&user(), Operation::ListChats, None)
        .await
        .unwrap_err();
    // fail-closed at the production bound, not at the toolkit default of 30 s
    assert_eq!(started.elapsed(), PDP_DEADLINE);
    assert!(
        matches!(err, DomainError::AuthzUnavailable { .. }),
        "{err:?}"
    );
    let response = axum::response::IntoResponse::into_response(
        toolkit_canonical_errors::CanonicalError::from(err),
    );
    assert_eq!(response.status(), 503);
    assert_eq!(response.headers().get("retry-after").unwrap(), "5");
}

#[tokio::test]
async fn a_refusal_never_returns_a_scope() {
    for behavior in [
        Pdp::Deny,
        Pdp::NoConstraints,
        Pdp::UnknownProperty,
        Pdp::Unreachable,
    ] {
        let (pep, _) = new_pep(behavior);
        assert!(
            pep.authorize(&user(), Operation::ListChats, None)
                .await
                .is_err(),
            "{behavior:?}"
        );
    }
}

// ── owner predicate (defence in depth) ───────────────────────────────────────

#[tokio::test]
async fn a_tenant_only_pdp_answer_still_yields_an_owner_scoped_scope_for_every_chat_operation() {
    let chat_ops = Operation::ALL
        .into_iter()
        .filter(|op| op.descriptor().resource == AuthzResource::Chat);
    for op in chat_ops {
        let (pep, _) = new_pep(Pdp::TenantOnly);
        let outcome = pep.authorize(&user(), op, chat_id_for(op)).await.unwrap();
        let scope = outcome
            .scope()
            .unwrap_or_else(|| panic!("{op:?} must be scoped"));
        assert!(
            scope.contains_uuid(pep_properties::OWNER_TENANT_ID, TENANT_A),
            "{op:?}"
        );
        assert!(
            scope.contains_uuid(pep_properties::OWNER_ID, USER_A1),
            "{op:?}"
        );
        assert!(
            !scope.contains_uuid(pep_properties::OWNER_ID, USER_A2),
            "{op:?}"
        );
        assert!(!scope.is_unconstrained() && !scope.is_deny_all(), "{op:?}");
    }
}

#[tokio::test]
async fn the_quota_status_scope_gets_the_owner_predicate_too() {
    let (pep, _) = new_pep(Pdp::TenantOnly);
    let outcome = pep
        .authorize(&user(), Operation::GetQuotaStatus, None)
        .await
        .unwrap();
    let scope = outcome.scope().expect("scope");
    assert!(scope.contains_uuid(pep_properties::OWNER_TENANT_ID, TENANT_A));
    assert!(scope.contains_uuid(pep_properties::OWNER_ID, USER_A1));
}

#[tokio::test]
async fn the_owner_predicate_is_the_calling_subject() {
    let (pep, _) = new_pep(Pdp::TenantOnly);
    let second_user = ctx_with(USER_A2, TENANT_A, Some(SUBJECT_USER_TYPE), &["*"]);
    let outcome = pep
        .authorize(&second_user, Operation::ListChats, None)
        .await
        .unwrap();
    let scope = outcome.scope().unwrap();
    assert!(scope.contains_uuid(pep_properties::OWNER_ID, USER_A2));
    assert!(!scope.contains_uuid(pep_properties::OWNER_ID, USER_A1));
}

#[tokio::test]
async fn a_pdp_that_already_returns_the_owner_predicate_is_kept_as_is() {
    let (pep, _) = new_pep(Pdp::TenantAndOwner);
    let outcome = pep
        .authorize(&user(), Operation::GetChat, Some(chat_id()))
        .await
        .unwrap();
    let scope = outcome.scope().unwrap();
    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_ID),
        vec![USER_A1]
    );
    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        vec![TENANT_A]
    );
}
