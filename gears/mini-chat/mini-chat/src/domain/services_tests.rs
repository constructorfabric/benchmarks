//! Models service tests: admission first, then the snapshot, then visibility
//! (flows `list-models`, `get-model` and `request-admission`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use mini_chat_sdk::ModelTier;
use serde_json::json;

use super::{ModelsService, Services};
use crate::domain::authz::PepService;
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::model_policy::ModelPolicyGateway;
use crate::test_support::{
    FakeGateway, Pdp, RecordingPdp, SUBJECT_USER_TYPE, TENANT_A, USER_A1, ctx_with, entry, snapshot,
};
use authz_resolver_sdk::PolicyEnforcer;

fn caller() -> toolkit_security::SecurityContext {
    ctx_with(USER_A1, TENANT_A, Some(SUBJECT_USER_TYPE), &["*"])
}

fn catalog() -> mini_chat_sdk::PolicySnapshot {
    snapshot(vec![
        entry("alpha", ModelTier::Premium, true, &json!({})),
        entry("hidden", ModelTier::Standard, false, &json!({})),
        entry("beta", ModelTier::Standard, true, &json!({})),
    ])
}

fn service(
    behavior: Pdp,
    gateway: Arc<FakeGateway>,
) -> (ModelsService, Arc<RecordingPdp>, Arc<FakeGateway>) {
    let pdp = RecordingPdp::new(behavior);
    let pep = Arc::new(PepService::new(PolicyEnforcer::new(pdp.clone())));
    let gw: Arc<dyn ModelPolicyGateway> = gateway.clone();
    (ModelsService::new(pep, gw), pdp, gateway)
}

#[tokio::test]
async fn list_returns_the_enabled_models_in_catalog_order() {
    let (svc, _, _) = service(Pdp::NoConstraints, FakeGateway::serving(catalog()));
    let models = svc.list(&caller()).await.unwrap();
    let ids: Vec<&str> = models.iter().map(|m| m.model_id.as_str()).collect();
    assert_eq!(ids, vec!["alpha", "beta"]);
}

#[tokio::test]
async fn list_of_an_empty_catalog_is_empty() {
    let (svc, _, _) = service(Pdp::NoConstraints, FakeGateway::serving(snapshot(vec![])));
    assert!(svc.list(&caller()).await.unwrap().is_empty());
}

#[tokio::test]
async fn list_authorizes_decision_only_on_the_model_resource() {
    let (svc, pdp, _) = service(Pdp::NoConstraints, FakeGateway::serving(catalog()));
    svc.list(&caller()).await.unwrap();
    let request = pdp.only_request();
    assert_eq!(request.action.name, "list");
    assert_eq!(
        request.resource.resource_type,
        "gts.cf.core.ai_chat.model.v1~cf.core.mini_chat.model.v1~"
    );
    assert!(!request.context.require_constraints);
    assert_eq!(request.resource.id, None);
}

#[tokio::test]
async fn get_authorizes_with_read_and_no_resource_id() {
    let (svc, pdp, _) = service(Pdp::NoConstraints, FakeGateway::serving(catalog()));
    svc.get(&caller(), "alpha").await.unwrap();
    let request = pdp.only_request();
    assert_eq!(request.action.name, "read");
    assert_eq!(request.resource.id, None);
    assert!(!request.context.require_constraints);
}

#[tokio::test]
async fn a_refused_admission_does_not_read_the_catalog() {
    for behavior in [Pdp::Deny, Pdp::Unreachable] {
        let (svc, _, gateway) = service(behavior, FakeGateway::serving(catalog()));
        let err = svc.list(&caller()).await.unwrap_err();
        assert!(
            matches!(
                err,
                DomainError::AuthzDenied | DomainError::AuthzUnavailable { .. }
            ),
            "{behavior:?}: {err:?}"
        );
        assert_eq!(gateway.call_count(), 0, "{behavior:?}");

        let (svc, _, gateway) = service(behavior, FakeGateway::serving(catalog()));
        let err = svc.get(&caller(), "alpha").await.unwrap_err();
        assert!(
            matches!(
                err,
                DomainError::AuthzDenied | DomainError::AuthzUnavailable { .. }
            ),
            "{behavior:?}: {err:?}"
        );
        assert_eq!(gateway.call_count(), 0, "{behavior:?}");
    }
}

#[tokio::test]
async fn a_deny_and_a_pdp_failure_keep_their_distinct_errors() {
    let (svc, _, _) = service(Pdp::Deny, FakeGateway::serving(catalog()));
    assert!(matches!(
        svc.list(&caller()).await.unwrap_err(),
        DomainError::AuthzDenied
    ));
    let (svc, _, _) = service(Pdp::Unreachable, FakeGateway::serving(catalog()));
    assert!(matches!(
        svc.list(&caller()).await.unwrap_err(),
        DomainError::AuthzUnavailable { .. }
    ));
}

#[tokio::test]
async fn a_plugin_failure_is_reported_as_policy_unavailable() {
    let (svc, _, _) = service(Pdp::NoConstraints, FakeGateway::failing());
    assert!(matches!(
        svc.list(&caller()).await.unwrap_err(),
        DomainError::PolicyUnavailable { .. }
    ));
    assert!(matches!(
        svc.get(&caller(), "alpha").await.unwrap_err(),
        DomainError::PolicyUnavailable { .. }
    ));
}

#[tokio::test]
async fn get_returns_the_projection_of_an_enabled_model() {
    let (svc, _, _) = service(Pdp::NoConstraints, FakeGateway::serving(catalog()));
    let model = svc.get(&caller(), "beta").await.unwrap();
    assert_eq!(model.model_id, "beta");
    assert_eq!(model.tier, ModelTier::Standard);
}

#[tokio::test]
async fn get_of_a_disabled_or_unknown_model_is_not_found_with_the_model_resource() {
    for id in ["hidden", "ghost"] {
        let (svc, _, _) = service(Pdp::NoConstraints, FakeGateway::serving(catalog()));
        match svc.get(&caller(), id).await {
            Err(DomainError::NotFound { resource, .. }) => {
                assert_eq!(resource, ResourceKind::Model, "{id}");
            }
            other => panic!("`{id}` must be not found, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn every_request_asks_the_plugin_again() {
    let (svc, _, gateway) = service(Pdp::NoConstraints, FakeGateway::serving(catalog()));
    svc.list(&caller()).await.unwrap();
    svc.list(&caller()).await.unwrap();
    svc.get(&caller(), "alpha").await.unwrap();
    assert_eq!(gateway.call_count(), 3, "no local snapshot cache");
}

#[tokio::test]
async fn services_share_one_pep_with_the_models_service() {
    let pdp = RecordingPdp::new(Pdp::NoConstraints);
    let pep = Arc::new(PepService::new(PolicyEnforcer::new(pdp.clone())));
    let gateway: Arc<dyn ModelPolicyGateway> = FakeGateway::serving(catalog());
    let services = Services::new(Arc::clone(&pep), gateway);
    assert!(Arc::ptr_eq(&services.pep, &pep));
    services.models.list(&caller()).await.unwrap();
    assert_eq!(pdp.requests().len(), 1);
}
