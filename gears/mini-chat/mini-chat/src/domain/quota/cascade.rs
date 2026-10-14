//! Downgrade cascade (`cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade`): the
//! effective model computed once at preflight from the selected model, the catalog, the kill
//! switches, the user limits and the usage rows.

use mini_chat_sdk::{ModelCatalogEntry, ModelTier, PolicySnapshot, QuotaDecision, UserLimits};
use toolkit_macros::domain_model;

use crate::domain::quota::availability::tier_available;
use crate::domain::quota::estimation::{
    RequestFacts, ReserveCalc, TokenEstimate, calculate_reserve, estimate_tokens,
};
use crate::domain::quota::rows::QuotaRows;
use crate::domain::quota::settings::QuotaSettings;

/// Why the effective model differs from the selected one.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DowngradeReason {
    PremiumQuotaExhausted,
    ForceStandardTier,
    DisablePremiumTier,
    ModelDisabled,
}

impl DowngradeReason {
    /// The wire value, for example `premium_quota_exhausted`.
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

/// The inputs of the cascade; `prior_context_tokens` is resolved once for the chat.
#[domain_model]
#[derive(Debug, Clone, Copy)]
pub struct CascadeInput<'a> {
    pub snapshot: &'a PolicySnapshot,
    pub selected_model: &'a str,
    pub limits: &'a UserLimits,
    pub rows: &'a QuotaRows,
    pub facts: &'a RequestFacts,
    pub prior_context_tokens: i64,
    pub settings: &'a QuotaSettings,
}

/// The effective model with its tier, decision and booked reserve.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct CascadeDecision {
    pub effective_model: String,
    pub tier: ModelTier,
    /// The `provider_id` of the effective model's catalog entry.
    pub provider_id: String,
    /// Whether the effective model's `multimodal_capabilities` contain `VISION_INPUT`.
    pub vision_input: bool,
    /// The `max_input_tokens` of the effective model's entry (0 means no separate limit).
    pub max_input_tokens: u32,
    pub quota_decision: QuotaDecision,
    /// The selected model for a downgrade, `None` for an allow.
    pub downgrade_from: Option<String>,
    pub downgrade_reason: Option<DowngradeReason>,
    /// The estimate and the reserve of the effective candidate (the reserve that passed the check).
    pub estimate: TokenEstimate,
    pub reserve: ReserveCalc,
}

/// Run the cascade from the tier of the selected model downward (premium, then standard; a
/// standard model never cascades up), checking one candidate per tier with the reserve it
/// would book. `None` is a reject with quota scope `tokens` (quota exhausted, no enabled model
/// in any tier of the cascade, or only uncomputable reserves).
#[must_use]
pub fn run_cascade(input: &CascadeInput<'_>) -> Option<CascadeDecision> {
    let snapshot = input.snapshot;
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-start
    // a disabled selected model keeps its own tier, a missing one starts at premium; both
    // carry the reason `model_disabled`
    let selected = snapshot
        .model_catalog
        .iter()
        .find(|entry| entry.id == input.selected_model);
    let (start_tier, mut reason) = match selected {
        Some(entry) if entry.enabled => (entry.tier, None),
        Some(entry) => (entry.tier, Some(DowngradeReason::ModelDisabled)),
        None => (ModelTier::Premium, Some(DowngradeReason::ModelDisabled)),
    };
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-start
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-order
    // premium gives premium then standard; a standard model never cascades up
    let cascade: &[ModelTier] = match start_tier {
        ModelTier::Premium => &[ModelTier::Premium, ModelTier::Standard],
        ModelTier::Standard => &[ModelTier::Standard],
    };
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-order
    let mut picked = None;
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-for-each
    for &tier in cascade {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-kill-switch
        let switched = (tier == ModelTier::Premium)
            .then(|| premium_kill_switch(snapshot))
            .flatten();
        if let Some(switch_reason) = switched {
            // the first reason stays: `model_disabled` is not replaced
            reason = reason.or(Some(switch_reason));
            continue;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-kill-switch
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-candidate
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-no-model
        // a tier without an enabled model counts as unavailable: the next tier is tried
        let Some(candidate) = candidate_of(snapshot, tier, input.selected_model) else {
            continue;
        };
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-no-model
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-candidate
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-check
        // only this one candidate per tier is estimated, priced and checked, with its own
        // budgets and tool support
        let estimate = estimate_tokens(
            candidate,
            &snapshot.kill_switches,
            input.facts,
            input.prior_context_tokens,
        );
        let reserve = calculate_reserve(
            candidate,
            estimate.estimated_input_tokens,
            input.settings.streaming_max_output_tokens,
            input.settings.minimal_generation_floor,
        );
        let available = tier_available(
            tier,
            reserve.reserved_credits_micro,
            reserve.credit_computation_failed,
            input.limits,
            input.rows,
        );
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-check
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-pick
        if available {
            picked = Some((candidate, tier, estimate, reserve));
            break;
        }
        if tier == ModelTier::Premium {
            reason = reason.or(Some(DowngradeReason::PremiumQuotaExhausted));
        }
        // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-pick
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-for-each
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-none
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-return-reject
    // no tier is available: quota exhausted, or no enabled model in any tier of the cascade
    let (candidate, tier, estimate, reserve) = picked?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-return-reject
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-none
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-decision
    let allowed = candidate.id == input.selected_model && reason.is_none();
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-decision
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-expose-capability
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-once
    // the effective model is resolved here once; the caller persists it and settlement reads it
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-return
    Some(CascadeDecision {
        effective_model: candidate.id.clone(),
        tier,
        provider_id: candidate.provider_id.clone(),
        vision_input: candidate
            .multimodal_capabilities
            .iter()
            .any(|capability| capability == VISION_INPUT),
        max_input_tokens: candidate.max_input_tokens,
        quota_decision: if allowed {
            QuotaDecision::Allow
        } else {
            QuotaDecision::Downgrade
        },
        downgrade_from: (!allowed).then(|| input.selected_model.to_owned()),
        downgrade_reason: reason,
        estimate,
        reserve,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-return
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-once
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-downgrade-cascade:p1:inst-cascade-expose-capability
}

/// The `multimodal_capabilities` value of image input.
const VISION_INPUT: &str = "VISION_INPUT";

/// The reason of a premium kill switch: `force_standard_tier` wins over `disable_premium_tier`.
fn premium_kill_switch(snapshot: &PolicySnapshot) -> Option<DowngradeReason> {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-quota-configuration:p1:inst-qc-kill-switches
    // the kill switches come from the snapshot; `disable_images` belongs to the image guards of
    // the other features
    let switches = &snapshot.kill_switches;
    if switches.force_standard_tier {
        Some(DowngradeReason::ForceStandardTier)
    } else if switches.disable_premium_tier {
        Some(DowngradeReason::DisablePremiumTier)
    } else {
        None
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-quota-configuration:p1:inst-qc-kill-switches
}

/// The candidate among the enabled models of `tier`: the selected model itself when it belongs
/// to the tier, otherwise the model whose tenant preference is the default, otherwise the first
/// enabled model of the tier in catalog order.
fn candidate_of<'a>(
    snapshot: &'a PolicySnapshot,
    tier: ModelTier,
    selected_model: &str,
) -> Option<&'a ModelCatalogEntry> {
    let enabled = || {
        snapshot
            .model_catalog
            .iter()
            .filter(move |entry| entry.enabled && entry.tier == tier)
    };
    enabled()
        .find(|entry| entry.id == selected_model)
        .or_else(|| enabled().find(|entry| entry.preference.as_ref().is_some_and(|p| p.is_default)))
        .or_else(|| enabled().next())
}

#[cfg(test)]
#[path = "cascade_tests.rs"]
mod cascade_tests;
