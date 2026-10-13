//! Quota service: credit arithmetic, preflight cascade, reserve (+ re-check) and settlement,
//! quota warnings and the quota status API (DESIGN §3.2 quota service, §5).
//!
//! OWNER: quota & billing work package. The public types and method signatures below are the
//! contract used by the stream / finalization / mutation services; keep them stable.

use std::sync::Arc;

use mini_chat_sdk::{ModelCatalogEntry, ModelTier, PolicySnapshot, UsageTokens, UserLimits};
use time::{Date, OffsetDateTime};
use toolkit_db::DbTx;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{QuotaStatusResponse, QuotaWarning};
use crate::domain::authz;
use crate::domain::error::DomainError;
use crate::domain::service::Deps;

pub mod credits;
pub mod status;
pub mod store;

pub use credits::{CandidateReserve, CreditError, candidate_reserve, credits_micro, estimate_text_tokens};
use store::{Bucket, PeriodKind, RowDelta, UsageRows};

/// Why the effective model differs from the selected one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DowngradeReason {
    PremiumQuotaExhausted,
    ForceStandardTier,
    DisablePremiumTier,
    ModelDisabled,
}

impl DowngradeReason {
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

/// Preflight period keys (UTC dates), persisted in memory for settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaPeriods {
    pub daily_start: Date,
    pub monthly_start: Date,
}

impl QuotaPeriods {
    /// Periods of a UTC instant (daily = date, monthly = 1st of the month).
    #[must_use]
    pub fn of(at: time::OffsetDateTime) -> Self {
        let d = at.to_offset(time::UtcOffset::UTC).date();
        let monthly = d.replace_day(1).unwrap_or(d);
        Self {
            daily_start: d,
            monthly_start: monthly,
        }
    }
}

/// Which tools the request sends with the effective model (gates of DESIGN §5.5.6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ToolGates {
    pub web_search: bool,
    pub file_search: bool,
    pub code_interpreter: bool,
}

/// Inputs of the quota preflight (computed before context assembly).
#[derive(Debug, Clone)]
pub struct PreflightInput {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    /// `chats.model` (may be missing or disabled in the catalog).
    pub selected_model: String,
    /// UTF-8 bytes of the current user message.
    pub message_bytes: usize,
    /// Number of images attached to the current message.
    pub image_count: u32,
    /// input+output tokens of the latest non-deleted assistant message with non-zero usage.
    pub prior_context_tokens: i64,
    /// `web_search.enabled` of the request (or of the original turn on retry/edit).
    pub web_search_requested: bool,
    /// Chat has at least one ready, non-deleted document with `for_file_search`.
    pub has_ready_documents: bool,
    /// Chat has at least one ready, non-deleted attachment with `for_code_interpreter`.
    pub has_ready_code_interpreter_files: bool,
}

/// Result of an allowed preflight (the reserve values are those of the effective model).
#[derive(Debug, Clone)]
pub struct PreflightDecision {
    pub snapshot: Arc<PolicySnapshot>,
    pub policy_version: u64,
    pub user_limits: UserLimits,
    pub selected_model: String,
    pub effective: ModelCatalogEntry,
    pub tier: ModelTier,
    /// `None` = `allow`; `Some(reason)` = `downgrade`.
    pub downgrade_reason: Option<DowngradeReason>,
    pub tools: ToolGates,
    pub vision_supported: bool,
    pub estimated_input_tokens: i64,
    pub max_output_tokens_applied: i32,
    pub reserve_tokens: i64,
    pub reserved_credits_micro: i64,
    pub minimal_generation_floor_applied: i32,
    pub periods: QuotaPeriods,
}

impl PreflightDecision {
    /// `done.quota_decision`.
    #[must_use]
    pub const fn is_downgrade(&self) -> bool {
        self.downgrade_reason.is_some()
    }
}

/// Settlement method of a usage event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementMethod {
    Actual,
    Estimated,
    Released,
}

impl SettlementMethod {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Actual => "actual",
            Self::Estimated => "estimated",
            Self::Released => "released",
        }
    }
}

/// Inputs of a settlement (read from the persisted turn row + terminal data).
#[derive(Debug, Clone)]
pub struct SettlementInput {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub effective_model: String,
    pub policy_version: u64,
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserved_credits_micro: i64,
    pub minimal_generation_floor_applied: i64,
    pub periods: QuotaPeriods,
    pub method: SettlementMethod,
    /// Provider usage (actual settlements).
    pub usage: Option<UsageTokens>,
    pub web_search_calls: u32,
    pub code_interpreter_calls: u32,
}

/// Result of a settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettlementResult {
    /// Committed credits (emitted as `actual_credits_micro`).
    pub committed_credits_micro: i64,
    pub overshoot_capped: bool,
}


/// Outcome of the downgrade cascade (pure part of the preflight).
#[derive(Debug, Clone)]
pub struct CascadeOutcome {
    pub effective: ModelCatalogEntry,
    pub reserve: CandidateReserve,
    pub downgrade_reason: Option<DowngradeReason>,
}

/// `spent + reserved + reserve <= limit` for a bucket in both periods.
fn bucket_fits(
    rows: &UsageRows,
    periods: &QuotaPeriods,
    limits: &UserLimits,
    bucket: Bucket,
    reserve: i64,
) -> bool {
    PeriodKind::ALL.iter().all(|&p| {
        let limit = status::limit_of(limits, bucket, p);
        rows.used(periods, p, bucket)
            .checked_add(reserve)
            .is_some_and(|total| total <= limit)
    })
}

/// Candidate model of `tier`: the selected model when enabled in that tier, else the enabled
/// `is_default` model of the tier, else the first enabled model of the tier (catalog order).
fn tier_candidate<'a>(snapshot: &'a PolicySnapshot, selected: &str, tier: ModelTier) -> Option<&'a ModelCatalogEntry> {
    let enabled = || snapshot.model_catalog.iter().filter(move |m| m.enabled && m.tier == tier);
    enabled()
        .find(|m| m.id == selected)
        .or_else(|| enabled().find(|m| m.is_default()))
        .or_else(|| enabled().next())
}

/// Runs the two-tier downgrade cascade (DESIGN §3.2 "Downgrade Decision Flow"). `None` = no
/// tier available (429 `quota_exceeded`, subject `tokens`).
#[must_use]
pub fn run_cascade(
    snapshot: &PolicySnapshot,
    limits: &UserLimits,
    rows: &UsageRows,
    periods: &QuotaPeriods,
    input: &PreflightInput,
    streaming_max_output_tokens: u32,
    minimal_generation_floor: u32,
) -> Option<CascadeOutcome> {
    let kill = &snapshot.kill_switches;
    let (start_tier, mut reason) = match snapshot.find(&input.selected_model) {
        Some(m) if m.enabled => (m.tier, None),
        Some(m) => (m.tier, Some(DowngradeReason::ModelDisabled)),
        None => (ModelTier::Premium, Some(DowngradeReason::ModelDisabled)),
    };
    let cascade: &[ModelTier] = match start_tier {
        ModelTier::Premium => &[ModelTier::Premium, ModelTier::Standard],
        ModelTier::Standard => &[ModelTier::Standard],
    };
    for &tier in cascade {
        if tier == ModelTier::Premium {
            if kill.force_standard_tier {
                reason.get_or_insert(DowngradeReason::ForceStandardTier);
                continue;
            }
            if kill.disable_premium_tier {
                reason.get_or_insert(DowngradeReason::DisablePremiumTier);
                continue;
            }
        }
        let Some(candidate) = tier_candidate(snapshot, &input.selected_model, tier) else {
            continue;
        };
        let reserve = match candidate_reserve(
            candidate,
            input,
            kill,
            streaming_max_output_tokens,
            minimal_generation_floor,
        ) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!(model = %candidate.id, error = %e, "candidate reserve cannot be computed; treated as unavailable");
                None
            }
        };
        let available = reserve.is_some_and(|r| {
            bucket_fits(rows, periods, limits, Bucket::Total, r.reserved_credits_micro)
                && (tier == ModelTier::Standard
                    || bucket_fits(rows, periods, limits, Bucket::Premium, r.reserved_credits_micro))
        });
        match reserve {
            Some(reserve) if available => {
                return Some(CascadeOutcome {
                    effective: candidate.clone(),
                    reserve,
                    downgrade_reason: reason,
                });
            }
            _ => {
                if tier == ModelTier::Premium {
                    reason.get_or_insert(DowngradeReason::PremiumQuotaExhausted);
                }
            }
        }
    }
    None
}

/// Settlement credits of a turn (pure part of [`QuotaService::settle_in_tx`]).
///
/// # Errors
/// [`CreditError`] on out-of-range tokens / multipliers or overflow.
pub fn settlement_credits(
    entry: &ModelCatalogEntry,
    input: &SettlementInput,
    overshoot_tolerance_factor: f64,
) -> Result<SettlementResult, CreditError> {
    match input.method {
        SettlementMethod::Released => Ok(SettlementResult {
            committed_credits_micro: 0,
            overshoot_capped: false,
        }),
        SettlementMethod::Estimated => {
            let est_input = input.reserve_tokens - input.max_output_tokens_applied;
            let credits =
                credits::entry_credits_micro(entry, est_input, input.minimal_generation_floor_applied)?;
            Ok(SettlementResult {
                committed_credits_micro: credits,
                overshoot_capped: false,
            })
        }
        SettlementMethod::Actual => {
            let usage = input.usage.unwrap_or_default();
            let actual = credits::entry_credits_micro(entry, usage.input_tokens, usage.output_tokens)?;
            let actual_tokens = usage.input_tokens.saturating_add(usage.output_tokens);
            #[allow(clippy::cast_precision_loss)]
            let capped = actual_tokens > input.reserve_tokens
                && (input.reserve_tokens <= 0
                    || (actual_tokens as f64) / (input.reserve_tokens as f64) > overshoot_tolerance_factor);
            Ok(SettlementResult {
                committed_credits_micro: if capped { input.reserved_credits_micro } else { actual },
                overshoot_capped: capped,
            })
        }
    }
}

fn clamp_i32(v: u32) -> i32 {
    i32::try_from(v).unwrap_or(i32::MAX)
}

fn credit_err(e: CreditError) -> DomainError {
    tracing::warn!(error = %e, "credit computation failed");
    DomainError::internal(format!("credit computation failed: {e}"))
}

pub struct QuotaService {
    deps: Arc<Deps>,
}

impl QuotaService {
    #[must_use]
    pub fn new(deps: Arc<Deps>) -> Self {
        Self { deps }
    }

    /// Reads the user's bucket rows of `periods` in one read transaction.
    async fn read_rows(&self, tenant_id: Uuid, user_id: Uuid, periods: QuotaPeriods) -> Result<UsageRows, DomainError> {
        let scope = store::user_scope(tenant_id, user_id);
        self.deps
            .db
            .transaction(move |tx| {
                Box::pin(async move { store::load_rows(tx, &scope, tenant_id, user_id, &periods).await })
            })
            .await
    }

    async fn current_limits(&self, user_id: Uuid) -> Result<UserLimits, DomainError> {
        let version = self
            .deps
            .policy
            .client()
            .await?
            .get_current_policy_version(user_id)
            .await
            .map_err(DomainError::internal)?
            .policy_version;
        self.deps.policy.user_limits(user_id, version).await
    }

    /// Preflight: resolves the policy snapshot, runs the downgrade cascade with per-candidate
    /// reserves and the daily web-search / code-interpreter quota checks. Does not write.
    ///
    /// # Errors
    /// 429 `quota_exceeded` (subject `tokens`, `web_search` or `code_interpreter`), 500 on
    /// policy plugin failure.
    pub async fn preflight(&self, input: &PreflightInput) -> Result<PreflightDecision, DomainError> {
        let snapshot = self.deps.policy.current_snapshot(input.user_id).await?;
        let policy_version = snapshot.policy_version;
        let user_limits = self.deps.policy.user_limits(input.user_id, policy_version).await?;
        let periods = QuotaPeriods::of(OffsetDateTime::now_utc());
        let rows = self.read_rows(input.tenant_id, input.user_id, periods).await?;
        let cfg = &self.deps.cfg;
        let Some(outcome) = run_cascade(
            &snapshot,
            &user_limits,
            &rows,
            &periods,
            input,
            cfg.streaming.max_output_tokens,
            cfg.estimation_budgets.minimal_generation_floor,
        ) else {
            return Err(DomainError::quota_exceeded("tokens"));
        };
        let tools = outcome.reserve.tools;
        let daily_total = rows.get(&periods, PeriodKind::Daily, Bucket::Total);
        let web_calls = daily_total.map_or(0, |r| i64::from(r.web_search_calls));
        let ci_calls = daily_total.map_or(0, |r| i64::from(r.code_interpreter_calls));
        if tools.web_search && web_calls >= i64::from(cfg.quota.web_search_daily_quota) {
            return Err(DomainError::quota_exceeded("web_search"));
        }
        if tools.code_interpreter && ci_calls >= i64::from(cfg.quota.code_interpreter_daily_quota) {
            return Err(DomainError::quota_exceeded("code_interpreter"));
        }
        let r = outcome.reserve;
        Ok(PreflightDecision {
            policy_version,
            user_limits,
            selected_model: input.selected_model.clone(),
            tier: outcome.effective.tier,
            downgrade_reason: outcome.downgrade_reason,
            tools,
            vision_supported: outcome.effective.supports_vision(),
            estimated_input_tokens: r.estimated_input_tokens,
            max_output_tokens_applied: r.max_output_tokens_applied,
            reserve_tokens: r.reserve_tokens,
            reserved_credits_micro: r.reserved_credits_micro,
            minimal_generation_floor_applied: r.minimal_generation_floor_applied,
            periods,
            effective: outcome.effective,
            snapshot: Arc::new(snapshot),
        })
    }

    /// Writes the reserve inside the caller's transaction (bucket `total`, plus `tier:premium`
    /// for premium) for both periods, then re-checks every bucket against its limit.
    ///
    /// # Errors
    /// 429 `quota_exceeded` (subject `tokens`) when a bucket is over its limit (caller rolls back).
    pub async fn reserve_in_tx(
        &self,
        tx: &DbTx<'_>,
        tenant_id: Uuid,
        user_id: Uuid,
        decision: &PreflightDecision,
    ) -> Result<(), DomainError> {
        let now = OffsetDateTime::now_utc();
        let buckets: &[Bucket] = match decision.tier {
            ModelTier::Premium => &[Bucket::Total, Bucket::Premium],
            ModelTier::Standard => &[Bucket::Total],
        };
        let delta = RowDelta {
            reserved: decision.reserved_credits_micro,
            ..RowDelta::default()
        };
        for p in PeriodKind::ALL {
            for &b in buckets {
                store::upsert_delta(tx, tenant_id, user_id, p, p.start(&decision.periods), b, delta, now).await?;
            }
        }
        let scope = store::user_scope(tenant_id, user_id);
        let rows = store::load_rows(tx, &scope, tenant_id, user_id, &decision.periods).await?;
        for p in PeriodKind::ALL {
            for &b in buckets {
                let limit = status::limit_of(&decision.user_limits, b, p);
                if rows.used(&decision.periods, p, b) > limit {
                    tracing::info!(bucket = b.as_str(), period = p.as_str(), "reserve re-check over limit");
                    return Err(DomainError::quota_exceeded("tokens"));
                }
            }
        }
        Ok(())
    }

    /// Settles a turn reserve inside the caller's transaction on the given periods.
    ///
    /// # Errors
    /// Credit computation failure (overflow / invalid multiplier) or DB error.
    pub async fn settle_in_tx(
        &self,
        tx: &DbTx<'_>,
        input: &SettlementInput,
    ) -> Result<SettlementResult, DomainError> {
        let snapshot = self
            .deps
            .policy
            .snapshot_version(input.user_id, input.policy_version)
            .await?;
        let entry = snapshot.find(&input.effective_model).ok_or_else(|| {
            DomainError::internal(format!(
                "settlement: model '{}' missing in policy version {}",
                input.effective_model, input.policy_version
            ))
        })?;
        let result = settlement_credits(entry, input, self.deps.cfg.quota.overshoot_tolerance_factor)
            .map_err(credit_err)?;
        let actual = input.method == SettlementMethod::Actual;
        let counted = input.method != SettlementMethod::Released;
        let usage = if actual { input.usage.unwrap_or_default() } else { UsageTokens::default() };
        let total = RowDelta {
            reserved: -input.reserved_credits_micro,
            spent: result.committed_credits_micro,
            calls: 1,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            web_search_calls: if counted { clamp_i32(input.web_search_calls) } else { 0 },
            code_interpreter_calls: if counted { clamp_i32(input.code_interpreter_calls) } else { 0 },
        };
        let premium = RowDelta {
            reserved: -input.reserved_credits_micro,
            spent: result.committed_credits_micro,
            calls: 1,
            ..RowDelta::default()
        };
        let now = OffsetDateTime::now_utc();
        for p in PeriodKind::ALL {
            let start = p.start(&input.periods);
            store::upsert_delta(tx, input.tenant_id, input.user_id, p, start, Bucket::Total, total, now).await?;
            if entry.tier == ModelTier::Premium {
                store::upsert_delta(tx, input.tenant_id, input.user_id, p, start, Bucket::Premium, premium, now)
                    .await?;
            }
        }
        Ok(result)
    }

    /// `done.quota_warnings` for the user (current periods). Call outside a DB transaction.
    ///
    /// # Errors
    /// DB / plugin failure.
    pub async fn quota_warnings(
        &self,
        tenant_id: Uuid,
        user_id: Uuid,
    ) -> Result<Vec<QuotaWarning>, DomainError> {
        let limits = self.current_limits(user_id).await?;
        let periods = QuotaPeriods::of(OffsetDateTime::now_utc());
        let rows = self.read_rows(tenant_id, user_id, periods).await?;
        Ok(status::warnings(&limits, &rows, &periods, self.deps.cfg.quota.warning_threshold_pct))
    }

    /// `GET /v1/quota/status`.
    ///
    /// # Errors
    /// 403/503 from the PEP, DB / plugin failure.
    pub async fn quota_status(
        &self,
        ctx: &SecurityContext,
    ) -> Result<QuotaStatusResponse, DomainError> {
        let scope = authz::user_quota_scope(&self.deps.enforcer, ctx).await?;
        if scope.is_deny_all() {
            return Err(DomainError::authz_denied());
        }
        let (tenant_id, user_id) = (ctx.subject_tenant_id(), ctx.subject_id());
        let limits = self.current_limits(user_id).await?;
        let periods = QuotaPeriods::of(OffsetDateTime::now_utc());
        let rows = {
            let conn = self.deps.db.conn()?;
            store::load_rows(&conn, &scope, tenant_id, user_id, &periods).await?
        };
        let pct = self.deps.cfg.quota.warning_threshold_pct;
        Ok(QuotaStatusResponse {
            tiers: status::tier_statuses(&limits, &rows, &periods, pct),
            warning_threshold_pct: u32::from(pct),
        })
    }
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod quota_tests;
