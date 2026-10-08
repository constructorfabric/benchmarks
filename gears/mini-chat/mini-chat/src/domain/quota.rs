//! Credit arithmetic, token estimation, the downgrade cascade and quota
//! warnings (DESIGN §5). Pure functions; persistence lives in the quota
//! service.

use std::collections::HashMap;

use mini_chat_sdk::{
    EstimationBudgets, KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, UserLimits,
};
use time::{Date, Month, OffsetDateTime, Time};

use crate::infra::db::entities::quota_usage;
use crate::infra::db::repo::quota::{BUCKET_PREMIUM, BUCKET_TOTAL, PERIOD_DAILY, PERIOD_MONTHLY};

pub const MAX_TOKENS: i64 = 10_000_000;
pub const MAX_MULT: i64 = 10_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CreditsError {
    #[error("token count out of range: {0}")]
    InvalidTokenCount(i64),
    #[error("zero credit multiplier")]
    ZeroMultiplier,
    #[error("credit multiplier out of range: {0}")]
    InvalidMultiplier(i64),
    #[error("credit overflow")]
    Overflow,
}

#[allow(clippy::integer_division)] // explicit ceiling division
fn ceil_div(n: i64, d: i64) -> i64 {
    n / d + i64::from(n % d != 0)
}

/// `ceil_div(in * in_mult, 1e6) + ceil_div(out * out_mult, 1e6)` with
/// checked bounds (DESIGN §5.3).
pub fn credits_micro(
    input_tokens: i64,
    output_tokens: i64,
    in_mult: i64,
    out_mult: i64,
) -> Result<i64, CreditsError> {
    for t in [input_tokens, output_tokens] {
        if !(0..=MAX_TOKENS).contains(&t) {
            return Err(CreditsError::InvalidTokenCount(t));
        }
    }
    for m in [in_mult, out_mult] {
        if m == 0 {
            return Err(CreditsError::ZeroMultiplier);
        }
        if !(1..=MAX_MULT).contains(&m) {
            return Err(CreditsError::InvalidMultiplier(m));
        }
    }
    let a = input_tokens
        .checked_mul(in_mult)
        .ok_or(CreditsError::Overflow)?;
    let b = output_tokens
        .checked_mul(out_mult)
        .ok_or(CreditsError::Overflow)?;
    ceil_div(a, 1_000_000)
        .checked_add(ceil_div(b, 1_000_000))
        .ok_or(CreditsError::Overflow)
}

/// Multipliers of a catalog entry as `i64`.
#[must_use]
pub fn multipliers(m: &ModelCatalogEntry) -> (i64, i64) {
    (
        i64::try_from(m.input_tokens_credit_multiplier_micro).unwrap_or(i64::MAX),
        i64::try_from(m.output_tokens_credit_multiplier_micro).unwrap_or(i64::MAX),
    )
}

/// Conservative token estimate of a text (DESIGN §5.5.4).
#[must_use]
pub fn estimate_text_tokens(bytes: usize, b: &EstimationBudgets) -> i64 {
    let bpt = i64::from(b.bytes_per_token_conservative.max(1));
    let bytes = i64::try_from(bytes).unwrap_or(i64::MAX);
    let base = ceil_div(bytes, bpt).saturating_add(i64::from(b.fixed_overhead_tokens));
    ceil_div(
        base.saturating_mul(100 + i64::from(b.safety_margin_pct)),
        100,
    )
}

// ════════════════════════════════════════════════════════════════════════════
// Periods
// ════════════════════════════════════════════════════════════════════════════

/// Period starts of a UTC instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Periods {
    pub daily: Date,
    pub monthly: Date,
}

impl Periods {
    #[must_use]
    pub fn at(ts: OffsetDateTime) -> Self {
        let d = ts.to_offset(time::UtcOffset::UTC).date();
        let monthly = Date::from_calendar_date(d.year(), d.month(), 1).unwrap_or(d);
        Self { daily: d, monthly }
    }

    #[must_use]
    pub fn list(self) -> [(&'static str, Date); 2] {
        [(PERIOD_DAILY, self.daily), (PERIOD_MONTHLY, self.monthly)]
    }

    /// Next reset (RFC 3339 instant) of a period.
    #[must_use]
    pub fn next_reset(self, period: &str) -> OffsetDateTime {
        let date = if period == PERIOD_DAILY {
            self.daily.next_day().unwrap_or(self.daily)
        } else {
            let (y, m) = (self.monthly.year(), self.monthly.month());
            let (ny, nm) = if m == Month::December {
                (y + 1, Month::January)
            } else {
                (y, m.next())
            };
            Date::from_calendar_date(ny, nm, 1).unwrap_or(self.monthly)
        };
        date.with_time(Time::MIDNIGHT).assume_utc()
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Bucket usage
// ════════════════════════════════════════════════════════════════════════════

/// Counters of one bucket row.
#[derive(Debug, Clone, Copy, Default)]
struct BucketRow {
    spent: i64,
    reserved: i64,
    web_search_calls: i32,
    code_interpreter_calls: i32,
}

/// `(spent, reserved)` per `(period_type, bucket)`.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    rows: HashMap<(String, String), BucketRow>,
}

impl Usage {
    #[must_use]
    pub fn from_rows(rows: &[quota_usage::Model], periods: Periods) -> Self {
        let mut map = HashMap::new();
        for r in rows {
            let expected = if r.period_type == PERIOD_DAILY {
                periods.daily
            } else {
                periods.monthly
            };
            if r.period_start != expected {
                continue;
            }
            map.insert(
                (r.period_type.clone(), r.bucket.clone()),
                BucketRow {
                    spent: r.spent_credits_micro,
                    reserved: r.reserved_credits_micro,
                    web_search_calls: r.web_search_calls,
                    code_interpreter_calls: r.code_interpreter_calls,
                },
            );
        }
        Self { rows: map }
    }

    #[must_use]
    pub fn used(&self, period: &str, bucket: &str) -> (i64, i64) {
        self.rows
            .get(&(period.to_owned(), bucket.to_owned()))
            .map_or((0, 0), |r| (r.spent, r.reserved))
    }

    /// Daily `(web_search_calls, code_interpreter_calls)` of bucket `total`.
    #[must_use]
    pub fn daily_tool_calls(&self) -> (i64, i64) {
        self.rows
            .get(&(PERIOD_DAILY.to_owned(), BUCKET_TOTAL.to_owned()))
            .map_or((0, 0), |r| {
                (
                    i64::from(r.web_search_calls),
                    i64::from(r.code_interpreter_calls),
                )
            })
    }
}

/// Limit of a bucket and period.
#[must_use]
pub fn limit_for(limits: &UserLimits, bucket: &str, period: &str) -> i64 {
    let tl = if bucket == BUCKET_PREMIUM {
        limits.premium
    } else {
        limits.standard
    };
    if period == PERIOD_DAILY {
        tl.limit_daily_credits_micro
    } else {
        tl.limit_monthly_credits_micro
    }
}

/// Buckets charged for a tier.
#[must_use]
pub fn buckets_for(tier: ModelTier) -> &'static [&'static str] {
    match tier {
        ModelTier::Premium => &[BUCKET_TOTAL, BUCKET_PREMIUM],
        ModelTier::Standard => &[BUCKET_TOTAL],
    }
}

/// `spent + reserved + reserve <= limit` for every bucket and period of the
/// tier.
#[must_use]
pub fn tier_available(tier: ModelTier, usage: &Usage, limits: &UserLimits, reserve: i64) -> bool {
    for bucket in buckets_for(tier) {
        for period in [PERIOD_DAILY, PERIOD_MONTHLY] {
            let (spent, reserved) = usage.used(period, bucket);
            let limit = limit_for(limits, bucket, period);
            let total = spent.saturating_add(reserved).saturating_add(reserve);
            if total > limit {
                return false;
            }
        }
    }
    true
}

// ════════════════════════════════════════════════════════════════════════════
// Reserve estimation and the cascade
// ════════════════════════════════════════════════════════════════════════════

/// Chat state relevant to tool gates.
#[derive(Debug, Clone, Copy, Default)]
#[allow(clippy::struct_excessive_bools)] // one independent gate per tool
pub struct ChatToolState {
    pub has_ready_documents: bool,
    pub has_ready_code_interpreter_files: bool,
    pub web_search_requested: bool,
}

/// Tools sent with a model.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // one flag per built-in tool
pub struct ToolSet {
    pub file_search: bool,
    pub web_search: bool,
    pub code_interpreter: bool,
}

/// Tool gates of a candidate model (`tool_support` + kill switches + chat).
#[must_use]
pub fn tools_for(m: &ModelCatalogEntry, chat: ChatToolState, ks: KillSwitches) -> ToolSet {
    let ts = m.general_config.tool_support;
    ToolSet {
        file_search: chat.has_ready_documents && ts.file_search && !ks.disable_file_search,
        web_search: chat.web_search_requested && ts.web_search,
        code_interpreter: chat.has_ready_code_interpreter_files
            && ts.code_interpreter
            && !ks.disable_code_interpreter,
    }
}

/// Inputs of the reserve estimate that do not depend on the model.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReserveInputs {
    pub message_bytes: usize,
    pub prior_context_tokens: i64,
    pub image_count: usize,
    pub chat: ChatToolState,
    pub max_output_tokens_cfg: u32,
}

/// Reserve of one candidate model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_field_names)] // names mirror the persisted `chat_turns` columns
pub struct Reserve {
    pub estimated_input_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserve_tokens: i64,
    /// `i64::MAX` when the credits cannot be computed.
    pub reserved_credits_micro: i64,
    pub tools: ToolSet,
}

#[must_use]
pub fn candidate_reserve(m: &ModelCatalogEntry, inp: &ReserveInputs, ks: KillSwitches) -> Reserve {
    let b = &m.estimation_budgets;
    let tools = tools_for(m, inp.chat, ks);
    let mut est = estimate_text_tokens(inp.message_bytes, b)
        .saturating_add(inp.prior_context_tokens)
        .saturating_add(
            i64::try_from(inp.image_count).unwrap_or(0) * i64::from(b.image_token_budget),
        );
    if tools.file_search {
        est = est.saturating_add(i64::from(b.tool_surcharge_tokens));
    }
    if tools.web_search {
        est = est.saturating_add(i64::from(b.web_search_surcharge_tokens));
    }
    if tools.code_interpreter {
        est = est.saturating_add(i64::from(b.code_interpreter_surcharge_tokens));
    }
    let max_out = i64::from(m.max_output_tokens.min(inp.max_output_tokens_cfg));
    let (im, om) = multipliers(m);
    let credits = credits_micro(est, max_out, im, om).unwrap_or_else(|e| {
        tracing::warn!(model = %m.id, error = %e, "cascade candidate reserve cannot be computed");
        i64::MAX
    });
    Reserve {
        estimated_input_tokens: est,
        max_output_tokens_applied: max_out,
        reserve_tokens: est.saturating_add(max_out),
        reserved_credits_micro: credits,
        tools,
    }
}

/// Quota decision of a preflight.
#[derive(Debug, Clone)]
pub struct Decision {
    pub effective: ModelCatalogEntry,
    pub tier: ModelTier,
    pub downgrade: bool,
    pub downgrade_reason: Option<&'static str>,
    pub reserve: Reserve,
}

impl Decision {
    #[must_use]
    pub const fn decision_str(&self) -> &'static str {
        if self.downgrade { "downgrade" } else { "allow" }
    }
}

fn candidate<'a>(
    snapshot: &'a PolicySnapshot,
    tier: ModelTier,
    selected: &str,
) -> Option<&'a ModelCatalogEntry> {
    let enabled = || {
        snapshot
            .model_catalog
            .iter()
            .filter(move |m| m.enabled && m.tier == tier)
    };
    enabled()
        .find(|m| m.id == selected)
        .or_else(|| enabled().find(|m| m.is_default()))
        .or_else(|| enabled().next())
}

/// Resolve the effective model (DESIGN "Downgrade Decision Flow"). `None`
/// when no tier is available (429 `quota_exceeded`).
#[must_use]
pub fn cascade(
    selected_id: &str,
    snapshot: &PolicySnapshot,
    usage: &Usage,
    limits: &UserLimits,
    inp: &ReserveInputs,
) -> Option<Decision> {
    let ks = snapshot.kill_switches;
    let (start_tier, mut reason) = match snapshot.find_model(selected_id) {
        Some(m) if m.enabled => (m.tier, None),
        Some(m) => (m.tier, Some("model_disabled")),
        None => (ModelTier::Premium, Some("model_disabled")),
    };
    let tiers: &[ModelTier] = match start_tier {
        ModelTier::Premium => &[ModelTier::Premium, ModelTier::Standard],
        ModelTier::Standard => &[ModelTier::Standard],
    };
    for &tier in tiers {
        if tier == ModelTier::Premium {
            if ks.force_standard_tier {
                reason.get_or_insert("force_standard_tier");
                continue;
            }
            if ks.disable_premium_tier {
                reason.get_or_insert("disable_premium_tier");
                continue;
            }
        }
        let Some(model) = candidate(snapshot, tier, selected_id) else {
            continue;
        };
        let reserve = candidate_reserve(model, inp, ks);
        if reserve.reserved_credits_micro == i64::MAX
            || !tier_available(tier, usage, limits, reserve.reserved_credits_micro)
        {
            if tier == ModelTier::Premium {
                reason.get_or_insert("premium_quota_exhausted");
            }
            continue;
        }
        let downgrade = model.id != selected_id || reason.is_some();
        return Some(Decision {
            effective: model.clone(),
            tier,
            downgrade,
            downgrade_reason: if downgrade { reason } else { None },
            reserve,
        });
    }
    None
}

// ════════════════════════════════════════════════════════════════════════════
// Warnings / status
// ════════════════════════════════════════════════════════════════════════════

/// Quota state of one tier and period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodStatus {
    pub tier: &'static str,
    pub period: &'static str,
    pub limit: i64,
    pub used: i64,
    pub remaining: i64,
    pub remaining_percentage: u32,
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

/// Per tier (`premium`, `total`) and period; periods with limit `<= 0` are
/// skipped.
#[must_use]
pub fn statuses(
    usage: &Usage,
    limits: &UserLimits,
    periods: Periods,
    warning_threshold_pct: u8,
) -> Vec<PeriodStatus> {
    let mut out = Vec::new();
    for (tier, bucket) in [("premium", BUCKET_PREMIUM), ("total", BUCKET_TOTAL)] {
        for period in [PERIOD_DAILY, PERIOD_MONTHLY] {
            let limit = limit_for(limits, bucket, period);
            if limit <= 0 {
                continue;
            }
            let (spent, reserved) = usage.used(period, bucket);
            let used = spent.saturating_add(reserved);
            let remaining = limit.saturating_sub(used).max(0);
            #[allow(clippy::integer_division)] // floored percentage by definition
            let pct = i128::from(remaining) * 100 / i128::from(limit);
            let pct = u32::try_from(pct.clamp(0, 100)).unwrap_or(0);
            out.push(PeriodStatus {
                tier,
                period,
                limit,
                used,
                remaining,
                remaining_percentage: pct,
                next_reset: periods.next_reset(period),
                warning: pct <= 100 - u32::from(warning_threshold_pct),
                exhausted: pct == 0,
            });
        }
    }
    out
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod tests;
