//! Tier availability and the preflight cascade
//! (`cpt-cf-mini-chat-algo-quota-billing-tier-availability`,
//! `cpt-cf-mini-chat-algo-quota-billing-preflight`,
//! `cpt-cf-mini-chat-dod-quota-billing-preflight`). The orchestration (policy,
//! reads, tool quotas, metrics) is `QuotaService::preflight`; this module holds
//! the pure parts.

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry, ModelTier, PolicySnapshot, UserLimits};
use toolkit_macros::domain_model;
use uuid::Uuid;

use super::estimate::{EstimateInput, ReserveEstimate, estimate_reserve};
use super::periods::PeriodKeys;
use super::types::{BucketRow, QuotaBucket, QuotaPeriod, find_row, limit_of};

/// Whether the selected model runs or another one does.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaDecision {
    /// The selected model runs.
    Allow,
    /// Another model runs; see [`PreflightDecision::downgrade_reason`].
    Downgrade,
}

impl QuotaDecision {
    /// Wire value (`policy_decisions.quota.decision`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Downgrade => "downgrade",
        }
    }
}

/// Why a turn does not run on the selected model.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DowngradeReason {
    /// A premium bucket cannot take the premium reserve.
    PremiumQuotaExhausted,
    /// Kill switch `force_standard_tier`.
    ForceStandardTier,
    /// Kill switch `disable_premium_tier`.
    DisablePremiumTier,
    /// The selected model is disabled or missing from the catalog.
    ModelDisabled,
}

impl DowngradeReason {
    /// Wire value (`done.downgrade_reason`).
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

/// What the turn pipeline hands to the preflight.
#[domain_model]
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightRequest {
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// Requesting user.
    pub user_id: Uuid,
    /// The already authorized chat.
    pub chat_id: Uuid,
    /// `chats.model`.
    pub selected_model: String,
    /// UTF-8 byte length of the message content.
    pub message_bytes: u64,
    /// Number of images of the message.
    pub image_count: u32,
    /// `web_search.enabled` of the request.
    pub web_search_enabled: bool,
    /// The chat has a non-deleted `ready` attachment for file search.
    pub has_ready_file_search_documents: bool,
    /// The chat has a non-deleted `ready` attachment for the code interpreter.
    pub has_ready_code_interpreter_files: bool,
}

/// The preflight decision: computed once, never recomputed at settlement.
#[domain_model]
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq)]
pub struct PreflightDecision {
    /// Id of the effective model.
    pub effective_model: String,
    /// Catalog entry of the effective model.
    pub entry: ModelCatalogEntry,
    /// Tier of the effective model.
    pub effective_tier: ModelTier,
    /// `provider_id` of the effective model.
    pub provider_id: String,
    /// `allow` or `downgrade`.
    pub quota_decision: QuotaDecision,
    /// The selected model, when downgraded.
    pub downgrade_from: Option<String>,
    /// The reason, when downgraded.
    pub downgrade_reason: Option<DowngradeReason>,
    /// The policy version bound to the turn.
    pub policy_version: u64,
    /// The snapshot of that version (reused by the stream finalization).
    pub snapshot: PolicySnapshot,
    /// The user limits of that version (reused for the warnings).
    pub limits: UserLimits,
    /// The period starts of the preflight instant.
    pub period_keys: PeriodKeys,
    /// The five reserve values of the effective model.
    pub reserve: ReserveEstimate,
    /// `web_search` may be sent: enabled by the request and supported by the
    /// effective model.
    pub web_search_allowed: bool,
    /// `code_interpreter` may be sent: ready files, supported by the effective
    /// model and not disabled by the kill switch.
    pub code_interpreter_allowed: bool,
    /// The effective model has `VISION_INPUT`.
    pub vision_input: bool,
}

/// Tier availability: every required bucket (`total` for a standard
/// candidate; `total` then `tier:premium` for a premium one) must satisfy
/// `spent + reserved + candidate_reserve <= limit` for the daily and the
/// monthly period. A missing row counts as 0 and 0; an overflowing addition
/// makes the tier unavailable.
#[must_use]
pub fn tier_available(
    tier: ModelTier,
    rows: &[BucketRow],
    limits: &UserLimits,
    candidate_reserve_credits_micro: i64,
) -> bool {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-tier-availability:p1:inst-required-buckets
    let buckets: &[QuotaBucket] = match tier {
        ModelTier::Premium => &[QuotaBucket::Total, QuotaBucket::TierPremium],
        ModelTier::Standard => &[QuotaBucket::Total],
    };
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-tier-availability:p1:inst-required-buckets
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-tier-availability:p1:inst-for-each-bucket-period
    for bucket in buckets {
        for period in QuotaPeriod::ALL {
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-tier-availability:p1:inst-limit-mapping
            let limit = limit_of(limits, *bucket, period);
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-tier-availability:p1:inst-limit-mapping
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-tier-availability:p1:inst-row-values
            let (spent, reserved) = find_row(rows, period, *bucket).map_or((0, 0), |row| {
                (row.spent_credits_micro, row.reserved_credits_micro)
            });
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-tier-availability:p1:inst-row-values
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-tier-availability:p1:inst-if-bucket-over
            let fits = spent
                .checked_add(reserved)
                .and_then(|used| used.checked_add(candidate_reserve_credits_micro))
                .is_some_and(|total| total <= limit);
            if !fits {
                // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-tier-availability:p1:inst-return-unavailable
                return false;
                // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-tier-availability:p1:inst-return-unavailable
            }
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-tier-availability:p1:inst-if-bucket-over
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-tier-availability:p1:inst-for-each-bucket-period
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-tier-availability:p1:inst-return-available
    true
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-tier-availability:p1:inst-return-available
}

/// Inputs of the cascade.
#[domain_model]
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy)]
pub struct CascadeInput<'a> {
    /// The snapshot (catalog and kill switches).
    pub snapshot: &'a PolicySnapshot,
    /// The user limits.
    pub limits: &'a UserLimits,
    /// The preflight bucket rows.
    pub rows: &'a [BucketRow],
    /// `chats.model`.
    pub selected_model: &'a str,
    /// UTF-8 byte length of the message content.
    pub message_bytes: u64,
    /// Number of images of the message.
    pub image_count: u32,
    /// `web_search.enabled` of the request.
    pub web_search_enabled: bool,
    /// The chat has a ready file-search document.
    pub has_ready_file_search_documents: bool,
    /// The chat has a ready code-interpreter file.
    pub has_ready_code_interpreter_files: bool,
    /// Prior context tokens of the chat.
    pub prior_context_tokens: i64,
    /// `streaming.max_output_tokens`.
    pub streaming_max_output_tokens: u32,
    /// `estimation_budgets.minimal_generation_floor`.
    pub minimal_generation_floor: u32,
}

/// The cascade picked a model.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct CascadeOutcome {
    /// The effective catalog entry.
    pub entry: ModelCatalogEntry,
    /// Its tier.
    pub tier: ModelTier,
    /// Its reserve values (the booked reserve is the one that passed the check).
    pub estimate: ReserveEstimate,
    /// The downgrade reason, if any.
    pub downgrade_reason: Option<DowngradeReason>,
}

/// No tier is available (quota exhausted, or no enabled model in any tier of
/// the cascade).
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CascadeRejected {
    /// The starting tier (the `tier` label of the `reject` metric).
    pub starting_tier: ModelTier,
}

/// The premium to standard cascade (steps 6 to 10 of the preflight): start
/// tier and reason from the selected model, kill-switch skips, one candidate
/// per tier, availability with the candidate's own reserve.
///
/// # Errors
///
/// [`CascadeRejected`] when no tier is available.
pub fn run_cascade(input: &CascadeInput<'_>) -> Result<CascadeOutcome, CascadeRejected> {
    let catalog = &input.snapshot.model_catalog;
    let switches = &input.snapshot.kill_switches;
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-lookup-selected
    let selected = catalog
        .iter()
        .find(|entry| entry.id == input.selected_model);
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-lookup-selected
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-match-selected
    let (starting_tier, mut reason) = match selected {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-case-selected-enabled
        Some(entry) if entry.enabled => (entry.tier, None),
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-case-selected-enabled
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-case-selected-disabled
        Some(entry) => (entry.tier, Some(DowngradeReason::ModelDisabled)),
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-case-selected-disabled
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-case-selected-missing
        None => (ModelTier::Premium, Some(DowngradeReason::ModelDisabled)),
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-case-selected-missing
    };
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-match-selected
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-build-cascade
    let cascade: &[ModelTier] = match starting_tier {
        ModelTier::Premium => &[ModelTier::Premium, ModelTier::Standard],
        ModelTier::Standard => &[ModelTier::Standard],
    };
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-build-cascade
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-for-each-tier
    for &tier in cascade {
        if tier == ModelTier::Premium
            && let Some(skip) = premium_skipped_by(*switches)
        {
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-skip-force-standard
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-skip-premium-disabled
            reason.get_or_insert(skip);
            continue;
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-skip-premium-disabled
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-skip-force-standard
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-select-candidate
        let enabled = || {
            catalog
                .iter()
                .filter(move |entry| entry.tier == tier && entry.enabled)
        };
        let candidate = enabled()
            .find(|entry| entry.id == input.selected_model)
            .or_else(|| enabled().find(|entry| entry.preference.is_some_and(|p| p.is_default)))
            .or_else(|| enabled().next());
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-select-candidate
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-if-no-candidate
        let Some(candidate) = candidate else {
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-skip-empty-tier
            continue;
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-skip-empty-tier
        };
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-if-no-candidate
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-estimate-candidate
        let estimate = estimate_reserve(&EstimateInput {
            candidate,
            kill_switches: switches,
            message_bytes: input.message_bytes,
            image_count: input.image_count,
            web_search_enabled: input.web_search_enabled,
            has_ready_file_search_documents: input.has_ready_file_search_documents,
            has_ready_code_interpreter_files: input.has_ready_code_interpreter_files,
            prior_context_tokens: input.prior_context_tokens,
            streaming_max_output_tokens: input.streaming_max_output_tokens,
            minimal_generation_floor: input.minimal_generation_floor,
        });
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-estimate-candidate
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-check-candidate
        let available = tier_available(
            tier,
            input.rows,
            input.limits,
            estimate.reserved_credits_micro,
        );
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-check-candidate
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-if-candidate-available
        if available {
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-set-effective
            return Ok(CascadeOutcome {
                entry: candidate.clone(),
                tier,
                estimate,
                downgrade_reason: reason,
            });
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-set-effective
        }
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-if-candidate-available
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-else-premium-unavailable
        if tier == ModelTier::Premium {
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-reason-premium-exhausted
            reason.get_or_insert(DowngradeReason::PremiumQuotaExhausted);
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-reason-premium-exhausted
        }
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-else-premium-unavailable
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-for-each-tier
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-if-no-tier
    Err(CascadeRejected { starting_tier })
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-if-no-tier
}

/// Whether the kill switches or the model allow the premium tier at all.
#[must_use]
pub const fn premium_skipped_by(kill_switches: KillSwitches) -> Option<DowngradeReason> {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-if-force-standard
    if kill_switches.force_standard_tier {
        return Some(DowngradeReason::ForceStandardTier);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-if-force-standard
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-if-premium-disabled
    if kill_switches.disable_premium_tier {
        return Some(DowngradeReason::DisablePremiumTier);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-preflight:p1:inst-if-premium-disabled
    None
}

#[cfg(test)]
#[path = "preflight_tests.rs"]
mod tests;
