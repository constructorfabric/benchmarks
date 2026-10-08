//! Quota policy: periods, the downgrade cascade with per-candidate reserves,
//! settlement math and quota status computation (DESIGN §3.2, §5).

use std::collections::HashMap;

use mini_chat_sdk::{
    KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, UsageTokens, UserLimits,
};
use time::{Date, Month, OffsetDateTime, Time};

use super::estimate::{credits_micro, estimate_text_tokens};

pub const BUCKET_TOTAL: &str = "total";
pub const BUCKET_PREMIUM: &str = "tier:premium";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Period {
    Daily,
    Monthly,
}

impl Period {
    pub const ALL: [Period; 2] = [Period::Daily, Period::Monthly];

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Monthly => "monthly",
        }
    }

    /// UTC calendar start (stored as DATE) of the period containing `now`.
    #[must_use]
    pub fn start(self, now: OffsetDateTime) -> Date {
        let d = now.to_offset(time::UtcOffset::UTC).date();
        match self {
            Self::Daily => d,
            Self::Monthly => d.replace_day(1).unwrap_or(d),
        }
    }

    /// Start of the next period (midnight UTC).
    #[must_use]
    pub fn next_reset(self, now: OffsetDateTime) -> OffsetDateTime {
        let start = self.start(now);
        let next = match self {
            Self::Daily => start.next_day().unwrap_or(start),
            Self::Monthly => {
                let (y, m) = if start.month() == Month::December {
                    (start.year() + 1, Month::January)
                } else {
                    (start.year(), start.month().next())
                };
                Date::from_calendar_date(y, m, 1).unwrap_or(start)
            }
        };
        next.with_time(Time::MIDNIGHT).assume_utc()
    }

    #[must_use]
    pub fn limit(self, limits: &mini_chat_sdk::TierLimits) -> i64 {
        match self {
            Self::Daily => limits.limit_daily_credits_micro,
            Self::Monthly => limits.limit_monthly_credits_micro,
        }
    }
}

/// Period starts captured at preflight and reused for settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeriodStarts {
    pub daily: Date,
    pub monthly: Date,
}

impl PeriodStarts {
    #[must_use]
    pub fn at(now: OffsetDateTime) -> Self {
        Self {
            daily: Period::Daily.start(now),
            monthly: Period::Monthly.start(now),
        }
    }

    #[must_use]
    pub fn get(self, p: Period) -> Date {
        match p {
            Period::Daily => self.daily,
            Period::Monthly => self.monthly,
        }
    }
}

/// Usage counters of one bucket row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketUsage {
    pub spent: i64,
    pub reserved: i64,
    pub web_search_calls: i64,
    pub code_interpreter_calls: i64,
}

/// Usage rows of a user for the current periods, keyed by (period, bucket).
#[derive(Debug, Clone, Default)]
pub struct UsageView {
    pub rows: HashMap<(Period, &'static str), BucketUsage>,
}

impl UsageView {
    #[must_use]
    pub fn get(&self, p: Period, bucket: &'static str) -> BucketUsage {
        self.rows.get(&(p, bucket)).copied().unwrap_or_default()
    }
}

#[must_use]
pub fn bucket_limit(limits: &UserLimits, bucket: &str, p: Period) -> i64 {
    if bucket == BUCKET_PREMIUM {
        p.limit(&limits.premium)
    } else {
        p.limit(&limits.standard)
    }
}

/// Request facts relevant to estimation.
#[derive(Debug, Clone, Copy, Default)]
#[allow(clippy::struct_excessive_bools, reason = "independent feature flags")]
pub struct RequestFacts {
    pub message_bytes: usize,
    pub prior_context_tokens: i64,
    pub num_images: u32,
    pub has_ready_docs: bool,
    pub has_ready_ci: bool,
    pub web_search_requested: bool,
}

/// Which built-in tools a model gets for the turn.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools, reason = "independent feature flags")]
pub struct ToolPlan {
    pub file_search: bool,
    pub web_search: bool,
    pub code_interpreter: bool,
}

impl ToolPlan {
    #[must_use]
    pub fn for_model(m: &ModelCatalogEntry, facts: &RequestFacts, ks: KillSwitches) -> Self {
        let ts = m.tool_support();
        Self {
            file_search: facts.has_ready_docs && ts.file_search && !ks.disable_file_search,
            web_search: facts.web_search_requested && ts.web_search,
            code_interpreter: facts.has_ready_ci
                && ts.code_interpreter
                && !ks.disable_code_interpreter,
        }
    }

    /// Value of the `metadata.feature` request field.
    #[must_use]
    pub fn feature_label(self) -> String {
        let mut parts = Vec::new();
        if self.file_search {
            parts.push("file_search");
        }
        if self.web_search {
            parts.push("web_search");
        }
        if self.code_interpreter {
            parts.push("code_interpreter");
        }
        if parts.is_empty() {
            "none".to_owned()
        } else {
            parts.join("+")
        }
    }

    /// Sum of the surcharges of the tools in the plan.
    #[must_use]
    pub fn surcharge(self, m: &ModelCatalogEntry) -> i64 {
        let b = &m.estimation_budgets;
        let mut s = 0i64;
        if self.file_search {
            s += i64::from(b.tool_surcharge_tokens);
        }
        if self.web_search {
            s += i64::from(b.web_search_surcharge_tokens);
        }
        if self.code_interpreter {
            s += i64::from(b.code_interpreter_surcharge_tokens);
        }
        s
    }
}

/// Reserve a candidate model would book.
#[derive(Debug, Clone)]
pub struct CandidateReserve {
    pub estimated_input_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserve_tokens: i64,
    /// `i64::MAX` when the credit computation failed.
    pub reserved_credits_micro: i64,
    pub tools: ToolPlan,
}

#[must_use]
pub fn candidate_reserve(
    m: &ModelCatalogEntry,
    facts: &RequestFacts,
    ks: KillSwitches,
    max_output_cap: u32,
) -> CandidateReserve {
    let tools = ToolPlan::for_model(m, facts, ks);
    let text = estimate_text_tokens(facts.message_bytes, &m.estimation_budgets);
    let images = i64::from(facts.num_images) * i64::from(m.estimation_budgets.image_token_budget);
    let estimated_input_tokens = text
        .saturating_add(facts.prior_context_tokens)
        .saturating_add(images)
        .saturating_add(tools.surcharge(m));
    let max_out = i64::from(m.max_output_tokens.min(max_output_cap));
    let reserved = credits_micro(
        estimated_input_tokens,
        max_out,
        m.input_tokens_credit_multiplier_micro,
        m.output_tokens_credit_multiplier_micro,
    )
    .unwrap_or_else(|e| {
        tracing::warn!(model = %m.id, error = %e, "cannot compute candidate reserve; candidate unavailable");
        i64::MAX
    });
    CandidateReserve {
        estimated_input_tokens,
        max_output_tokens_applied: max_out,
        reserve_tokens: estimated_input_tokens.saturating_add(max_out),
        reserved_credits_micro: reserved,
        tools,
    }
}

/// Downgrade reason values.
pub const REASON_PREMIUM_EXHAUSTED: &str = "premium_quota_exhausted";
pub const REASON_FORCE_STANDARD: &str = "force_standard_tier";
pub const REASON_DISABLE_PREMIUM: &str = "disable_premium_tier";
pub const REASON_MODEL_DISABLED: &str = "model_disabled";

#[derive(Debug, Clone)]
pub struct PreflightDecision {
    pub effective: ModelCatalogEntry,
    pub downgrade_reason: Option<String>,
    pub is_downgrade: bool,
    pub reserve: CandidateReserve,
}

fn tier_buckets(t: ModelTier) -> &'static [&'static str] {
    match t {
        ModelTier::Premium => &[BUCKET_TOTAL, BUCKET_PREMIUM],
        ModelTier::Standard => &[BUCKET_TOTAL],
    }
}

/// `spent + reserved + this <= limit` for all buckets of the tier and all
/// periods.
#[must_use]
pub fn tier_available(tier: ModelTier, usage: &UsageView, limits: &UserLimits, this: i64) -> bool {
    for bucket in tier_buckets(tier) {
        for p in Period::ALL {
            let u = usage.get(p, bucket);
            let limit = bucket_limit(limits, bucket, p);
            let total = u.spent.saturating_add(u.reserved).saturating_add(this);
            if total > limit {
                return false;
            }
        }
    }
    true
}

fn candidate_for_tier<'a>(
    snapshot: &'a PolicySnapshot,
    tier: ModelTier,
    selected: &str,
) -> Option<&'a ModelCatalogEntry> {
    let enabled: Vec<&ModelCatalogEntry> = snapshot
        .enabled_models()
        .filter(|m| m.tier == tier)
        .collect();
    enabled
        .iter()
        .find(|m| m.id == selected)
        .or_else(|| enabled.iter().find(|m| m.is_default()))
        .or_else(|| enabled.first())
        .copied()
}

/// Run the downgrade cascade. Returns `None` when no tier is available
/// (429 `quota_exceeded`, scope `tokens`).
#[must_use]
pub fn resolve_effective_model(
    snapshot: &PolicySnapshot,
    limits: &UserLimits,
    usage: &UsageView,
    selected_model: &str,
    facts: &RequestFacts,
    max_output_cap: u32,
) -> Option<PreflightDecision> {
    let ks = snapshot.kill_switches;
    let (start_tier, mut reason) = match snapshot.find(selected_model) {
        Some(m) if m.enabled => (m.tier, None),
        Some(m) => (m.tier, Some(REASON_MODEL_DISABLED.to_owned())),
        None => (ModelTier::Premium, Some(REASON_MODEL_DISABLED.to_owned())),
    };
    let cascade: &[ModelTier] = match start_tier {
        ModelTier::Premium => &[ModelTier::Premium, ModelTier::Standard],
        ModelTier::Standard => &[ModelTier::Standard],
    };
    for tier in cascade {
        if *tier == ModelTier::Premium && (ks.force_standard_tier || ks.disable_premium_tier) {
            if reason.is_none() {
                reason = Some(
                    if ks.force_standard_tier {
                        REASON_FORCE_STANDARD
                    } else {
                        REASON_DISABLE_PREMIUM
                    }
                    .to_owned(),
                );
            }
            continue;
        }
        let Some(candidate) = candidate_for_tier(snapshot, *tier, selected_model) else {
            continue;
        };
        let reserve = candidate_reserve(candidate, facts, ks, max_output_cap);
        if tier_available(*tier, usage, limits, reserve.reserved_credits_micro) {
            let is_downgrade = candidate.id != selected_model || reason.is_some();
            return Some(PreflightDecision {
                effective: candidate.clone(),
                downgrade_reason: if is_downgrade { reason } else { None },
                is_downgrade,
                reserve,
            });
        }
        if *tier == ModelTier::Premium && reason.is_none() {
            reason = Some(REASON_PREMIUM_EXHAUSTED.to_owned());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Settlement
// ---------------------------------------------------------------------------

/// Terminal trigger classification used by the billing outcome derivation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Terminal {
    Completed,
    Failed { error_code: String },
    Cancelled,
    Orphan,
}

impl Terminal {
    #[must_use]
    pub fn state(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed { .. } | Self::Orphan => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

const RELEASED_CODES: &[&str] = &[
    "context_length_exceeded",
    "validation_error",
    "input_too_long",
    "turn_setup_failed",
];

/// Persisted preflight values of a turn.
#[derive(Debug, Clone, Copy)]
pub struct ReserveFields {
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i64,
    pub reserved_credits_micro: i64,
    pub minimal_generation_floor_applied: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools, reason = "independent feature flags")]
#[allow(
    clippy::struct_field_names,
    reason = "names mirror the DB columns / wire contract"
)]
pub struct Settlement {
    pub billing_outcome: &'static str,
    pub settlement_method: &'static str,
    /// Credits charged to quota and emitted as `actual_credits_micro`.
    pub committed_credits_micro: i64,
    /// Token telemetry added to bucket `total` (actual settlements only).
    pub telemetry_input_tokens: i64,
    pub telemetry_output_tokens: i64,
    /// Whether tool-call counters are added (actual and estimated).
    pub count_tool_calls: bool,
    pub overshoot: bool,
    pub overshoot_capped: bool,
}

fn usage_known(u: Option<&UsageTokens>) -> bool {
    u.is_some_and(|u| u.input_tokens > 0 || u.output_tokens > 0)
}

/// Billing outcome derivation + settlement amount (DESIGN §5.7–§5.9).
///
/// # Errors
/// Returns the credit computation error (finalization must fail).
pub fn settle(
    terminal: &Terminal,
    usage: Option<&UsageTokens>,
    reserve: &ReserveFields,
    in_mult: i64,
    out_mult: i64,
    tolerance: f64,
) -> Result<Settlement, super::estimate::CreditError> {
    let estimated = |outcome: &'static str| -> Result<Settlement, super::estimate::CreditError> {
        let est_input = (reserve.reserve_tokens - reserve.max_output_tokens_applied).max(0);
        let credits = credits_micro(
            est_input,
            reserve.minimal_generation_floor_applied.max(0),
            in_mult,
            out_mult,
        )?;
        Ok(Settlement {
            billing_outcome: outcome,
            settlement_method: "estimated",
            committed_credits_micro: credits,
            telemetry_input_tokens: 0,
            telemetry_output_tokens: 0,
            count_tool_calls: true,
            overshoot: false,
            overshoot_capped: false,
        })
    };
    let actual = |outcome: &'static str,
                  u: &UsageTokens|
     -> Result<Settlement, super::estimate::CreditError> {
        let actual_credits = credits_micro(u.input_tokens, u.output_tokens, in_mult, out_mult)?;
        let actual_tokens = u.input_tokens.saturating_add(u.output_tokens);
        let mut committed = actual_credits;
        let mut overshoot = false;
        let mut capped = false;
        if actual_tokens > reserve.reserve_tokens {
            overshoot = true;
            #[allow(clippy::cast_precision_loss)]
            let factor = actual_tokens as f64 / (reserve.reserve_tokens.max(1) as f64);
            if factor > tolerance {
                committed = reserve.reserved_credits_micro;
                capped = true;
            }
        }
        Ok(Settlement {
            billing_outcome: outcome,
            settlement_method: "actual",
            committed_credits_micro: committed,
            telemetry_input_tokens: u.input_tokens,
            telemetry_output_tokens: u.output_tokens,
            count_tool_calls: true,
            overshoot,
            overshoot_capped: capped,
        })
    };
    match terminal {
        Terminal::Completed => {
            let u = usage.copied().unwrap_or_default();
            actual("completed", &u)
        }
        Terminal::Cancelled | Terminal::Orphan => estimated("aborted"),
        Terminal::Failed { error_code } => {
            if RELEASED_CODES.contains(&error_code.as_str()) {
                Ok(Settlement {
                    billing_outcome: "failed",
                    settlement_method: "released",
                    committed_credits_micro: 0,
                    telemetry_input_tokens: 0,
                    telemetry_output_tokens: 0,
                    count_tool_calls: false,
                    overshoot: false,
                    overshoot_capped: false,
                })
            } else if usage_known(usage) {
                actual("failed", usage.unwrap_or(&UsageTokens::default()))
            } else {
                estimated("failed")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Quota status
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodStatus {
    pub period: Period,
    pub limit: i64,
    pub used: i64,
    pub remaining: i64,
    pub remaining_percentage: u32,
    pub next_reset: OffsetDateTime,
    pub warning: bool,
    pub exhausted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierStatus {
    /// `premium` or `total`.
    pub tier: &'static str,
    pub periods: Vec<PeriodStatus>,
}

#[must_use]
#[allow(
    clippy::integer_division,
    reason = "intentional integer arithmetic (explicit rounding)"
)]
pub fn quota_status(
    limits: &UserLimits,
    usage: &UsageView,
    warning_threshold_pct: u8,
    now: OffsetDateTime,
) -> Vec<TierStatus> {
    let mut tiers = Vec::new();
    for (name, bucket) in [("premium", BUCKET_PREMIUM), ("total", BUCKET_TOTAL)] {
        let mut periods = Vec::new();
        for p in Period::ALL {
            let limit = bucket_limit(limits, bucket, p);
            if limit <= 0 {
                continue;
            }
            let u = usage.get(p, bucket);
            let used = u.spent.saturating_add(u.reserved).max(0);
            let remaining = (limit - used).max(0);
            let pct_i = i128::from(remaining) * 100 / i128::from(limit);
            let pct = u32::try_from(pct_i.clamp(0, 100)).unwrap_or(0);
            periods.push(PeriodStatus {
                period: p,
                limit,
                used,
                remaining,
                remaining_percentage: pct,
                next_reset: p.next_reset(now),
                warning: pct <= u32::from(100 - warning_threshold_pct.min(100)),
                exhausted: pct == 0,
            });
        }
        tiers.push(TierStatus {
            tier: name,
            periods,
        });
    }
    tiers
}

#[cfg(test)]
mod tests {
    use super::*;
    use mini_chat_sdk::{ModelPreference, TierLimits};
    use time::macros::datetime;
    use uuid::Uuid;

    fn model(id: &str, tier: ModelTier, mult: i64) -> ModelCatalogEntry {
        let mut m: ModelCatalogEntry =
            serde_json::from_value(serde_json::json!({"id": id, "tier": "standard"})).unwrap();
        m.tier = tier;
        m.enabled = true;
        m.context_window = 128_000;
        m.max_output_tokens = 500;
        m.input_tokens_credit_multiplier_micro = mult;
        m.output_tokens_credit_multiplier_micro = mult;
        m.preference = Some(ModelPreference::default());
        m
    }

    fn limits(total_daily: i64, prem_daily: i64) -> UserLimits {
        UserLimits {
            user_id: Uuid::nil(),
            policy_version: 1,
            standard: TierLimits {
                limit_daily_credits_micro: total_daily,
                limit_monthly_credits_micro: 600_000_000,
            },
            premium: TierLimits {
                limit_daily_credits_micro: prem_daily,
                limit_monthly_credits_micro: 300_000_000,
            },
        }
    }

    fn snapshot(models: Vec<ModelCatalogEntry>) -> PolicySnapshot {
        PolicySnapshot {
            policy_version: 1,
            model_catalog: models,
            kill_switches: KillSwitches::default(),
        }
    }

    #[test]
    fn periods() {
        let now = datetime!(2026-02-28 15:30:00 UTC);
        assert_eq!(Period::Daily.start(now).to_string(), "2026-02-28");
        assert_eq!(Period::Monthly.start(now).to_string(), "2026-02-01");
        assert_eq!(
            Period::Daily.next_reset(now),
            datetime!(2026-03-01 00:00:00 UTC)
        );
        assert_eq!(
            Period::Monthly.next_reset(datetime!(2026-12-10 00:00:00 UTC)),
            datetime!(2027-01-01 00:00:00 UTC)
        );
    }

    #[test]
    fn premium_allowed_when_available() {
        let snap = snapshot(vec![
            model("p", ModelTier::Premium, 1_000_000),
            model("s", ModelTier::Standard, 1_000_000),
        ]);
        let d = resolve_effective_model(
            &snap,
            &limits(60_000_000, 22_000_000),
            &UsageView::default(),
            "p",
            &RequestFacts::default(),
            32_768,
        )
        .unwrap();
        assert_eq!(d.effective.id, "p");
        assert!(!d.is_downgrade);
        assert!(d.downgrade_reason.is_none());
    }

    #[test]
    fn premium_exhausted_downgrades_to_standard() {
        let snap = snapshot(vec![
            model("p", ModelTier::Premium, 2_500_000_000),
            model("s", ModelTier::Standard, 1_000_000_000),
        ]);
        let mut usage = UsageView::default();
        usage.rows.insert(
            (Period::Daily, BUCKET_PREMIUM),
            BucketUsage {
                spent: 21_999_000,
                ..BucketUsage::default()
            },
        );
        let d = resolve_effective_model(
            &snap,
            &limits(60_000_000, 22_000_000),
            &usage,
            "p",
            &RequestFacts::default(),
            32_768,
        )
        .unwrap();
        assert_eq!(d.effective.id, "s");
        assert!(d.is_downgrade);
        assert_eq!(
            d.downgrade_reason.as_deref(),
            Some(REASON_PREMIUM_EXHAUSTED)
        );
    }

    #[test]
    fn all_tiers_exhausted_rejects() {
        let snap = snapshot(vec![
            model("p", ModelTier::Premium, 1_000_000),
            model("s", ModelTier::Standard, 1_000_000),
        ]);
        let mut usage = UsageView::default();
        usage.rows.insert(
            (Period::Monthly, BUCKET_TOTAL),
            BucketUsage {
                spent: 600_000_000,
                ..BucketUsage::default()
            },
        );
        assert!(
            resolve_effective_model(
                &snap,
                &limits(60_000_000, 22_000_000),
                &usage,
                "p",
                &RequestFacts::default(),
                32_768
            )
            .is_none()
        );
    }

    #[test]
    fn standard_never_upgrades_and_disabled_model_downgrades() {
        let mut s = model("s", ModelTier::Standard, 1_000_000);
        s.enabled = false;
        let snap = snapshot(vec![
            model("p", ModelTier::Premium, 1_000_000),
            s,
            model("s2", ModelTier::Standard, 1_000_000),
        ]);
        let d = resolve_effective_model(
            &snap,
            &limits(60_000_000, 22_000_000),
            &UsageView::default(),
            "s",
            &RequestFacts::default(),
            32_768,
        )
        .unwrap();
        assert_eq!(d.effective.id, "s2");
        assert_eq!(d.downgrade_reason.as_deref(), Some(REASON_MODEL_DISABLED));
    }

    #[test]
    fn kill_switch_skips_premium() {
        let mut snap = snapshot(vec![
            model("p", ModelTier::Premium, 1_000_000),
            model("s", ModelTier::Standard, 1_000_000),
        ]);
        snap.kill_switches.force_standard_tier = true;
        let d = resolve_effective_model(
            &snap,
            &limits(60_000_000, 22_000_000),
            &UsageView::default(),
            "p",
            &RequestFacts::default(),
            32_768,
        )
        .unwrap();
        assert_eq!(d.effective.id, "s");
        assert_eq!(d.downgrade_reason.as_deref(), Some(REASON_FORCE_STANDARD));
    }

    #[test]
    fn surcharges_follow_tool_support() {
        let mut m = model("p", ModelTier::Premium, 1_000_000);
        m.general_config.tool_support.web_search = true;
        let facts = RequestFacts {
            has_ready_docs: true,
            web_search_requested: true,
            ..RequestFacts::default()
        };
        let r = candidate_reserve(&m, &facts, KillSwitches::default(), 32_768);
        assert!(r.tools.web_search);
        assert!(!r.tools.file_search);
        assert_eq!(r.tools.feature_label(), "web_search");
        // text 110 + web search 500
        assert_eq!(r.estimated_input_tokens, 610);
        assert_eq!(r.max_output_tokens_applied, 500);
    }

    #[test]
    fn settlement_paths() {
        let reserve = ReserveFields {
            reserve_tokens: 10_000,
            max_output_tokens_applied: 4_000,
            reserved_credits_micro: 2_500_000,
            minimal_generation_floor_applied: 50,
        };
        let u = UsageTokens {
            input_tokens: 900,
            output_tokens: 300,
            ..UsageTokens::default()
        };
        let s = settle(
            &Terminal::Completed,
            Some(&u),
            &reserve,
            1_000_000_000,
            1_000_000_000,
            1.1,
        )
        .unwrap();
        assert_eq!(
            (s.billing_outcome, s.settlement_method),
            ("completed", "actual")
        );
        assert_eq!(s.committed_credits_micro, 1_200_000);
        // overshoot beyond tolerance is capped at the reserve
        let big = UsageTokens {
            input_tokens: 11_000,
            output_tokens: 500,
            ..UsageTokens::default()
        };
        let s = settle(
            &Terminal::Completed,
            Some(&big),
            &reserve,
            1_000,
            1_000,
            1.1,
        )
        .unwrap();
        assert!(s.overshoot_capped);
        assert_eq!(s.committed_credits_micro, 2_500_000);
        // cancelled -> estimated
        let s = settle(
            &Terminal::Cancelled,
            None,
            &reserve,
            1_000_000,
            1_000_000,
            1.1,
        )
        .unwrap();
        assert_eq!(
            (s.billing_outcome, s.settlement_method),
            ("aborted", "estimated")
        );
        assert_eq!(s.committed_credits_micro, 6_000 + 50);
        // failed without usage -> estimated, with usage -> actual
        let f = Terminal::Failed {
            error_code: "provider_error".into(),
        };
        let s = settle(
            &f,
            Some(&UsageTokens::default()),
            &reserve,
            1_000_000,
            1_000_000,
            1.1,
        )
        .unwrap();
        assert_eq!(s.settlement_method, "estimated");
        let s = settle(&f, Some(&u), &reserve, 1_000_000, 1_000_000, 1.1).unwrap();
        assert_eq!(s.settlement_method, "actual");
        // pre-provider codes release
        let s = settle(
            &Terminal::Failed {
                error_code: "turn_setup_failed".into(),
            },
            None,
            &reserve,
            1,
            1,
            1.1,
        )
        .unwrap();
        assert_eq!(s.settlement_method, "released");
        assert_eq!(s.committed_credits_micro, 0);
        // orphan -> aborted/estimated
        let s = settle(&Terminal::Orphan, None, &reserve, 1_000_000, 1_000_000, 1.1).unwrap();
        assert_eq!(s.billing_outcome, "aborted");
    }

    #[test]
    fn status_flags() {
        let lim = limits(100, 0);
        let mut usage = UsageView::default();
        usage.rows.insert(
            (Period::Daily, BUCKET_TOTAL),
            BucketUsage {
                spent: 81,
                reserved: 0,
                ..BucketUsage::default()
            },
        );
        let st = quota_status(&lim, &usage, 80, datetime!(2026-01-01 00:00:00 UTC));
        // premium daily limit 0 -> skipped, monthly kept
        assert_eq!(st[0].tier, "premium");
        assert_eq!(st[0].periods.len(), 1);
        let total_daily = &st[1].periods[0];
        assert_eq!(total_daily.remaining_percentage, 19);
        assert!(total_daily.warning);
        assert!(!total_daily.exhausted);
    }
}
