#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]
//! Fixtures of the quota unit tests: catalog and snapshot builders, a fake
//! policy catalog and a fake quota repository with call logs, a recording
//! metrics sink, and the service assembled over them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverApi;
use mini_chat_sdk::{
    KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, PolicyVersionInfo, TierLimits,
    UserLimits,
};
use serde_json::{Value, json};
use time::{Date, Month, OffsetDateTime};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::finalize::{FinalizeRequest, FinalizeResult};
use super::periods::PeriodKeys;
use super::policy::SettlementSnapshot;
use super::ports::{PreflightLabel, PreflightRead, PreflightRows, QuotaMetrics, QuotaRepository};
use super::preflight::PreflightRequest;
use super::reserve::{ReserveRequest, ReservedColumns};
use super::service::{Clock, QuotaDeps, QuotaService, QuotaServiceConfig};
use super::types::{BucketRow, QuotaPeriod};
use crate::config::QuotaConfig;
use crate::domain::authz::AuthzService;
use crate::domain::error::DomainError;
use crate::domain::ports::PolicyCatalog;
use crate::test_doubles::{FakePdp, PdpReply, USER, USER_TENANT, entry, entry_json};

// ---- time --------------------------------------------------------------------------------------

/// A UTC instant.
pub fn utc(year: i32, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> OffsetDateTime {
    Date::from_calendar_date(year, Month::try_from(month).unwrap(), day)
        .unwrap()
        .with_hms(hour, minute, second)
        .unwrap()
        .assume_utc()
}

/// A calendar date.
pub fn date(year: i32, month: u8, day: u8) -> Date {
    Date::from_calendar_date(year, Month::try_from(month).unwrap(), day).unwrap()
}

/// The clock instant of the service tests: 2026-03-10T12:00:00Z.
pub fn noon() -> OffsetDateTime {
    utc(2026, 3, 10, 12, 0, 0)
}

/// Period keys of [`noon`].
pub fn noon_keys() -> PeriodKeys {
    PeriodKeys {
        daily_start: date(2026, 3, 10),
        monthly_start: date(2026, 3, 1),
    }
}

/// A fixed clock.
pub fn fixed_clock(at: OffsetDateTime) -> Clock {
    Arc::new(move || at)
}

// ---- catalog -----------------------------------------------------------------------------------

/// Builder of a catalog entry on top of the shared `entry_json` fixture.
pub struct ModelBuilder(Value);

/// A model of `tier`, enabled, default, with the multipliers
/// `1_000_000_000` / `1_000_000_000` (one credit per 1K tokens), `max_output_tokens`
/// 500 and the estimation budgets 1 / 0 / 0 / 1000 / 500 / 500 / 1000 (so the
/// estimated text tokens equal the message bytes), all tools supported.
pub fn model(id: &str, tier: ModelTier) -> ModelBuilder {
    let mut value = entry_json(id, true);
    value["tier"] = json!(match tier {
        ModelTier::Premium => "premium",
        ModelTier::Standard => "standard",
    });
    value["input_tokens_credit_multiplier_micro"] = json!(1_000_000_000_u64);
    value["output_tokens_credit_multiplier_micro"] = json!(1_000_000_000_u64);
    value["max_output_tokens"] = json!(500);
    value["estimation_budgets"] = json!({
        "bytes_per_token_conservative": 1,
        "fixed_overhead_tokens": 0,
        "safety_margin_pct": 0,
        "image_token_budget": 1000,
        "tool_surcharge_tokens": 500,
        "web_search_surcharge_tokens": 500,
        "code_interpreter_surcharge_tokens": 1000,
        "minimal_generation_floor": 50
    });
    value["preference"] = json!({ "is_default": true, "sort_order": 1 });
    ModelBuilder(value)
}

impl ModelBuilder {
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.0["enabled"] = json!(enabled);
        self
    }

    /// `preference.is_default`; `None` removes the preference.
    pub fn default_flag(mut self, flag: Option<bool>) -> Self {
        self.0["preference"] = match flag {
            Some(is_default) => json!({ "is_default": is_default, "sort_order": 1 }),
            None => Value::Null,
        };
        self
    }

    pub fn mult(mut self, input: u64, output: u64) -> Self {
        self.0["input_tokens_credit_multiplier_micro"] = json!(input);
        self.0["output_tokens_credit_multiplier_micro"] = json!(output);
        self
    }

    pub fn max_output(mut self, tokens: u32) -> Self {
        self.0["max_output_tokens"] = json!(tokens);
        self
    }

    /// `(bytes_per_token_conservative, fixed_overhead_tokens, safety_margin_pct)`.
    pub fn text_budgets(mut self, ratio: u32, overhead: u32, margin: u32) -> Self {
        self.0["estimation_budgets"]["bytes_per_token_conservative"] = json!(ratio);
        self.0["estimation_budgets"]["fixed_overhead_tokens"] = json!(overhead);
        self.0["estimation_budgets"]["safety_margin_pct"] = json!(margin);
        self
    }

    /// `(image, tool, web_search, code_interpreter)` surcharge tokens.
    pub fn surcharges(mut self, image: u32, tool: u32, web: u32, code: u32) -> Self {
        let budgets = &mut self.0["estimation_budgets"];
        budgets["image_token_budget"] = json!(image);
        budgets["tool_surcharge_tokens"] = json!(tool);
        budgets["web_search_surcharge_tokens"] = json!(web);
        budgets["code_interpreter_surcharge_tokens"] = json!(code);
        self
    }

    /// `(web_search, file_search, code_interpreter)` support flags.
    pub fn tools(mut self, web: bool, file: bool, code: bool) -> Self {
        let tools = &mut self.0["general_config"]["tool_support"];
        tools["web_search"] = json!(web);
        tools["file_search"] = json!(file);
        tools["code_interpreter"] = json!(code);
        self
    }

    pub fn vision(mut self, vision: bool) -> Self {
        self.0["multimodal_capabilities"] = if vision {
            json!(["VISION_INPUT", "RAG"])
        } else {
            json!(["RAG"])
        };
        self
    }

    pub fn provider(mut self, provider_id: &str) -> Self {
        self.0["provider_id"] = json!(provider_id);
        self
    }

    pub fn build(self) -> ModelCatalogEntry {
        entry(self.0)
    }
}

/// All six kill switches off.
pub const NO_SWITCHES: KillSwitches = KillSwitches {
    disable_premium_tier: false,
    force_standard_tier: false,
    disable_web_search: false,
    disable_file_search: false,
    disable_images: false,
    disable_code_interpreter: false,
};

/// A snapshot of `version` without kill switches.
pub fn snapshot(version: u64, catalog: Vec<ModelCatalogEntry>) -> PolicySnapshot {
    PolicySnapshot {
        policy_version: version,
        model_catalog: catalog,
        kill_switches: NO_SWITCHES,
    }
}

/// The same snapshot with the given kill switches.
pub fn with_switches(mut snapshot: PolicySnapshot, switches: KillSwitches) -> PolicySnapshot {
    snapshot.kill_switches = switches;
    snapshot
}

/// User limits of `version` (the `total` bucket is `standard`).
pub fn limits(
    version: u64,
    total_daily: i64,
    total_monthly: i64,
    premium_daily: i64,
    premium_monthly: i64,
) -> UserLimits {
    UserLimits {
        user_id: USER,
        policy_version: version,
        standard: TierLimits {
            limit_daily_credits_micro: total_daily,
            limit_monthly_credits_micro: total_monthly,
        },
        premium: TierLimits {
            limit_daily_credits_micro: premium_daily,
            limit_monthly_credits_micro: premium_monthly,
        },
    }
}

/// The limits of DESIGN 5.10: premium `22_000_000` / `300_000_000`, total
/// `60_000_000` / `600_000_000`.
pub fn design_limits(version: u64) -> UserLimits {
    limits(version, 60_000_000, 600_000_000, 22_000_000, 300_000_000)
}

/// The catalog of DESIGN 5.10: premium model `p` with the multipliers
/// `2_500_000_000` / `2_500_000_000` and standard model `s` with `1_000_000_000` /
/// `1_000_000_000`, both with `max_output_tokens` 500.
pub fn design_catalog() -> Vec<ModelCatalogEntry> {
    vec![
        model("p", ModelTier::Premium)
            .mult(2_500_000_000, 2_500_000_000)
            .build(),
        model("s", ModelTier::Standard).build(),
    ]
}

/// The bucket rows of DESIGN 5.10: `tier:premium` spent `20_000_000` daily and
/// `200_000_000` monthly, `total` spent `25_000_000` daily and `240_000_000`
/// monthly, nothing reserved.
pub fn design_rows() -> Vec<BucketRow> {
    use super::types::QuotaBucket::{TierPremium, Total};
    use QuotaPeriod::{Daily, Monthly};
    vec![
        BucketRow::new(Daily, Total, 25_000_000, 0),
        BucketRow::new(Daily, TierPremium, 20_000_000, 0),
        BucketRow::new(Monthly, Total, 240_000_000, 0),
        BucketRow::new(Monthly, TierPremium, 200_000_000, 0),
    ]
}

/// A request of the user with 1000 message bytes (1000 estimated text tokens
/// with the budgets of [`model`]), no images, no tools.
pub fn request(selected_model: &str) -> PreflightRequest {
    PreflightRequest {
        tenant_id: USER_TENANT,
        user_id: USER,
        chat_id: Uuid::from_u128(0xC1),
        selected_model: selected_model.to_owned(),
        message_bytes: 1000,
        image_count: 0,
        web_search_enabled: false,
        has_ready_file_search_documents: false,
        has_ready_code_interpreter_files: false,
    }
}

// ---- fake policy catalog --------------------------------------------------------------------------

/// One call of the fake policy catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyCall {
    Version(Uuid),
    Snapshot(Uuid, u64),
    Limits(Uuid, u64),
}

/// Which call fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyStep {
    Version,
    Snapshot,
    Limits,
}

/// In-memory `PolicyCatalog` with versioned snapshots, versioned limits, a call
/// log and per-step failures.
pub struct FakePolicy {
    pub current: Mutex<u64>,
    pub snapshots: Mutex<HashMap<u64, PolicySnapshot>>,
    pub limits: Mutex<HashMap<u64, UserLimits>>,
    pub failing: Mutex<Vec<PolicyStep>>,
    /// Steps that fail on their next call only (one entry, one failure).
    pub transient: Mutex<Vec<PolicyStep>>,
    /// Snapshots of one `(user, version)`, taking precedence over `snapshots`.
    pub user_snapshots: Mutex<HashMap<(Uuid, u64), PolicySnapshot>>,
    pub calls: Mutex<Vec<PolicyCall>>,
}

impl FakePolicy {
    /// Current version 7 serving `snapshot` and `limits` for it.
    pub fn serving(snapshot: PolicySnapshot, limits: UserLimits) -> Arc<Self> {
        let version = snapshot.policy_version;
        let this = Self {
            current: Mutex::new(version),
            snapshots: Mutex::new(HashMap::new()),
            limits: Mutex::new(HashMap::new()),
            failing: Mutex::new(Vec::new()),
            transient: Mutex::new(Vec::new()),
            user_snapshots: Mutex::new(HashMap::new()),
            calls: Mutex::new(Vec::new()),
        };
        this.snapshots.lock().unwrap().insert(version, snapshot);
        this.limits.lock().unwrap().insert(version, limits);
        Arc::new(this)
    }

    pub fn add_snapshot(&self, snapshot: PolicySnapshot) {
        self.snapshots
            .lock()
            .unwrap()
            .insert(snapshot.policy_version, snapshot);
    }

    /// The snapshot served to `user` for its version, instead of the shared one.
    pub fn add_user_snapshot(&self, user: Uuid, snapshot: PolicySnapshot) {
        self.user_snapshots
            .lock()
            .unwrap()
            .insert((user, snapshot.policy_version), snapshot);
    }

    /// The next call of `step` fails once; the calls after it succeed.
    pub fn fail_once(&self, step: PolicyStep) {
        self.transient.lock().unwrap().push(step);
    }

    pub fn set_current(&self, version: u64) {
        *self.current.lock().unwrap() = version;
    }

    pub fn fail(&self, step: PolicyStep) {
        self.failing.lock().unwrap().push(step);
    }

    pub fn calls(&self) -> Vec<PolicyCall> {
        self.calls.lock().unwrap().clone()
    }

    fn check(&self, step: PolicyStep) -> Result<(), DomainError> {
        {
            let mut transient = self.transient.lock().unwrap();
            if let Some(at) = transient.iter().position(|known| *known == step) {
                transient.remove(at);
                return Err(DomainError::PolicyResolution(format!(
                    "{step:?} failed once"
                )));
            }
        }
        if self.failing.lock().unwrap().contains(&step) {
            Err(DomainError::PolicyResolution(format!("{step:?} failed")))
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl PolicyCatalog for FakePolicy {
    async fn current_policy_version(
        &self,
        user_id: Uuid,
    ) -> Result<PolicyVersionInfo, DomainError> {
        self.calls
            .lock()
            .unwrap()
            .push(PolicyCall::Version(user_id));
        self.check(PolicyStep::Version)?;
        Ok(PolicyVersionInfo {
            policy_version: *self.current.lock().unwrap(),
            generated_at: OffsetDateTime::UNIX_EPOCH,
        })
    }

    async fn policy_snapshot(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<PolicySnapshot, DomainError> {
        self.calls
            .lock()
            .unwrap()
            .push(PolicyCall::Snapshot(user_id, policy_version));
        self.check(PolicyStep::Snapshot)?;
        if let Some(own) = self
            .user_snapshots
            .lock()
            .unwrap()
            .get(&(user_id, policy_version))
        {
            return Ok(own.clone());
        }
        self.snapshots
            .lock()
            .unwrap()
            .get(&policy_version)
            .cloned()
            .ok_or_else(|| DomainError::PolicyResolution(format!("no snapshot {policy_version}")))
    }

    async fn user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, DomainError> {
        self.calls
            .lock()
            .unwrap()
            .push(PolicyCall::Limits(user_id, policy_version));
        self.check(PolicyStep::Limits)?;
        self.limits
            .lock()
            .unwrap()
            .get(&policy_version)
            .copied()
            .ok_or_else(|| DomainError::PolicyResolution(format!("no limits {policy_version}")))
    }
}

// ---- fake repository --------------------------------------------------------------------------

/// A call of the fake repository.
#[derive(Debug, Clone, PartialEq)]
pub enum RepoCall {
    ReadPreflight(PreflightRead),
    ReadBuckets,
    Reserve,
    Finalize(Box<FinalizeRequest>),
}

/// In-memory `QuotaRepository`: serves configured rows and answers reserve and
/// finalize with configured results; logs every call.
pub struct FakeRepo {
    pub rows: Mutex<PreflightRows>,
    pub buckets: Mutex<Vec<BucketRow>>,
    pub reserve_result: Mutex<Result<ReservedColumns, String>>,
    pub finalize_result: Mutex<FinalizeResult>,
    pub calls: Mutex<Vec<RepoCall>>,
    pub read_error: Mutex<bool>,
}

impl FakeRepo {
    pub fn with_rows(buckets: Vec<BucketRow>, prior_context_tokens: i64) -> Arc<Self> {
        Arc::new(Self {
            rows: Mutex::new(PreflightRows {
                prior_context_tokens,
                buckets: buckets.clone(),
            }),
            buckets: Mutex::new(buckets),
            reserve_result: Mutex::new(Err("reserve not configured".to_owned())),
            finalize_result: Mutex::new(FinalizeResult::Lost),
            calls: Mutex::new(Vec::new()),
            read_error: Mutex::new(false),
        })
    }

    pub fn empty() -> Arc<Self> {
        Self::with_rows(Vec::new(), 0)
    }

    pub fn calls(&self) -> Vec<RepoCall> {
        self.calls.lock().unwrap().clone()
    }

    pub fn fail_reads(&self) {
        *self.read_error.lock().unwrap() = true;
    }
}

#[async_trait]
impl QuotaRepository for FakeRepo {
    async fn read_preflight(&self, read: &PreflightRead) -> Result<PreflightRows, DomainError> {
        self.calls
            .lock()
            .unwrap()
            .push(RepoCall::ReadPreflight(*read));
        if *self.read_error.lock().unwrap() {
            return Err(DomainError::Database("read failed".to_owned()));
        }
        Ok(self.rows.lock().unwrap().clone())
    }

    async fn read_buckets(
        &self,
        _scope: &AccessScope,
        _keys: PeriodKeys,
    ) -> Result<Vec<BucketRow>, DomainError> {
        self.calls.lock().unwrap().push(RepoCall::ReadBuckets);
        if *self.read_error.lock().unwrap() {
            return Err(DomainError::Database("read failed".to_owned()));
        }
        Ok(self.buckets.lock().unwrap().clone())
    }

    async fn reserve(&self, _request: &ReserveRequest<'_>) -> Result<ReservedColumns, DomainError> {
        self.calls.lock().unwrap().push(RepoCall::Reserve);
        self.reserve_result
            .lock()
            .unwrap()
            .clone()
            .map_err(DomainError::Internal)
    }

    async fn finalize_turn(
        &self,
        request: &FinalizeRequest,
    ) -> Result<FinalizeResult, DomainError> {
        self.calls
            .lock()
            .unwrap()
            .push(RepoCall::Finalize(Box::new(request.clone())));
        Ok(self.finalize_result.lock().unwrap().clone())
    }
}

// ---- recording metrics --------------------------------------------------------------------------

/// One metric observation.
#[derive(Debug, Clone, PartialEq)]
pub enum Metric {
    Preflight(PreflightLabel, String, ModelTier),
    Estimated(i64),
    Reserve(QuotaPeriod),
    Commit(QuotaPeriod),
    Actual(i64),
    Overshoot(QuotaPeriod),
    CodeInterpreter(String, u64),
}

/// Records every observation in order.
#[derive(Default)]
pub struct RecordingMetrics {
    pub events: Mutex<Vec<Metric>>,
}

impl RecordingMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn events(&self) -> Vec<Metric> {
        self.events.lock().unwrap().clone()
    }

    fn push(&self, metric: Metric) {
        self.events.lock().unwrap().push(metric);
    }
}

impl QuotaMetrics for RecordingMetrics {
    fn record_preflight(&self, decision: PreflightLabel, model: &str, tier: ModelTier) {
        self.push(Metric::Preflight(decision, model.to_owned(), tier));
    }

    fn record_estimated_tokens(&self, tokens: i64) {
        self.push(Metric::Estimated(tokens));
    }

    fn record_reserve(&self, period: QuotaPeriod) {
        self.push(Metric::Reserve(period));
    }

    fn record_commit(&self, period: QuotaPeriod) {
        self.push(Metric::Commit(period));
    }

    fn record_actual_tokens(&self, tokens: i64) {
        self.push(Metric::Actual(tokens));
    }

    fn record_overshoot(&self, period: QuotaPeriod) {
        self.push(Metric::Overshoot(period));
    }

    fn record_code_interpreter_calls(&self, model: &str, calls: u64) {
        self.push(Metric::CodeInterpreter(model.to_owned(), calls));
    }
}

// ---- the service ------------------------------------------------------------------------------

/// The service over the fakes, and handles to inspect them.
pub struct Fixture {
    pub service: QuotaService,
    pub policy: Arc<FakePolicy>,
    pub repo: Arc<FakeRepo>,
    pub metrics: Arc<RecordingMetrics>,
    pub pdp: Arc<FakePdp>,
}

/// The default quota configuration of the gear (`overshoot_tolerance_factor`
/// 1.10, `warning_threshold_pct` 80, web search 2 per message and 75 per day,
/// code interpreter 10 per message and 50 per day), `streaming.max_output_tokens`
/// 32768 and the minimal generation floor 50.
pub fn default_config() -> QuotaServiceConfig {
    QuotaServiceConfig {
        quota: QuotaConfig::default(),
        streaming_max_output_tokens: 32768,
        minimal_generation_floor: 50,
    }
}

/// Builds the service at [`noon`] over the given fakes.
pub fn fixture_with(
    policy: Arc<FakePolicy>,
    repo: Arc<FakeRepo>,
    config: QuotaServiceConfig,
) -> Fixture {
    let metrics = RecordingMetrics::new();
    let pdp = FakePdp::new(PdpReply::AllowScoped);
    let service = QuotaService::new(QuotaDeps {
        authz: Arc::new(AuthzService::with_timeout(
            Arc::clone(&pdp) as Arc<dyn AuthZResolverApi>,
            Duration::from_secs(5),
        )),
        policy: Arc::clone(&policy) as Arc<dyn PolicyCatalog>,
        repo: Arc::clone(&repo) as Arc<dyn QuotaRepository>,
        metrics: Arc::clone(&metrics) as Arc<dyn QuotaMetrics>,
        config,
    })
    .with_clock(fixed_clock(noon()));
    Fixture {
        service,
        policy,
        repo,
        metrics,
        pdp,
    }
}

/// The DESIGN 5.10 world: catalog [`design_catalog`], limits [`design_limits`],
/// rows [`design_rows`], policy version 7, no prior context.
pub fn design_fixture() -> Fixture {
    fixture_with(
        FakePolicy::serving(snapshot(7, design_catalog()), design_limits(7)),
        FakeRepo::with_rows(design_rows(), 0),
        default_config(),
    )
}

/// A fixture over `catalog`, `limits` and `rows` at policy version 7.
pub fn fixture(
    catalog: Vec<ModelCatalogEntry>,
    limits: UserLimits,
    rows: Vec<BucketRow>,
) -> Fixture {
    fixture_with(
        FakePolicy::serving(snapshot(7, catalog), limits),
        FakeRepo::with_rows(rows, 0),
        default_config(),
    )
}

/// A settlement snapshot of `catalog` at `version`.
pub fn settlement_snapshot(version: u64, catalog: Vec<ModelCatalogEntry>) -> SettlementSnapshot {
    SettlementSnapshot::Available(Box::new(snapshot(version, catalog)))
}

// ---- ready-made decisions ---------------------------------------------------------------------

/// The decision of DESIGN 5.10 after the downgrade: standard model `s`,
/// 1000 estimated input and 500 output tokens, reserve `1_500_000`, policy
/// version 7, the period keys of [`noon`] and the limits of [`design_limits`].
pub fn standard_decision() -> super::preflight::PreflightDecision {
    use super::estimate::ReserveEstimate;
    use super::preflight::{DowngradeReason, PreflightDecision, QuotaDecision};
    let catalog = design_catalog();
    let entry = catalog[1].clone();
    let provider_id = entry.provider_id.clone();
    PreflightDecision {
        effective_model: "s".to_owned(),
        entry,
        effective_tier: ModelTier::Standard,
        provider_id,
        quota_decision: QuotaDecision::Downgrade,
        downgrade_from: Some("p".to_owned()),
        downgrade_reason: Some(DowngradeReason::PremiumQuotaExhausted),
        policy_version: 7,
        snapshot: snapshot(7, catalog),
        limits: design_limits(7),
        period_keys: noon_keys(),
        reserve: ReserveEstimate {
            estimated_input_tokens: 1000,
            max_output_tokens_applied: 500,
            reserve_tokens: 1500,
            reserved_credits_micro: 1_500_000,
            minimal_generation_floor_applied: 50,
        },
        web_search_allowed: false,
        code_interpreter_allowed: false,
        vision_input: true,
    }
}

/// The allowed premium decision: model `p`, reserve `3_750_000`.
pub fn premium_decision() -> super::preflight::PreflightDecision {
    use super::preflight::QuotaDecision;
    let mut decision = standard_decision();
    let catalog = design_catalog();
    decision.entry = catalog[0].clone();
    decision.effective_model = "p".to_owned();
    decision.provider_id = decision.entry.provider_id.clone();
    decision.effective_tier = ModelTier::Premium;
    decision.quota_decision = QuotaDecision::Allow;
    decision.downgrade_from = None;
    decision.downgrade_reason = None;
    decision.reserve.reserved_credits_micro = 3_750_000;
    decision
}
