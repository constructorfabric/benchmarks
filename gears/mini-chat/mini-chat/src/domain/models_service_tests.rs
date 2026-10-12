#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverApi;
use authz_resolver_sdk::models::{
    EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError,
    ModelCatalogEntry, PolicySnapshot, PublishError, UsageEvent, UserLimits,
};
use serde_json::json;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::{PlatformSecurityContext, SecurityContext};
use uuid::Uuid;

use super::*;
use crate::domain::authz::AuthzService;
use crate::domain::error::{DomainError, InvalidArgumentReason, ResourceKind};

// ── Shared fixtures (also used by the REST route tests) ──────────────────

/// Catalog entry `id` with every required key; `extra` overrides or adds keys.
pub fn catalog_entry(id: &str, extra: serde_json::Value) -> ModelCatalogEntry {
    let mut entry = json!({
        "id": id,
        "provider_model_id": format!("provider-{id}"),
        "display_name": format!("Model {id}"),
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": "standard",
        "enabled": true,
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "multiplier_display": "1x",
    });
    let object = entry.as_object_mut().unwrap();
    let serde_json::Value::Object(extra) = extra else {
        panic!("extra must be an object");
    };
    object.extend(extra);
    serde_json::from_value(entry).expect("valid catalog entry")
}

pub fn snapshot(version: u64, catalog: Vec<ModelCatalogEntry>) -> PolicySnapshot {
    PolicySnapshot {
        policy_version: version,
        model_catalog: catalog,
        kill_switches: KillSwitches::default(),
    }
}

/// Fake model-policy plugin serving a replaceable snapshot; counts reads.
pub struct FakePolicy {
    version: AtomicU64,
    catalog: Mutex<Vec<ModelCatalogEntry>>,
    fail: Option<MiniChatModelPolicyPluginError>,
    snapshot_reads: AtomicUsize,
}

impl FakePolicy {
    pub fn new(catalog: Vec<ModelCatalogEntry>) -> Arc<Self> {
        Arc::new(Self {
            version: AtomicU64::new(1),
            catalog: Mutex::new(catalog),
            fail: None,
            snapshot_reads: AtomicUsize::new(0),
        })
    }

    pub fn failing(err: MiniChatModelPolicyPluginError) -> Arc<Self> {
        Arc::new(Self {
            version: AtomicU64::new(1),
            catalog: Mutex::new(Vec::new()),
            fail: Some(err),
            snapshot_reads: AtomicUsize::new(0),
        })
    }

    /// Publishes a new policy version with `catalog`.
    pub fn publish(&self, catalog: Vec<ModelCatalogEntry>) {
        *self.catalog.lock().unwrap() = catalog;
        self.version.fetch_add(1, Ordering::SeqCst);
    }

    pub fn snapshot_reads(&self) -> usize {
        self.snapshot_reads.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for FakePolicy {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<u64, MiniChatModelPolicyPluginError> {
        match &self.fail {
            Some(err) => Err(err.clone()),
            None => Ok(self.version.load(Ordering::SeqCst)),
        }
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        self.snapshot_reads.fetch_add(1, Ordering::SeqCst);
        if policy_version != self.version.load(Ordering::SeqCst) {
            return Err(MiniChatModelPolicyPluginError::VersionNotFound { policy_version });
        }
        Ok(snapshot(
            policy_version,
            self.catalog.lock().unwrap().clone(),
        ))
    }

    async fn get_user_limits(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        Err(MiniChatModelPolicyPluginError::VersionNotFound { policy_version })
    }

    async fn publish_usage(&self, _payload: UsageEvent) -> Result<(), PublishError> {
        Ok(())
    }
}

/// PDP fake allowing every request without constraints (or denying all).
pub struct DecisionPdp {
    pub allow: bool,
}

#[async_trait]
impl AuthZResolverApi for DecisionPdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: self.allow,
            context: EvaluationResponseContext::default(),
        })
    }
}

pub fn allow_all_authz() -> Arc<AuthzService> {
    Arc::new(AuthzService::from_client(Arc::new(DecisionPdp {
        allow: true,
    })))
}

pub fn deny_all_authz() -> Arc<AuthzService> {
    Arc::new(AuthzService::from_client(Arc::new(DecisionPdp {
        allow: false,
    })))
}

pub fn test_ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(0xa11c))
        .subject_tenant_id(Uuid::from_u128(0x7e7a))
        .build()
        .unwrap()
}

/// Catalog: `premium` (enabled), `hidden` (disabled), `standard` (enabled,
/// no description), `default` (enabled, `is_default`).
pub fn sample_catalog() -> Vec<ModelCatalogEntry> {
    vec![
        catalog_entry(
            "premium",
            json!({
                "tier": "Premium",
                "description": "Best for complex reasoning",
                "multiplier_display": "3x",
                "multimodal_capabilities": ["VISION_INPUT", "RAG"],
            }),
        ),
        catalog_entry("hidden", json!({ "enabled": false })),
        catalog_entry("standard", json!({})),
        catalog_entry(
            "default",
            json!({ "preference": { "is_default": true, "sort_order": 1 } }),
        ),
    ]
}

fn service(policy: &Arc<FakePolicy>) -> ModelsService {
    ModelsService::new(
        Arc::clone(policy) as Arc<dyn MiniChatModelPolicyPluginClientV1>,
        allow_all_authz(),
    )
}

fn ids(entries: &[ModelCatalogEntry]) -> Vec<&str> {
    entries.iter().map(|e| e.id.as_str()).collect()
}

fn is_invalid_model(err: &DomainError) -> bool {
    matches!(
        err,
        DomainError::InvalidArgument {
            field: "model",
            reason: InvalidArgumentReason::InvalidModel
        }
    )
}

// ── list_visible / get_visible ───────────────────────────────────────────

#[tokio::test]
async fn list_returns_enabled_models_in_catalog_order() {
    let policy = FakePolicy::new(sample_catalog());

    let models = service(&policy).list_visible(&test_ctx()).await.unwrap();

    assert_eq!(ids(&models), ["premium", "standard", "default"]);
}

#[tokio::test]
async fn list_reads_the_snapshot_on_every_call() {
    let policy = FakePolicy::new(sample_catalog());
    let svc = service(&policy);

    svc.list_visible(&test_ctx()).await.unwrap();
    policy.publish(vec![catalog_entry("next", json!({}))]);
    let models = svc.list_visible(&test_ctx()).await.unwrap();

    assert_eq!(ids(&models), ["next"]);
    assert_eq!(policy.snapshot_reads(), 2);
}

#[tokio::test]
async fn list_is_denied_when_the_pdp_denies() {
    let policy = FakePolicy::new(sample_catalog());
    let svc = ModelsService::new(
        Arc::clone(&policy) as Arc<dyn MiniChatModelPolicyPluginClientV1>,
        deny_all_authz(),
    );

    let err = svc.list_visible(&test_ctx()).await.unwrap_err();

    assert!(matches!(
        err,
        DomainError::PermissionDenied {
            resource: ResourceKind::Model
        }
    ));
    assert_eq!(
        policy.snapshot_reads(),
        0,
        "authorization precedes the read"
    );
}

#[tokio::test]
async fn get_returns_an_enabled_model() {
    let policy = FakePolicy::new(sample_catalog());

    let model = service(&policy)
        .get_visible(&test_ctx(), "premium")
        .await
        .unwrap();

    assert_eq!(model.id, "premium");
}

#[tokio::test]
async fn get_disabled_or_unknown_model_is_not_found_on_the_model_resource() {
    let policy = FakePolicy::new(sample_catalog());
    let svc = service(&policy);

    for id in ["hidden", "missing"] {
        let err = svc.get_visible(&test_ctx(), id).await.unwrap_err();
        match err {
            DomainError::NotFound {
                resource: ResourceKind::Model,
                id: reported,
            } => assert_eq!(reported, id),
            other => panic!("expected model not_found for {id}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn get_is_denied_when_the_pdp_denies() {
    let policy = FakePolicy::new(sample_catalog());
    let svc = ModelsService::new(
        Arc::clone(&policy) as Arc<dyn MiniChatModelPolicyPluginClientV1>,
        deny_all_authz(),
    );

    let err = svc.get_visible(&test_ctx(), "premium").await.unwrap_err();

    assert!(matches!(err, DomainError::PermissionDenied { .. }));
}

// ── current_policy ───────────────────────────────────────────────────────

#[tokio::test]
async fn current_policy_returns_version_and_snapshot() {
    let policy = FakePolicy::new(sample_catalog());
    policy.publish(sample_catalog());

    let (version, snapshot) = service(&policy)
        .current_policy(Uuid::from_u128(1))
        .await
        .unwrap();

    assert_eq!(version, 2);
    assert_eq!(snapshot.policy_version, 2);
    assert_eq!(snapshot.model_catalog.len(), 4);
}

#[tokio::test]
async fn plugin_errors_are_internal() {
    for err in [
        MiniChatModelPolicyPluginError::Transient("source down".to_owned()),
        MiniChatModelPolicyPluginError::Internal("boom".to_owned()),
        MiniChatModelPolicyPluginError::UserNotFound {
            user_id: Uuid::from_u128(1),
        },
    ] {
        let policy = FakePolicy::failing(err);
        let svc = service(&policy);

        assert!(matches!(
            svc.current_policy(Uuid::from_u128(1)).await.unwrap_err(),
            DomainError::Internal(_)
        ));
        assert!(matches!(
            svc.list_visible(&test_ctx()).await.unwrap_err(),
            DomainError::Internal(_)
        ));
    }
}

// ── resolve_default_model ────────────────────────────────────────────────

#[test]
fn default_model_is_the_first_enabled_is_default_entry() {
    let snap = snapshot(1, sample_catalog());

    assert_eq!(resolve_default_model(&snap).unwrap().id, "default");
}

#[test]
fn default_model_ignores_disabled_is_default_entries() {
    let snap = snapshot(
        1,
        vec![
            catalog_entry(
                "off-default",
                json!({ "enabled": false, "preference": { "is_default": true } }),
            ),
            catalog_entry("first", json!({ "tier": "premium" })),
            catalog_entry("second", json!({})),
        ],
    );

    assert_eq!(resolve_default_model(&snap).unwrap().id, "first");
}

#[test]
fn default_model_falls_back_to_the_first_enabled_entry_regardless_of_tier() {
    let snap = snapshot(
        1,
        vec![
            catalog_entry("off", json!({ "enabled": false })),
            catalog_entry("premium-first", json!({ "tier": "premium" })),
            catalog_entry("standard", json!({ "preference": { "is_default": false } })),
        ],
    );

    assert_eq!(resolve_default_model(&snap).unwrap().id, "premium-first");
}

#[test]
fn default_model_is_none_without_enabled_entries() {
    let snap = snapshot(1, vec![catalog_entry("off", json!({ "enabled": false }))]);

    assert!(resolve_default_model(&snap).is_none());
}

// ── resolve_for_new_chat / resolve_chat_model ────────────────────────────

#[tokio::test]
async fn new_chat_without_request_uses_the_default_model() {
    let policy = FakePolicy::new(sample_catalog());

    let resolved = service(&policy)
        .resolve_for_new_chat(&test_ctx(), None)
        .await
        .unwrap();

    assert_eq!(resolved.model.id, "default");
    assert_eq!(resolved.policy_version, 1);
    assert_eq!(resolved.snapshot.policy_version, 1);
}

#[tokio::test]
async fn new_chat_with_an_enabled_model_uses_it() {
    let policy = FakePolicy::new(sample_catalog());

    let resolved = service(&policy)
        .resolve_for_new_chat(&test_ctx(), Some("premium"))
        .await
        .unwrap();

    assert_eq!(resolved.model.id, "premium");
}

#[tokio::test]
async fn new_chat_with_a_disabled_or_unknown_model_is_invalid_model() {
    let policy = FakePolicy::new(sample_catalog());
    let svc = service(&policy);

    for id in ["hidden", "missing", ""] {
        let err = svc
            .resolve_for_new_chat(&test_ctx(), Some(id))
            .await
            .unwrap_err();
        assert!(is_invalid_model(&err), "{id}: {err:?}");
    }
}

#[tokio::test]
async fn new_chat_without_enabled_models_is_invalid_model() {
    let policy = FakePolicy::new(vec![catalog_entry("off", json!({ "enabled": false }))]);

    let err = service(&policy)
        .resolve_for_new_chat(&test_ctx(), None)
        .await
        .unwrap_err();

    assert!(is_invalid_model(&err));
}

#[tokio::test]
async fn chat_model_resolution_ignores_enabled() {
    let policy = FakePolicy::new(sample_catalog());

    let resolved = service(&policy)
        .resolve_chat_model(Uuid::from_u128(1), "hidden")
        .await
        .unwrap();

    assert_eq!(resolved.model.id, "hidden");
    assert!(!resolved.model.enabled);
}

#[tokio::test]
async fn chat_model_missing_from_the_catalog_is_invalid_model() {
    let policy = FakePolicy::new(sample_catalog());

    let err = service(&policy)
        .resolve_chat_model(Uuid::from_u128(1), "retired")
        .await
        .unwrap_err();

    assert!(is_invalid_model(&err));
}

#[tokio::test]
async fn resolution_plugin_errors_are_internal() {
    let policy = FakePolicy::failing(MiniChatModelPolicyPluginError::Transient("x".to_owned()));
    let svc = service(&policy);

    assert!(matches!(
        svc.resolve_for_new_chat(&test_ctx(), None)
            .await
            .unwrap_err(),
        DomainError::Internal(_)
    ));
    assert!(matches!(
        svc.resolve_chat_model(Uuid::from_u128(1), "premium")
            .await
            .unwrap_err(),
        DomainError::Internal(_)
    ));
}
