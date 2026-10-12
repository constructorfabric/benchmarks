//! Token estimation, tool availability and the preflight reserve (DESIGN sections 5.4.1, 5.5).

use mini_chat_sdk::{EstimationBudgets, KillSwitches, ModelCatalogEntry};
use toolkit_macros::domain_model;

use super::credits::{CreditsError, credits_micro};

/// `ceil((ceil(bytes / bytes_per_token) + fixed_overhead) * (100 + margin) / 100)`: the conservative
/// token estimate of a text of `bytes` UTF-8 bytes (no tokenizer). A zero `bytes_per_token` counts as 1.
#[must_use]
pub fn estimated_text_tokens(bytes: u64, budgets: &EstimationBudgets) -> i64 {
    let per_token = u64::from(budgets.bytes_per_token_conservative.max(1));
    let base = bytes
        .div_ceil(per_token)
        .saturating_add(u64::from(budgets.fixed_overhead_tokens));
    let margin = 100_u64.saturating_add(u64::from(budgets.safety_margin_pct));
    let tokens = base.saturating_mul(margin).div_ceil(100);
    i64::try_from(tokens).unwrap_or(i64::MAX)
}

/// The built-in tools of one provider request.
#[domain_model]
#[allow(clippy::struct_excessive_bools, reason = "one flag per built-in tool")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolSet {
    /// `file_search` over the chat vector store.
    pub file_search: bool,
    /// `web_search`.
    pub web_search: bool,
    /// `code_interpreter` with the chat's ready spreadsheet files.
    pub code_interpreter: bool,
}

impl ToolSet {
    /// The `feature` metadata value of the request: `none` or the tools joined with `+`.
    #[must_use]
    pub fn feature_label(self) -> String {
        let mut names = Vec::new();
        if self.file_search {
            names.push("file_search");
        }
        if self.web_search {
            names.push("web_search");
        }
        if self.code_interpreter {
            names.push("code_interpreter");
        }
        if names.is_empty() {
            "none".to_owned()
        } else {
            names.join("+")
        }
    }
}

/// What the request and the chat offer to a model before the model's own tool support is applied.
#[domain_model]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per fact the tool gates read"
)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolFacts {
    /// `web_search.enabled` of the request.
    pub web_search_requested: bool,
    /// The chat has at least one ready, non-deleted attachment with `for_file_search = true`.
    pub ready_documents: bool,
    /// The chat has at least one ready code-interpreter attachment.
    pub ready_code_interpreter_files: bool,
}

/// The tools a model receives: the request/chat facts gated by the model's catalog `tool_support`
/// and the kill switches. `web_search` under `disable_web_search` is rejected before this point.
#[must_use]
pub fn tools_for(
    entry: &ModelCatalogEntry,
    kill_switches: KillSwitches,
    facts: ToolFacts,
) -> ToolSet {
    let support = &entry.general_config.tool_support;
    ToolSet {
        file_search: facts.ready_documents
            && support.file_search
            && !kill_switches.disable_file_search,
        web_search: facts.web_search_requested && support.web_search,
        code_interpreter: facts.ready_code_interpreter_files
            && support.code_interpreter
            && !kill_switches.disable_code_interpreter,
    }
}

/// Inputs of a candidate's reserve that do not depend on the model.
#[domain_model]
#[derive(Debug, Clone, Copy)]
pub struct ReserveInputs {
    /// UTF-8 bytes of the current user message.
    pub content_bytes: u64,
    /// `input_tokens + output_tokens` of the latest assistant message with usage.
    pub prior_context_tokens: i64,
    /// Images attached to the current message.
    pub image_count: u32,
    /// `streaming.max_output_tokens`.
    pub streaming_max_output_tokens: u32,
}

/// The reserve a candidate model would book.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReserveEstimate {
    /// Estimated input tokens (text, prior context, image and tool surcharges).
    pub estimated_input_tokens: i64,
    /// `min(catalog max_output_tokens, streaming.max_output_tokens)`.
    pub max_output_tokens_applied: u32,
    /// `estimated_input_tokens + max_output_tokens_applied`.
    pub reserve_tokens: i64,
    /// `credits_micro(estimated_input_tokens, max_output_tokens_applied, ...)`.
    pub reserved_credits_micro: i64,
}

/// `min(catalog max_output_tokens, streaming cap)`.
#[must_use]
pub fn max_output_tokens_applied(entry: &ModelCatalogEntry, streaming_cap: u32) -> u32 {
    entry.max_output_tokens.min(streaming_cap)
}

/// The surcharge of the tools in the request (DESIGN section 5.5.6).
#[must_use]
pub fn tool_surcharge_tokens(budgets: &EstimationBudgets, tools: ToolSet) -> i64 {
    let mut total = 0_i64;
    if tools.file_search {
        total += i64::from(budgets.tool_surcharge_tokens);
    }
    if tools.web_search {
        total += i64::from(budgets.web_search_surcharge_tokens);
    }
    if tools.code_interpreter {
        total += i64::from(budgets.code_interpreter_surcharge_tokens);
    }
    total
}

/// The reserve of `entry` for a request offering `tools`.
///
/// # Errors
/// [`CreditsError`] when the credit computation is refused (zero multiplier, out-of-range tokens).
pub fn candidate_reserve(
    entry: &ModelCatalogEntry,
    inputs: &ReserveInputs,
    tools: ToolSet,
) -> Result<ReserveEstimate, CreditsError> {
    let budgets = &entry.estimation_budgets;
    let image_surcharge =
        i64::from(inputs.image_count).saturating_mul(i64::from(budgets.image_token_budget));
    let estimated_input_tokens = estimated_text_tokens(inputs.content_bytes, budgets)
        .saturating_add(inputs.prior_context_tokens)
        .saturating_add(image_surcharge)
        .saturating_add(tool_surcharge_tokens(budgets, tools));
    let max_output = max_output_tokens_applied(entry, inputs.streaming_max_output_tokens);
    let reserved_credits_micro = credits_micro(
        estimated_input_tokens,
        i64::from(max_output),
        i64::try_from(entry.input_tokens_credit_multiplier_micro).unwrap_or(i64::MAX),
        i64::try_from(entry.output_tokens_credit_multiplier_micro).unwrap_or(i64::MAX),
    )?;
    Ok(ReserveEstimate {
        estimated_input_tokens,
        max_output_tokens_applied: max_output,
        reserve_tokens: estimated_input_tokens.saturating_add(i64::from(max_output)),
        reserved_credits_micro,
    })
}

#[cfg(test)]
#[path = "estimation_tests.rs"]
mod tests;
