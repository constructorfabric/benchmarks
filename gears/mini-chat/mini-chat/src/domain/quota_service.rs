//! Quota service (DESIGN section 3.2 "quota service", "Quota Warning
//! Thresholds", "Quota Status Endpoint"; section 3.4 "Downgrade Decision
//! Flow", "Effective Model Computed Once"; sections 5.4-5.5, 5.7-5.9).
//!
//! - [`QuotaService::preflight`] — resolves the effective model with the
//!   two-tier downgrade cascade (premium → standard → 429), computes the
//!   reserve of the effective model and checks the daily tool quotas. The
//!   bucket rows are read in one transaction; no reserve is written.
//! - [`QuotaService::reserve_in_tx`] — books the reserve inside the caller's
//!   transaction (atomic increments, missing rows created) and re-checks the
//!   limits after the increments (TOCTOU, section 5.4.2): a bucket over its
//!   limit is `429 quota_exceeded` and the caller's transaction rolls back.
//! - [`QuotaService::settle_in_tx`] — settles a finalized turn inside the
//!   caller's finalization transaction, from the persisted turn fields and
//!   the policy snapshot of `policy_version_applied` (never the current
//!   policy, never the cascade again). The orphan watchdog uses its two
//!   halves: [`QuotaService::price_settlement`] (classifies a pricing
//!   failure as transient or permanent) and
//!   [`QuotaService::apply_settlement_in_tx`], or
//!   [`QuotaService::release_reserve_in_tx`] when the turn can never be
//!   priced (with the resolved tier when the permanent failure carries it).
//! - [`QuotaService::status`] / [`QuotaService::warnings_for`] — `GET
//!   /v1/quota/status` and `done.quota_warnings`.
//!
//! Every `quota_usage` access is confined to the turn's `(tenant_id,
//! user_id)`; the status endpoint additionally applies the `UserQuota` PEP
//! scope.

use std::collections::HashMap;
use std::sync::Arc;

use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError,
    ModelCatalogEntry, ModelTier, PolicySnapshot, QuotaDecision, UsageTokens, UserLimits,
};
use time::OffsetDateTime;
use toolkit_db::secure::DbTx;
use toolkit_macros::domain_model;
use toolkit_security::{AccessScope, SecurityContext};

use crate::config::MiniChatConfig;
use crate::domain::authz::{AuthzService, confine_to_subject};
use crate::domain::billing::{BillingDecision, SettlementMethod, TerminalState, derive_billing};
use crate::domain::error::{DomainError, PreconditionSubject, QuotaScope};
use crate::domain::ports::{
    BucketSettlement, QuotaBucketKey, QuotaOwner, QuotaRepository, QuotaUsageRecord, storage_now,
};
use crate::domain::quota::{
    Overshoot, PeriodStarts, QuotaBucket, QuotaPeriod, ReserveEstimate, ReserveInputs,
    credits_micro_checked, estimate_reserve, overshoot, remaining,
};

/// `multimodal_capabilities` flag of a model that accepts image input.
pub const VISION_INPUT: &str = "VISION_INPUT";

// ── Settings ─────────────────────────────────────────────────────────────

/// The gear configuration the quota service reads (Appendix B.4-B.6).
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaSettings {
    /// `streaming.max_output_tokens`: cap of `max_output_tokens_applied`.
    pub max_output_tokens: u32,
    /// `estimation_budgets.minimal_generation_floor`.
    pub minimal_generation_floor: u32,
    /// `quota.overshoot_tolerance_factor`.
    pub overshoot_tolerance_factor: f64,
    /// `quota.warning_threshold_pct`.
    pub warning_threshold_pct: u8,
    /// `quota.web_search_daily_quota`.
    pub web_search_daily_quota: u32,
    /// `quota.code_interpreter_daily_quota`.
    pub code_interpreter_daily_quota: u32,
}

impl QuotaSettings {
    /// The quota settings of the gear configuration.
    #[must_use]
    pub fn from_config(config: &MiniChatConfig) -> Self {
        Self {
            max_output_tokens: config.streaming.max_output_tokens,
            minimal_generation_floor: config.estimation_budgets.minimal_generation_floor,
            overshoot_tolerance_factor: config.quota.overshoot_tolerance_factor,
            warning_threshold_pct: config.quota.warning_threshold_pct,
            web_search_daily_quota: config.quota.web_search_daily_quota,
            code_interpreter_daily_quota: config.quota.code_interpreter_daily_quota,
        }
    }
}

// ── Preflight types ──────────────────────────────────────────────────────

/// Why the effective model differs from the selected one (`done.downgrade_reason`).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DowngradeReason {
    PremiumQuotaExhausted,
    ForceStandardTier,
    DisablePremiumTier,
    ModelDisabled,
}

impl DowngradeReason {
    /// Wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PremiumQuotaExhausted => "premium_quota_exhausted",
            Self::ForceStandardTier => "force_standard_tier",
            Self::DisablePremiumTier => "disable_premium_tier",
            Self::ModelDisabled => "model_disabled",
        }
    }
}

/// `done.quota_decision`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuotaDecisionKind {
    Allow,
    Downgrade,
}

impl QuotaDecisionKind {
    /// Wire value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => QuotaDecision::ALLOW,
            Self::Downgrade => QuotaDecision::DOWNGRADE,
        }
    }
}

/// What the request asks for, independent of the model.
#[domain_model]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent request facts: web search flag and the chat's ready attachment kinds"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PreflightRequest {
    /// UTF-8 size of the current user message.
    pub message_utf8_bytes: u64,
    /// Images attached to the current message.
    pub image_count: u32,
    /// `web_search.enabled` of the request.
    pub web_search_requested: bool,
    /// The chat has at least one ready document.
    pub has_ready_documents: bool,
    /// The chat has at least one ready code-interpreter (XLSX) attachment.
    pub has_ready_code_interpreter_files: bool,
}

/// Preflight inputs: the policy the turn binds to and the request.
#[domain_model]
#[derive(Debug, Clone, Copy)]
pub struct PreflightInput<'a> {
    pub owner: QuotaOwner,
    /// `chats.model`.
    pub selected_model: &'a str,
    /// Version of `snapshot`, persisted as `policy_version_applied`.
    pub policy_version: u64,
    pub snapshot: &'a PolicySnapshot,
    pub user_limits: &'a UserLimits,
    pub request: PreflightRequest,
    /// `input_tokens + output_tokens` of the latest non-deleted assistant
    /// message with non-zero usage (0 when none).
    pub prior_context_tokens: u64,
    /// Preflight time; the period starts are derived from it once.
    pub now: OffsetDateTime,
}

/// Built-in tools sent with a model for a request.
#[domain_model]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one independent gate per built-in tool"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolGates {
    /// Requested, supported by the model (the kill switch rejects earlier).
    pub web_search: bool,
    /// Ready document, model support, `disable_file_search` off.
    pub file_search: bool,
    /// Ready XLSX attachment, model support, `disable_code_interpreter` off.
    pub code_interpreter: bool,
}

impl ToolGates {
    /// The gates of `model` for `request` under `kill`.
    #[must_use]
    pub const fn for_model(
        model: &ModelCatalogEntry,
        request: &PreflightRequest,
        kill: KillSwitches,
    ) -> Self {
        let tools = &model.general_config.tool_support;
        Self {
            web_search: request.web_search_requested
                && tools.web_search
                && !kill.disable_web_search,
            file_search: request.has_ready_documents
                && tools.file_search
                && !kill.disable_file_search,
            code_interpreter: request.has_ready_code_interpreter_files
                && tools.code_interpreter
                && !kill.disable_code_interpreter,
        }
    }
}

/// The preflight decision: effective model, reserve and tool gates. Its
/// reserve fields are persisted on `chat_turns`.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct PreflightDecision {
    /// `chats.model`.
    pub selected_model: String,
    /// Catalog entry of the effective model (provider, provider model id,
    /// budgets of everything after preflight).
    pub effective_model: ModelCatalogEntry,
    pub effective_tier: ModelTier,
    pub decision: QuotaDecisionKind,
    /// `Some(selected_model)` on a downgrade.
    pub downgrade_from: Option<String>,
    /// Set on a downgrade.
    pub downgrade_reason: Option<DowngradeReason>,
    pub estimated_input_tokens: i64,
    pub max_output_tokens_applied: u32,
    pub reserve_tokens: i64,
    pub reserved_credits_micro: i64,
    /// `min(estimation_budgets.minimal_generation_floor, max_output_tokens_applied)`.
    pub minimal_generation_floor_applied: u32,
    pub policy_version_applied: u64,
    /// Period starts of the turn (settlement targets the same rows),
    /// derived from [`Self::at`].
    pub periods: PeriodStarts,
    /// The preflight instant `periods` derive from. The turn row is inserted
    /// with `started_at` equal to it, so the orphan watchdog's
    /// `PeriodStarts::at(started_at)` targets the reserved rows even when the
    /// insert happens after a day or month boundary.
    pub at: OffsetDateTime,
    /// Tools sent with the effective model.
    pub tools: ToolGates,
    /// The effective model has `VISION_INPUT`.
    pub vision_supported: bool,
    /// Limits the reserve re-check uses.
    pub user_limits: UserLimits,
}

impl PreflightDecision {
    /// Id of the effective model (`chat_turns.effective_model`).
    #[must_use]
    pub fn effective_model_id(&self) -> &str {
        &self.effective_model.id
    }

    /// The quota decision of the turn audit event.
    #[must_use]
    pub fn audit_quota_decision(&self) -> QuotaDecision {
        QuotaDecision {
            decision: self.decision.as_str().to_owned(),
            downgrade_from: self.downgrade_from.clone(),
            downgrade_reason: self.downgrade_reason.map(|r| r.as_str().to_owned()),
        }
    }
}

/// The policy a preflight binds to: current version, its snapshot and the
/// user's limits under it.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyContext {
    pub policy_version: u64,
    pub snapshot: PolicySnapshot,
    pub user_limits: UserLimits,
}

// ── Settlement types ─────────────────────────────────────────────────────

/// A finalized turn to settle: the persisted `chat_turns` reserve fields, the
/// terminal classification and the provider usage.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettlementInput {
    /// `(chats.tenant_id, chat_turns.requester_user_id)`.
    pub owner: QuotaOwner,
    /// `chat_turns.effective_model`.
    pub effective_model: String,
    /// `chat_turns.policy_version_applied`.
    pub policy_version_applied: u64,
    /// `chat_turns.reserve_tokens`.
    pub reserve_tokens: i64,
    /// `chat_turns.max_output_tokens_applied`.
    pub max_output_tokens_applied: i64,
    /// `chat_turns.reserved_credits_micro`.
    pub reserved_credits_micro: i64,
    /// `chat_turns.minimal_generation_floor_applied`.
    pub minimal_generation_floor_applied: i64,
    /// Preflight period starts (the orphan watchdog derives them from
    /// `started_at` with [`PeriodStarts::at`]).
    pub periods: PeriodStarts,
    pub terminal: TerminalState,
    /// `chat_turns.error_code`.
    pub error_code: Option<String>,
    /// Provider-reported usage; `None` when the provider reported none.
    pub usage: Option<UsageTokens>,
    /// `chat_turns.web_search_completed_count`.
    pub web_search_calls: u32,
    /// `chat_turns.code_interpreter_completed_count`.
    pub code_interpreter_calls: u32,
}

/// Outcome of a settlement.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettlementResult {
    pub billing: BillingDecision,
    /// Credits added to `spent_credits_micro`; the usage event
    /// `actual_credits_micro`.
    pub committed_credits_micro: i64,
    /// Tokens charged (`committed_tokens` / `charged_tokens`).
    pub charged_tokens: i64,
    /// Actual tokens exceeded the reserve on an actual settlement.
    pub overshoot: bool,
    /// The overshoot exceeded the tolerance and the commit was capped.
    pub overshoot_capped: bool,
    /// Tier of the effective model in the applied snapshot.
    pub effective_tier: ModelTier,
}

/// Why a settlement could not be priced.
#[derive(Debug)]
pub enum PricingError {
    /// The same call may succeed later (policy plugin `Transient` or
    /// `Internal` failure).
    Transient(DomainError),
    /// The persisted turn fields can never be priced: the plugin does not
    /// serve `policy_version_applied` for the user, the effective model is
    /// not in that snapshot, or the credit computation fails on the
    /// persisted reserve columns.
    Permanent {
        error: DomainError,
        /// Tier of the effective model in the snapshot of
        /// `policy_version_applied` when it was resolved (credit computation
        /// failure); `None` when the snapshot or the model is unavailable.
        tier: Option<ModelTier>,
    },
}

impl PricingError {
    /// The underlying [`DomainError`].
    #[must_use]
    pub fn into_domain(self) -> DomainError {
        match self {
            Self::Transient(err) | Self::Permanent { error: err, .. } => err,
        }
    }
}

/// A settlement priced against the snapshot of `policy_version_applied`,
/// ready to be written by [`QuotaService::apply_settlement_in_tx`].
pub struct PricedSettlement {
    billing: BillingDecision,
    tier: ModelTier,
    charge: Charge,
}

impl std::fmt::Debug for PricedSettlement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PricedSettlement")
            .field("billing", &self.billing)
            .field("tier", &self.tier)
            .field(
                "committed_credits_micro",
                &self.charge.committed_credits_micro,
            )
            .finish_non_exhaustive()
    }
}

// ── Status types ─────────────────────────────────────────────────────────

/// One period of one tier of the quota status.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeriodStatus {
    pub period: QuotaPeriod,
    pub limit_credits_micro: i64,
    /// `spent + reserved`.
    pub used_credits_micro: i64,
    /// `limit - used`, clamped at 0.
    pub remaining_credits_micro: i64,
    /// Floored, `0..=100`.
    pub remaining_percentage: u8,
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

/// One tier (`premium` / `total`) of the quota status.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierStatus {
    /// Wire name via [`QuotaBucket::status_tier`].
    pub tier: QuotaBucket,
    /// Periods with a positive limit.
    pub periods: Vec<PeriodStatus>,
}

/// `GET /v1/quota/status`.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaStatus {
    pub tiers: Vec<TierStatus>,
    pub warning_threshold_pct: u8,
}

/// One entry of `done.quota_warnings`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaWarning {
    /// Wire name via [`QuotaBucket::status_tier`].
    pub tier: QuotaBucket,
    pub period: QuotaPeriod,
    pub remaining_percentage: u8,
    pub warning: bool,
    pub exhausted: bool,
    /// Set only when `warning` or `exhausted`.
    pub next_reset: Option<OffsetDateTime>,
}

// ── Service ──────────────────────────────────────────────────────────────

/// Quota domain service.
pub struct QuotaService {
    repo: Arc<dyn QuotaRepository>,
    policy: Arc<dyn MiniChatModelPolicyPluginClientV1>,
    authz: Arc<AuthzService>,
    settings: QuotaSettings,
}

impl std::fmt::Debug for QuotaService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuotaService")
            .field("authz", &self.authz)
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

/// The selected candidate of the cascade.
struct Candidate<'s> {
    model: &'s ModelCatalogEntry,
    tier: ModelTier,
    tools: ToolGates,
    reserve: ReserveEstimate,
}

/// The committed charge of a settlement.
struct Charge {
    committed_credits_micro: i64,
    charged_tokens: i64,
    overshoot: Overshoot,
    /// Telemetry tokens (actual settlements only).
    input_tokens: i64,
    output_tokens: i64,
}

impl Charge {
    const ZERO: Self = Self {
        committed_credits_micro: 0,
        charged_tokens: 0,
        overshoot: Overshoot::None,
        input_tokens: 0,
        output_tokens: 0,
    };
}

impl QuotaService {
    /// Builds the service over the `quota_usage` port, the model-policy
    /// plugin client, the gear PEP and the quota settings.
    #[must_use]
    pub fn new(
        repo: Arc<dyn QuotaRepository>,
        policy: Arc<dyn MiniChatModelPolicyPluginClientV1>,
        authz: Arc<AuthzService>,
        settings: QuotaSettings,
    ) -> Self {
        Self {
            repo,
            policy,
            authz,
            settings,
        }
    }

    /// The quota settings.
    #[must_use]
    pub const fn settings(&self) -> &QuotaSettings {
        &self.settings
    }

    /// Reads the user's current policy version, its snapshot and the user's
    /// limits under it (the inputs of [`Self::preflight`]).
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] when a plugin call fails.
    pub async fn load_policy(&self, user_id: uuid::Uuid) -> Result<PolicyContext, DomainError> {
        let policy_version = self
            .policy
            .get_current_policy_version(user_id)
            .await
            .map_err(|err| plugin_error("get_current_policy_version", &err))?;
        let snapshot = self
            .policy
            .get_policy_snapshot(user_id, policy_version)
            .await
            .map_err(|err| plugin_error("get_policy_snapshot", &err))?;
        let user_limits = self
            .policy
            .get_user_limits(user_id, policy_version)
            .await
            .map_err(|err| plugin_error("get_user_limits", &err))?;
        Ok(PolicyContext {
            policy_version,
            snapshot,
            user_limits,
        })
    }

    /// Runs the quota preflight (section 3.4 "Downgrade Decision Flow").
    ///
    /// Order: the web search kill switch (before anything else), the bucket
    /// rows of both periods and buckets (one transaction), the cascade with
    /// the per-candidate reserve, then the daily tool quotas of the tools
    /// sent with the effective model. A candidate whose reserve cannot be
    /// computed is logged and treated as unavailable.
    ///
    /// # Errors
    ///
    /// `400 failed_precondition` (`web_search`, `FEATURE_DISABLED`) when web
    /// search is requested under `disable_web_search`; `429 quota_exceeded`
    /// with scope `tokens` when no tier is available, `web_search` /
    /// `code_interpreter` when a daily tool quota is used up;
    /// [`DomainError::Internal`] on a storage failure.
    pub async fn preflight(
        &self,
        input: PreflightInput<'_>,
    ) -> Result<PreflightDecision, DomainError> {
        let kill = input.snapshot.kill_switches;
        if input.request.web_search_requested && kill.disable_web_search {
            return Err(DomainError::failed_precondition(
                PreconditionSubject::WebSearch,
            ));
        }
        let periods = PeriodStarts::at(input.now);
        let usage = BucketUsage::new(
            self.repo
                .read_buckets(
                    &owner_scope(input.owner),
                    input.owner,
                    &all_keys(periods),
                    true,
                )
                .await?,
        );

        let (start, mut reason) = start_tier(input.snapshot, input.selected_model);
        let mut chosen = None;
        for &tier in cascade(start) {
            if tier == ModelTier::Premium
                && let Some(skip) = premium_kill_switch(kill)
            {
                reason.get_or_insert(skip);
                continue;
            }
            let Some(model) = candidate(input.snapshot, tier, input.selected_model) else {
                continue;
            };
            let tools = ToolGates::for_model(model, &input.request, kill);
            if let Some(reserve) = self.candidate_reserve(model, &input, tools)
                && usage.tier_available(tier, input.user_limits, reserve.reserved_credits_micro)
            {
                chosen = Some(Candidate {
                    model,
                    tier,
                    tools,
                    reserve,
                });
                break;
            }
            if tier == ModelTier::Premium {
                reason.get_or_insert(DowngradeReason::PremiumQuotaExhausted);
            }
        }
        let Some(chosen) = chosen else {
            return Err(DomainError::quota_exceeded(QuotaScope::Tokens));
        };
        self.check_tool_quotas(&usage, chosen.tools)?;
        Ok(self.decision(&input, &chosen, reason, periods))
    }

    fn candidate_reserve(
        &self,
        model: &ModelCatalogEntry,
        input: &PreflightInput<'_>,
        tools: ToolGates,
    ) -> Option<ReserveEstimate> {
        let inputs = ReserveInputs {
            message_utf8_bytes: input.request.message_utf8_bytes,
            image_count: input.request.image_count,
            prior_context_tokens: input.prior_context_tokens,
            file_search: tools.file_search,
            web_search: tools.web_search,
            code_interpreter: tools.code_interpreter,
        };
        estimate_reserve(model, &inputs, self.settings.max_output_tokens)
            .inspect_err(|err| {
                tracing::warn!(
                    target: "mini_chat.quota",
                    model = %model.id,
                    error = %err,
                    "candidate reserve cannot be computed; candidate unavailable"
                );
            })
            .ok()
    }

    fn check_tool_quotas(&self, usage: &BucketUsage, tools: ToolGates) -> Result<(), DomainError> {
        let daily_total = usage.record(QuotaPeriod::Daily, QuotaBucket::Total);
        let web_search_calls = daily_total.map_or(0, |row| row.web_search_calls);
        if tools.web_search
            && i64::from(web_search_calls) >= i64::from(self.settings.web_search_daily_quota)
        {
            return Err(DomainError::quota_exceeded(QuotaScope::WebSearch));
        }
        let code_interpreter_calls = daily_total.map_or(0, |row| row.code_interpreter_calls);
        if tools.code_interpreter
            && i64::from(code_interpreter_calls)
                >= i64::from(self.settings.code_interpreter_daily_quota)
        {
            return Err(DomainError::quota_exceeded(QuotaScope::CodeInterpreter));
        }
        Ok(())
    }

    fn decision(
        &self,
        input: &PreflightInput<'_>,
        chosen: &Candidate<'_>,
        reason: Option<DowngradeReason>,
        periods: PeriodStarts,
    ) -> PreflightDecision {
        let downgraded = chosen.model.id != input.selected_model || reason.is_some();
        let (decision, downgrade_from, downgrade_reason) = if downgraded {
            (
                QuotaDecisionKind::Downgrade,
                Some(input.selected_model.to_owned()),
                reason,
            )
        } else {
            (QuotaDecisionKind::Allow, None, None)
        };
        let reserve = chosen.reserve;
        PreflightDecision {
            selected_model: input.selected_model.to_owned(),
            effective_model: chosen.model.clone(),
            effective_tier: chosen.tier,
            decision,
            downgrade_from,
            downgrade_reason,
            estimated_input_tokens: reserve.estimated_input_tokens,
            max_output_tokens_applied: reserve.max_output_tokens_applied,
            reserve_tokens: reserve.reserve_tokens,
            reserved_credits_micro: reserve.reserved_credits_micro,
            minimal_generation_floor_applied: self
                .settings
                .minimal_generation_floor
                .min(reserve.max_output_tokens_applied),
            policy_version_applied: input.policy_version,
            periods,
            at: input.now,
            tools: chosen.tools,
            vision_supported: chosen
                .model
                .multimodal_capabilities
                .iter()
                .any(|capability| capability == VISION_INPUT),
            user_limits: *input.user_limits,
        }
    }

    /// Books the reserve of `decision` inside `tx` and re-checks the limits
    /// (section 5.4.2 "TOCTOU"): `reserved_credits_micro` of bucket `total`
    /// (and `tier:premium` for a premium turn) of both periods is incremented
    /// first, then the rows are read back and every bucket must satisfy
    /// `spent + reserved <= limit`.
    ///
    /// # Errors
    ///
    /// `429 quota_exceeded` (scope `tokens`) when a bucket is over its limit
    /// after the increments — the caller must roll `tx` back;
    /// [`DomainError::Internal`] on a storage failure.
    pub async fn reserve_in_tx(
        &self,
        tx: &DbTx<'_>,
        decision: &PreflightDecision,
        owner: QuotaOwner,
    ) -> Result<(), DomainError> {
        let keys = bucket_keys(decision.effective_tier, decision.periods);
        let scope = owner_scope(owner);
        self.repo
            .add_reserve_in_tx(
                tx,
                &scope,
                owner,
                &keys,
                decision.reserved_credits_micro,
                storage_now(),
            )
            .await?;
        let usage = BucketUsage::new(
            self.repo
                .read_buckets_in_tx(tx, &scope, owner, &keys)
                .await?,
        );
        if usage.tier_available(decision.effective_tier, &decision.user_limits, 0) {
            Ok(())
        } else {
            tracing::info!(
                target: "mini_chat.quota",
                model = %decision.effective_model.id,
                "reserve re-check over the limit; reserve rejected"
            );
            Err(DomainError::quota_exceeded(QuotaScope::Tokens))
        }
    }

    /// Settles a finalized turn inside `tx` (section 5.4.4, section 3.7
    /// "Commit semantics").
    ///
    /// The billing outcome comes from [`derive_billing`]; multipliers and tier
    /// come from the snapshot of `policy_version_applied` (fetched from the
    /// policy plugin). `actual`: provider usage, capped at the reserve when
    /// the overshoot exceeds `quota.overshoot_tolerance_factor`; `estimated`:
    /// `credits_micro(reserve_tokens - max_output_tokens_applied,
    /// minimal_generation_floor_applied)`; `released`: 0. The rows of the
    /// stored period starts get `reserved -= turn reserve`, `spent +=
    /// committed`, `calls += 1`; bucket `total` also gets the actual tokens
    /// (actual only) and the tool calls (actual and estimated).
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] when the snapshot cannot be loaded, the
    /// effective model is not in it, the credit computation fails (logged at
    /// `warn`) or a write fails; the caller's transaction must roll back.
    pub async fn settle_in_tx(
        &self,
        tx: &DbTx<'_>,
        input: &SettlementInput,
    ) -> Result<SettlementResult, DomainError> {
        let priced = self
            .price_settlement(input)
            .await
            .map_err(PricingError::into_domain)?;
        self.apply_settlement_in_tx(tx, input, priced).await
    }

    /// The pricing half of [`QuotaService::settle_in_tx`]: billing outcome,
    /// snapshot of `policy_version_applied`, tier and charge; no write.
    ///
    /// # Errors
    ///
    /// [`PricingError::Transient`] on a retryable plugin failure;
    /// [`PricingError::Permanent`] when the snapshot or the user is unknown
    /// to the plugin, the effective model is not in the snapshot, or the
    /// credit computation fails.
    pub async fn price_settlement(
        &self,
        input: &SettlementInput,
    ) -> Result<PricedSettlement, PricingError> {
        let billing = derive_billing(
            input.terminal,
            input.error_code.as_deref(),
            input.usage.as_ref(),
        );
        let model = self.applied_model(input).await?;
        let charge = self
            .charge(input, billing.method, &model)
            .map_err(|error| PricingError::Permanent {
                error,
                tier: Some(model.tier),
            })?;
        Ok(PricedSettlement {
            billing,
            tier: model.tier,
            charge,
        })
    }

    /// The write half of [`QuotaService::settle_in_tx`]: the bucket rows of
    /// the stored period starts get the priced settlement inside `tx`.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] when a write fails; the caller's
    /// transaction must roll back.
    pub async fn apply_settlement_in_tx(
        &self,
        tx: &DbTx<'_>,
        input: &SettlementInput,
        priced: PricedSettlement,
    ) -> Result<SettlementResult, DomainError> {
        let PricedSettlement {
            billing,
            tier,
            charge,
        } = priced;
        let settlements = settlements(input, billing.method, tier, &charge);
        self.repo
            .settle_in_tx(
                tx,
                &owner_scope(input.owner),
                input.owner,
                &settlements,
                storage_now(),
            )
            .await?;
        Ok(SettlementResult {
            billing,
            committed_credits_micro: charge.committed_credits_micro,
            charged_tokens: charge.charged_tokens,
            overshoot: charge.overshoot != Overshoot::None,
            overshoot_capped: charge.overshoot == Overshoot::Capped,
            effective_tier: tier,
        })
    }

    /// Releases the reserve of a turn that can never be priced
    /// ([`PricingError::Permanent`]) inside `tx`: the buckets of `tier`
    /// (`total`, and `tier:premium` for a premium turn) of both stored
    /// periods get `reserved -= reserved_credits_micro` (clamped at zero),
    /// `calls += 1`, no spend, no tokens, no tool calls. `tier` is the tier
    /// carried by the permanent pricing error; when it is `None` (snapshot or
    /// model unavailable) only bucket `total` is released and a
    /// `tier:premium` reserve ends with its period.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] when a write fails; the caller's
    /// transaction must roll back.
    pub async fn release_reserve_in_tx(
        &self,
        tx: &DbTx<'_>,
        input: &SettlementInput,
        tier: Option<ModelTier>,
    ) -> Result<(), DomainError> {
        let settlements = settlements(
            input,
            SettlementMethod::Released,
            tier.unwrap_or(ModelTier::Standard),
            &Charge::ZERO,
        );
        self.repo
            .settle_in_tx(
                tx,
                &owner_scope(input.owner),
                input.owner,
                &settlements,
                storage_now(),
            )
            .await
    }

    async fn applied_model(
        &self,
        input: &SettlementInput,
    ) -> Result<ModelCatalogEntry, PricingError> {
        let snapshot = self
            .policy
            .get_policy_snapshot(input.owner.user_id, input.policy_version_applied)
            .await
            .map_err(|err| {
                let domain = plugin_error("get_policy_snapshot", &err);
                match err {
                    MiniChatModelPolicyPluginError::VersionNotFound { .. }
                    | MiniChatModelPolicyPluginError::UserNotFound { .. } => {
                        PricingError::Permanent {
                            error: domain,
                            tier: None,
                        }
                    }
                    MiniChatModelPolicyPluginError::Transient(_)
                    | MiniChatModelPolicyPluginError::Internal(_) => {
                        PricingError::Transient(domain)
                    }
                }
            })?;
        snapshot
            .model_catalog
            .into_iter()
            .find(|entry| entry.id == input.effective_model)
            .ok_or_else(|| PricingError::Permanent {
                error: DomainError::internal(format!(
                    "effective model `{}` is not in policy version {}",
                    input.effective_model, input.policy_version_applied
                )),
                tier: None,
            })
    }

    fn charge(
        &self,
        input: &SettlementInput,
        method: SettlementMethod,
        model: &ModelCatalogEntry,
    ) -> Result<Charge, DomainError> {
        let in_mult = model.input_tokens_credit_multiplier_micro;
        let out_mult = model.output_tokens_credit_multiplier_micro;
        match method {
            SettlementMethod::Actual => {
                let usage = input.usage.unwrap_or_default();
                let actual = credits_micro_checked(
                    usage.input_tokens,
                    usage.output_tokens,
                    in_mult,
                    out_mult,
                )
                .map_err(|err| settlement_error(input, &err))?;
                let actual_tokens = usage.input_tokens.saturating_add(usage.output_tokens);
                let outcome = overshoot(
                    actual_tokens,
                    input.reserve_tokens,
                    self.settings.overshoot_tolerance_factor,
                );
                let (committed_credits_micro, charged_tokens) = if outcome == Overshoot::Capped {
                    (input.reserved_credits_micro, input.reserve_tokens)
                } else {
                    (actual, actual_tokens)
                };
                Ok(Charge {
                    committed_credits_micro,
                    charged_tokens,
                    overshoot: outcome,
                    input_tokens: usage.input_tokens,
                    output_tokens: usage.output_tokens,
                })
            }
            SettlementMethod::Estimated => {
                let estimated_input = input
                    .reserve_tokens
                    .checked_sub(input.max_output_tokens_applied)
                    .filter(|tokens| *tokens >= 0)
                    .ok_or_else(|| {
                        DomainError::internal(format!(
                            "turn reserve_tokens {} is below max_output_tokens_applied {}",
                            input.reserve_tokens, input.max_output_tokens_applied
                        ))
                    })?;
                let floor = input.minimal_generation_floor_applied;
                let committed_credits_micro =
                    credits_micro_checked(estimated_input, floor, in_mult, out_mult)
                        .map_err(|err| settlement_error(input, &err))?;
                Ok(Charge {
                    committed_credits_micro,
                    charged_tokens: input
                        .reserve_tokens
                        .min(estimated_input.saturating_add(floor)),
                    ..Charge::ZERO
                })
            }
            SettlementMethod::Released => Ok(Charge::ZERO),
        }
    }

    /// `GET /v1/quota/status` for the caller at the current time.
    ///
    /// # Errors
    ///
    /// PEP errors of [`AuthzService::authorize_user_quota`];
    /// [`DomainError::Internal`] on plugin or storage failure.
    pub async fn status(&self, ctx: &SecurityContext) -> Result<QuotaStatus, DomainError> {
        self.status_at(ctx, OffsetDateTime::now_utc()).await
    }

    /// `GET /v1/quota/status` for the caller at `now`: tiers `premium` and
    /// `total` under the user's current limits; periods with a limit `<= 0`
    /// are skipped.
    ///
    /// # Errors
    ///
    /// PEP errors of [`AuthzService::authorize_user_quota`];
    /// [`DomainError::Internal`] on plugin or storage failure.
    pub async fn status_at(
        &self,
        ctx: &SecurityContext,
        now: OffsetDateTime,
    ) -> Result<QuotaStatus, DomainError> {
        let scope = self.authz.authorize_user_quota(ctx).await?;
        let owner = QuotaOwner {
            tenant_id: ctx.subject_tenant_id(),
            user_id: ctx.subject_id(),
        };
        let policy_version = self
            .policy
            .get_current_policy_version(owner.user_id)
            .await
            .map_err(|err| plugin_error("get_current_policy_version", &err))?;
        let limits = self
            .policy
            .get_user_limits(owner.user_id, policy_version)
            .await
            .map_err(|err| plugin_error("get_user_limits", &err))?;
        let usage = self.usage_at(&scope, owner, now).await?;
        let tiers = QuotaBucket::ALL
            .iter()
            .map(|&bucket| TierStatus {
                tier: bucket,
                periods: QuotaPeriod::ALL
                    .iter()
                    .filter_map(|&period| self.period_status(&usage, bucket, period, &limits, now))
                    .collect(),
            })
            .collect();
        Ok(QuotaStatus {
            tiers,
            warning_threshold_pct: self.settings.warning_threshold_pct,
        })
    }

    /// `done.quota_warnings` of `owner` under `limits` at `now` (the same
    /// data as the status; `next_reset` only on warning or exhaustion).
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] on a storage failure.
    pub async fn warnings_for(
        &self,
        owner: QuotaOwner,
        limits: &UserLimits,
        now: OffsetDateTime,
    ) -> Result<Vec<QuotaWarning>, DomainError> {
        let usage = &self.usage_at(&owner_scope(owner), owner, now).await?;
        Ok(QuotaBucket::ALL
            .iter()
            .flat_map(|&bucket| {
                QuotaPeriod::ALL
                    .iter()
                    .filter_map(move |&period| {
                        self.period_status(usage, bucket, period, limits, now)
                    })
                    .map(move |status| QuotaWarning {
                        tier: bucket,
                        period: status.period,
                        remaining_percentage: status.remaining_percentage,
                        warning: status.warning,
                        exhausted: status.exhausted,
                        next_reset: (status.warning || status.exhausted)
                            .then_some(status.next_reset),
                    })
            })
            .collect())
    }

    async fn usage_at(
        &self,
        scope: &AccessScope,
        owner: QuotaOwner,
        now: OffsetDateTime,
    ) -> Result<BucketUsage, DomainError> {
        let keys = all_keys(PeriodStarts::at(now));
        Ok(BucketUsage::new(
            self.repo.read_buckets(scope, owner, &keys, false).await?,
        ))
    }

    fn period_status(
        &self,
        usage: &BucketUsage,
        bucket: QuotaBucket,
        period: QuotaPeriod,
        limits: &UserLimits,
        now: OffsetDateTime,
    ) -> Option<PeriodStatus> {
        let limit = period.limit(&bucket.limits(limits));
        let used = usage.used(period, bucket);
        let left = remaining(limit, used)?;
        let warning_at = 100_u8.saturating_sub(self.settings.warning_threshold_pct);
        Some(PeriodStatus {
            period,
            limit_credits_micro: limit,
            used_credits_micro: used,
            remaining_credits_micro: left.credits_micro,
            remaining_percentage: left.percentage,
            next_reset: period.next_reset(now),
            warning: left.percentage <= warning_at,
            exhausted: left.percentage == 0,
        })
    }
}

// ── Cascade helpers ──────────────────────────────────────────────────────

/// Starting tier and initial downgrade reason (step 1).
fn start_tier(snapshot: &PolicySnapshot, selected: &str) -> (ModelTier, Option<DowngradeReason>) {
    match snapshot
        .model_catalog
        .iter()
        .find(|entry| entry.id == selected)
    {
        Some(entry) if entry.enabled => (entry.tier, None),
        Some(entry) => (entry.tier, Some(DowngradeReason::ModelDisabled)),
        None => (ModelTier::Premium, Some(DowngradeReason::ModelDisabled)),
    }
}

/// The cascade from `start` downward (step 2); never upgrades.
const fn cascade(start: ModelTier) -> &'static [ModelTier] {
    match start {
        ModelTier::Premium => &[ModelTier::Premium, ModelTier::Standard],
        ModelTier::Standard => &[ModelTier::Standard],
    }
}

/// The kill switch that skips the premium tier, if any (step 3a).
const fn premium_kill_switch(kill: KillSwitches) -> Option<DowngradeReason> {
    if kill.force_standard_tier {
        Some(DowngradeReason::ForceStandardTier)
    } else if kill.disable_premium_tier {
        Some(DowngradeReason::DisablePremiumTier)
    } else {
        None
    }
}

/// The candidate of `tier` among its enabled models (step 3b): the selected
/// model, else the `is_default` model, else the first one.
fn candidate<'s>(
    snapshot: &'s PolicySnapshot,
    tier: ModelTier,
    selected: &str,
) -> Option<&'s ModelCatalogEntry> {
    let enabled = || {
        snapshot
            .model_catalog
            .iter()
            .filter(move |entry| entry.enabled && entry.tier == tier)
    };
    enabled()
        .find(|entry| entry.id == selected)
        .or_else(|| enabled().find(|entry| entry.preference.is_some_and(|p| p.is_default)))
        .or_else(|| enabled().next())
}

// ── Bucket helpers ───────────────────────────────────────────────────────

/// The bucket rows of one read, by period and bucket (missing rows are zero).
struct BucketUsage(HashMap<(QuotaPeriod, QuotaBucket), QuotaUsageRecord>);

impl BucketUsage {
    fn new(rows: Vec<QuotaUsageRecord>) -> Self {
        Self(
            rows.into_iter()
                .map(|row| ((row.key.period, row.key.bucket), row))
                .collect(),
        )
    }

    fn record(&self, period: QuotaPeriod, bucket: QuotaBucket) -> Option<&QuotaUsageRecord> {
        self.0.get(&(period, bucket))
    }

    /// `spent + reserved` (saturating).
    fn used(&self, period: QuotaPeriod, bucket: QuotaBucket) -> i64 {
        self.record(period, bucket).map_or(0, |row| {
            row.spent_credits_micro
                .saturating_add(row.reserved_credits_micro)
        })
    }

    /// `spent + reserved + extra <= limit` for every bucket of `tier` in
    /// every period (section 5.4.2 `bucket_available`).
    fn tier_available(&self, tier: ModelTier, limits: &UserLimits, extra: i64) -> bool {
        QuotaBucket::for_tier(tier).iter().all(|&bucket| {
            QuotaPeriod::ALL.iter().all(|&period| {
                let limit = period.limit(&bucket.limits(limits));
                self.record(period, bucket)
                    .map_or(Some(0), |row| {
                        row.spent_credits_micro
                            .checked_add(row.reserved_credits_micro)
                    })
                    .and_then(|used| used.checked_add(extra))
                    .is_some_and(|total| total <= limit)
            })
        })
    }
}

fn key(periods: PeriodStarts, period: QuotaPeriod, bucket: QuotaBucket) -> QuotaBucketKey {
    QuotaBucketKey {
        period,
        period_start: periods.start(period),
        bucket,
    }
}

/// The rows a turn on `tier` books: `total` (and `tier:premium`) of both periods.
fn bucket_keys(tier: ModelTier, periods: PeriodStarts) -> Vec<QuotaBucketKey> {
    QuotaBucket::for_tier(tier)
        .iter()
        .flat_map(|&bucket| {
            QuotaPeriod::ALL
                .iter()
                .map(move |&period| key(periods, period, bucket))
        })
        .collect()
}

/// Every enforcement row of both periods.
fn all_keys(periods: PeriodStarts) -> Vec<QuotaBucketKey> {
    bucket_keys(ModelTier::Premium, periods)
}

fn settlements(
    input: &SettlementInput,
    method: SettlementMethod,
    tier: ModelTier,
    charge: &Charge,
) -> Vec<BucketSettlement> {
    let counts_tools = method != SettlementMethod::Released;
    QuotaBucket::for_tier(tier)
        .iter()
        .flat_map(|&bucket| {
            QuotaPeriod::ALL.iter().map(move |&period| {
                let total = bucket == QuotaBucket::Total;
                BucketSettlement {
                    key: key(input.periods, period, bucket),
                    release_reserved_credits_micro: input.reserved_credits_micro,
                    spent_credits_micro: charge.committed_credits_micro,
                    input_tokens: if total { charge.input_tokens } else { 0 },
                    output_tokens: if total { charge.output_tokens } else { 0 },
                    web_search_calls: if total && counts_tools {
                        input.web_search_calls
                    } else {
                        0
                    },
                    code_interpreter_calls: if total && counts_tools {
                        input.code_interpreter_calls
                    } else {
                        0
                    },
                }
            })
        })
        .collect()
}

/// Tenant + owner scope of `owner`'s `quota_usage` rows.
fn owner_scope(owner: QuotaOwner) -> AccessScope {
    confine_to_subject(&AccessScope::allow_all(), owner.tenant_id, owner.user_id)
}

/// The diagnostic is logged by the platform error middleware, never serialized.
fn plugin_error(call: &str, err: &MiniChatModelPolicyPluginError) -> DomainError {
    DomainError::internal(format!("model policy plugin {call} failed: {err}"))
}

fn settlement_error(
    input: &SettlementInput,
    err: &crate::domain::quota::CreditsError,
) -> DomainError {
    tracing::warn!(
        target: "mini_chat.quota",
        model = %input.effective_model,
        error = %err,
        "settlement credit computation failed"
    );
    DomainError::internal(format!("settlement credit computation failed: {err}"))
}

#[cfg(test)]
#[path = "quota_service_tests.rs"]
mod tests;
