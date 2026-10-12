//! Quota arithmetic and decisions (DESIGN 5.3 - 5.5, 3.2 "Quota Period Reset
//! Semantics" / "Quota Warning Thresholds", 4 "Downgrade Decision Flow").
//!
//! Everything here is pure: credit computation, the preflight reserve, UTC
//! period math, bucket / tier availability, the downgrade cascade, the status
//! and warning builders, the billing outcome derivation and the settlement
//! formulas. Persistence (`quota_usage`) and orchestration live elsewhere.

use std::collections::HashMap;

use mini_chat_sdk::{
    BillingOutcome, KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, SettlementMethod,
    TerminalState, UserLimits,
};
use time::{Date, Month, OffsetDateTime, UtcOffset};

use crate::domain::provider::TokenBreakdown;
use crate::domain::turns::TurnState;

/// Largest token count a credit computation accepts (DESIGN 5.3 "Overflow
/// Protection").
pub const MAX_TOKENS: u64 = 10_000_000;

/// Largest credit multiplier a credit computation accepts.
pub const MAX_MULTIPLIER_MICRO: u64 = 10_000_000_000;

/// Micro-credits per credit unit of a multiplier (`multiplier` is per 1M tokens).
const MICRO: u64 = 1_000_000;

/// Why a credit computation was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreditsError {
    /// A token count is above [`MAX_TOKENS`].
    TokenCountOutOfRange,
    /// A multiplier is zero (the usage would be free).
    ZeroMultiplier,
    /// A multiplier is above [`MAX_MULTIPLIER_MICRO`].
    MultiplierOutOfRange,
}

impl std::fmt::Display for CreditsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TokenCountOutOfRange => "token count is out of range",
            Self::ZeroMultiplier => "credit multiplier is zero",
            Self::MultiplierOutOfRange => "credit multiplier is out of range",
        })
    }
}

impl std::error::Error for CreditsError {}

/// The canonical credit formula (DESIGN 5.3):
/// `ceil_div(input * in_mult, 1e6) + ceil_div(output * out_mult, 1e6)`.
///
/// # Errors
/// [`CreditsError`] when a token count or a multiplier is out of range; the
/// check runs before any multiplication.
pub fn credits_micro(
    input_tokens: u64,
    output_tokens: u64,
    in_mult: u64,
    out_mult: u64,
) -> Result<i64, CreditsError> {
    if input_tokens > MAX_TOKENS || output_tokens > MAX_TOKENS {
        return Err(CreditsError::TokenCountOutOfRange);
    }
    if in_mult == 0 || out_mult == 0 {
        return Err(CreditsError::ZeroMultiplier);
    }
    if in_mult > MAX_MULTIPLIER_MICRO || out_mult > MAX_MULTIPLIER_MICRO {
        return Err(CreditsError::MultiplierOutOfRange);
    }
    // Both products stay below 1e17: no overflow possible after the checks.
    let credits =
        (input_tokens * in_mult).div_ceil(MICRO) + (output_tokens * out_mult).div_ceil(MICRO);
    Ok(i64::try_from(credits).unwrap_or(i64::MAX))
}

// ── periods and buckets ──────────────────────────────────────────────────────

/// Quota period type (`quota_usage.period_type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Period {
    /// UTC calendar day.
    Daily,
    /// UTC calendar month.
    Monthly,
}

impl Period {
    /// Every enforced period, daily first.
    pub const ALL: [Self; 2] = [Self::Daily, Self::Monthly];

    /// The stored / wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Monthly => "monthly",
        }
    }

    /// Parses the stored spelling.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "daily" => Some(Self::Daily),
            "monthly" => Some(Self::Monthly),
            _ => None,
        }
    }
}

/// Quota bucket (`quota_usage.bucket`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Bucket {
    /// The overall cap across every tier.
    Total,
    /// The premium-only sub-cap.
    PremiumTier,
}

impl Bucket {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Total => "total",
            Self::PremiumTier => "tier:premium",
        }
    }

    /// Parses the stored spelling (`tier:standard` and unknown buckets are not
    /// enforcement buckets).
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "total" => Some(Self::Total),
            "tier:premium" => Some(Self::PremiumTier),
            _ => None,
        }
    }

    /// The buckets a turn of `tier` reserves from and settles against, in the
    /// fixed write order (`total` first).
    #[must_use]
    pub const fn of_tier(tier: ModelTier) -> &'static [Self] {
        match tier {
            ModelTier::Premium => &[Self::Total, Self::PremiumTier],
            ModelTier::Standard => &[Self::Total],
        }
    }
}

/// Tier as the status endpoint and `quota_warnings` name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaTier {
    /// The premium sub-cap (bucket `tier:premium`).
    Premium,
    /// The overall cap (bucket `total`).
    Total,
}

impl QuotaTier {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Premium => "premium",
            Self::Total => "total",
        }
    }

    const fn bucket(self) -> Bucket {
        match self {
            Self::Premium => Bucket::PremiumTier,
            Self::Total => Bucket::Total,
        }
    }
}

fn utc_date(at: OffsetDateTime) -> Date {
    at.to_offset(UtcOffset::UTC).date()
}

/// Start of the period of `period` that contains `at` (UTC day, or the 1st of
/// the UTC month).
#[must_use]
pub fn period_start(period: Period, at: OffsetDateTime) -> Date {
    let date = utc_date(at);
    match period {
        Period::Daily => date,
        Period::Monthly => date.replace_day(1).unwrap_or(date),
    }
}

/// The instant the period after the one containing `at` starts: the next UTC
/// midnight, or 00:00 UTC of the 1st of next month. At exactly a boundary the
/// next period is a full period away.
#[must_use]
pub fn next_reset(period: Period, at: OffsetDateTime) -> OffsetDateTime {
    let date = utc_date(at);
    let next = match period {
        Period::Daily => date.next_day(),
        Period::Monthly => {
            let year = if date.month() == Month::December {
                date.year() + 1
            } else {
                date.year()
            };
            Date::from_calendar_date(year, date.month().next(), 1).ok()
        }
    };
    next.unwrap_or(Date::MAX).midnight().assume_utc()
}

/// The `period_start` of every enforced period, fixed at preflight and carried
/// to the settlement (DESIGN 3.2 "Quota period boundary invariant").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeriodStarts {
    /// Start of the UTC day.
    pub daily: Date,
    /// First day of the UTC month.
    pub monthly: Date,
}

impl PeriodStarts {
    /// The starts of the periods containing `at`.
    #[must_use]
    pub fn at(at: OffsetDateTime) -> Self {
        Self {
            daily: period_start(Period::Daily, at),
            monthly: period_start(Period::Monthly, at),
        }
    }

    /// The start of `period`.
    #[must_use]
    pub const fn of(self, period: Period) -> Date {
        match period {
            Period::Daily => self.daily,
            Period::Monthly => self.monthly,
        }
    }
}

// ── usage ────────────────────────────────────────────────────────────────────

/// Credits already used in one bucket row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BucketUsage {
    /// Settled credits.
    pub spent_credits_micro: i64,
    /// Credits reserved by in-flight turns.
    pub reserved_credits_micro: i64,
}

/// One `quota_usage` row as the domain reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaRow {
    /// Bucket of the row.
    pub bucket: Bucket,
    /// Period type of the row.
    pub period: Period,
    /// Settled credits.
    pub spent_credits_micro: i64,
    /// Reserved credits.
    pub reserved_credits_micro: i64,
    /// Completed web search calls.
    pub web_search_calls: i64,
    /// Completed code interpreter calls.
    pub code_interpreter_calls: i64,
}

/// The current-period rows of one user, keyed by `(bucket, period)`.
#[derive(Debug, Clone, Default)]
pub struct QuotaUsage {
    rows: HashMap<(Bucket, Period), BucketUsage>,
    web_search_calls: HashMap<(Bucket, Period), i64>,
    code_interpreter_calls: HashMap<(Bucket, Period), i64>,
}

impl QuotaUsage {
    /// No rows: every bucket reads as zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the credits of one bucket row.
    #[must_use]
    pub fn with(mut self, bucket: Bucket, period: Period, spent: i64, reserved: i64) -> Self {
        self.rows.insert(
            (bucket, period),
            BucketUsage {
                spent_credits_micro: spent,
                reserved_credits_micro: reserved,
            },
        );
        self
    }

    /// The usage of the rows read from storage.
    #[must_use]
    pub fn from_rows(rows: &[QuotaRow]) -> Self {
        let mut usage = Self::new();
        for row in rows {
            usage = usage.with(
                row.bucket,
                row.period,
                row.spent_credits_micro,
                row.reserved_credits_micro,
            );
            usage
                .web_search_calls
                .insert((row.bucket, row.period), row.web_search_calls);
            usage
                .code_interpreter_calls
                .insert((row.bucket, row.period), row.code_interpreter_calls);
        }
        usage
    }

    /// The usage of one bucket (zeros without a row).
    #[must_use]
    pub fn get(&self, bucket: Bucket, period: Period) -> BucketUsage {
        self.rows
            .get(&(bucket, period))
            .copied()
            .unwrap_or_default()
    }

    /// Web search calls counted today in bucket `total` (zero without a row).
    #[must_use]
    pub fn daily_web_search_calls(&self) -> i64 {
        self.web_search_calls
            .get(&(Bucket::Total, Period::Daily))
            .copied()
            .unwrap_or(0)
    }

    /// Code interpreter calls counted today in bucket `total` (zero without a
    /// row).
    #[must_use]
    pub fn daily_code_interpreter_calls(&self) -> i64 {
        self.code_interpreter_calls
            .get(&(Bucket::Total, Period::Daily))
            .copied()
            .unwrap_or(0)
    }
}

fn limit_of(limits: &UserLimits, bucket: Bucket, period: Period) -> i64 {
    let tier = match bucket {
        Bucket::Total => &limits.standard,
        Bucket::PremiumTier => &limits.premium,
    };
    let raw = match period {
        Period::Daily => tier.limit_daily_credits_micro,
        Period::Monthly => tier.limit_monthly_credits_micro,
    };
    i64::try_from(raw).unwrap_or(i64::MAX)
}

/// The bucket check of DESIGN 5.4.2: `spent + reserved + this_request_reserve
/// <= limit`, overflow-safe.
#[must_use]
pub fn bucket_available(usage: BucketUsage, this_request_reserve: i64, limit: i64) -> bool {
    usage
        .spent_credits_micro
        .checked_add(usage.reserved_credits_micro)
        .and_then(|used| used.checked_add(this_request_reserve))
        .is_some_and(|total| total <= limit)
}

/// Whether `tier` has room for `reserve_credits_micro` in every enforced
/// period: bucket `total` for standard, `total` and `tier:premium` for
/// premium.
#[must_use]
pub fn tier_available(
    tier: ModelTier,
    usage: &QuotaUsage,
    limits: &UserLimits,
    reserve_credits_micro: i64,
) -> bool {
    Bucket::of_tier(tier).iter().all(|bucket| {
        Period::ALL.iter().all(|period| {
            bucket_available(
                usage.get(*bucket, *period),
                reserve_credits_micro,
                limit_of(limits, *bucket, *period),
            )
        })
    })
}

// ── reserve ──────────────────────────────────────────────────────────────────

/// The reserve a candidate model books (DESIGN 5.4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::struct_field_names,
    reason = "the field names are the pinned DESIGN 5.4.1 / chat_turns vocabulary"
)]
pub struct Reserve {
    /// `estimated_input_tokens`.
    pub estimated_input_tokens: u64,
    /// `min(model max output, streaming max output)`.
    pub max_output_tokens_applied: u32,
    /// `estimated_input_tokens + max_output_tokens_applied`.
    pub reserve_tokens: u64,
    /// Worst-case credits; `i64::MAX` when they cannot be computed.
    pub reserved_credits_micro: i64,
}

/// What the preflight knows about the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one fact per surcharge of DESIGN 5.4.1"
)]
pub struct ReserveFacts<'a> {
    /// The user message.
    pub message: &'a str,
    /// `input + output` tokens of the newest assistant message with usage.
    pub prior_context_tokens: u64,
    /// Number of images attached.
    pub image_count: u32,
    /// The chat has a ready document the `file_search` tool can read.
    pub has_ready_documents: bool,
    /// The request enabled web search.
    pub web_search_requested: bool,
    /// The chat has a ready code-interpreter attachment.
    pub has_code_interpreter_files: bool,
}

/// DESIGN 5.5.4: `ceil((ceil(bytes / bytes_per_token) + overhead) * (100 +
/// margin) / 100)` with `bytes_per_token` clamped to at least 1.
fn estimate_text_tokens(entry: &ModelCatalogEntry, text: &str) -> u64 {
    let budgets = &entry.estimation_budgets;
    let bytes = u64::try_from(text.len()).unwrap_or(u64::MAX);
    let per_token = u64::from(budgets.bytes_per_token_conservative.max(1));
    let base = bytes
        .div_ceil(per_token)
        .saturating_add(u64::from(budgets.fixed_overhead_tokens));
    base.saturating_mul(100 + u64::from(budgets.safety_margin_pct))
        .div_ceil(100)
}

/// The reserve `entry` would book for a request (DESIGN 5.4.1, 5.5.6): the
/// surcharge of a tool counts only when the model's `tool_support` allows it
/// and, for `file_search` and `code_interpreter`, its kill switch is off.
#[must_use]
pub fn estimate_reserve(
    entry: &ModelCatalogEntry,
    kill_switches: &KillSwitches,
    streaming_max_output_tokens: u32,
    facts: &ReserveFacts<'_>,
) -> Reserve {
    let budgets = &entry.estimation_budgets;
    let support = &entry.general_config.tool_support;
    let mut input = estimate_text_tokens(entry, facts.message)
        .saturating_add(facts.prior_context_tokens)
        .saturating_add(u64::from(facts.image_count) * u64::from(budgets.image_token_budget));
    if facts.has_ready_documents && support.file_search && !kill_switches.disable_file_search {
        input = input.saturating_add(u64::from(budgets.tool_surcharge_tokens));
    }
    if facts.web_search_requested && support.web_search {
        input = input.saturating_add(u64::from(budgets.web_search_surcharge_tokens));
    }
    if facts.has_code_interpreter_files
        && support.code_interpreter
        && !kill_switches.disable_code_interpreter
    {
        input = input.saturating_add(u64::from(budgets.code_interpreter_surcharge_tokens));
    }
    let max_output = entry.max_output_tokens.min(streaming_max_output_tokens);
    let reserved = credits_micro(
        input,
        u64::from(max_output),
        entry.input_tokens_credit_multiplier_micro,
        entry.output_tokens_credit_multiplier_micro,
    )
    .unwrap_or_else(|error| {
        tracing::warn!(model = %entry.id, %error, "reserve credits cannot be computed; candidate is unavailable");
        i64::MAX
    });
    Reserve {
        estimated_input_tokens: input,
        max_output_tokens_applied: max_output,
        reserve_tokens: input.saturating_add(u64::from(max_output)),
        reserved_credits_micro: reserved,
    }
}

// ── downgrade cascade ────────────────────────────────────────────────────────

/// Why a turn runs on another model than the chat's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DowngradeReason {
    /// A premium bucket is out of room.
    PremiumQuotaExhausted,
    /// The `force_standard_tier` kill switch.
    ForceStandardTier,
    /// The `disable_premium_tier` kill switch.
    DisablePremiumTier,
    /// The selected model is disabled or left the catalog.
    ModelDisabled,
}

impl DowngradeReason {
    /// The wire spelling.
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

/// The effective model of a turn and the reserve it books.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelDecision {
    /// Catalog id of the effective model.
    pub model_id: String,
    /// Its tier.
    pub tier: ModelTier,
    /// Its reserve (the one that passed the availability check).
    pub reserve: Reserve,
    /// Why the turn is downgraded, when it is.
    pub downgrade_reason: Option<DowngradeReason>,
}

/// No tier of the cascade has room (HTTP 429, quota scope `tokens`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaExhausted;

/// Inputs of [`resolve_effective_model`].
pub struct CascadeInput<'a> {
    /// `chats.model`.
    pub selected_model: &'a str,
    /// The policy snapshot of the turn.
    pub snapshot: &'a PolicySnapshot,
    /// Current-period usage.
    pub usage: &'a QuotaUsage,
    /// Per-user limits of the snapshot's version.
    pub limits: &'a UserLimits,
    /// The reserve a candidate model books.
    pub reserve_of: &'a dyn Fn(&ModelCatalogEntry) -> Reserve,
}

/// The candidate model of `tier`: the selected model when it is enabled and of
/// the tier, else the tier's `is_default` model, else its first enabled model.
fn candidate_of<'a>(
    catalog: &'a [ModelCatalogEntry],
    selected: &str,
    tier: ModelTier,
) -> Option<&'a ModelCatalogEntry> {
    let in_tier = || {
        catalog
            .iter()
            .filter(move |entry| entry.enabled && entry.tier == tier)
    };
    in_tier()
        .find(|entry| entry.id == selected)
        .or_else(|| {
            in_tier().find(|entry| {
                entry
                    .preference
                    .as_ref()
                    .is_some_and(|preference| preference.is_default)
            })
        })
        .or_else(|| in_tier().next())
}

/// DESIGN 4 "Downgrade Decision Flow": the first tier of the cascade whose
/// candidate fits every bucket of every period. The first downgrade reason
/// wins.
///
/// # Errors
/// [`QuotaExhausted`] when no tier is available (quota exhausted or no enabled
/// model in the cascade).
pub fn resolve_effective_model(input: &CascadeInput<'_>) -> Result<ModelDecision, QuotaExhausted> {
    let catalog = &input.snapshot.model_catalog;
    let switches = &input.snapshot.kill_switches;
    let selected = catalog
        .iter()
        .find(|entry| entry.id == input.selected_model);
    let (start, mut reason) = match selected {
        Some(entry) if entry.enabled => (entry.tier, None),
        Some(entry) => (entry.tier, Some(DowngradeReason::ModelDisabled)),
        None => (ModelTier::Premium, Some(DowngradeReason::ModelDisabled)),
    };
    let cascade: &[ModelTier] = match start {
        ModelTier::Premium => &[ModelTier::Premium, ModelTier::Standard],
        ModelTier::Standard => &[ModelTier::Standard],
    };
    for tier in cascade.iter().copied() {
        if tier == ModelTier::Premium {
            if switches.force_standard_tier {
                reason.get_or_insert(DowngradeReason::ForceStandardTier);
                continue;
            }
            if switches.disable_premium_tier {
                reason.get_or_insert(DowngradeReason::DisablePremiumTier);
                continue;
            }
        }
        let Some(candidate) = candidate_of(catalog, input.selected_model, tier) else {
            continue;
        };
        let reserve = (input.reserve_of)(candidate);
        if tier_available(
            tier,
            input.usage,
            input.limits,
            reserve.reserved_credits_micro,
        ) {
            return Ok(ModelDecision {
                model_id: candidate.id.clone(),
                tier,
                reserve,
                downgrade_reason: reason,
            });
        }
        if tier == ModelTier::Premium {
            reason.get_or_insert(DowngradeReason::PremiumQuotaExhausted);
        }
    }
    Err(QuotaExhausted)
}

// ── status and warnings ──────────────────────────────────────────────────────

/// One period of a tier in the status endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodStatus {
    /// The period.
    pub period: Period,
    /// The limit (always positive).
    pub limit_credits_micro: i64,
    /// `spent + reserved`.
    pub used_credits_micro: i64,
    /// `max(0, limit - used)`.
    pub remaining_credits_micro: i64,
    /// Floored `remaining * 100 / limit`.
    pub remaining_percentage: u8,
    /// When the period resets.
    pub next_reset: OffsetDateTime,
    /// `remaining_percentage <= 100 - warning_threshold_pct`.
    pub warning: bool,
    /// `remaining_percentage == 0`.
    pub exhausted: bool,
}

/// One tier of the status endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierStatus {
    /// The tier.
    pub tier: QuotaTier,
    /// Its periods, daily then monthly; periods without a positive limit are
    /// not listed.
    pub periods: Vec<PeriodStatus>,
}

fn period_status(
    limits: &UserLimits,
    usage: &QuotaUsage,
    tier: QuotaTier,
    period: Period,
    threshold_pct: u8,
    now: OffsetDateTime,
) -> Option<PeriodStatus> {
    let limit = limit_of(limits, tier.bucket(), period);
    if limit <= 0 {
        return None;
    }
    let row = usage.get(tier.bucket(), period);
    let used = row
        .spent_credits_micro
        .saturating_add(row.reserved_credits_micro);
    let remaining = limit.saturating_sub(used).max(0);
    let percentage = i128::from(remaining)
        .saturating_mul(100)
        .checked_div(i128::from(limit))
        .unwrap_or(0);
    let remaining_percentage = u8::try_from(percentage.clamp(0, 100)).unwrap_or(0);
    Some(PeriodStatus {
        period,
        limit_credits_micro: limit,
        used_credits_micro: used,
        remaining_credits_micro: remaining,
        remaining_percentage,
        next_reset: next_reset(period, now),
        warning: remaining_percentage <= 100u8.saturating_sub(threshold_pct),
        exhausted: remaining_percentage == 0,
    })
}

/// The status of every tier: premium from `UserLimits.premium` / bucket
/// `tier:premium`, total from `UserLimits.standard` / bucket `total`.
#[must_use]
pub fn quota_status(
    limits: &UserLimits,
    usage: &QuotaUsage,
    warning_threshold_pct: u8,
    now: OffsetDateTime,
) -> Vec<TierStatus> {
    [QuotaTier::Premium, QuotaTier::Total]
        .into_iter()
        .map(|tier| TierStatus {
            tier,
            periods: Period::ALL
                .into_iter()
                .filter_map(|period| {
                    period_status(limits, usage, tier, period, warning_threshold_pct, now)
                })
                .collect(),
        })
        .collect()
}

/// One entry of `done.quota_warnings`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaWarning {
    /// The tier.
    pub tier: QuotaTier,
    /// The period.
    pub period: Period,
    /// Floored remaining percentage.
    pub remaining_percentage: u8,
    /// Threshold crossed.
    pub warning: bool,
    /// Nothing (less than 1 %) left.
    pub exhausted: bool,
    /// Present only when `warning` or `exhausted`.
    pub next_reset: Option<OffsetDateTime>,
}

/// One warning entry per `(tier, period)` of the status.
#[must_use]
pub fn quota_warnings(status: &[TierStatus]) -> Vec<QuotaWarning> {
    status
        .iter()
        .flat_map(|tier| {
            tier.periods.iter().map(|period| QuotaWarning {
                tier: tier.tier,
                period: period.period,
                remaining_percentage: period.remaining_percentage,
                warning: period.warning,
                exhausted: period.exhausted,
                next_reset: (period.warning || period.exhausted).then_some(period.next_reset),
            })
        })
        .collect()
}

// ── billing and settlement ───────────────────────────────────────────────────

/// How a terminal outcome is billed (DESIGN 5.8 "Normative Billing Outcome
/// Derivation").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::struct_field_names,
    reason = "the field names are the usage event vocabulary of DESIGN 5.8"
)]
pub struct Billing {
    /// `terminal_state` of the usage event.
    pub terminal_state: TerminalState,
    /// `billing_outcome` of the usage event.
    pub billing_outcome: BillingOutcome,
    /// `settlement_method` of the usage event.
    pub settlement_method: SettlementMethod,
}

/// Error codes of failures after the provider request started.
const POST_PROVIDER_CODES: [&str; 8] = [
    "provider_error",
    "provider_timeout",
    "rate_limited",
    "web_search_calls_exceeded",
    "code_interpreter_calls_exceeded",
    "agentic_iterations_exceeded",
    "unexpected_tool_use",
    "message_persistence_failed",
];

/// Error codes of failures before the provider request started.
const PRE_PROVIDER_CODES: [&str; 4] = [
    "context_length_exceeded",
    "validation_error",
    "input_too_long",
    "turn_setup_failed",
];

/// The error code of a turn finalized by the orphan watchdog.
const ORPHAN_TIMEOUT: &str = "orphan_timeout";

/// Derives the billing of a terminal outcome. Usage is "known" for a failed
/// turn only when a field is non-zero; a completed turn always settles actual.
#[must_use]
pub fn derive_billing(
    state: TurnState,
    error_code: Option<&str>,
    usage: Option<(u64, u64)>,
) -> Billing {
    let failed = |outcome, method| Billing {
        terminal_state: TerminalState::Failed,
        billing_outcome: outcome,
        settlement_method: method,
    };
    match state {
        TurnState::Completed => Billing {
            terminal_state: TerminalState::Completed,
            billing_outcome: BillingOutcome::Completed,
            settlement_method: SettlementMethod::Actual,
        },
        TurnState::Cancelled => Billing {
            terminal_state: TerminalState::Cancelled,
            billing_outcome: BillingOutcome::Aborted,
            settlement_method: SettlementMethod::Estimated,
        },
        // A turn that is still running has no outcome yet; it is never settled,
        // so it falls into the conservative unknown-code case.
        TurnState::Failed | TurnState::Running => match error_code {
            Some(code) if POST_PROVIDER_CODES.contains(&code) => {
                let known = usage.is_some_and(|(input, output)| input > 0 || output > 0);
                failed(
                    BillingOutcome::Failed,
                    if known {
                        SettlementMethod::Actual
                    } else {
                        SettlementMethod::Estimated
                    },
                )
            }
            Some(code) if PRE_PROVIDER_CODES.contains(&code) => {
                failed(BillingOutcome::Failed, SettlementMethod::Released)
            }
            Some(ORPHAN_TIMEOUT) => failed(BillingOutcome::Aborted, SettlementMethod::Estimated),
            _ => failed(BillingOutcome::Failed, SettlementMethod::Estimated),
        },
    }
}

/// The persisted reserve of a turn (immutable columns of `chat_turns`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnReserve {
    /// `chat_turns.reserve_tokens`.
    pub reserve_tokens: u64,
    /// `chat_turns.max_output_tokens_applied`.
    pub max_output_tokens_applied: u32,
    /// `chat_turns.reserved_credits_micro`.
    pub reserved_credits_micro: i64,
    /// `chat_turns.minimal_generation_floor_applied`.
    pub minimal_generation_floor_applied: u32,
}

/// What a settlement debits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Charge {
    /// Committed credits.
    pub credits_micro: i64,
    /// How they were settled.
    pub method: SettlementMethod,
    /// Provider input tokens (actual settlements only; telemetry).
    pub input_tokens: u64,
    /// Provider output tokens (actual settlements only; telemetry).
    pub output_tokens: u64,
    /// Provider cache / reasoning breakdown (actual settlements only;
    /// telemetry, subsets of the totals above).
    pub breakdown: TokenBreakdown,
}

impl Charge {
    /// A release: the reserve is returned, nothing is debited.
    #[must_use]
    pub const fn released() -> Self {
        Self {
            credits_micro: 0,
            method: SettlementMethod::Released,
            input_tokens: 0,
            output_tokens: 0,
            breakdown: TokenBreakdown::NONE,
        }
    }
}

/// DESIGN 5.5.14: the floor persisted for a turn.
#[must_use]
pub fn minimal_generation_floor_applied(config_floor: u32, max_output_tokens_applied: u32) -> u32 {
    config_floor.min(max_output_tokens_applied)
}

/// Settlement by provider usage with the bounded overshoot rule (DESIGN
/// 5.4.5): usage above the reserve is committed in full while
/// `actual_tokens / reserve_tokens` (floating point) stays within the
/// tolerance, otherwise the charge is capped at the reserved credits.
///
/// # Errors
/// [`CreditsError`] when the usage or a multiplier is out of range.
#[allow(
    clippy::cast_precision_loss,
    reason = "the overshoot ratio is a floating-point comparison by contract (DESIGN 5.4.5)"
)]
pub fn settle_actual(
    reserve: &TurnReserve,
    usage: (u64, u64),
    in_mult: u64,
    out_mult: u64,
    overshoot_tolerance_factor: f64,
) -> Result<Charge, CreditsError> {
    let (input, output) = usage;
    let actual = credits_micro(input, output, in_mult, out_mult)?;
    let tokens = input.saturating_add(output);
    let capped = tokens > reserve.reserve_tokens
        && (tokens as f64) / (reserve.reserve_tokens as f64) > overshoot_tolerance_factor;
    Ok(Charge {
        credits_micro: if capped {
            reserve.reserved_credits_micro
        } else {
            actual
        },
        method: SettlementMethod::Actual,
        input_tokens: input,
        output_tokens: output,
        breakdown: TokenBreakdown::NONE,
    })
}

/// Deterministic estimated settlement from the persisted columns (DESIGN
/// 5.5.14): the input estimate plus the minimal generation floor, never more
/// than the reserve.
///
/// # Errors
/// [`CreditsError`] when a multiplier is out of range.
pub fn settle_estimated(
    reserve: &TurnReserve,
    in_mult: u64,
    out_mult: u64,
) -> Result<Charge, CreditsError> {
    let estimated_input = reserve
        .reserve_tokens
        .saturating_sub(u64::from(reserve.max_output_tokens_applied));
    let charged_output = u64::from(reserve.minimal_generation_floor_applied)
        .min(reserve.reserve_tokens.saturating_sub(estimated_input));
    Ok(Charge {
        credits_micro: credits_micro(estimated_input, charged_output, in_mult, out_mult)?,
        method: SettlementMethod::Estimated,
        input_tokens: 0,
        output_tokens: 0,
        breakdown: TokenBreakdown::NONE,
    })
}
