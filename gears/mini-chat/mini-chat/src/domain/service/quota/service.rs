//! The quota service: the entry point the turn pipeline, the orphan watchdog,
//! the thread-summary handler and the REST layer call
//! (`cpt-cf-mini-chat-component-quota-service`).

use std::sync::Arc;

use mini_chat_sdk::SettlementMethod;
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::finalize::{FinalizeRequest, FinalizeResult, FinalizeWon};
use super::periods::period_keys;
use super::policy::{
    SettlementSnapshot, prefetch_settlement_snapshot, resolve_preflight_policy,
    resolve_status_policy,
};
use super::ports::{PreflightLabel, PreflightRead, QuotaMetrics, QuotaRepository};
use super::preflight::{
    CascadeInput, PreflightDecision, PreflightRequest, QuotaDecision, run_cascade,
};
use super::reserve::{ReserveRequest, ReservedColumns};
use super::status::{QuotaStatusView, QuotaWarningView, build_status_view, build_warnings};
use super::tools::{ToolKind, ToolLimit, tool_call_limit};
use super::types::{QuotaBucket, QuotaPeriod, find_row};
use crate::config::{MiniChatConfig, QuotaConfig};
use crate::domain::authz::AuthzService;
use crate::domain::error::{DisabledFeature, DomainError, QuotaSubject};
use crate::domain::ports::PolicyCatalog;
use mini_chat_sdk::UserLimits;

/// Source of the current time; tests inject a fixed clock.
pub type Clock = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

/// The configuration the quota service reads.
#[domain_model]
#[derive(Debug, Clone)]
pub struct QuotaServiceConfig {
    /// The `quota` section.
    pub quota: QuotaConfig,
    /// `streaming.max_output_tokens`.
    pub streaming_max_output_tokens: u32,
    /// `estimation_budgets.minimal_generation_floor`.
    pub minimal_generation_floor: u32,
}

impl QuotaServiceConfig {
    /// The values of the gear configuration.
    #[must_use]
    pub fn from_config(config: &MiniChatConfig) -> Self {
        Self {
            quota: config.quota.clone(),
            streaming_max_output_tokens: config.streaming.max_output_tokens,
            minimal_generation_floor: config.estimation_budgets.minimal_generation_floor,
        }
    }
}

/// Collaborators of the quota service.
pub struct QuotaDeps {
    /// PEP (status endpoint).
    pub authz: Arc<AuthzService>,
    /// Model-policy plugin gateway.
    pub policy: Arc<dyn PolicyCatalog>,
    /// Quota persistence.
    pub repo: Arc<dyn QuotaRepository>,
    /// Cost metrics sink.
    pub metrics: Arc<dyn QuotaMetrics>,
    /// Configuration.
    pub config: QuotaServiceConfig,
}

/// Quota preflight, reserve, settlement, status and warnings.
pub struct QuotaService {
    deps: QuotaDeps,
    clock: Clock,
}

impl QuotaService {
    /// Creates the service with the system clock.
    #[must_use]
    pub fn new(deps: QuotaDeps) -> Self {
        Self {
            deps,
            clock: Arc::new(crate::domain::clock::now_micros),
        }
    }

    /// Replaces the clock (tests).
    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// The current time of the service.
    #[must_use]
    pub fn now(&self) -> OffsetDateTime {
        (self.clock)()
    }

    /// The configuration the service was built with.
    #[must_use]
    pub fn config(&self) -> &QuotaServiceConfig {
        &self.deps.config
    }

    /// Preflight evaluation
    /// (`cpt-cf-mini-chat-algo-quota-billing-preflight`): resolves the policy,
    /// rejects `web_search` under `disable_web_search`, reads the prior context
    /// and the bucket rows, runs the cascade, checks the daily web search and
    /// code interpreter quotas and records the preflight metrics. Writes
    /// nothing.
    ///
    /// # Errors
    ///
    /// `FeatureDisabled(WebSearch)`, `QuotaExceeded(Tokens | WebSearch |
    /// CodeInterpreter)`, [`DomainError::PolicyResolution`] and
    /// [`DomainError::Database`].
    // @cpt-dod:cpt-cf-mini-chat-dod-quota-billing-preflight:p1
    // @cpt-begin:cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps:p1:inst-call-preflight
    pub async fn preflight(
        &self,
        request: &PreflightRequest,
    ) -> Result<PreflightDecision, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-preflight-policy
        let policy = resolve_preflight_policy(&*self.deps.policy, request.user_id).await?;
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-preflight-policy
        let quota = &self.deps.config.quota;
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-if-web-disabled
        if request.web_search_enabled && policy.snapshot.kill_switches.disable_web_search {
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-return-web-disabled
            return Err(DomainError::FeatureDisabled(DisabledFeature::WebSearch));
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-return-web-disabled
        }
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-if-web-disabled
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-preflight-periods
        let keys = period_keys(self.now());
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-preflight-periods
        let read = self
            .deps
            .repo
            .read_preflight(&PreflightRead {
                tenant_id: request.tenant_id,
                user_id: request.user_id,
                chat_id: request.chat_id,
                keys,
            })
            .await?;
        let outcome = match run_cascade(&CascadeInput {
            snapshot: &policy.snapshot,
            limits: &policy.limits,
            rows: &read.buckets,
            selected_model: &request.selected_model,
            message_bytes: request.message_bytes,
            image_count: request.image_count,
            web_search_enabled: request.web_search_enabled,
            has_ready_file_search_documents: request.has_ready_file_search_documents,
            has_ready_code_interpreter_files: request.has_ready_code_interpreter_files,
            prior_context_tokens: read.prior_context_tokens,
            streaming_max_output_tokens: self.deps.config.streaming_max_output_tokens,
            minimal_generation_floor: self.deps.config.minimal_generation_floor,
        }) {
            Ok(outcome) => outcome,
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-return-tokens-exhausted
            Err(rejected) => {
                self.deps.metrics.record_preflight(
                    PreflightLabel::Reject,
                    &request.selected_model,
                    rejected.starting_tier,
                );
                return Err(DomainError::QuotaExceeded(QuotaSubject::Tokens));
            } // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-return-tokens-exhausted
        };
        let entry = outcome.entry;
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-set-decision
        let (quota_decision, downgrade_from, label) =
            if entry.id == request.selected_model && outcome.downgrade_reason.is_none() {
                (QuotaDecision::Allow, None, PreflightLabel::Allow)
            } else {
                (
                    QuotaDecision::Downgrade,
                    Some(request.selected_model.clone()),
                    PreflightLabel::Downgrade,
                )
            };
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-set-decision
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-record-preflight
        self.deps
            .metrics
            .record_preflight(label, &entry.id, outcome.tier);
        self.deps
            .metrics
            .record_estimated_tokens(outcome.estimate.reserve_tokens);
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-record-preflight
        let tools = &entry.general_config.tool_support;
        let switches = &policy.snapshot.kill_switches;
        // @cpt-dod:cpt-cf-mini-chat-dod-quota-billing-tool-quotas:p1
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-if-web-quota
        let web_search_allowed = request.web_search_enabled && tools.web_search;
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-if-web-quota
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-if-ci-quota
        let code_interpreter_allowed = request.has_ready_code_interpreter_files
            && tools.code_interpreter
            && !switches.disable_code_interpreter;
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-if-ci-quota
        let daily_total = find_row(&read.buckets, QuotaPeriod::Daily, QuotaBucket::Total);
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-if-web-exhausted
        if web_search_allowed
            && daily_total.map_or(0, |row| row.web_search_calls)
                >= i64::from(quota.web_search_daily_quota)
        {
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-return-web-exhausted
            return Err(DomainError::QuotaExceeded(QuotaSubject::WebSearch));
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-return-web-exhausted
        }
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-if-web-exhausted
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-if-ci-exhausted
        if code_interpreter_allowed
            && daily_total.map_or(0, |row| row.code_interpreter_calls)
                >= i64::from(quota.code_interpreter_daily_quota)
        {
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-return-ci-exhausted
            return Err(DomainError::QuotaExceeded(QuotaSubject::CodeInterpreter));
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-return-ci-exhausted
        }
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-if-ci-exhausted
        let vision_input = entry
            .multimodal_capabilities
            .iter()
            .any(|capability| capability == "VISION_INPUT");
        let provider_id = entry.provider_id.clone();
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-return-decision
        Ok(PreflightDecision {
            effective_model: entry.id.clone(),
            entry,
            effective_tier: outcome.tier,
            provider_id,
            quota_decision,
            downgrade_from,
            downgrade_reason: outcome.downgrade_reason,
            policy_version: policy.policy_version,
            snapshot: policy.snapshot,
            limits: policy.limits,
            period_keys: keys,
            reserve: outcome.estimate,
            web_search_allowed,
            code_interpreter_allowed,
            vision_input,
        })
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-return-decision
    }
    // @cpt-end:cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps:p1:inst-call-preflight

    /// Reserve write in the repository's own transaction, then the reserve
    /// metrics. Pipelines that insert their own rows in the same transaction
    /// use `SeaQuotaRepository::reserve_with` and call
    /// [`Self::record_reserve_committed`] after the commit.
    ///
    /// # Errors
    ///
    /// See [`QuotaRepository::reserve`].
    pub async fn reserve(
        &self,
        request: &ReserveRequest<'_>,
    ) -> Result<ReservedColumns, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps:p1:inst-reserve-transaction
        let columns = self.deps.repo.reserve(request).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps:p1:inst-reserve-transaction
        self.record_reserve_committed();
        Ok(columns)
    }

    /// Counts `mini_chat_quota_reserve` once per period (call after the reserve
    /// transaction committed).
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-record-metrics:p1:inst-count-reserve
    pub fn record_reserve_committed(&self) {
        for period in QuotaPeriod::ALL {
            self.deps.metrics.record_reserve(period);
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-record-metrics:p1:inst-count-reserve

    /// Fetches the snapshot of `policy_version_applied` for the orphan
    /// variant; never fails (see
    /// [`super::policy::prefetch_settlement_snapshot`]). Call before the
    /// finalization transaction opens.
    pub async fn prefetch_settlement_snapshot(
        &self,
        user_id: Uuid,
        turn_id: Uuid,
        policy_version_applied: Option<u64>,
    ) -> SettlementSnapshot {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-prefetch-snapshot
        prefetch_settlement_snapshot(&*self.deps.policy, user_id, turn_id, policy_version_applied)
            .await
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-finalize-turn:p1:inst-prefetch-snapshot
    }

    /// Finalization in the repository's own transaction, then the settlement
    /// metrics of a won CAS (nothing for a lost one).
    ///
    /// # Errors
    ///
    /// See [`QuotaRepository::finalize_turn`].
    pub async fn finalize_turn(
        &self,
        request: &FinalizeRequest,
    ) -> Result<FinalizeResult, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps:p1:inst-call-finalize
        let result = self.deps.repo.finalize_turn(request).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps:p1:inst-call-finalize
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps:p1:inst-commit-finalization
        if let FinalizeResult::Won(won) = &result {
            self.record_finalized(won);
        }
        // @cpt-end:cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps:p1:inst-commit-finalization
        Ok(result)
    }

    /// Records the settlement metrics of a committed won finalization: for an
    /// `actual` settlement `mini_chat_quota_commit` once per period, the actual
    /// tokens and the code interpreter calls of the effective model; the
    /// overshoot once per period when the actual tokens exceed `reserve_tokens`.
    pub fn record_finalized(&self, won: &FinalizeWon) {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-record-metrics:p1:inst-if-actual-metrics
        if !won.settled || won.settlement_method != SettlementMethod::Actual {
            return;
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-record-metrics:p1:inst-actual-metrics
        let actual_tokens = won
            .actual_input_tokens
            .saturating_add(won.actual_output_tokens);
        for period in QuotaPeriod::ALL {
            self.deps.metrics.record_commit(period);
        }
        self.deps.metrics.record_actual_tokens(actual_tokens);
        if won.code_interpreter_calls > 0
            && let Some(model) = &won.effective_model
        {
            self.deps
                .metrics
                .record_code_interpreter_calls(model, u64::from(won.code_interpreter_calls));
        }
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-record-metrics:p1:inst-actual-metrics
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-record-metrics:p1:inst-overshoot-metric
        if actual_tokens > won.reserve_tokens {
            for period in QuotaPeriod::ALL {
                self.deps.metrics.record_overshoot(period);
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-record-metrics:p1:inst-overshoot-metric
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-record-metrics:p1:inst-if-actual-metrics
    }

    /// Per-turn tool call limit decision.
    #[must_use]
    // @cpt-begin:cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps:p1:inst-check-tool-limits
    pub fn tool_call_limit(&self, tool: ToolKind, start_count: u32) -> ToolLimit {
        tool_call_limit(tool, start_count, &self.deps.config.quota)
    }
    // @cpt-end:cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps:p1:inst-check-tool-limits

    /// `GET /v1/quota/status`: authorizes `read` on the user-quota resource,
    /// resolves the current limits (no snapshot) and builds the view.
    ///
    /// # Errors
    ///
    /// `AuthzDenied`, `PdpUnavailable`, [`DomainError::PolicyResolution`] and
    /// [`DomainError::Database`].
    // @cpt-dod:cpt-cf-mini-chat-dod-quota-billing-quota-status:p1
    pub async fn quota_status(
        &self,
        ctx: &SecurityContext,
    ) -> Result<QuotaStatusView, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-billing-get-quota-status:p1:inst-authorize-status
        let scope = self.deps.authz.user_quota_scope(ctx).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-quota-billing-get-quota-status:p1:inst-authorize-status
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-billing-get-quota-status:p1:inst-resolve-limits
        let policy = resolve_status_policy(&*self.deps.policy, ctx.subject_id()).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-quota-billing-get-quota-status:p1:inst-resolve-limits
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-billing-get-quota-status:p1:inst-status-periods
        let keys = period_keys(self.now());
        // @cpt-end:cpt-cf-mini-chat-flow-quota-billing-get-quota-status:p1:inst-status-periods
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-billing-get-quota-status:p1:inst-select-buckets
        let rows = self.deps.repo.read_buckets(&scope, keys).await?;
        // @cpt-end:cpt-cf-mini-chat-flow-quota-billing-get-quota-status:p1:inst-select-buckets
        let threshold = self.deps.config.quota.warning_threshold_pct;
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-billing-get-quota-status:p1:inst-build-status
        Ok(QuotaStatusView {
            tiers: build_status_view(&policy.limits, &rows, threshold, keys),
            warning_threshold_pct: threshold,
        })
        // @cpt-end:cpt-cf-mini-chat-flow-quota-billing-get-quota-status:p1:inst-build-status
    }

    /// `done.quota_warnings`: the warnings view over the user limits captured
    /// by the turn's preflight and the bucket rows of the current UTC periods
    /// at call time (read after the finalization commit).
    ///
    /// # Errors
    ///
    /// [`DomainError::Database`].
    pub async fn quota_warnings(
        &self,
        tenant_id: Uuid,
        user_id: Uuid,
        limits: &UserLimits,
    ) -> Result<Vec<QuotaWarningView>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps:p1:inst-compute-warnings
        let scope = AccessScope::for_tenant(tenant_id).ensure_owner(user_id);
        let keys = period_keys(self.now());
        let rows = self.deps.repo.read_buckets(&scope, keys).await?;
        Ok(build_warnings(
            limits,
            &rows,
            self.deps.config.quota.warning_threshold_pct,
            keys,
        ))
        // @cpt-end:cpt-cf-mini-chat-flow-quota-billing-turn-quota-steps:p1:inst-compute-warnings
    }
}

#[cfg(test)]
#[path = "service_tests.rs"]
mod tests;
