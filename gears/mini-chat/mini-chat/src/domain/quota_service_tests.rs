#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverApi;
use authz_resolver_sdk::constraints::{Constraint, EqPredicate, Predicate};
use authz_resolver_sdk::models::{
    EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError,
    ModelCatalogEntry, ModelTier, PolicySnapshot, PublishError, TierLimits, UsageEvent,
    UsageTokens, UserLimits,
};
use serde_json::json;
use time::OffsetDateTime;
use time::macros::{date, datetime};
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::{AccessScope, PlatformSecurityContext, SecurityContext, pep_properties};
use uuid::Uuid;

use super::*;
use crate::domain::authz::AuthzService;
use crate::domain::billing::{BillingOutcome, SettlementMethod, TerminalState};
use crate::domain::error::{DomainError, ExhaustedLimit, PreconditionSubject, QuotaScope};
use crate::domain::models_service::tests::{allow_all_authz, catalog_entry};
use crate::domain::ports::{QuotaBucketKey, QuotaOwner, QuotaRepository, QuotaUsageRecord};
use crate::domain::quota::{
    PeriodStarts, QuotaBucket, QuotaPeriod, ReserveInputs, estimate_reserve,
};
use crate::infra::db::repo::quota as quota_repo;
use crate::infra::db::repo::tests::TestStore;

const TENANT: Uuid = Uuid::from_u128(0x7e7a);
const USER: Uuid = Uuid::from_u128(0xa11c);
const OWNER: QuotaOwner = QuotaOwner {
    tenant_id: TENANT,
    user_id: USER,
};
const NOW: OffsetDateTime = datetime!(2026-10-11 12:00 UTC);
const BIG: i64 = 1_000_000_000_000;

// ── Fixtures ─────────────────────────────────────────────────────────────

/// Fake policy plugin: snapshots by version, one current version, limits.
struct FakePolicy {
    current: AtomicU64,
    snapshots: Mutex<HashMap<u64, PolicySnapshot>>,
    limits: Mutex<(TierLimits, TierLimits)>,
}

impl FakePolicy {
    fn new(catalog: Vec<ModelCatalogEntry>) -> Arc<Self> {
        let policy = Arc::new(Self {
            current: AtomicU64::new(0),
            snapshots: Mutex::new(HashMap::new()),
            limits: Mutex::new((tier(BIG, BIG), tier(BIG, BIG))),
        });
        policy.publish(catalog, KillSwitches::default());
        policy
    }

    fn publish(&self, catalog: Vec<ModelCatalogEntry>, kill_switches: KillSwitches) -> u64 {
        let version = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        self.snapshots.lock().unwrap().insert(
            version,
            PolicySnapshot {
                policy_version: version,
                model_catalog: catalog,
                kill_switches,
            },
        );
        version
    }

    fn set_limits(&self, standard: TierLimits, premium: TierLimits) {
        *self.limits.lock().unwrap() = (standard, premium);
    }

    fn limits(&self, version: u64) -> UserLimits {
        let (standard, premium) = *self.limits.lock().unwrap();
        UserLimits {
            user_id: USER,
            policy_version: version,
            standard,
            premium,
        }
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for FakePolicy {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<u64, MiniChatModelPolicyPluginError> {
        Ok(self.current.load(Ordering::SeqCst))
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        self.snapshots
            .lock()
            .unwrap()
            .get(&policy_version)
            .cloned()
            .ok_or(MiniChatModelPolicyPluginError::VersionNotFound { policy_version })
    }

    async fn get_user_limits(
        &self,
        _user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        Ok(self.limits(policy_version))
    }

    async fn publish_usage(&self, _payload: UsageEvent) -> Result<(), PublishError> {
        Ok(())
    }
}

/// PDP allowing the user quota with the subject's owner constraints.
struct OwnerPdp;

#[async_trait]
impl AuthZResolverApi for OwnerPdp {
    async fn evaluate(
        &self,
        _ctx: PlatformSecurityContext,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, CanonicalError> {
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![
                        Predicate::Eq(EqPredicate::new(pep_properties::OWNER_TENANT_ID, TENANT)),
                        Predicate::Eq(EqPredicate::new(pep_properties::OWNER_ID, USER)),
                    ],
                }],
                ..Default::default()
            },
        })
    }
}

const fn tier(daily: i64, monthly: i64) -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: daily,
        limit_monthly_credits_micro: monthly,
    }
}

fn settings() -> QuotaSettings {
    QuotaSettings {
        max_output_tokens: 32_768,
        minimal_generation_floor: 50,
        overshoot_tolerance_factor: 1.10,
        warning_threshold_pct: 80,
        web_search_daily_quota: 3,
        code_interpreter_daily_quota: 2,
    }
}

/// Catalog: `premium` (all tools, vision, 2x/4x), `premium-alt` (default),
/// `premium-off` (disabled), `standard` (file search only),
/// `standard-default` (default), `standard-off` (disabled).
fn catalog() -> Vec<ModelCatalogEntry> {
    let all_tools = json!({ "web_search": true, "file_search": true, "code_interpreter": true });
    vec![
        catalog_entry(
            "premium",
            json!({
                "tier": "premium",
                "multimodal_capabilities": ["VISION_INPUT"],
                "input_tokens_credit_multiplier_micro": 2_000_000,
                "output_tokens_credit_multiplier_micro": 4_000_000,
                "general_config": { "tool_support": all_tools },
            }),
        ),
        catalog_entry(
            "premium-alt",
            json!({ "tier": "premium", "preference": { "is_default": true, "sort_order": 1 } }),
        ),
        catalog_entry(
            "premium-off",
            json!({ "tier": "premium", "enabled": false }),
        ),
        catalog_entry(
            "standard",
            json!({ "general_config": { "tool_support": { "file_search": true } } }),
        ),
        catalog_entry(
            "standard-default",
            json!({ "preference": { "is_default": true, "sort_order": 2 } }),
        ),
        catalog_entry("standard-off", json!({ "enabled": false })),
    ]
}

struct Harness {
    store: TestStore,
    policy: Arc<FakePolicy>,
    service: Arc<QuotaService>,
}

impl Harness {
    async fn new() -> Self {
        Self::with(catalog(), settings()).await
    }

    async fn with(catalog: Vec<ModelCatalogEntry>, settings: QuotaSettings) -> Self {
        let store = TestStore::without_outbox().await;
        let policy = FakePolicy::new(catalog);
        let authz = Arc::new(AuthzService::from_client(Arc::new(OwnerPdp)));
        let service = Arc::new(QuotaService::new(
            Arc::clone(&store.repo) as Arc<dyn QuotaRepository>,
            Arc::clone(&policy) as Arc<dyn MiniChatModelPolicyPluginClientV1>,
            authz,
            settings,
        ));
        Self {
            store,
            policy,
            service,
        }
    }

    fn kill(&self, kill_switches: KillSwitches) {
        self.policy.publish(catalog(), kill_switches);
    }

    async fn preflight_with(
        &self,
        selected: &str,
        request: PreflightRequest,
        prior_context_tokens: u64,
        now: OffsetDateTime,
    ) -> Result<PreflightDecision, DomainError> {
        let policy = self.service.load_policy(USER).await.expect("policy");
        self.service
            .preflight(PreflightInput {
                owner: OWNER,
                selected_model: selected,
                policy_version: policy.policy_version,
                snapshot: &policy.snapshot,
                user_limits: &policy.user_limits,
                request,
                prior_context_tokens,
                now,
            })
            .await
    }

    async fn preflight(&self, selected: &str) -> Result<PreflightDecision, DomainError> {
        self.preflight_with(selected, request(), 0, NOW).await
    }

    async fn reserve(&self, decision: &PreflightDecision) -> Result<(), DomainError> {
        let service = Arc::clone(&self.service);
        let decision = decision.clone();
        self.store
            .db
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move { service.reserve_in_tx(tx, &decision, OWNER).await })
            })
            .await
    }

    async fn settle(&self, input: SettlementInput) -> Result<SettlementResult, DomainError> {
        let service = Arc::clone(&self.service);
        self.store
            .db
            .transaction_ref_mapped(move |tx| {
                Box::pin(async move { service.settle_in_tx(tx, &input).await })
            })
            .await
    }

    /// Seeds a bucket row of the `NOW` periods.
    async fn seed(&self, period: QuotaPeriod, bucket: QuotaBucket, spent: i64, reserved: i64) {
        self.seed_calls(period, bucket, spent, reserved, (0, 0))
            .await;
    }

    async fn seed_calls(
        &self,
        period: QuotaPeriod,
        bucket: QuotaBucket,
        spent: i64,
        reserved: i64,
        (web_search_calls, code_interpreter_calls): (u32, u32),
    ) {
        let key = key_at(NOW, period, bucket);
        let conn = self.store.conn();
        let scope = AccessScope::allow_all();
        quota_repo::add_reserve(&conn, self.store.ts(), &scope, OWNER, &[key], reserved, NOW)
            .await
            .expect("seed reserve");
        quota_repo::settle(
            &conn,
            self.store.ts(),
            &scope,
            OWNER,
            &[crate::domain::ports::BucketSettlement {
                key,
                release_reserved_credits_micro: 0,
                spent_credits_micro: spent,
                input_tokens: 0,
                output_tokens: 0,
                web_search_calls,
                code_interpreter_calls,
            }],
            NOW,
        )
        .await
        .expect("seed spent");
    }

    async fn row(
        &self,
        at: OffsetDateTime,
        period: QuotaPeriod,
        bucket: QuotaBucket,
    ) -> Option<QuotaUsageRecord> {
        let key = key_at(at, period, bucket);
        quota_repo::read_buckets(
            &self.store.conn(),
            &AccessScope::allow_all(),
            OWNER,
            &[key],
            false,
        )
        .await
        .expect("read")
        .into_iter()
        .next()
    }
}

fn model(id: &str) -> ModelCatalogEntry {
    catalog().into_iter().find(|entry| entry.id == id).unwrap()
}

fn key_at(at: OffsetDateTime, period: QuotaPeriod, bucket: QuotaBucket) -> QuotaBucketKey {
    QuotaBucketKey {
        period,
        period_start: PeriodStarts::at(at).start(period),
        bucket,
    }
}

fn request() -> PreflightRequest {
    PreflightRequest {
        message_utf8_bytes: 400,
        ..PreflightRequest::default()
    }
}

fn reserve_of(model: &ModelCatalogEntry, inputs: &ReserveInputs) -> i64 {
    estimate_reserve(model, inputs, 32_768)
        .unwrap()
        .reserved_credits_micro
}

fn plain_inputs() -> ReserveInputs {
    ReserveInputs {
        message_utf8_bytes: 400,
        ..ReserveInputs::default()
    }
}

fn is_quota_exceeded(err: &DomainError, expected: QuotaScope) -> bool {
    matches!(
        err,
        DomainError::ResourceExhausted {
            limit: ExhaustedLimit::Quota(scope)
        } if *scope == expected
    )
}

fn assert_allow(decision: &PreflightDecision, model: &str) {
    assert_eq!(decision.effective_model_id(), model);
    assert_eq!(decision.decision, QuotaDecisionKind::Allow);
    assert_eq!(decision.downgrade_from, None);
    assert_eq!(decision.downgrade_reason, None);
}

fn assert_downgrade(
    decision: &PreflightDecision,
    selected: &str,
    model: &str,
    reason: DowngradeReason,
) {
    assert_eq!(decision.effective_model_id(), model);
    assert_eq!(decision.decision, QuotaDecisionKind::Downgrade);
    assert_eq!(decision.downgrade_from.as_deref(), Some(selected));
    assert_eq!(decision.downgrade_reason, Some(reason));
}

// ── Cascade ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn enabled_selected_models_are_allowed_on_their_tier() {
    let h = Harness::new().await;
    let premium = h.preflight("premium").await.unwrap();
    assert_allow(&premium, "premium");
    assert_eq!(premium.effective_tier, ModelTier::Premium);
    assert!(premium.vision_supported);

    let standard = h.preflight("standard").await.unwrap();
    assert_allow(&standard, "standard");
    assert_eq!(standard.effective_tier, ModelTier::Standard);
    assert!(!standard.vision_supported);
}

#[tokio::test]
async fn exhausted_premium_daily_downgrades_to_the_standard_default() {
    let h = Harness::new().await;
    h.policy.set_limits(tier(BIG, BIG), tier(1000, BIG));
    h.seed(QuotaPeriod::Daily, QuotaBucket::Premium, 1000, 0)
        .await;

    let decision = h.preflight("premium").await.unwrap();
    assert_downgrade(
        &decision,
        "premium",
        "standard-default",
        DowngradeReason::PremiumQuotaExhausted,
    );
    assert_eq!(decision.effective_tier, ModelTier::Standard);
    let audit = decision.audit_quota_decision();
    assert_eq!(audit.decision, "downgrade");
    assert_eq!(audit.downgrade_from.as_deref(), Some("premium"));
    assert_eq!(
        audit.downgrade_reason.as_deref(),
        Some("premium_quota_exhausted")
    );
}

#[tokio::test]
async fn exhausted_premium_monthly_reserve_also_downgrades() {
    let h = Harness::new().await;
    h.policy.set_limits(tier(BIG, BIG), tier(BIG, 5000));
    h.seed(QuotaPeriod::Monthly, QuotaBucket::Premium, 0, 5000)
        .await;

    let decision = h.preflight("premium").await.unwrap();
    assert_downgrade(
        &decision,
        "premium",
        "standard-default",
        DowngradeReason::PremiumQuotaExhausted,
    );
}

#[tokio::test]
async fn exhausted_total_rejects_every_tier() {
    let h = Harness::new().await;
    h.policy.set_limits(tier(BIG, 1000), tier(BIG, BIG));
    h.seed(QuotaPeriod::Monthly, QuotaBucket::Total, 1000, 0)
        .await;

    for selected in ["premium", "standard"] {
        let err = h.preflight(selected).await.unwrap_err();
        assert!(
            is_quota_exceeded(&err, QuotaScope::Tokens),
            "{selected}: {err:?}"
        );
    }
}

#[tokio::test]
async fn premium_kill_switches_skip_premium_with_their_reason() {
    let cases = [
        (
            KillSwitches {
                force_standard_tier: true,
                ..KillSwitches::default()
            },
            DowngradeReason::ForceStandardTier,
        ),
        (
            KillSwitches {
                disable_premium_tier: true,
                ..KillSwitches::default()
            },
            DowngradeReason::DisablePremiumTier,
        ),
        (
            KillSwitches {
                force_standard_tier: true,
                disable_premium_tier: true,
                ..KillSwitches::default()
            },
            DowngradeReason::ForceStandardTier,
        ),
    ];
    for (kill, reason) in cases {
        let h = Harness::new().await;
        h.kill(kill);
        let decision = h.preflight("premium").await.unwrap();
        assert_downgrade(&decision, "premium", "standard-default", reason);
        // A standard selection is unaffected.
        assert_allow(&h.preflight("standard").await.unwrap(), "standard");
    }
}

#[tokio::test]
async fn disabled_selected_model_keeps_its_tier_with_reason_model_disabled() {
    let h = Harness::new().await;
    assert_downgrade(
        &h.preflight("premium-off").await.unwrap(),
        "premium-off",
        "premium-alt",
        DowngradeReason::ModelDisabled,
    );
    assert_downgrade(
        &h.preflight("standard-off").await.unwrap(),
        "standard-off",
        "standard-default",
        DowngradeReason::ModelDisabled,
    );
}

#[tokio::test]
async fn missing_selected_model_starts_at_premium_with_reason_model_disabled() {
    let h = Harness::new().await;
    let decision = h.preflight("gone").await.unwrap();
    assert_downgrade(
        &decision,
        "gone",
        "premium-alt",
        DowngradeReason::ModelDisabled,
    );
    assert_eq!(decision.effective_tier, ModelTier::Premium);
}

#[tokio::test]
async fn model_disabled_reason_wins_over_a_later_premium_exhaustion() {
    let h = Harness::new().await;
    h.policy.set_limits(tier(BIG, BIG), tier(0, 0));
    let decision = h.preflight("gone").await.unwrap();
    assert_downgrade(
        &decision,
        "gone",
        "standard-default",
        DowngradeReason::ModelDisabled,
    );
}

#[tokio::test]
async fn candidate_falls_back_to_the_first_enabled_model_of_the_tier() {
    let catalog = vec![
        catalog_entry("p-off", json!({ "tier": "premium", "enabled": false })),
        catalog_entry("p-first", json!({ "tier": "premium" })),
        catalog_entry("p-second", json!({ "tier": "premium" })),
        catalog_entry("s-first", json!({})),
    ];
    let h = Harness::with(catalog, settings()).await;
    assert_downgrade(
        &h.preflight("p-off").await.unwrap(),
        "p-off",
        "p-first",
        DowngradeReason::ModelDisabled,
    );
}

#[tokio::test]
async fn standard_selection_never_upgrades_to_premium() {
    let catalog = vec![
        catalog_entry("p", json!({ "tier": "premium" })),
        catalog_entry("s-off", json!({ "enabled": false })),
    ];
    let h = Harness::with(catalog, settings()).await;
    let err = h.preflight("s-off").await.unwrap_err();
    assert!(is_quota_exceeded(&err, QuotaScope::Tokens));
}

#[tokio::test]
async fn tier_without_enabled_models_continues_the_cascade() {
    let catalog = vec![
        catalog_entry("p-off", json!({ "tier": "premium", "enabled": false })),
        catalog_entry("s", json!({})),
    ];
    let h = Harness::with(catalog, settings()).await;
    assert_downgrade(
        &h.preflight("p-off").await.unwrap(),
        "p-off",
        "s",
        DowngradeReason::ModelDisabled,
    );
    let empty = Harness::with(Vec::new(), settings()).await;
    assert!(is_quota_exceeded(
        &empty.preflight("anything").await.unwrap_err(),
        QuotaScope::Tokens
    ));
}

#[tokio::test]
async fn candidate_with_an_uncomputable_reserve_is_unavailable() {
    let catalog = vec![
        catalog_entry(
            "p-free",
            json!({ "tier": "premium", "output_tokens_credit_multiplier_micro": 0 }),
        ),
        catalog_entry("s", json!({})),
    ];
    let h = Harness::with(catalog, settings()).await;
    assert_downgrade(
        &h.preflight("p-free").await.unwrap(),
        "p-free",
        "s",
        DowngradeReason::PremiumQuotaExhausted,
    );
}

// ── Reserve formula and tool gates ───────────────────────────────────────

#[tokio::test]
async fn reserve_fields_use_the_effective_model_and_every_gated_surcharge() {
    let h = Harness::new().await;
    let request = PreflightRequest {
        message_utf8_bytes: 400,
        image_count: 2,
        web_search_requested: true,
        has_ready_documents: true,
        has_ready_code_interpreter_files: true,
    };
    let decision = h
        .preflight_with("premium", request, 1000, NOW)
        .await
        .unwrap();

    // 220 text + 1000 prior + 2000 images + 500 file + 500 web + 1000 code.
    assert_eq!(decision.estimated_input_tokens, 5220);
    assert_eq!(decision.max_output_tokens_applied, 4096);
    assert_eq!(decision.reserve_tokens, 5220 + 4096);
    assert_eq!(decision.reserved_credits_micro, 5220 * 2 + 4096 * 4);
    assert_eq!(decision.minimal_generation_floor_applied, 50);
    assert_eq!(decision.policy_version_applied, 1);
    assert_eq!(
        decision.tools,
        ToolGates {
            web_search: true,
            file_search: true,
            code_interpreter: true,
        }
    );
    assert_eq!(decision.periods, PeriodStarts::at(NOW));
    assert_eq!(decision.periods.daily, date!(2026 - 10 - 11));
    assert_eq!(decision.periods.monthly, date!(2026 - 10 - 01));
}

#[tokio::test]
async fn downgraded_candidate_is_estimated_with_its_own_tool_support() {
    let h = Harness::new().await;
    h.kill(KillSwitches {
        force_standard_tier: true,
        ..KillSwitches::default()
    });
    let request = PreflightRequest {
        message_utf8_bytes: 400,
        image_count: 0,
        web_search_requested: true,
        has_ready_documents: true,
        has_ready_code_interpreter_files: true,
    };
    // `standard-default` has no tool support at all.
    let decision = h.preflight_with("premium", request, 0, NOW).await.unwrap();
    assert_eq!(decision.effective_model_id(), "standard-default");
    assert_eq!(decision.estimated_input_tokens, 220);
    assert_eq!(decision.tools, ToolGates::default());
}

#[tokio::test]
async fn tool_kill_switches_remove_their_surcharges() {
    let h = Harness::new().await;
    h.kill(KillSwitches {
        disable_file_search: true,
        disable_code_interpreter: true,
        ..KillSwitches::default()
    });
    let request = PreflightRequest {
        message_utf8_bytes: 400,
        image_count: 0,
        web_search_requested: false,
        has_ready_documents: true,
        has_ready_code_interpreter_files: true,
    };
    let decision = h.preflight_with("premium", request, 0, NOW).await.unwrap();
    assert_eq!(decision.estimated_input_tokens, 220);
    assert_eq!(decision.tools, ToolGates::default());
}

#[tokio::test]
async fn output_cap_and_floor_follow_the_settings() {
    let capped = QuotaSettings {
        max_output_tokens: 30,
        ..settings()
    };
    let h = Harness::with(catalog(), capped).await;
    let decision = h.preflight("standard").await.unwrap();
    assert_eq!(decision.max_output_tokens_applied, 30);
    assert_eq!(decision.minimal_generation_floor_applied, 30);
    assert_eq!(decision.reserve_tokens, 220 + 30);
}

// ── Availability boundary ────────────────────────────────────────────────

#[tokio::test]
async fn availability_admits_a_reserve_reaching_the_limit_exactly() {
    let h = Harness::new().await;
    let reserve = reserve_of(&model("standard"), &plain_inputs());
    h.policy
        .set_limits(tier(reserve + 100, BIG), tier(BIG, BIG));
    h.seed(QuotaPeriod::Daily, QuotaBucket::Total, 60, 40).await;
    assert_allow(&h.preflight("standard").await.unwrap(), "standard");

    h.policy.set_limits(tier(reserve + 99, BIG), tier(BIG, BIG));
    assert!(is_quota_exceeded(
        &h.preflight("standard").await.unwrap_err(),
        QuotaScope::Tokens
    ));
}

#[tokio::test]
async fn premium_needs_both_the_total_and_the_premium_bucket() {
    let h = Harness::new().await;
    let reserve = reserve_of(&model("premium"), &plain_inputs());
    h.policy.set_limits(tier(BIG, reserve), tier(BIG, reserve));
    assert_allow(&h.preflight("premium").await.unwrap(), "premium");

    // The total cap is short by one credit; premium falls to standard.
    h.policy
        .set_limits(tier(BIG, reserve - 1), tier(BIG, reserve));
    let decision = h.preflight("premium").await.unwrap();
    assert_downgrade(
        &decision,
        "premium",
        "standard-default",
        DowngradeReason::PremiumQuotaExhausted,
    );
}

// ── Web search / code interpreter preflight checks ───────────────────────

#[tokio::test]
async fn disabled_web_search_is_rejected_before_the_cascade() {
    let h = Harness::new().await;
    h.kill(KillSwitches {
        disable_web_search: true,
        ..KillSwitches::default()
    });
    h.policy.set_limits(tier(0, 0), tier(0, 0));
    let request = PreflightRequest {
        web_search_requested: true,
        ..request()
    };
    let err = h
        .preflight_with("premium", request, 0, NOW)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        DomainError::FailedPrecondition {
            subject: PreconditionSubject::WebSearch
        }
    ));
    // Without web search the request reaches the cascade (and its 429).
    assert!(is_quota_exceeded(
        &h.preflight("premium").await.unwrap_err(),
        QuotaScope::Tokens
    ));
}

#[tokio::test]
async fn daily_web_search_quota_applies_only_when_the_tool_is_sent() {
    let h = Harness::new().await;
    h.seed_calls(QuotaPeriod::Daily, QuotaBucket::Total, 0, 0, (3, 0))
        .await;
    let with_web = PreflightRequest {
        web_search_requested: true,
        ..request()
    };
    let err = h
        .preflight_with("premium", with_web, 0, NOW)
        .await
        .unwrap_err();
    assert!(is_quota_exceeded(&err, QuotaScope::WebSearch));
    // `standard` has no web search support: no tool, no check.
    let decision = h
        .preflight_with("standard", with_web, 0, NOW)
        .await
        .unwrap();
    assert!(!decision.tools.web_search);
    // Not requested: no check.
    assert!(h.preflight("premium").await.is_ok());
}

#[tokio::test]
async fn daily_web_search_quota_admits_the_last_call() {
    let h = Harness::new().await;
    h.seed_calls(QuotaPeriod::Daily, QuotaBucket::Total, 0, 0, (2, 0))
        .await;
    let with_web = PreflightRequest {
        web_search_requested: true,
        ..request()
    };
    assert!(h.preflight_with("premium", with_web, 0, NOW).await.is_ok());
}

#[tokio::test]
async fn daily_code_interpreter_quota_applies_only_when_the_tool_is_sent() {
    let h = Harness::new().await;
    h.seed_calls(QuotaPeriod::Daily, QuotaBucket::Total, 0, 0, (0, 2))
        .await;
    let with_xlsx = PreflightRequest {
        has_ready_code_interpreter_files: true,
        ..request()
    };
    let err = h
        .preflight_with("premium", with_xlsx, 0, NOW)
        .await
        .unwrap_err();
    assert!(is_quota_exceeded(&err, QuotaScope::CodeInterpreter));

    h.kill(KillSwitches {
        disable_code_interpreter: true,
        ..KillSwitches::default()
    });
    let decision = h
        .preflight_with("premium", with_xlsx, 0, NOW)
        .await
        .unwrap();
    assert!(!decision.tools.code_interpreter);
}

// ── Reserve transaction ──────────────────────────────────────────────────

#[tokio::test]
async fn reserve_books_total_and_premium_rows_of_both_periods() {
    let h = Harness::new().await;
    let decision = h.preflight("premium").await.unwrap();
    h.reserve(&decision).await.unwrap();

    for period in QuotaPeriod::ALL {
        for bucket in QuotaBucket::ALL {
            let row = h.row(NOW, period, bucket).await.expect("row");
            assert_eq!(row.reserved_credits_micro, decision.reserved_credits_micro);
            assert_eq!(row.spent_credits_micro, 0);
        }
    }
}

#[tokio::test]
async fn standard_reserve_does_not_touch_the_premium_bucket() {
    let h = Harness::new().await;
    let decision = h.preflight("standard").await.unwrap();
    h.reserve(&decision).await.unwrap();
    h.reserve(&decision).await.unwrap();

    let total = h
        .row(NOW, QuotaPeriod::Daily, QuotaBucket::Total)
        .await
        .unwrap();
    assert_eq!(
        total.reserved_credits_micro,
        2 * decision.reserved_credits_micro
    );
    assert!(
        h.row(NOW, QuotaPeriod::Daily, QuotaBucket::Premium)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn reserve_recheck_rejects_concurrent_overspend() {
    let h = Harness::new().await;
    let reserve = reserve_of(&model("standard"), &plain_inputs());
    // Room for exactly one reserve.
    h.policy.set_limits(tier(reserve, BIG), tier(BIG, BIG));

    // Both requests pass preflight before either books its reserve.
    let first = h.preflight("standard").await.unwrap();
    let second = h.preflight("standard").await.unwrap();

    h.reserve(&first).await.unwrap();
    let err = h.reserve(&second).await.unwrap_err();
    assert!(is_quota_exceeded(&err, QuotaScope::Tokens));

    // The losing transaction rolled back its increment.
    let total = h
        .row(NOW, QuotaPeriod::Daily, QuotaBucket::Total)
        .await
        .unwrap();
    assert_eq!(total.reserved_credits_micro, reserve);
}

// ── Settlement ───────────────────────────────────────────────────────────

fn settlement_for(
    decision: &PreflightDecision,
    terminal: TerminalState,
    error_code: Option<&str>,
    usage: Option<UsageTokens>,
) -> SettlementInput {
    SettlementInput {
        owner: OWNER,
        effective_model: decision.effective_model_id().to_owned(),
        policy_version_applied: decision.policy_version_applied,
        reserve_tokens: decision.reserve_tokens,
        max_output_tokens_applied: i64::from(decision.max_output_tokens_applied),
        reserved_credits_micro: decision.reserved_credits_micro,
        minimal_generation_floor_applied: i64::from(decision.minimal_generation_floor_applied),
        periods: decision.periods,
        terminal,
        error_code: error_code.map(str::to_owned),
        usage,
        web_search_calls: 2,
        code_interpreter_calls: 1,
    }
}

fn tokens(input: i64, output: i64) -> UsageTokens {
    UsageTokens {
        input_tokens: input,
        output_tokens: output,
        ..UsageTokens::default()
    }
}

async fn reserved(h: &Harness, selected: &str) -> PreflightDecision {
    let decision = h.preflight(selected).await.unwrap();
    h.reserve(&decision).await.unwrap();
    decision
}

#[tokio::test]
async fn completed_turn_commits_actual_usage_on_every_bucket() {
    let h = Harness::new().await;
    let decision = reserved(&h, "premium").await;

    let result = h
        .settle(settlement_for(
            &decision,
            TerminalState::Completed,
            None,
            Some(tokens(300, 200)),
        ))
        .await
        .unwrap();

    assert_eq!(result.billing.outcome, BillingOutcome::Completed);
    assert_eq!(result.billing.method, SettlementMethod::Actual);
    // premium multipliers: 2x in, 4x out.
    assert_eq!(result.committed_credits_micro, 300 * 2 + 200 * 4);
    assert_eq!(result.charged_tokens, 500);
    assert!(!result.overshoot);
    assert_eq!(result.effective_tier, ModelTier::Premium);

    for period in QuotaPeriod::ALL {
        let total = h.row(NOW, period, QuotaBucket::Total).await.unwrap();
        assert_eq!(total.reserved_credits_micro, 0);
        assert_eq!(total.spent_credits_micro, 1400);
        assert_eq!(total.calls, 1);
        assert_eq!((total.input_tokens, total.output_tokens), (300, 200));
        assert_eq!(
            (total.web_search_calls, total.code_interpreter_calls),
            (2, 1)
        );
        let premium = h.row(NOW, period, QuotaBucket::Premium).await.unwrap();
        assert_eq!(premium.reserved_credits_micro, 0);
        assert_eq!(premium.spent_credits_micro, 1400);
        assert_eq!(premium.calls, 1);
        assert_eq!((premium.input_tokens, premium.output_tokens), (0, 0));
        assert_eq!(
            (premium.web_search_calls, premium.code_interpreter_calls),
            (0, 0)
        );
    }
}

#[tokio::test]
async fn completed_turn_without_usage_is_actual_and_free() {
    let h = Harness::new().await;
    let decision = reserved(&h, "standard").await;
    let result = h
        .settle(settlement_for(
            &decision,
            TerminalState::Completed,
            None,
            None,
        ))
        .await
        .unwrap();
    assert_eq!(result.billing.method, SettlementMethod::Actual);
    assert_eq!(result.committed_credits_micro, 0);
    let total = h
        .row(NOW, QuotaPeriod::Daily, QuotaBucket::Total)
        .await
        .unwrap();
    assert_eq!(
        (total.spent_credits_micro, total.reserved_credits_micro),
        (0, 0)
    );
    assert_eq!(total.calls, 1);
}

#[tokio::test]
async fn overshoot_within_tolerance_commits_actual() {
    let h = Harness::new().await;
    let decision = reserved(&h, "standard").await;
    // 110 % of the reserve, all on the input side.
    let input = decision.reserve_tokens + decision.reserve_tokens.div_euclid(10);
    let result = h
        .settle(settlement_for(
            &decision,
            TerminalState::Completed,
            None,
            Some(tokens(input, 0)),
        ))
        .await
        .unwrap();
    assert!(result.overshoot);
    assert!(!result.overshoot_capped);
    assert_eq!(result.committed_credits_micro, input);
    assert_eq!(result.charged_tokens, input);
}

#[tokio::test]
async fn overshoot_beyond_tolerance_is_capped_at_the_reserve() {
    let h = Harness::new().await;
    let decision = reserved(&h, "standard").await;
    let input = decision.reserve_tokens + decision.reserve_tokens.div_euclid(5);
    let result = h
        .settle(settlement_for(
            &decision,
            TerminalState::Completed,
            None,
            Some(tokens(input, 0)),
        ))
        .await
        .unwrap();
    assert!(result.overshoot);
    assert!(result.overshoot_capped);
    assert_eq!(
        result.committed_credits_micro,
        decision.reserved_credits_micro
    );
    assert_eq!(result.charged_tokens, decision.reserve_tokens);
    let total = h
        .row(NOW, QuotaPeriod::Daily, QuotaBucket::Total)
        .await
        .unwrap();
    assert_eq!(total.spent_credits_micro, decision.reserved_credits_micro);
    // Telemetry keeps the actual tokens.
    assert_eq!(total.input_tokens, input);
}

#[tokio::test]
async fn failed_turn_with_usage_settles_actual_and_without_usage_estimated() {
    let h = Harness::new().await;
    let first = reserved(&h, "standard").await;
    let actual = h
        .settle(settlement_for(
            &first,
            TerminalState::Failed,
            Some("provider_error"),
            Some(tokens(100, 10)),
        ))
        .await
        .unwrap();
    assert_eq!(actual.billing.method, SettlementMethod::Actual);
    assert_eq!(actual.committed_credits_micro, 100 + 30);

    let second = reserved(&h, "standard").await;
    let estimated = h
        .settle(settlement_for(
            &second,
            TerminalState::Failed,
            Some("provider_error"),
            Some(tokens(0, 0)),
        ))
        .await
        .unwrap();
    assert_eq!(estimated.billing.method, SettlementMethod::Estimated);
    // credits_micro(estimated_input = 220, floor = 50) with 1x / 3x.
    assert_eq!(estimated.committed_credits_micro, 220 + 150);
    assert_eq!(estimated.charged_tokens, 270);

    let total = h
        .row(NOW, QuotaPeriod::Daily, QuotaBucket::Total)
        .await
        .unwrap();
    assert_eq!(total.reserved_credits_micro, 0);
    assert_eq!(total.spent_credits_micro, 130 + 370);
    assert_eq!(total.calls, 2);
    // Estimated settlements add no tokens but count the tool calls.
    assert_eq!((total.input_tokens, total.output_tokens), (100, 10));
    assert_eq!(
        (total.web_search_calls, total.code_interpreter_calls),
        (4, 2)
    );
}

#[tokio::test]
async fn cancelled_turn_is_aborted_and_estimated() {
    let h = Harness::new().await;
    let decision = reserved(&h, "premium").await;
    let result = h
        .settle(settlement_for(
            &decision,
            TerminalState::Cancelled,
            None,
            Some(tokens(1000, 1000)),
        ))
        .await
        .unwrap();
    assert_eq!(result.billing.outcome, BillingOutcome::Aborted);
    assert_eq!(result.billing.method, SettlementMethod::Estimated);
    assert_eq!(result.committed_credits_micro, 220 * 2 + 50 * 4);
    let premium = h
        .row(NOW, QuotaPeriod::Monthly, QuotaBucket::Premium)
        .await
        .unwrap();
    assert_eq!(premium.spent_credits_micro, 640);
    assert_eq!(premium.reserved_credits_micro, 0);
}

#[tokio::test]
async fn released_settlement_charges_nothing_and_counts_no_tools() {
    let h = Harness::new().await;
    let decision = reserved(&h, "standard").await;
    let result = h
        .settle(settlement_for(
            &decision,
            TerminalState::Failed,
            Some("turn_setup_failed"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(result.billing.method, SettlementMethod::Released);
    assert_eq!(result.committed_credits_micro, 0);
    let total = h
        .row(NOW, QuotaPeriod::Daily, QuotaBucket::Total)
        .await
        .unwrap();
    assert_eq!(total.spent_credits_micro, 0);
    assert_eq!(total.reserved_credits_micro, 0);
    assert_eq!(total.calls, 1);
    assert_eq!(
        (total.web_search_calls, total.code_interpreter_calls),
        (0, 0)
    );
}

#[tokio::test]
async fn settlement_uses_the_snapshot_of_the_applied_policy_version() {
    let h = Harness::new().await;
    let decision = reserved(&h, "standard").await;
    // A new version changes the multipliers and the tier of the model.
    let mut changed = catalog();
    for entry in &mut changed {
        if entry.id == "standard" {
            entry.input_tokens_credit_multiplier_micro = 9_000_000;
            entry.tier = ModelTier::Premium;
        }
    }
    h.policy.publish(changed, KillSwitches::default());

    let result = h
        .settle(settlement_for(
            &decision,
            TerminalState::Completed,
            None,
            Some(tokens(10, 0)),
        ))
        .await
        .unwrap();
    assert_eq!(result.committed_credits_micro, 10);
    assert_eq!(result.effective_tier, ModelTier::Standard);
    assert!(
        h.row(NOW, QuotaPeriod::Daily, QuotaBucket::Premium)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn settlement_targets_the_preflight_period_rows() {
    let h = Harness::new().await;
    let before_midnight = datetime!(2026-02-28 23:59:59 UTC);
    let decision = h
        .preflight_with("standard", request(), 0, before_midnight)
        .await
        .unwrap();
    h.reserve(&decision).await.unwrap();

    h.settle(settlement_for(
        &decision,
        TerminalState::Completed,
        None,
        Some(tokens(5, 5)),
    ))
    .await
    .unwrap();

    let day = h
        .row(before_midnight, QuotaPeriod::Daily, QuotaBucket::Total)
        .await
        .unwrap();
    assert_eq!(day.reserved_credits_micro, 0);
    assert_eq!(day.spent_credits_micro, 5 + 15);
    let month = h
        .row(before_midnight, QuotaPeriod::Monthly, QuotaBucket::Total)
        .await
        .unwrap();
    assert_eq!(month.reserved_credits_micro, 0);
    let next_day = datetime!(2026-03-01 00:00:01 UTC);
    assert!(
        h.row(next_day, QuotaPeriod::Daily, QuotaBucket::Total)
            .await
            .is_none()
    );
    assert!(
        h.row(next_day, QuotaPeriod::Monthly, QuotaBucket::Total)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn settlement_errors_are_internal() {
    let h = Harness::new().await;
    let decision = reserved(&h, "standard").await;

    let mut unknown_model = settlement_for(&decision, TerminalState::Completed, None, None);
    unknown_model.effective_model = "gone".to_owned();
    assert!(matches!(
        h.settle(unknown_model).await,
        Err(DomainError::Internal(_))
    ));

    let mut unknown_version = settlement_for(&decision, TerminalState::Completed, None, None);
    unknown_version.policy_version_applied = 99;
    assert!(matches!(
        h.settle(unknown_version).await,
        Err(DomainError::Internal(_))
    ));

    let oversized = settlement_for(
        &decision,
        TerminalState::Completed,
        None,
        Some(tokens(20_000_000, 0)),
    );
    assert!(matches!(
        h.settle(oversized).await,
        Err(DomainError::Internal(_))
    ));

    // Nothing was settled: the reserve is still booked.
    let total = h
        .row(NOW, QuotaPeriod::Daily, QuotaBucket::Total)
        .await
        .unwrap();
    assert_eq!(
        total.reserved_credits_micro,
        decision.reserved_credits_micro
    );
    assert_eq!(total.calls, 0);
}

// ── Status and warnings ──────────────────────────────────────────────────

fn ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(USER)
        .subject_tenant_id(TENANT)
        .build()
        .unwrap()
}

#[tokio::test]
async fn status_reports_remaining_quota_per_tier_and_period() {
    let h = Harness::new().await;
    h.policy.set_limits(tier(1000, 10_000), tier(500, 0));
    h.seed(QuotaPeriod::Daily, QuotaBucket::Total, 700, 101)
        .await;
    h.seed(QuotaPeriod::Monthly, QuotaBucket::Total, 2000, 0)
        .await;
    h.seed(QuotaPeriod::Daily, QuotaBucket::Premium, 600, 0)
        .await;

    let status = h.service.status_at(&ctx(), NOW).await.unwrap();
    assert_eq!(status.warning_threshold_pct, 80);
    let tiers: Vec<_> = status.tiers.iter().map(|t| t.tier.status_tier()).collect();
    assert_eq!(tiers, ["premium", "total"]);

    // Premium: monthly limit 0 is skipped; daily is over its limit.
    let premium = &status.tiers[0];
    assert_eq!(premium.periods.len(), 1);
    let premium_daily = premium.periods[0];
    assert_eq!(premium_daily.period, QuotaPeriod::Daily);
    assert_eq!(premium_daily.used_credits_micro, 600);
    assert_eq!(premium_daily.remaining_credits_micro, 0);
    assert_eq!(premium_daily.remaining_percentage, 0);
    assert!(premium_daily.warning && premium_daily.exhausted);

    let total = &status.tiers[1];
    let daily = total.periods[0];
    assert_eq!(daily.limit_credits_micro, 1000);
    assert_eq!(daily.used_credits_micro, 801);
    assert_eq!(daily.remaining_credits_micro, 199);
    assert_eq!(daily.remaining_percentage, 19);
    assert!(daily.warning);
    assert!(!daily.exhausted);
    assert_eq!(daily.next_reset, datetime!(2026-10-12 00:00 UTC));
    let monthly = total.periods[1];
    assert_eq!(monthly.remaining_percentage, 80);
    assert!(!monthly.warning);
    assert_eq!(monthly.next_reset, datetime!(2026-11-01 00:00 UTC));
}

#[tokio::test]
async fn status_warning_starts_at_the_threshold() {
    let h = Harness::new().await;
    h.policy.set_limits(tier(1000, 0), tier(0, 0));
    h.seed(QuotaPeriod::Daily, QuotaBucket::Total, 800, 0).await;
    let status = h.service.status_at(&ctx(), NOW).await.unwrap();
    let daily = status.tiers[1].periods[0];
    assert_eq!(daily.remaining_percentage, 20);
    assert!(daily.warning);
    assert!(status.tiers[0].periods.is_empty());
}

#[tokio::test]
async fn status_is_denied_without_quota_constraints() {
    let h = Harness::new().await;
    let denied = QuotaService::new(
        Arc::clone(&h.store.repo) as Arc<dyn QuotaRepository>,
        Arc::clone(&h.policy) as Arc<dyn MiniChatModelPolicyPluginClientV1>,
        allow_all_authz(),
        settings(),
    );
    assert!(matches!(
        denied.status_at(&ctx(), NOW).await,
        Err(DomainError::PermissionDenied { .. })
    ));
}

#[tokio::test]
async fn warnings_carry_next_reset_only_when_warning_or_exhausted() {
    let h = Harness::new().await;
    h.seed(QuotaPeriod::Daily, QuotaBucket::Total, 950, 0).await;
    h.seed(QuotaPeriod::Daily, QuotaBucket::Premium, 100, 0)
        .await;
    let limits = UserLimits {
        user_id: USER,
        policy_version: 1,
        standard: tier(1000, 100_000),
        premium: tier(100, -1),
    };
    let warnings = h.service.warnings_for(OWNER, &limits, NOW).await.unwrap();

    let summary: Vec<_> = warnings
        .iter()
        .map(|w| {
            (
                w.tier.status_tier(),
                w.period.as_str(),
                w.remaining_percentage,
                w.warning,
                w.exhausted,
                w.next_reset,
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (
                "premium",
                "daily",
                0,
                true,
                true,
                Some(datetime!(2026-10-12 00:00 UTC))
            ),
            (
                "total",
                "daily",
                5,
                true,
                false,
                Some(datetime!(2026-10-12 00:00 UTC))
            ),
            ("total", "monthly", 100, false, false, None),
        ]
    );
}

#[tokio::test]
async fn load_policy_returns_version_snapshot_and_limits() {
    let h = Harness::new().await;
    let policy = h.service.load_policy(USER).await.unwrap();
    assert_eq!(policy.policy_version, 1);
    assert_eq!(policy.snapshot.policy_version, 1);
    assert_eq!(policy.user_limits.policy_version, 1);
    assert_eq!(policy.user_limits.user_id, USER);
}
