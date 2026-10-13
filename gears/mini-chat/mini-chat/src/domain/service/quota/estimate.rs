//! Reserve estimation
//! (`cpt-cf-mini-chat-algo-quota-billing-estimate-reserve`,
//! `cpt-cf-mini-chat-dod-quota-billing-token-estimation`).

use mini_chat_sdk::{KillSwitches, ModelCatalogEntry};
use toolkit_macros::domain_model;

use super::arith::credits_micro;

/// Inputs of the estimate of one cascade candidate.
#[domain_model]
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy)]
pub struct EstimateInput<'a> {
    /// The candidate catalog entry (budgets, multipliers, tool support).
    pub candidate: &'a ModelCatalogEntry,
    /// Kill switches of the snapshot.
    pub kill_switches: &'a KillSwitches,
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
    /// Tokens of the latest non-deleted assistant message with non-zero counts.
    pub prior_context_tokens: i64,
    /// `streaming.max_output_tokens` of the gear configuration.
    pub streaming_max_output_tokens: u32,
    /// `estimation_budgets.minimal_generation_floor` of the gear configuration.
    pub minimal_generation_floor: u32,
}

/// The five reserve values of one candidate.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReserveEstimate {
    /// Text, prior context and surcharges.
    pub estimated_input_tokens: i64,
    /// `min(catalog max_output_tokens, streaming.max_output_tokens)`.
    pub max_output_tokens_applied: i64,
    /// `estimated_input_tokens + max_output_tokens_applied`.
    pub reserve_tokens: i64,
    /// Credits of `estimated_input_tokens` and `max_output_tokens_applied`;
    /// [`i64::MAX`] when the sum or the credit computation fails.
    pub reserved_credits_micro: i64,
    /// `min(configured floor, max_output_tokens_applied)`.
    pub minimal_generation_floor_applied: i64,
}

/// Estimates the reserve of `input.candidate`.
///
/// On an overflow or a credit error (invalid multiplier, token count above
/// `10_000_000`) a warning naming the candidate model and the error is logged
/// and `reserved_credits_micro` is [`i64::MAX`], so that the candidate cannot
/// fit any bucket; no error is returned.
#[must_use]
// @cpt-dod:cpt-cf-mini-chat-dod-quota-billing-token-estimation:p1
pub fn estimate_reserve(input: &EstimateInput<'_>) -> ReserveEstimate {
    let candidate = input.candidate;
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-max-output
    let max_output = i64::from(
        candidate
            .max_output_tokens
            .min(input.streaming_max_output_tokens),
    );
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-max-output
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-floor-applied
    let floor = i64::from(input.minimal_generation_floor).min(max_output);
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-floor-applied
    let estimated_input = estimated_input_tokens(input);
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-reserve-tokens
    let reserve_tokens =
        estimated_input.and_then(|input_tokens| input_tokens.checked_add(max_output));
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-reserve-tokens
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-if-estimate-error
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-reserve-credits
    let credits = if let (Some(input_tokens), Some(_)) = (estimated_input, reserve_tokens) {
        credits_micro(
            input_tokens,
            max_output,
            candidate.input_tokens_credit_multiplier_micro,
            candidate.output_tokens_credit_multiplier_micro,
        )
        .unwrap_or_else(|error| {
            // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-max-reserve
            tracing::warn!(
                model = %candidate.id,
                %error,
                "reserve credits cannot be computed, the candidate cannot fit any bucket"
            );
            i64::MAX
            // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-max-reserve
        })
    } else {
        // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-max-reserve
        tracing::warn!(
            model = %candidate.id,
            "estimated tokens overflow, the candidate cannot fit any bucket"
        );
        i64::MAX
        // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-max-reserve
    };
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-reserve-credits
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-if-estimate-error
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-return-estimate
    ReserveEstimate {
        estimated_input_tokens: estimated_input.unwrap_or(i64::MAX),
        max_output_tokens_applied: max_output,
        reserve_tokens: reserve_tokens.unwrap_or(i64::MAX),
        reserved_credits_micro: credits,
        minimal_generation_floor_applied: floor,
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-return-estimate
}

/// The text estimate plus the prior context and every surcharge, in checked
/// arithmetic; `None` on overflow.
fn estimated_input_tokens(input: &EstimateInput<'_>) -> Option<i64> {
    let candidate = input.candidate;
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-read-budgets
    let budgets = &candidate.estimation_budgets;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-read-budgets
    let tools = &candidate.general_config.tool_support;
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-clamp-ratio
    let ratio = u128::from(budgets.bytes_per_token_conservative.max(1));
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-clamp-ratio
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-base-estimate
    let base = u128::from(input.message_bytes)
        .div_ceil(ratio)
        .checked_add(u128::from(budgets.fixed_overhead_tokens))?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-base-estimate
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-text-tokens
    let with_margin = base
        .checked_mul(100_u128.checked_add(u128::from(budgets.safety_margin_pct))?)?
        .div_ceil(100);
    let text = i64::try_from(with_margin).ok()?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-text-tokens
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-image-surcharge
    let images = i64::from(budgets.image_token_budget).checked_mul(i64::from(input.image_count))?;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-image-surcharge
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-tool-surcharge
    let file_search = surcharge(
        input.has_ready_file_search_documents
            && tools.file_search
            && !input.kill_switches.disable_file_search,
        budgets.tool_surcharge_tokens,
    );
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-tool-surcharge
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-web-surcharge
    let web_search = surcharge(
        input.web_search_enabled && tools.web_search,
        budgets.web_search_surcharge_tokens,
    );
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-web-surcharge
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-ci-surcharge
    let code_interpreter = surcharge(
        input.has_ready_code_interpreter_files
            && tools.code_interpreter
            && !input.kill_switches.disable_code_interpreter,
        budgets.code_interpreter_surcharge_tokens,
    );
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-ci-surcharge
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-estimated-input
    [
        text,
        input.prior_context_tokens,
        images,
        file_search,
        web_search,
        code_interpreter,
    ]
    .into_iter()
    .try_fold(0_i64, i64::checked_add)
    // @cpt-end:cpt-cf-mini-chat-algo-quota-billing-estimate-reserve:p1:inst-estimated-input
}

fn surcharge(applies: bool, tokens: u32) -> i64 {
    if applies { i64::from(tokens) } else { 0 }
}

#[cfg(test)]
#[path = "estimate_tests.rs"]
mod tests;
