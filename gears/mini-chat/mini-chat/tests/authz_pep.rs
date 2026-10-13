//! Authorization foundation (PEP helpers) — DESIGN 3.8.
//!
//! Pins: resource-type ids, action names of the per-operation matrix, what the
//! PEP sends to the PDP (`require_constraints`, supported properties,
//! capabilities, resource properties on create), the owner predicate added to
//! every chat scope, and the fail-closed table (denied -> `AccessDenied`,
//! unreachable PDP -> `ServiceUnavailable` with a 5 s retry hint).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

mod common;

use authz_resolver_sdk::pep::{ConstraintCompileError, EnforcerError};
use common::{FakePdp, PdpMode, TENANT_A, USER_A, ctx_a};
use mini_chat::domain::authz::{
    CHAT_RESOURCE_TYPE, MODEL_RESOURCE_TYPE, USER_QUOTA_RESOURCE_TYPE, actions, authorize_decision,
    chat_scope, map_enforcer_err, model_resource,
};
use mini_chat::domain::error::DomainError;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_security::pep_properties;
use uuid::Uuid;

// ── constants ────────────────────────────────────────────────────────────────

#[test]
fn resource_type_ids_match_the_design() {
    assert_eq!(
        CHAT_RESOURCE_TYPE,
        "gts.cf.core.ai_chat.chat.v1~cf.core.mini_chat.chat.v1~"
    );
    assert_eq!(
        MODEL_RESOURCE_TYPE,
        "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~"
    );
    assert_eq!(
        USER_QUOTA_RESOURCE_TYPE,
        "gts.cf.core.ai_chat.user_quota.v1~cf.core.mini_chat.user_quota.v1~"
    );
}

#[test]
fn action_names_follow_the_per_operation_matrix() {
    for (got, want) in [
        (actions::CREATE, "create"),
        (actions::LIST, "list"),
        (actions::READ, "read"),
        (actions::UPDATE, "update"),
        (actions::DELETE, "delete"),
        (actions::LIST_MESSAGES, "list_messages"),
        (actions::SEND_MESSAGE, "send_message"),
        (actions::UPLOAD_ATTACHMENT, "upload_attachment"),
        (actions::READ_ATTACHMENT, "read_attachment"),
        (actions::DELETE_ATTACHMENT, "delete_attachment"),
        (actions::READ_TURN, "read_turn"),
        (actions::RETRY_TURN, "retry_turn"),
        (actions::EDIT_TURN, "edit_turn"),
        (actions::DELETE_TURN, "delete_turn"),
        (actions::SET_REACTION, "set_reaction"),
        (actions::DELETE_REACTION, "delete_reaction"),
    ] {
        assert_eq!(got, want);
    }
}

// ── decision-only authorization (Models API) ────────────────────────────────

#[tokio::test]
async fn authorize_decision_allows_without_requiring_constraints() {
    // A PDP that says `decision: true` with no constraints is fine for a
    // permission-only resource (`require_constraints = false`).
    let pdp = FakePdp::new(PdpMode::AllowWithoutConstraints);
    let enforcer = pdp.enforcer();

    authorize_decision(&enforcer, &ctx_a(), &model_resource(), actions::LIST)
        .await
        .expect("permission-only decision is honoured");

    let req = pdp.last();
    assert_eq!(req.resource.resource_type, MODEL_RESOURCE_TYPE);
    assert_eq!(req.action.name, "list");
    assert!(!req.context.require_constraints);
    assert!(req.resource.id.is_none());
}

#[tokio::test]
async fn authorize_decision_denial_is_access_denied() {
    let pdp = FakePdp::deny("hidden-reason");
    let err = authorize_decision(&pdp.enforcer(), &ctx_a(), &model_resource(), actions::READ)
        .await
        .expect_err("denied");
    assert!(matches!(err, DomainError::AccessDenied { .. }), "{err:?}");
}

#[tokio::test]
async fn authorize_decision_pdp_failure_is_service_unavailable_with_retry_after_5() {
    let pdp = FakePdp::unreachable("transport down");
    let err = authorize_decision(&pdp.enforcer(), &ctx_a(), &model_resource(), actions::LIST)
        .await
        .expect_err("unreachable PDP is an error");
    assert!(
        matches!(
            err,
            DomainError::ServiceUnavailable {
                retry_after_secs: Some(5),
                ..
            }
        ),
        "PDP outage keeps the fail-closed 503 + Retry-After: 5, got {err:?}"
    );
}

// ── chat-scoped authorization (PEP -> AccessScope) ───────────────────────────

#[tokio::test]
async fn chat_scope_asks_the_pdp_for_constraints_on_the_chat_resource() {
    let pdp = FakePdp::allow();
    let chat_id = Uuid::new_v4();

    chat_scope(&pdp.enforcer(), &ctx_a(), actions::READ, Some(chat_id))
        .await
        .expect("allowed");

    let req = pdp.last();
    assert_eq!(req.resource.resource_type, CHAT_RESOURCE_TYPE);
    assert_eq!(req.action.name, "read");
    assert_eq!(req.resource.id, Some(chat_id));
    assert!(
        req.context.require_constraints,
        "chat scopes need constraints"
    );
}

#[tokio::test]
async fn chat_scope_declares_supported_properties_and_no_capabilities() {
    let pdp = FakePdp::allow();
    chat_scope(&pdp.enforcer(), &ctx_a(), actions::LIST, None)
        .await
        .expect("allowed");

    let req = pdp.last();
    let mut props = req.context.supported_properties.clone();
    props.sort();
    let mut want = vec![
        pep_properties::OWNER_TENANT_ID.to_owned(),
        pep_properties::OWNER_ID.to_owned(),
        pep_properties::RESOURCE_ID.to_owned(),
    ];
    want.sort();
    assert_eq!(
        props, want,
        "supported properties: owner_tenant_id, owner_id, id (DESIGN 3.8)"
    );
    assert!(
        req.context.capabilities.is_empty(),
        "P1 declares no tenant_hierarchy / group capabilities"
    );
}

#[tokio::test]
async fn chat_create_passes_owner_properties_from_the_security_context() {
    let pdp = FakePdp::allow();
    chat_scope(&pdp.enforcer(), &ctx_a(), actions::CREATE, None)
        .await
        .expect("allowed");

    let req = pdp.last();
    assert!(req.resource.id.is_none(), "create has no resource.id");
    assert_eq!(
        req.resource.properties[pep_properties::OWNER_TENANT_ID],
        serde_json::json!(TENANT_A.to_string())
    );
    assert_eq!(
        req.resource.properties[pep_properties::OWNER_ID],
        serde_json::json!(USER_A.to_string())
    );
}

#[tokio::test]
async fn chat_scope_adds_the_owner_predicate_even_if_the_pdp_returned_only_a_tenant() {
    // The fake PDP answers with `eq(owner_tenant_id)` only.
    let pdp = FakePdp::allow();
    let scope = chat_scope(
        &pdp.enforcer(),
        &ctx_a(),
        actions::READ,
        Some(Uuid::new_v4()),
    )
    .await
    .expect("allowed");

    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_TENANT_ID),
        vec![TENANT_A]
    );
    assert_eq!(
        scope.all_uuid_values_for(pep_properties::OWNER_ID),
        vec![USER_A],
        "defence in depth: every chat-scoped operation filters on the subject (DESIGN 3.8)"
    );
    assert!(!scope.is_unconstrained());
    assert!(!scope.is_deny_all());
}

#[tokio::test]
async fn chat_scope_denial_is_access_denied() {
    let pdp = FakePdp::deny("nope");
    let err = chat_scope(
        &pdp.enforcer(),
        &ctx_a(),
        actions::DELETE,
        Some(Uuid::new_v4()),
    )
    .await
    .expect_err("denied");
    assert!(matches!(err, DomainError::AccessDenied { .. }), "{err:?}");
}

#[tokio::test]
async fn chat_scope_without_constraints_is_access_denied() {
    // `decision: true` + no constraints + `require_constraints: true` -> 403.
    let pdp = FakePdp::new(PdpMode::AllowWithoutConstraints);
    let err = chat_scope(&pdp.enforcer(), &ctx_a(), actions::LIST, None)
        .await
        .expect_err("constraints required");
    assert!(matches!(err, DomainError::AccessDenied { .. }), "{err:?}");
}

#[tokio::test]
async fn chat_scope_pdp_outage_is_service_unavailable() {
    let pdp = FakePdp::unreachable("down");
    let err = chat_scope(&pdp.enforcer(), &ctx_a(), actions::LIST, None)
        .await
        .expect_err("PDP unreachable");
    assert!(
        matches!(
            err,
            DomainError::ServiceUnavailable {
                retry_after_secs: Some(5),
                ..
            }
        ),
        "{err:?}"
    );
}

// ── enforcer error classification ────────────────────────────────────────────

#[test]
fn map_enforcer_err_follows_the_fail_closed_table() {
    // decision:false (with or without details) -> 403
    assert!(matches!(
        map_enforcer_err(EnforcerError::Denied { deny_reason: None }),
        DomainError::AccessDenied { .. }
    ));
    // constraint compile failure -> 403
    assert!(matches!(
        map_enforcer_err(EnforcerError::CompileFailed(
            ConstraintCompileError::ConstraintsRequiredButAbsent
        )),
        DomainError::AccessDenied { .. }
    ));
    assert!(matches!(
        map_enforcer_err(EnforcerError::CompileFailed(
            ConstraintCompileError::AllConstraintsFailed {
                reason: "unknown predicate".to_owned()
            }
        )),
        DomainError::AccessDenied { .. }
    ));
    // PDP unreachable / timeout / evaluation failure -> 503, Retry-After 5
    let outage = EnforcerError::EvaluationFailed(
        CanonicalError::service_unavailable()
            .with_detail("down".to_owned())
            .create(),
    );
    assert!(matches!(
        map_enforcer_err(outage),
        DomainError::ServiceUnavailable {
            retry_after_secs: Some(5),
            ..
        }
    ));
}
