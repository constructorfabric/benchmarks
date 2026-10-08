#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::Arc;

use super::ModelService;
use crate::api::rest::dto::ModelDto;
use crate::domain::error::{DomainError, ResourceKind};
use crate::test_support::{FakeAuthz, FakePolicy, catalog_entry, snapshot, test_ctx};

/// Catalog `c` (enabled), `a` (disabled), `b` (enabled).
fn service_with(authz: Arc<FakeAuthz>) -> (ModelService, Arc<FakePolicy>) {
    let policy = Arc::new(FakePolicy::new(snapshot(vec![
        catalog_entry("c", true),
        catalog_entry("a", false),
        catalog_entry("b", true),
    ])));
    (ModelService::new(authz, policy.clone()), policy)
}

#[tokio::test]
async fn list_returns_only_enabled_in_catalog_order() {
    let authz = Arc::new(FakeAuthz::default());
    let (svc, _) = service_with(authz.clone());

    let models = svc.list(&test_ctx()).await.unwrap();

    let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["c", "b"]);
    assert_eq!(authz.model_actions(), ["list"]);
}

#[tokio::test]
async fn get_disabled_is_not_found_model() {
    let authz = Arc::new(FakeAuthz::default());
    let (svc, _) = service_with(authz.clone());
    let ctx = test_ctx();

    let not_found = DomainError::NotFound {
        resource: ResourceKind::Model,
    };
    assert_eq!(svc.get(&ctx, "a").await.unwrap_err(), not_found);
    assert_eq!(svc.get(&ctx, "missing").await.unwrap_err(), not_found);
    assert_eq!(svc.get(&ctx, "b").await.unwrap().id, "b");
    assert_eq!(authz.model_actions(), ["read", "read", "read"]);
}

#[tokio::test]
async fn denied_caller_never_reads_the_catalog() {
    let (svc, policy) = service_with(Arc::new(FakeAuthz::denying()));
    let ctx = test_ctx();

    assert_eq!(svc.list(&ctx).await.unwrap_err(), DomainError::AuthzDenied);
    assert_eq!(
        svc.get(&ctx, "b").await.unwrap_err(),
        DomainError::AuthzDenied
    );
    assert_eq!(policy.current_calls(), 0);
}

#[tokio::test]
async fn resolve_for_chat_respects_enabled_only() {
    let (svc, _) = service_with(Arc::new(FakeAuthz::default()));
    let user = uuid::Uuid::new_v4();

    let (snap, entry) = svc.resolve_for_chat(user, "b", true).await.unwrap();
    assert_eq!(entry.id, "b");
    assert_eq!(snap.policy_version, 1);

    assert_eq!(
        svc.resolve_for_chat(user, "a", true).await.unwrap_err(),
        DomainError::InvalidModel
    );
    let (_, disabled) = svc.resolve_for_chat(user, "a", false).await.unwrap();
    assert_eq!(disabled.id, "a");
    for enabled_only in [true, false] {
        assert_eq!(
            svc.resolve_for_chat(user, "missing", enabled_only)
                .await
                .unwrap_err(),
            DomainError::InvalidModel
        );
    }
}

#[test]
fn dto_hides_internal_fields() {
    let dto = ModelDto::from(&catalog_entry("gpt-x", true));
    let json = serde_json::to_value(&dto).unwrap();

    let keys: BTreeSet<_> = json.as_object().unwrap().keys().cloned().collect();
    let expected: BTreeSet<_> = [
        "model_id",
        "display_name",
        "tier",
        "multiplier_display",
        "description",
        "multimodal_capabilities",
        "context_window",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(keys, expected);
    assert_eq!(
        json,
        serde_json::json!({
            "model_id": "gpt-x",
            "display_name": "Model gpt-x",
            "tier": "premium",
            "multiplier_display": "2x",
            "description": "About gpt-x",
            "multimodal_capabilities": ["VISION_INPUT", "RAG"],
            "context_window": 128_000
        })
    );
    let text = json.to_string();
    for leaked in ["secret-provider", "prov-model-", "1500000", "is_default"] {
        assert!(!text.contains(leaked), "{leaked} leaked: {text}");
    }
}

#[test]
fn dto_omits_empty_description() {
    let mut entry = catalog_entry("m", true);
    entry.description = String::new();
    let json = serde_json::to_value(ModelDto::from(&entry)).unwrap();
    assert!(json.get("description").is_none(), "{json}");
}
