//! Fakes for the app harness: model policy (`PolicyPort`), PDP
//! (`AuthZResolverApi`) and catalog fixtures.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use authz_resolver_sdk::{
    AuthZResolverApi, Constraint, EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
    InPredicate, Predicate,
};
use mini_chat_sdk::{
    AuditPluginError, KillSwitches, MiniChatAuditEvent, ModelCatalogEntry, ModelPreference,
    PolicySnapshot, PublishError, TierLimits, UsageEvent, UserLimits,
};
use serde_json::json;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::{PlatformSecurityContext, pep_properties};
use uuid::Uuid;

use mini_chat::domain::error::DomainError;
use mini_chat::domain::ports::{AuditPort, AuditResolution, PolicyPort};

// ---------------------------------------------------------------------------
// Catalog fixtures
// ---------------------------------------------------------------------------

fn base_entry(id: &str, tier: &str, caps: &[&str]) -> ModelCatalogEntry {
    serde_json::from_value(json!({
        "id": id,
        "provider_model_id": format!("{id}-provider-model"),
        "display_name": format!("Model {id}"),
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": tier,
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "max_input_tokens": 0,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "max_num_results": 5,
        "general_config": {
            "type": "chat",
            "available_from": "",
            "max_file_size_mb": 25,
            "api_params": {"stop": []},
            "features": {"streaming": true, "structured_output": false},
            "tool_support": {"web_search": true, "file_search": true, "image_generation": false,
                             "code_interpreter": true, "mcp": false},
            "supported_endpoints": {"chat_completions": false, "responses": true, "embeddings": false,
                                    "image_generation": false, "audio_speech_generation": false,
                                    "audio_transcription": false, "audio_translation": false}
        },
        "multimodal_capabilities": caps,
        "enabled": true
    }))
    .expect("catalog entry fixture")
}

/// Enabled premium model with vision + RAG, a description and multiplier `2x`.
pub fn premium_model(id: &str) -> ModelCatalogEntry {
    let mut e = base_entry(id, "premium", &["VISION_INPUT", "RAG"]);
    e.description = format!("Premium model {id}");
    "2x".clone_into(&mut e.multiplier_display);
    e
}

/// Enabled standard model with vision + RAG, empty description, multiplier
/// `1x`, `preference.is_default = true`.
pub fn standard_model(id: &str) -> ModelCatalogEntry {
    let mut e = base_entry(id, "standard", &["VISION_INPUT", "RAG"]);
    "1x".clone_into(&mut e.multiplier_display);
    e.preference = Some(ModelPreference {
        is_default: true,
        sort_order: 0,
    });
    e
}

/// Enabled standard model without `VISION_INPUT`.
pub fn standard_no_vision(id: &str) -> ModelCatalogEntry {
    let mut e = base_entry(id, "standard", &["RAG"]);
    "1x".clone_into(&mut e.multiplier_display);
    e
}

/// `entry` with `enabled = false`.
pub fn disabled(mut entry: ModelCatalogEntry) -> ModelCatalogEntry {
    entry.enabled = false;
    entry
}

/// Generous default limits (standard, premium).
pub fn default_limits() -> (TierLimits, TierLimits) {
    (
        TierLimits {
            limit_daily_credits_micro: 1_000_000_000_000,
            limit_monthly_credits_micro: 10_000_000_000_000,
        },
        TierLimits {
            limit_daily_credits_micro: 500_000_000_000,
            limit_monthly_credits_micro: 5_000_000_000_000,
        },
    )
}

// ---------------------------------------------------------------------------
// FakePolicy
// ---------------------------------------------------------------------------

/// In-memory [`PolicyPort`]: one mutable snapshot, per-tier limits, recorded
/// usage publications (scripted publish errors are returned first, FIFO).
pub struct FakePolicy {
    snapshot: Mutex<Arc<PolicySnapshot>>,
    limits: Mutex<(TierLimits, TierLimits)>,
    published: Mutex<Vec<UsageEvent>>,
    publish_errors: Mutex<VecDeque<PublishError>>,
    snapshot_errors: Mutex<VecDeque<DomainError>>,
}

impl FakePolicy {
    pub fn new(
        catalog: Vec<ModelCatalogEntry>,
        kill_switches: KillSwitches,
        limits: (TierLimits, TierLimits),
    ) -> Self {
        Self {
            snapshot: Mutex::new(Arc::new(PolicySnapshot {
                policy_version: 1,
                model_catalog: catalog,
                kill_switches,
            })),
            limits: Mutex::new(limits),
            published: Mutex::new(Vec::new()),
            publish_errors: Mutex::new(VecDeque::new()),
            snapshot_errors: Mutex::new(VecDeque::new()),
        }
    }

    fn update(&self, f: impl FnOnce(&mut PolicySnapshot)) {
        let mut guard = self.snapshot.lock().unwrap();
        let mut next = (**guard).clone();
        f(&mut next);
        *guard = Arc::new(next);
    }

    pub fn set_catalog(&self, catalog: Vec<ModelCatalogEntry>) {
        self.update(|s| s.model_catalog = catalog);
    }

    pub fn set_kill_switches(&self, ks: KillSwitches) {
        self.update(|s| s.kill_switches = ks);
    }

    pub fn set_limits(&self, standard: TierLimits, premium: TierLimits) {
        *self.limits.lock().unwrap() = (standard, premium);
    }

    pub fn snapshot(&self) -> Arc<PolicySnapshot> {
        Arc::clone(&self.snapshot.lock().unwrap())
    }

    pub fn published_usage(&self) -> Vec<UsageEvent> {
        self.published.lock().unwrap().clone()
    }

    /// The next `snapshot_for_version` call fails with `e`.
    pub fn push_snapshot_error(&self, e: DomainError) {
        self.snapshot_errors.lock().unwrap().push_back(e);
    }

    /// The next `publish_usage` call fails with `e` (nothing is recorded).
    pub fn push_publish_error(&self, e: PublishError) {
        self.publish_errors.lock().unwrap().push_back(e);
    }
}

#[async_trait]
impl PolicyPort for FakePolicy {
    async fn current_snapshot(&self, _user_id: Uuid) -> Result<Arc<PolicySnapshot>, DomainError> {
        Ok(self.snapshot())
    }

    async fn snapshot_for_version(
        &self,
        _user_id: Uuid,
        _version: u64,
    ) -> Result<Arc<PolicySnapshot>, DomainError> {
        if let Some(e) = self.snapshot_errors.lock().unwrap().pop_front() {
            return Err(e);
        }
        Ok(self.snapshot())
    }

    async fn user_limits(&self, user_id: Uuid, version: u64) -> Result<UserLimits, DomainError> {
        let (standard, premium) = *self.limits.lock().unwrap();
        Ok(UserLimits {
            user_id,
            policy_version: version,
            standard,
            premium,
        })
    }

    async fn publish_usage(&self, ev: UsageEvent) -> Result<(), PublishError> {
        if let Some(e) = self.publish_errors.lock().unwrap().pop_front() {
            return Err(e);
        }
        self.published.lock().unwrap().push(ev);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// FakeAudit
// ---------------------------------------------------------------------------

/// Recording [`AuditPort`] (every event is delivered).
#[derive(Default)]
pub struct FakeAudit {
    events: Mutex<Vec<MiniChatAuditEvent>>,
}

impl FakeAudit {
    pub fn events(&self) -> Vec<MiniChatAuditEvent> {
        self.events.lock().unwrap().clone()
    }
}

#[async_trait]
impl AuditPort for FakeAudit {
    async fn emit(&self, ev: MiniChatAuditEvent) -> Result<AuditResolution, AuditPluginError> {
        self.events.lock().unwrap().push(ev);
        Ok(AuditResolution::Delivered)
    }
}

// ---------------------------------------------------------------------------
// FakePdp
// ---------------------------------------------------------------------------

/// Decision mode of [`FakePdp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdpMode {
    /// `decision: true` with an `In(owner_tenant_id, [tenant])` constraint
    /// when the PEP declares `owner_tenant_id` (like static-authz).
    Allow,
    /// `decision: false`.
    Deny,
    /// The evaluation call fails (PDP unreachable).
    Fail,
}

/// Fake PDP recording every evaluation request.
pub struct FakePdp {
    mode: Mutex<PdpMode>,
    requests: Mutex<Vec<EvaluationRequest>>,
}

impl Default for FakePdp {
    fn default() -> Self {
        Self {
            mode: Mutex::new(PdpMode::Allow),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl FakePdp {
    pub fn set_mode(&self, mode: PdpMode) {
        *self.mode.lock().unwrap() = mode;
    }

    pub fn requests(&self) -> Vec<EvaluationRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn last_request(&self) -> EvaluationRequest {
        self.requests().pop().expect("at least one PDP request")
    }
}

#[async_trait]
impl AuthZResolverApi for FakePdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        req: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        self.requests.lock().unwrap().push(req.clone());
        let mode = *self.mode.lock().unwrap();
        match mode {
            PdpMode::Fail => Err(CanonicalError::service_unavailable()
                .with_detail("fake pdp unavailable")
                .create()),
            PdpMode::Deny => Ok(EvaluationResponse {
                decision: false,
                context: EvaluationResponseContext::default(),
            }),
            PdpMode::Allow => {
                let tenant = req
                    .context
                    .tenant_context
                    .as_ref()
                    .and_then(|t| t.root_id)
                    .or_else(|| {
                        req.subject
                            .properties
                            .get("tenant_id")
                            .and_then(|v| v.as_str())
                            .and_then(|s| Uuid::parse_str(s).ok())
                    })
                    .expect("tenant in PDP request");
                let mut constraints = Vec::new();
                if req
                    .context
                    .supported_properties
                    .iter()
                    .any(|p| p == pep_properties::OWNER_TENANT_ID)
                {
                    constraints.push(Constraint {
                        predicates: vec![Predicate::In(InPredicate::new(
                            pep_properties::OWNER_TENANT_ID,
                            [tenant],
                        ))],
                    });
                }
                Ok(EvaluationResponse {
                    decision: true,
                    context: EvaluationResponseContext {
                        constraints,
                        ..Default::default()
                    },
                })
            }
        }
    }
}
