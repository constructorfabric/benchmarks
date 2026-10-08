//! Quota engine (DESIGN §5.4): preflight downgrade cascade, reserve write with
//! re-check, and settlement of `quota_usage` bucket rows.

use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};
use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, UserLimits};
use sea_orm::DbBackend;
use toolkit_db::DBProvider;
use toolkit_db::secure::DbTx;
use tracing::warn;
use uuid::Uuid;

use crate::config::MiniChatConfig;
use crate::domain::clock::now_utc;
use crate::domain::credits::credits_micro;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::estimation::{estimated_text_tokens, period_starts};
use crate::domain::model::{
    Bucket, DowngradeReason, PeriodType, QuotaDecision, QuotaScope, SettlementMethod,
};
use crate::infra::db::entities::quota_usage;
use crate::infra::db::repos::QuotaRepo;
use crate::infra::db::repos::quota::{BucketDelta, BucketKey};
use crate::infra::db::tx::with_tx_retry;

/// Inputs of the preflight.
#[derive(Debug, Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent request facts, not a state machine"
)]
pub struct PreflightInput {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub selected_model_id: String,
    pub snapshot: Arc<PolicySnapshot>,
    pub user_limits: UserLimits,
    pub content: String,
    pub image_count: u32,
    pub prior_context_tokens: i64,
    pub chat_has_ready_docs: bool,
    pub chat_has_ready_xlsx: bool,
    pub web_search_requested: bool,
    pub now: DateTime<Utc>,
}

/// Outcome of a successful preflight.
#[derive(Debug, Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent per-tool flags of the provider request"
)]
pub struct PreflightDecision {
    pub effective: ModelCatalogEntry,
    pub effective_tier: ModelTier,
    pub decision: QuotaDecision,
    pub downgrade_reason: Option<DowngradeReason>,
    pub estimated_input_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserve_tokens: i64,
    pub reserved_credits_micro: i64,
    pub minimal_generation_floor_applied: i64,
    pub policy_version: u64,
    pub daily_start: NaiveDate,
    pub monthly_start: NaiveDate,
    pub send_file_search: bool,
    pub send_web_search: bool,
    pub send_code_interpreter: bool,
}

/// Settlement of one turn.
#[derive(Debug, Clone)]
pub struct Settlement {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub daily_start: NaiveDate,
    pub monthly_start: NaiveDate,
    pub premium: bool,
    pub turn_reserved_credits_micro: i64,
    pub committed_credits_micro: i64,
    pub actual_input_tokens: i64,
    pub actual_output_tokens: i64,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
    pub method: SettlementMethod,
}

/// Tools a candidate gets for this request: `(file_search, web_search,
/// code_interpreter)` — the same gates decide the surcharges, the daily tool
/// quota checks and the provider tool list (DESIGN §5.5.6).
fn tools_for(
    entry: &ModelCatalogEntry,
    input: &PreflightInput,
    ks: KillSwitches,
) -> (bool, bool, bool) {
    let ts = &entry.general_config.tool_support;
    (
        input.chat_has_ready_docs && ts.file_search && !ks.disable_file_search,
        input.web_search_requested && ts.web_search && !ks.disable_web_search,
        input.chat_has_ready_xlsx && ts.code_interpreter && !ks.disable_code_interpreter,
    )
}

/// Reserve of a cascade candidate (DESIGN §5.4.1): `(estimated_input_tokens,
/// max_output_tokens_applied, reserved_credits_micro)` from the candidate's
/// `estimation_budgets`, multipliers and `min(max_output_tokens,
/// streaming.max_output_tokens)`. A reserve whose credits cannot be computed
/// (out-of-range tokens or multipliers, overflow) is [`UNCOMPUTABLE_RESERVE`],
/// which never fits.
#[must_use]
#[allow(clippy::trivially_copy_pass_by_ref, reason = "plan-mandated signature")]
pub fn candidate_reserve(
    entry: &ModelCatalogEntry,
    input: &PreflightInput,
    cfg: &MiniChatConfig,
    ks: &KillSwitches,
) -> (i64, i64, i64) {
    let b = &entry.estimation_budgets;
    let (file_search, web_search, code_interpreter) = tools_for(entry, input, *ks);
    let surcharge = |on: bool, tokens: u32| if on { i64::from(tokens) } else { 0 };
    let estimated_input = estimated_text_tokens(&input.content, b)
        .saturating_add(input.prior_context_tokens.max(0))
        .saturating_add(
            i64::from(input.image_count).saturating_mul(i64::from(b.image_token_budget)),
        )
        .saturating_add(surcharge(file_search, b.tool_surcharge_tokens))
        .saturating_add(surcharge(web_search, b.web_search_surcharge_tokens))
        .saturating_add(surcharge(
            code_interpreter,
            b.code_interpreter_surcharge_tokens,
        ));
    let max_output = i64::from(entry.max_output_tokens.min(cfg.streaming.max_output_tokens));
    let credits = credits_micro(
        estimated_input,
        max_output,
        entry.input_tokens_credit_multiplier_micro,
        entry.output_tokens_credit_multiplier_micro,
    )
    .unwrap_or_else(|err| {
        warn!(model = %entry.id, %err, "cascade candidate reserve cannot be computed; candidate unavailable");
        UNCOMPUTABLE_RESERVE
    });
    (estimated_input, max_output, credits)
}

/// Credits of a candidate whose reserve cannot be computed: the candidate is
/// unavailable, whatever the limits (even `i64::MAX`).
pub const UNCOMPUTABLE_RESERVE: i64 = i64::MAX;

/// Buckets a turn of `tier` is checked against and reserved in.
const fn buckets_for(tier: ModelTier) -> &'static [Bucket] {
    match tier {
        ModelTier::Premium => &[Bucket::Total, Bucket::TierPremium],
        ModelTier::Standard => &[Bucket::Total],
    }
}

/// `limit_credits_micro(bucket, period)`: `total` → standard limits (overall
/// cap), `tier:premium` → premium limits (subcap).
const fn limit_for(limits: &UserLimits, bucket: Bucket, period: PeriodType) -> i64 {
    let tier = match bucket {
        Bucket::Total => limits.standard,
        Bucket::TierPremium => limits.premium,
    };
    match period {
        PeriodType::Daily => tier.limit_daily_credits_micro,
        PeriodType::Monthly => tier.limit_monthly_credits_micro,
    }
}

const fn periods(daily: NaiveDate, monthly: NaiveDate) -> [(PeriodType, NaiveDate); 2] {
    [(PeriodType::Daily, daily), (PeriodType::Monthly, monthly)]
}

fn find_row(
    rows: &[quota_usage::Model],
    bucket: Bucket,
    period: PeriodType,
    start: NaiveDate,
) -> Option<&quota_usage::Model> {
    rows.iter().find(|r| {
        r.bucket == bucket.as_str() && r.period_type == period.as_str() && r.period_start == start
    })
}

/// `spent + reserved + extra <= limit` for every bucket of `buckets` in both
/// periods; a missing row counts as zero. An [`UNCOMPUTABLE_RESERVE`] never fits.
fn fits(
    rows: &[quota_usage::Model],
    limits: &UserLimits,
    buckets: &[Bucket],
    daily: NaiveDate,
    monthly: NaiveDate,
    extra: i64,
) -> bool {
    if extra == UNCOMPUTABLE_RESERVE {
        return false;
    }
    buckets.iter().all(|&bucket| {
        periods(daily, monthly).into_iter().all(|(period, start)| {
            let used = find_row(rows, bucket, period, start).map_or(0, |r| {
                i128::from(r.spent_credits_micro) + i128::from(r.reserved_credits_micro)
            });
            used + i128::from(extra) <= i128::from(limit_for(limits, bucket, period))
        })
    })
}

/// Candidate of `tier` among its enabled models: the selected model, else the
/// `is_default` one, else the first in catalog order.
fn candidate_for<'a>(
    snapshot: &'a PolicySnapshot,
    tier: ModelTier,
    selected_id: &str,
) -> Option<&'a ModelCatalogEntry> {
    let mut enabled = snapshot
        .model_catalog
        .iter()
        .filter(move |m| m.enabled && m.tier == tier);
    enabled
        .clone()
        .find(|m| m.id == selected_id)
        .or_else(|| {
            enabled
                .clone()
                .find(|m| m.preference.is_some_and(|p| p.is_default))
        })
        .or_else(|| enabled.next())
}

/// The preflight decision over the user's bucket rows of the current periods
/// (DESIGN §4 "Downgrade Decision Flow", §5.4.2, daily tool quotas).
///
/// # Errors
/// `QuotaExceeded { Tokens }` when no tier of the cascade is available;
/// `QuotaExceeded { WebSearch | CodeInterpreter }` when the effective model gets
/// the tool and the daily call quota is used up.
pub(crate) fn decide(
    input: &PreflightInput,
    cfg: &MiniChatConfig,
    rows: &[quota_usage::Model],
) -> DomainResult<PreflightDecision> {
    let snapshot = &*input.snapshot;
    let ks = snapshot.kill_switches;
    let (daily, monthly) = period_starts(input.now);

    let (start_tier, mut reason) = match snapshot.find(&input.selected_model_id) {
        Some(m) if m.enabled => (m.tier, None),
        Some(m) => (m.tier, Some(DowngradeReason::ModelDisabled)),
        None => (ModelTier::Premium, Some(DowngradeReason::ModelDisabled)),
    };
    let cascade: &[ModelTier] = match start_tier {
        ModelTier::Premium => &[ModelTier::Premium, ModelTier::Standard],
        ModelTier::Standard => &[ModelTier::Standard],
    };

    let mut chosen = None;
    for &tier in cascade {
        if tier == ModelTier::Premium {
            let switch = if ks.force_standard_tier {
                Some(DowngradeReason::ForceStandardTier)
            } else if ks.disable_premium_tier {
                Some(DowngradeReason::DisablePremiumTier)
            } else {
                None
            };
            if let Some(r) = switch {
                reason.get_or_insert(r);
                continue;
            }
        }
        let Some(candidate) = candidate_for(snapshot, tier, &input.selected_model_id) else {
            continue;
        };
        let (estimated_input, max_output, credits) = candidate_reserve(candidate, input, cfg, &ks);
        if fits(
            rows,
            &input.user_limits,
            buckets_for(tier),
            daily,
            monthly,
            credits,
        ) {
            chosen = Some((candidate, estimated_input, max_output, credits));
            break;
        }
        if tier == ModelTier::Premium {
            reason.get_or_insert(DowngradeReason::PremiumQuotaExhausted);
        }
    }
    let Some((effective, estimated_input, max_output, credits)) = chosen else {
        return Err(DomainError::QuotaExceeded {
            scope: QuotaScope::Tokens,
        });
    };

    let (send_file_search, send_web_search, send_code_interpreter) =
        tools_for(effective, input, ks);
    let daily_total = find_row(rows, Bucket::Total, PeriodType::Daily, daily);
    if send_web_search
        && daily_total.map_or(0, |r| i64::from(r.web_search_calls))
            >= i64::from(cfg.quota.web_search_daily_quota)
    {
        return Err(DomainError::QuotaExceeded {
            scope: QuotaScope::WebSearch,
        });
    }
    if send_code_interpreter
        && daily_total.map_or(0, |r| i64::from(r.code_interpreter_calls))
            >= i64::from(cfg.quota.code_interpreter_daily_quota)
    {
        return Err(DomainError::QuotaExceeded {
            scope: QuotaScope::CodeInterpreter,
        });
    }

    let decision = if effective.id == input.selected_model_id && reason.is_none() {
        QuotaDecision::Allow
    } else {
        QuotaDecision::Downgrade
    };
    Ok(PreflightDecision {
        effective: effective.clone(),
        effective_tier: effective.tier,
        decision,
        downgrade_reason: reason,
        estimated_input_tokens: estimated_input,
        max_output_tokens_applied: max_output,
        reserve_tokens: estimated_input.saturating_add(max_output),
        reserved_credits_micro: credits,
        minimal_generation_floor_applied: i64::from(
            cfg.estimation_budgets.minimal_generation_floor,
        )
        .min(max_output),
        policy_version: snapshot.policy_version,
        daily_start: daily,
        monthly_start: monthly,
        send_file_search,
        send_web_search,
        send_code_interpreter,
    })
}

fn credits_of(
    input_tokens: i64,
    output_tokens: i64,
    entry: &ModelCatalogEntry,
) -> DomainResult<i64> {
    credits_micro(
        input_tokens,
        output_tokens,
        entry.input_tokens_credit_multiplier_micro,
        entry.output_tokens_credit_multiplier_micro,
    )
    .map_err(|err| {
        DomainError::internal(format!(
            "credits of model {} for {input_tokens} input / {output_tokens} output tokens: {err}",
            entry.id
        ))
    })
}

/// Committed credits of an actual settlement (DESIGN §5.4.5): the credits of the
/// actual usage, or `reserved_credits` (and `true`) when `actual / reserve_tokens`
/// exceeds `tolerance`.
///
/// # Errors
/// `Internal` when the actual credits cannot be computed (out-of-range usage or
/// multipliers).
pub fn committed_credits(
    actual_in: i64,
    actual_out: i64,
    reserve_tokens: i64,
    reserved_credits: i64,
    tolerance: f64,
    entry: &ModelCatalogEntry,
) -> DomainResult<(i64, bool)> {
    let actual_credits = credits_of(actual_in, actual_out, entry)?;
    let actual_tokens = actual_in.saturating_add(actual_out);
    #[allow(
        clippy::cast_precision_loss,
        reason = "DESIGN §5.4.5 mandates a floating-point ratio"
    )]
    let over = actual_tokens > reserve_tokens
        && (actual_tokens as f64) / (reserve_tokens as f64) > tolerance;
    Ok(if over {
        (reserved_credits, true)
    } else {
        (actual_credits, false)
    })
}

/// Credits of an estimated settlement (DESIGN §5.8):
/// `credits_micro(reserve_tokens - max_out_applied, floor_applied, …)`.
///
/// # Errors
/// `Internal` when the credits cannot be computed.
pub fn estimated_credits(
    reserve_tokens: i64,
    max_out_applied: i64,
    floor_applied: i64,
    entry: &ModelCatalogEntry,
) -> DomainResult<i64> {
    credits_of(
        reserve_tokens.saturating_sub(max_out_applied),
        floor_applied,
        entry,
    )
}

/// Preflight, reserve and settlement over `quota_usage`.
pub struct QuotaService {
    cfg: Arc<MiniChatConfig>,
    db: Arc<DBProvider<DomainError>>,
}

impl QuotaService {
    #[must_use]
    pub fn new(cfg: Arc<MiniChatConfig>, db: Arc<DBProvider<DomainError>>) -> Self {
        Self { cfg, db }
    }

    /// Run the cascade and the daily tool quota checks on the user's bucket rows,
    /// read in one transaction (`SELECT ... FOR UPDATE` on `PostgreSQL`) that
    /// writes nothing.
    ///
    /// # Errors
    /// `QuotaExceeded { scope }`; database failures.
    pub async fn preflight(&self, input: &PreflightInput) -> DomainResult<PreflightDecision> {
        let (daily, monthly) = period_starts(input.now);
        let (tenant_id, user_id) = (input.tenant_id, input.user_id);
        let for_update = self.db.db().backend() == DbBackend::Postgres;
        let rows = with_tx_retry(&self.db, "quota preflight", move |tx| {
            Box::pin(async move {
                QuotaRepo::rows_for_periods_locked(
                    tx, tenant_id, user_id, daily, monthly, for_update,
                )
                .await
            })
        })
        .await?;
        decide(input, &self.cfg, &rows)
    }

    /// Book the reserve of `d` inside the caller's transaction: increment
    /// `reserved_credits_micro` of bucket `total` (and `tier:premium` for a premium
    /// turn) in both periods, then re-read the rows and check `spent + reserved <=
    /// limit` for each of them (DESIGN §5.4.2 "TOCTOU", §5.4.3).
    ///
    /// # Errors
    /// `QuotaExceeded { Tokens }` when a bucket is over its limit after the
    /// increments (the caller rolls the transaction back); database failures.
    pub async fn reserve_in_tx(
        &self,
        tx: &DbTx<'_>,
        tenant_id: Uuid,
        user_id: Uuid,
        d: &PreflightDecision,
        user_limits: &UserLimits,
    ) -> DomainResult<()> {
        let now = now_utc();
        let buckets = buckets_for(d.effective_tier);
        for &bucket in buckets {
            for (period, period_start) in periods(d.daily_start, d.monthly_start) {
                let key = BucketKey {
                    tenant_id,
                    user_id,
                    period,
                    period_start,
                    bucket,
                };
                let delta = BucketDelta {
                    reserved_credits_micro: d.reserved_credits_micro,
                    ..BucketDelta::default()
                };
                QuotaRepo::apply_delta(tx, key, delta, now).await?;
            }
        }
        let rows =
            QuotaRepo::rows_for_periods(tx, tenant_id, user_id, d.daily_start, d.monthly_start)
                .await?;
        if fits(
            &rows,
            user_limits,
            buckets,
            d.daily_start,
            d.monthly_start,
            0,
        ) {
            Ok(())
        } else {
            Err(DomainError::QuotaExceeded {
                scope: QuotaScope::Tokens,
            })
        }
    }

    /// Settle a turn inside the caller's transaction (DESIGN §3.7 "Commit
    /// semantics"). Bucket `total`: `reserved -= turn reserve`, `spent +=
    /// committed`, `calls += 1`, token telemetry (actual only), tool call counts
    /// (actual and estimated). Bucket `tier:premium` (premium turns): `reserved -=`,
    /// `spent +=`, `calls += 1`. A released settlement charges nothing.
    ///
    /// # Errors
    /// `Internal` for `SettlementMethod::None`; database failures.
    pub async fn settle_in_tx(&self, tx: &DbTx<'_>, s: &Settlement) -> DomainResult<()> {
        let (spent, tokens, tool_calls) = match s.method {
            SettlementMethod::Actual => (s.committed_credits_micro, true, true),
            SettlementMethod::Estimated => (s.committed_credits_micro, false, true),
            SettlementMethod::Released => (0, false, false),
            SettlementMethod::None => {
                return Err(DomainError::internal(
                    "settlement method `none` is not a turn settlement",
                ));
            }
        };
        let tier_delta = BucketDelta {
            reserved_credits_micro: s.turn_reserved_credits_micro.saturating_neg(),
            spent_credits_micro: spent,
            calls: 1,
            ..BucketDelta::default()
        };
        let total_delta = BucketDelta {
            input_tokens: if tokens { s.actual_input_tokens } else { 0 },
            output_tokens: if tokens { s.actual_output_tokens } else { 0 },
            web_search_calls: if tool_calls { s.web_search_calls } else { 0 },
            code_interpreter_calls: if tool_calls {
                s.code_interpreter_calls
            } else {
                0
            },
            ..tier_delta
        };
        let mut targets = vec![(Bucket::Total, total_delta)];
        if s.premium {
            targets.push((Bucket::TierPremium, tier_delta));
        }
        let now = now_utc();
        for (bucket, delta) in targets {
            for (period, period_start) in periods(s.daily_start, s.monthly_start) {
                let key = BucketKey {
                    tenant_id: s.tenant_id,
                    user_id: s.user_id,
                    period,
                    period_start,
                    bucket,
                };
                QuotaRepo::apply_delta(tx, key, delta, now).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod quota_tests;
