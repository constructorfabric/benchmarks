//! Token estimation (`cpt-cf-mini-chat-algo-quota-and-billing-token-estimation`) and reserve
//! calculation (`cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation`) of one cascade
//! candidate. Preflight only: no budget but the persisted floor is used at settlement.

use mini_chat_sdk::{EstimationBudgets, KillSwitches, ModelCatalogEntry};
use toolkit_macros::domain_model;

use crate::domain::quota::credits::credits_micro;

/// What the request and the chat contribute to the estimate.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestFacts {
    /// UTF-8 byte length of the current message.
    pub message_bytes: u64,
    pub image_count: u32,
    /// Ready documents of the chat (file search).
    pub ready_documents: u32,
    /// Ready XLSX attachments of the chat (code interpreter).
    pub ready_xlsx_attachments: u32,
    /// The request flag `web_search.enabled`.
    pub web_search_enabled: bool,
}

/// The components of `estimated_input_tokens` (all integer, never floating point).
#[allow(
    clippy::struct_field_names,
    reason = "the field names are the glossary names of DESIGN 5.4.1"
)]
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenEstimate {
    pub estimated_text_tokens: i64,
    pub prior_context_tokens: i64,
    pub image_surcharge_tokens: i64,
    pub tool_surcharge_tokens: i64,
    pub web_search_surcharge_tokens: i64,
    pub code_interpreter_surcharge_tokens: i64,
    /// The sum of the six components above.
    pub estimated_input_tokens: i64,
}

/// The estimate of `entry` for the request: `estimated_text_tokens` is
/// `ceil((ceil(bytes / bytes_per_token_conservative) + fixed_overhead_tokens) * (100 +
/// safety_margin_pct) / 100)` in integer arithmetic (multiplying first; a
/// `bytes_per_token_conservative` of 0 is clamped to 1); the surcharges are fixed per-turn
/// additions applied once under the `tool_support` and kill switch gates of the candidate.
#[must_use]
pub fn estimate_tokens(
    entry: &ModelCatalogEntry,
    kill_switches: &KillSwitches,
    facts: &RequestFacts,
    prior_context_tokens: i64,
) -> TokenEstimate {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-before-context
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-budget-source
    // the estimate runs before context assembly, from the current message bytes, the prior
    // context and the fixed surcharges, with the budgets of the entry it is computed for
    let budgets = &entry.estimation_budgets;
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-budget-source
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-before-context
    let estimated_text_tokens = text_tokens(budgets, facts.message_bytes);
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-image
    let image_surcharge_tokens =
        i64::from(facts.image_count) * i64::from(budgets.image_token_budget);
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-image
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-surcharges
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-fixed-surcharges
    // each surcharge is added once under its gates and never scales with the number of
    // documents, searches or retrieval passes; `search_knowledge` carries none
    let tools = &entry.general_config.tool_support;
    let surcharge = |applies: bool, tokens: u32| if applies { i64::from(tokens) } else { 0 };
    let tool_surcharge_tokens = surcharge(
        facts.ready_documents > 0 && tools.file_search && !kill_switches.disable_file_search,
        budgets.tool_surcharge_tokens,
    );
    let web_search_surcharge_tokens = surcharge(
        facts.web_search_enabled && tools.web_search,
        budgets.web_search_surcharge_tokens,
    );
    let code_interpreter_surcharge_tokens = surcharge(
        facts.ready_xlsx_attachments > 0
            && tools.code_interpreter
            && !kill_switches.disable_code_interpreter,
        budgets.code_interpreter_surcharge_tokens,
    );
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-fixed-surcharges
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-surcharges
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-sum
    let estimated_input_tokens = [
        estimated_text_tokens,
        prior_context_tokens,
        image_surcharge_tokens,
        tool_surcharge_tokens,
        web_search_surcharge_tokens,
        code_interpreter_surcharge_tokens,
    ]
    .into_iter()
    .fold(0_i64, i64::saturating_add);
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-sum
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-preflight-only
    // preflight only: the budgets are never persisted and never read at settlement
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-preflight-only
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-return
    TokenEstimate {
        estimated_text_tokens,
        prior_context_tokens,
        image_surcharge_tokens,
        tool_surcharge_tokens,
        web_search_surcharge_tokens,
        code_interpreter_surcharge_tokens,
        estimated_input_tokens,
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-return
}

/// `ceil((ceil(bytes / bytes_per_token) + overhead) * (100 + margin) / 100)` in 128-bit integer
/// arithmetic, multiplying first and saturating at `i64::MAX`.
pub(crate) fn text_tokens(budgets: &EstimationBudgets, message_bytes: u64) -> i64 {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-text
    let bytes_per_token = u128::from(budgets.bytes_per_token_conservative.max(1));
    let base = u128::from(message_bytes).div_ceil(bytes_per_token)
        + u128::from(budgets.fixed_overhead_tokens);
    let scaled = (base * (100 + u128::from(budgets.safety_margin_pct))).div_ceil(100);
    i64::try_from(scaled).unwrap_or(i64::MAX)
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-text
}

/// The input limit check: the catalog `max_input_tokens` of the effective model is not 0 and
/// the estimated text tokens exceed it (0 means no separate limit).
#[must_use]
pub fn input_too_long(entry: &ModelCatalogEntry, estimated_text_tokens: i64) -> bool {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-input-limit
    // a `max_input_tokens` of 0 means no separate limit
    entry.max_input_tokens != 0 && estimated_text_tokens > i64::from(entry.max_input_tokens)
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-token-estimation:p1:inst-est-input-limit
}

/// The reserve of one candidate.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReserveCalc {
    /// The smaller of the catalog `max_output_tokens` and `streaming.max_output_tokens`.
    pub max_output_tokens_applied: i64,
    pub estimated_input_tokens: i64,
    /// `estimated_input_tokens + max_output_tokens_applied`.
    pub reserve_tokens: i64,
    /// The credits of the reserve, or `i64::MAX` when the computation failed.
    pub reserved_credits_micro: i64,
    /// True only when the credit computation failed: the candidate is never booked.
    pub credit_computation_failed: bool,
    /// The smaller of the gear configuration floor and `max_output_tokens_applied`.
    pub minimal_generation_floor_applied: i64,
}

/// The reserve of `entry` for an estimate; the same formula is used for the availability check
/// and for the booked reserve.
#[must_use]
pub fn calculate_reserve(
    entry: &ModelCatalogEntry,
    estimated_input_tokens: i64,
    streaming_max_output_tokens: u32,
    minimal_generation_floor: u32,
) -> ReserveCalc {
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation:p1:inst-reserve-max-output
    let max_output_tokens_applied =
        i64::from(entry.max_output_tokens.min(streaming_max_output_tokens));
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation:p1:inst-reserve-max-output
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation:p1:inst-reserve-tokens
    let reserve_tokens = estimated_input_tokens.saturating_add(max_output_tokens_applied);
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation:p1:inst-reserve-tokens
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation:p1:inst-reserve-credits
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation:p1:inst-reserve-same-formula
    // one function serves the availability check and the booked reserve
    let credits = credits_micro(
        estimated_input_tokens,
        max_output_tokens_applied,
        entry.input_tokens_credit_multiplier_micro,
        entry.output_tokens_credit_multiplier_micro,
    );
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation:p1:inst-reserve-same-formula
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation:p1:inst-reserve-credits
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation:p1:inst-reserve-compute-failed
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation:p1:inst-reserve-substitute-max
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation:p1:inst-reserve-mark-failed
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic:p1:inst-credit-error-handling
    // at preflight a failed computation is never a 500: the reserve is the `i64::MAX`
    // sentinel and the candidate is marked, so it is unavailable and never booked
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic:p1:inst-credit-failed
    let (reserved_credits_micro, credit_computation_failed) = match credits {
        Ok(credits) => (credits, false),
        Err(error) => {
            tracing::warn!(
                model = %entry.id,
                %error,
                "the reserve credits cannot be computed; the candidate is unavailable"
            );
            (i64::MAX, true)
        }
    };
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic:p1:inst-credit-failed
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-credit-arithmetic:p1:inst-credit-error-handling
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation:p1:inst-reserve-mark-failed
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation:p1:inst-reserve-substitute-max
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation:p1:inst-reserve-compute-failed
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation:p1:inst-reserve-floor
    // the floor comes from the gear configuration, never from the catalog entry
    let minimal_generation_floor_applied =
        i64::from(minimal_generation_floor).min(max_output_tokens_applied);
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation:p1:inst-reserve-floor
    // @cpt-begin:cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation:p1:inst-reserve-return
    ReserveCalc {
        max_output_tokens_applied,
        estimated_input_tokens,
        reserve_tokens,
        reserved_credits_micro,
        credit_computation_failed,
        minimal_generation_floor_applied,
    }
    // @cpt-end:cpt-cf-mini-chat-algo-quota-and-billing-reserve-calculation:p1:inst-reserve-return
}

#[cfg(test)]
#[path = "estimation_tests.rs"]
mod estimation_tests;
