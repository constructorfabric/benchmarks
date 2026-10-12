//! Shared fakes and helpers for the mini-chat integration tests (S1).
//!
//! Every item here depends only on the public contracts listed in
//! `S1-test-interface.md`: the SDK crate, `authz_resolver_sdk` and the toolkit.
//! Each test binary uses a subset, hence the `dead_code` allowance.

#![allow(
    dead_code,
    unused_imports,
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

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use authz_resolver_sdk::constraints::{Constraint, EqPredicate, Predicate};
use authz_resolver_sdk::models::{
    DenyReason, EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use authz_resolver_sdk::{AuthZResolverApi, PolicyEnforcer};
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, ModelCatalogEntry, ModelPolicyPluginError,
    PolicySnapshot, PolicyVersionInfo, PublishError, UsageEvent, UserLimits,
};
use serde_json::{Value, json};
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_security::{PlatformSecurityContext, SecurityContext, pep_properties};
use tower::ServiceExt;
use uuid::Uuid;

// ── identities ───────────────────────────────────────────────────────────────

pub const USER_A: Uuid = Uuid::from_u128(0x1111_1111_6a88_4768_9dfc_6bcd_5187_d9ed);
pub const TENANT_A: Uuid = Uuid::from_u128(0x0000_0000_df51_5b42_9538_d2b5_6b7e_e953);
pub const USER_B: Uuid = Uuid::from_u128(0x2222_2222_6a88_4768_9dfc_6bcd_5187_d9ed);
pub const TENANT_B: Uuid = Uuid::from_u128(0xbbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb_bbbb);

pub fn ctx(subject: Uuid, tenant: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(subject)
        .subject_tenant_id(tenant)
        .build()
        .expect("test security context")
}

pub fn ctx_a() -> SecurityContext {
    ctx(USER_A, TENANT_A)
}

// ── fake PDP ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub enum PdpMode {
    /// `decision: true`; resources declaring `owner_tenant_id` get one
    /// `eq(owner_tenant_id, <caller tenant>)` constraint (like the static PDP).
    Allow,
    /// `decision: true` with no constraints at all.
    AllowWithoutConstraints,
    /// `decision: false` with the given `deny_reason.details`.
    Deny { details: Option<String> },
    /// The evaluation call itself fails (PDP unreachable / timeout).
    Unreachable { message: String },
}

/// Recording PDP fake. `requests` holds every `EvaluationRequest` the PEP sent.
pub struct FakePdp {
    mode: PdpMode,
    pub requests: Mutex<Vec<EvaluationRequest>>,
}

impl FakePdp {
    pub fn new(mode: PdpMode) -> Arc<Self> {
        Arc::new(Self {
            mode,
            requests: Mutex::new(Vec::new()),
        })
    }

    pub fn allow() -> Arc<Self> {
        Self::new(PdpMode::Allow)
    }

    pub fn deny(details: &str) -> Arc<Self> {
        Self::new(PdpMode::Deny {
            details: Some(details.to_owned()),
        })
    }

    pub fn unreachable(message: &str) -> Arc<Self> {
        Self::new(PdpMode::Unreachable {
            message: message.to_owned(),
        })
    }

    pub fn enforcer(self: &Arc<Self>) -> PolicyEnforcer {
        PolicyEnforcer::new(self.clone())
    }

    pub fn recorded(&self) -> Vec<EvaluationRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn last(&self) -> EvaluationRequest {
        self.recorded()
            .last()
            .cloned()
            .expect("the PDP was never called")
    }
}

#[async_trait]
impl AuthZResolverApi for FakePdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        let tenant = request
            .subject
            .properties
            .get("tenant_id")
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok());
        let declares_owner_tenant = request
            .context
            .supported_properties
            .iter()
            .any(|p| p == pep_properties::OWNER_TENANT_ID);
        self.requests.lock().unwrap().push(request);

        match &self.mode {
            // Like the platform's static PDP: a tenant clamp is emitted only for a
            // resource that declares `owner_tenant_id` in `supported_properties`
            // (an undeclared property would fail constraint compilation), so a
            // permission-only resource gets a plain `decision: true`.
            PdpMode::Allow if !declares_owner_tenant => Ok(EvaluationResponse {
                decision: true,
                context: EvaluationResponseContext::default(),
            }),
            PdpMode::Allow => {
                let tenant = tenant.expect("subject.properties[\"tenant_id\"] is always sent");
                Ok(EvaluationResponse {
                    decision: true,
                    context: EvaluationResponseContext {
                        constraints: vec![Constraint {
                            predicates: vec![Predicate::Eq(EqPredicate::new(
                                pep_properties::OWNER_TENANT_ID,
                                tenant,
                            ))],
                        }],
                        ..Default::default()
                    },
                })
            }
            PdpMode::AllowWithoutConstraints => Ok(EvaluationResponse {
                decision: true,
                context: EvaluationResponseContext::default(),
            }),
            PdpMode::Deny { details } => Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext {
                    deny_reason: Some(DenyReason {
                        error_code: "NO_GRANT".to_owned(),
                        details: details.clone(),
                    }),
                    ..Default::default()
                },
            }),
            PdpMode::Unreachable { message } => Err(CanonicalError::service_unavailable()
                .with_detail(message.clone())
                .create()),
        }
    }
}

// ── catalog / snapshot builders ──────────────────────────────────────────────

/// Provider-side marker strings that must never appear on the public wire.
pub const SECRET_PROVIDER_ID: &str = "secret-provider-key";
pub const SECRET_PROVIDER_MODEL_PREFIX: &str = "secret-deployment-";

pub fn catalog_entry_json(id: &str, tier: &str, enabled: bool) -> Value {
    let premium = tier.eq_ignore_ascii_case("premium");
    let multiplier_display = if premium { "3x" } else { "1x" };
    json!({
        "id": id,
        "provider_model_id": format!("{SECRET_PROVIDER_MODEL_PREFIX}{id}"),
        "display_name": format!("Display {id}"),
        "description": format!("Description of {id}"),
        "provider_id": SECRET_PROVIDER_ID,
        "provider_display_name": "Secret Provider Inc.",
        "icon": "",
        "tier": tier,
        "enabled": enabled,
        "system_prompt": "SECRET-SYSTEM-PROMPT",
        "thread_summary_prompt": "",
        "multimodal_capabilities": ["VISION_INPUT", "RAG"],
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "max_input_tokens": 120_000,
        "input_tokens_credit_multiplier_micro": 7_654_321,
        "output_tokens_credit_multiplier_micro": 8_765_432,
        "multiplier_display": multiplier_display,
        "max_num_results": 5,
        "max_tool_calls": 2,
        "general_config": {
            "type": "model.general.v1",
            "available_from": "1970-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": {"temperature": 0.7, "top_p": 1.0, "frequency_penalty": 0.0,
                           "presence_penalty": 0.0, "stop": []},
            "features": {"streaming": true, "structured_output": false},
            "tool_support": {"web_search": true, "file_search": true,
                             "image_generation": false, "code_interpreter": true, "mcp": false},
            "supported_endpoints": {"chat_completions": false, "responses": true,
                                    "embeddings": false, "image_generation": false,
                                    "audio_speech_generation": false,
                                    "audio_transcription": false, "audio_translation": false}
        },
        "preference": {"is_default": premium, "sort_order": 0}
    })
}

pub fn catalog_entry(id: &str, tier: &str, enabled: bool) -> ModelCatalogEntry {
    serde_json::from_value(catalog_entry_json(id, tier, enabled)).expect("catalog entry")
}

pub fn kill_switches_off() -> KillSwitches {
    serde_json::from_value(json!({
        "disable_premium_tier": false,
        "force_standard_tier": false,
        "disable_web_search": false,
        "disable_file_search": false,
        "disable_images": false,
        "disable_code_interpreter": false,
    }))
    .expect("kill switches")
}

pub fn snapshot_of(entries: Vec<ModelCatalogEntry>) -> PolicySnapshot {
    serde_json::from_value(json!({
        "policy_version": 1,
        "model_catalog": entries,
        "kill_switches": kill_switches_off(),
    }))
    .expect("policy snapshot")
}

// ── fake model-policy plugin ─────────────────────────────────────────────────

/// Model-policy plugin fake with call counters; the catalog can change between
/// requests (`set_catalog`) and the whole plugin can be made to fail.
pub struct FakePolicyPlugin {
    snapshot: Mutex<PolicySnapshot>,
    pub version_calls: AtomicUsize,
    pub snapshot_calls: AtomicUsize,
    pub limits_calls: AtomicUsize,
    pub seen_users: Mutex<Vec<Uuid>>,
    fail_with: Mutex<Option<String>>,
}

impl FakePolicyPlugin {
    pub fn new(entries: Vec<ModelCatalogEntry>) -> Arc<Self> {
        Arc::new(Self {
            snapshot: Mutex::new(snapshot_of(entries)),
            version_calls: AtomicUsize::new(0),
            snapshot_calls: AtomicUsize::new(0),
            limits_calls: AtomicUsize::new(0),
            seen_users: Mutex::new(Vec::new()),
            fail_with: Mutex::new(None),
        })
    }

    pub fn set_catalog(&self, entries: Vec<ModelCatalogEntry>) {
        self.snapshot.lock().unwrap().model_catalog = entries;
    }

    pub fn fail_with(&self, message: &str) {
        *self.fail_with.lock().unwrap() = Some(message.to_owned());
    }

    pub fn total_calls(&self) -> usize {
        self.version_calls.load(Ordering::SeqCst)
            + self.snapshot_calls.load(Ordering::SeqCst)
            + self.limits_calls.load(Ordering::SeqCst)
    }

    fn check(&self, user_id: Uuid) -> Result<(), ModelPolicyPluginError> {
        self.seen_users.lock().unwrap().push(user_id);
        match self.fail_with.lock().unwrap().clone() {
            Some(message) => Err(ModelPolicyPluginError::Internal(message)),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for FakePolicyPlugin {
    async fn get_current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, ModelPolicyPluginError> {
        self.version_calls.fetch_add(1, Ordering::SeqCst);
        self.check(user_id)?;
        let version = self.snapshot.lock().unwrap().policy_version;
        Ok(serde_json::from_value(json!({
            "policy_version": version,
            "generated_at": "2026-02-27T13:00:00Z",
        }))
        .expect("policy version info"))
    }

    async fn get_policy_snapshot(
        &self,
        user_id: Uuid,
        _policy_version: u64,
    ) -> Result<PolicySnapshot, ModelPolicyPluginError> {
        self.snapshot_calls.fetch_add(1, Ordering::SeqCst);
        self.check(user_id)?;
        Ok(self.snapshot.lock().unwrap().clone())
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, ModelPolicyPluginError> {
        self.limits_calls.fetch_add(1, Ordering::SeqCst);
        self.check(user_id)?;
        Ok(serde_json::from_value(json!({
            "user_id": user_id,
            "policy_version": policy_version,
            "standard": {"limit_daily_credits_micro": 100, "limit_monthly_credits_micro": 1000},
            "premium": {"limit_daily_credits_micro": 50, "limit_monthly_credits_micro": 500},
        }))
        .expect("user limits"))
    }

    async fn publish_usage(&self, _payload: UsageEvent) -> Result<(), PublishError> {
        Ok(())
    }
}

// ── HTTP helpers ─────────────────────────────────────────────────────────────

pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub raw: String,
}

impl Resp {
    /// Parsed JSON body (`Value::Null` when the body is empty / not JSON).
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.raw).unwrap_or(Value::Null)
    }

    pub fn content_type(&self) -> String {
        self.headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }
}

/// Send one request through `router`, injecting `ctx` as the request's
/// `SecurityContext` extension (what the api-gateway auth middleware does).
pub async fn send(router: &Router, method: &str, uri: &str, ctx: Option<&SecurityContext>) -> Resp {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    if let Some(ctx) = ctx {
        req.extensions_mut().insert(ctx.clone());
    }
    let resp = router
        .clone()
        .oneshot(req)
        .await
        .expect("router is infallible");
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .expect("body");
    Resp {
        status,
        headers,
        raw: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

pub async fn get(router: &Router, uri: &str, ctx: &SecurityContext) -> Resp {
    send(router, "GET", uri, Some(ctx)).await
}

/// Assert the RFC 9457 envelope of ADR-0004: `type`, `title`, `status`,
/// `detail`, `context` present, no top-level `code`, `application/problem+json`.
pub fn assert_problem(resp: &Resp, status: u16, category: &str) -> Value {
    assert_eq!(
        resp.status.as_u16(),
        status,
        "unexpected status, body: {}",
        resp.raw
    );
    assert!(
        resp.content_type().starts_with("application/problem+json"),
        "error responses are application/problem+json, got `{}`",
        resp.content_type()
    );
    let body = resp.json();
    let obj = body.as_object().expect("problem body is a JSON object");
    for key in ["type", "title", "status", "detail", "context"] {
        assert!(obj.contains_key(key), "problem is missing `{key}`: {body}");
    }
    assert!(
        !obj.contains_key("code"),
        "ADR-0004: there is no top-level `code` field: {body}"
    );
    assert_eq!(obj["status"], json!(status));
    assert!(
        obj["type"].as_str().unwrap_or_default().contains(category),
        "problem type `{}` should name the `{category}` category",
        obj["type"]
    );
    body
}
