//! The downgrade cascade (DESIGN "Downgrade Decision Flow", section 3.8) and the quota warnings.

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, UserLimits};
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use tracing::warn;

use super::model::{
    Bucket, BucketLimit, PeriodBreakdown, PeriodType, QuotaPeriod, QuotaSnapshot, QuotaWarning,
    ReservePlan,
};
use crate::config::QuotaConfig;
use crate::domain::error::{DomainError, QuotaScope};
use crate::domain::estimation::{
    ReserveEstimate, ReserveInputs, ToolFacts, ToolSet, candidate_reserve, tools_for,
};

/// Why the effective model differs from the selected one.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DowngradeReason {
    /// The premium bucket is exhausted.
    PremiumQuotaExhausted,
    /// `force_standard_tier`.
    ForceStandardTier,
    /// `disable_premium_tier`.
    DisablePremiumTier,
    /// The selected model is disabled or missing from the catalog.
    ModelDisabled,
}

impl DowngradeReason {
    /// The wire value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PremiumQuotaExhausted => "premium_quota_exhausted",
            Self::ForceStandardTier => "force_standard_tier",
            Self::DisablePremiumTier => "disable_premium_tier",
            Self::ModelDisabled => "model_disabled",
        }
    }
}

/// The outcome of the preflight.
#[domain_model]
#[derive(Debug, Clone)]
pub struct PreflightDecision {
    /// The effective model entry.
    pub effective: ModelCatalogEntry,
    /// The effective model's built-in tools.
    pub tools: ToolSet,
    /// The reserve of the effective model.
    pub reserve: ReserveEstimate,
    /// The buckets the reserve is booked in.
    pub plan: ReservePlan,
    /// `None` for an allow decision.
    pub downgrade_reason: Option<DowngradeReason>,
    /// The policy version the decision was taken under.
    pub policy_version: i64,
}

impl PreflightDecision {
    /// `allow` or `downgrade`.
    #[must_use]
    pub fn decision(&self) -> &'static str {
        if self.downgrade_reason.is_some() {
            "downgrade"
        } else {
            "allow"
        }
    }
}

/// Everything the cascade reads.
#[domain_model]
#[derive(Debug, Clone)]
pub struct CascadeInputs<'a> {
    /// The policy snapshot.
    pub snapshot: &'a PolicySnapshot,
    /// The user's limits under the snapshot's version.
    pub limits: &'a UserLimits,
    /// The user's bucket rows for the current periods.
    pub usage: &'a QuotaSnapshot,
    /// The chat's model (`chats.model`).
    pub selected_model: &'a str,
    /// Model-independent reserve inputs.
    pub reserve_inputs: ReserveInputs,
    /// What the request and the chat offer to a model.
    pub tool_facts: ToolFacts,
    /// The periods the reserve is booked in.
    pub periods: [QuotaPeriod; 2],
    /// The tool quotas.
    pub tool_quotas: &'a QuotaConfig,
}

fn limit_of(limits: &UserLimits, bucket: Bucket, period: PeriodType) -> i64 {
    let tier = match bucket {
        Bucket::Total => limits.standard,
        Bucket::PremiumTier => limits.premium,
    };
    match period {
        PeriodType::Daily => tier.limit_daily_credits_micro,
        PeriodType::Monthly => tier.limit_monthly_credits_micro,
    }
}

fn plan_for(
    tier: ModelTier,
    limits: &UserLimits,
    periods: &[QuotaPeriod; 2],
    reserved_credits_micro: i64,
) -> ReservePlan {
    let buckets: &[Bucket] = match tier {
        ModelTier::Premium => &[Bucket::Total, Bucket::PremiumTier],
        ModelTier::Standard => &[Bucket::Total],
    };
    let mut entries = Vec::new();
    for &bucket in buckets {
        for period in periods {
            entries.push(BucketLimit {
                bucket,
                period: *period,
                limit_credits_micro: limit_of(limits, bucket, period.kind),
            });
        }
    }
    ReservePlan {
        reserved_credits_micro,
        periods: *periods,
        limits: entries,
    }
}

fn tier_available(plan: &ReservePlan, usage: &QuotaSnapshot) -> bool {
    plan.limits.iter().all(|entry| {
        let row = usage.usage(entry.period.kind, entry.bucket);
        row.spent_credits_micro
            .saturating_add(row.reserved_credits_micro)
            .saturating_add(plan.reserved_credits_micro)
            <= entry.limit_credits_micro
    })
}

fn candidate_for<'a>(
    catalog: &'a [ModelCatalogEntry],
    tier: ModelTier,
    selected: &str,
) -> Option<&'a ModelCatalogEntry> {
    let enabled = || {
        catalog
            .iter()
            .filter(move |entry| entry.enabled && entry.tier == tier)
    };
    enabled()
        .find(|entry| entry.id == selected)
        .or_else(|| enabled().find(|entry| entry.is_default()))
        .or_else(|| enabled().next())
}

/// Runs the downgrade cascade and the daily tool-quota checks.
///
/// # Errors
/// `QuotaExceeded` with scope `Tokens` when no tier is available, or with scope `WebSearch` /
/// `CodeInterpreter` when the daily tool quota of the effective model's tools is used up.
pub fn resolve_effective_model(
    inputs: &CascadeInputs<'_>,
) -> Result<PreflightDecision, DomainError> {
    let catalog = &inputs.snapshot.model_catalog;
    let kill: KillSwitches = inputs.snapshot.kill_switches;
    let (start_tier, mut reason) = match catalog
        .iter()
        .find(|entry| entry.id == inputs.selected_model)
    {
        Some(entry) if entry.enabled => (entry.tier, None),
        Some(entry) => (entry.tier, Some(DowngradeReason::ModelDisabled)),
        None => (ModelTier::Premium, Some(DowngradeReason::ModelDisabled)),
    };
    let cascade: &[ModelTier] = match start_tier {
        ModelTier::Premium => &[ModelTier::Premium, ModelTier::Standard],
        ModelTier::Standard => &[ModelTier::Standard],
    };

    for &tier in cascade {
        if tier == ModelTier::Premium && (kill.force_standard_tier || kill.disable_premium_tier) {
            reason.get_or_insert(if kill.force_standard_tier {
                DowngradeReason::ForceStandardTier
            } else {
                DowngradeReason::DisablePremiumTier
            });
            continue;
        }
        let Some(candidate) = candidate_for(catalog, tier, inputs.selected_model) else {
            continue;
        };
        let tools = tools_for(candidate, kill, inputs.tool_facts);
        let reserve = match candidate_reserve(candidate, &inputs.reserve_inputs, tools) {
            Ok(reserve) => reserve,
            Err(err) => {
                warn!(model = %candidate.id, error = %err, "the reserve of a cascade candidate cannot be computed; treating the tier as unavailable");
                if tier == ModelTier::Premium {
                    reason.get_or_insert(DowngradeReason::PremiumQuotaExhausted);
                }
                continue;
            }
        };
        let plan = plan_for(
            tier,
            inputs.limits,
            &inputs.periods,
            reserve.reserved_credits_micro,
        );
        if !tier_available(&plan, inputs.usage) {
            if tier == ModelTier::Premium {
                reason.get_or_insert(DowngradeReason::PremiumQuotaExhausted);
            }
            continue;
        }
        check_tool_quotas(inputs, tools)?;
        let downgrade_reason = if candidate.id == inputs.selected_model {
            reason
        } else {
            Some(reason.unwrap_or(DowngradeReason::PremiumQuotaExhausted))
        };
        return Ok(PreflightDecision {
            effective: candidate.clone(),
            tools,
            reserve,
            plan,
            downgrade_reason,
            policy_version: inputs.snapshot.policy_version,
        });
    }
    Err(DomainError::QuotaExceeded {
        scope: QuotaScope::Tokens,
    })
}

fn check_tool_quotas(inputs: &CascadeInputs<'_>, tools: ToolSet) -> Result<(), DomainError> {
    let daily = inputs.usage.usage(PeriodType::Daily, Bucket::Total);
    if tools.web_search
        && daily.web_search_calls >= i64::from(inputs.tool_quotas.web_search_daily_quota)
    {
        return Err(DomainError::QuotaExceeded {
            scope: QuotaScope::WebSearch,
        });
    }
    if tools.code_interpreter
        && daily.code_interpreter_calls
            >= i64::from(inputs.tool_quotas.code_interpreter_daily_quota)
    {
        return Err(DomainError::QuotaExceeded {
            scope: QuotaScope::CodeInterpreter,
        });
    }
    Ok(())
}

/// The remaining quota of one tier in one period, as `done.quota_warnings` and the quota status
/// endpoint report it.
///
/// Periods whose limit is `<= 0` have no entry.
#[must_use]
pub fn quota_breakdown(
    limits: &UserLimits,
    usage: &QuotaSnapshot,
    periods: &[QuotaPeriod; 2],
    warning_threshold_pct: u8,
) -> Vec<PeriodBreakdown> {
    let mut out = Vec::new();
    for (tier, bucket) in [("premium", Bucket::PremiumTier), ("total", Bucket::Total)] {
        for period in periods {
            let limit = limit_of(limits, bucket, period.kind);
            if limit <= 0 {
                continue;
            }
            let row = usage.usage(period.kind, bucket);
            let used = row
                .spent_credits_micro
                .saturating_add(row.reserved_credits_micro);
            let remaining = limit.saturating_sub(used).max(0);
            let remaining_percentage = percentage_of(remaining, limit);
            out.push(PeriodBreakdown {
                tier,
                period: period.kind.as_str(),
                limit_credits_micro: limit,
                used_credits_micro: used,
                remaining_credits_micro: remaining,
                remaining_percentage,
                warning: remaining_percentage <= 100 - i64::from(warning_threshold_pct),
                exhausted: remaining_percentage == 0,
                next_reset: period.next_reset(),
            });
        }
    }
    out
}

/// Per-tier, per-period remaining quota after a settlement (`done.quota_warnings`).
///
/// Periods whose limit is `<= 0` are skipped; `next_reset` is set only for warning or exhausted
/// entries.
#[must_use]
pub fn quota_warnings(
    limits: &UserLimits,
    usage: &QuotaSnapshot,
    periods: &[QuotaPeriod; 2],
    warning_threshold_pct: u8,
) -> Vec<QuotaWarning> {
    quota_breakdown(limits, usage, periods, warning_threshold_pct)
        .into_iter()
        .map(|entry| QuotaWarning {
            tier: entry.tier,
            period: entry.period,
            remaining_percentage: entry.remaining_percentage,
            warning: entry.warning,
            exhausted: entry.exhausted,
            next_reset: (entry.warning || entry.exhausted).then_some(entry.next_reset),
        })
        .collect()
}

#[allow(
    clippy::integer_division,
    reason = "the remaining percentage is a floored integer percentage by definition"
)]
fn percentage_of(part: i64, whole: i64) -> i64 {
    part.saturating_mul(100) / whole
}

/// The current periods at `now`.
#[must_use]
pub fn periods_at(now: OffsetDateTime) -> [QuotaPeriod; 2] {
    QuotaPeriod::containing(now)
}

#[cfg(test)]
#[path = "cascade_tests.rs"]
mod tests;
