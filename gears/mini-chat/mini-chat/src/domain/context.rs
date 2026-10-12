//! Context plan assembly and truncation (DESIGN sections "Context Window
//! Budget", "Context Plan Assembly and Truncation", "Context Plan Truncation
//! Algorithm", "`ContextPlan` Determinism and Snapshot Boundary", 3.6 "Thread
//! Summary" trigger, 5.5.4 / 5.5.5 estimation, B.5.5 summary preamble).
//!
//! Pure functions only: the caller loads the system prompt, the thread summary
//! and the recent messages (already limited to `context.recent_messages_limit`
//! by the recent-messages query) and resolves the effective model; this module
//! decides what fits the input budget.
//!
//! # Budget
//!
//! ```text
//! effective_budget = min(max_input_tokens, context_window - max_output_tokens_applied)
//!                    # max_input_tokens = 0: no separate input limit
//! token_budget     = effective_budget
//!                    - tool / web_search / code_interpreter surcharges (tools sent only)
//!                    - fixed_overhead_tokens
//! ```
//!
//! `max_output_tokens_applied >= context_window`, or deductions that reach
//! `effective_budget`, fail with `CONTEXT_BUDGET_EXCEEDED`.
//!
//! # Per-item estimate
//!
//! Every assembled item (instructions, thread summary with its preamble, each
//! recent message, the current user message) is estimated with the 5.5.4
//! formula of the effective model's `estimation_budgets`, without the framing
//! overhead, which [`token_budget`] deducts once for the whole request:
//!
//! ```text
//! item_tokens = ceil(ceil(utf8_bytes / bytes_per_token_conservative) * (100 + safety_margin_pct) / 100)
//! ```
//!
//! (`bytes_per_token_conservative = 0` is clamped to 1; an empty item is 0.)
//! Each current image costs `image_token_budget` (5.5.5). No tokenizer is used.

use std::cmp::Ordering;

use mini_chat_sdk::EstimationBudgets;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::config::{ContextConfig, KnowledgeSearchConfig};
use crate::domain::error::{DomainError, OutOfRangeReason};
use crate::domain::quota::estimate_text_tokens;

/// Thread summary preamble (DESIGN B.5.5), prepended to the summary text.
pub const THREAD_SUMMARY_PREAMBLE: &str = "This conversation has earlier messages that have been summarized. The summary below covers the earlier portion of the conversation. Recent messages follow after.";

/// Separator between the system prompt and each tool guard, and between the
/// summary preamble and the summary text.
pub const SECTION_SEPARATOR: &str = "\n\n";

// ── Input types ──────────────────────────────────────────────────────────

/// Role of a history or planned message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContextRole {
    User,
    Assistant,
}

impl ContextRole {
    /// Provider/wire role value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// A stored message of the recent-messages window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryMessage {
    pub id: Uuid,
    pub role: ContextRole,
    pub content: String,
    pub created_at: OffsetDateTime,
}

/// The chat's current thread summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryForContext {
    /// Stored summary text (without the preamble).
    pub text: String,
    /// Stored token estimate of the summary; reported as
    /// [`ContextPlan::summary_applied`] when the summary is kept.
    pub token_estimate: u32,
}

/// Token limits of the effective model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelLimits {
    /// Catalog `context_window`.
    pub context_window: u32,
    /// Catalog `max_input_tokens`; `0` means no separate input limit.
    pub max_input_tokens: u32,
    /// `min(catalog max_output_tokens, streaming.max_output_tokens)`.
    pub max_output_tokens_applied: u32,
}

impl ModelLimits {
    /// `effective_budget = min(max_input_tokens, context_window -
    /// max_output_tokens_applied)` (`max_input_tokens = 0`: window only),
    /// without surcharges and `fixed_overhead_tokens` (DESIGN 3.6).
    ///
    /// `None` when `max_output_tokens_applied >= context_window`.
    #[must_use]
    pub fn effective_budget(self) -> Option<u64> {
        effective_budget(
            self.context_window,
            self.max_input_tokens,
            self.max_output_tokens_applied,
        )
    }
}

/// `effective_budget = min(max_input_tokens, context_window -
/// max_output_tokens_applied)`; `max_input_tokens = 0` means no separate input
/// limit. Surcharges and `fixed_overhead_tokens` are not subtracted; this is
/// the base of the proactive summary threshold (DESIGN 3.6).
///
/// `None` when `max_output_tokens_applied >= context_window` (no room for input).
#[must_use]
pub fn effective_budget(
    context_window: u32,
    max_input_tokens: u32,
    max_output_tokens_applied: u32,
) -> Option<u64> {
    if max_output_tokens_applied >= context_window {
        return None;
    }
    let window_limit = u64::from(context_window - max_output_tokens_applied);
    if max_input_tokens == 0 {
        Some(window_limit)
    } else {
        Some(window_limit.min(u64::from(max_input_tokens)))
    }
}

/// Surcharges deducted from the input budget; each is non-zero only when its
/// tool is sent in the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ToolSurcharges {
    /// `tool_surcharge_tokens` (`file_search` sent).
    pub file_search: u32,
    /// `web_search_surcharge_tokens` (`web_search` sent).
    pub web_search: u32,
    /// `code_interpreter_surcharge_tokens` (`code_interpreter` sent).
    pub code_interpreter: u32,
}

impl ToolSurcharges {
    /// Surcharges of the effective model's `budgets` for the tools sent.
    #[must_use]
    pub const fn for_tools(
        budgets: &EstimationBudgets,
        file_search: bool,
        web_search: bool,
        code_interpreter: bool,
    ) -> Self {
        Self {
            file_search: if file_search {
                budgets.tool_surcharge_tokens
            } else {
                0
            },
            web_search: if web_search {
                budgets.web_search_surcharge_tokens
            } else {
                0
            },
            code_interpreter: if code_interpreter {
                budgets.code_interpreter_surcharge_tokens
            } else {
                0
            },
        }
    }

    /// Sum of all surcharges.
    #[must_use]
    pub fn total(self) -> u64 {
        u64::from(self.file_search) + u64::from(self.web_search) + u64::from(self.code_interpreter)
    }
}

/// Everything context assembly needs for one turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextInput {
    /// Effective model's system prompt (may be empty).
    pub system_prompt: String,
    /// Tool guard texts for the tools that will be sent, in the order
    /// `file_search`, `web_search`, knowledge (see [`select_guards`]).
    pub guards: Vec<String>,
    /// Current thread summary, if any.
    pub summary: Option<SummaryForContext>,
    /// Recent messages, chronological, limited to `recent_messages_limit`.
    pub recent: Vec<HistoryMessage>,
    /// Current user message (never truncated, not part of the planned messages).
    pub user_message: String,
    /// Number of images attached to the current user message.
    pub image_count: u32,
    /// Effective model limits.
    pub limits: ModelLimits,
    /// Effective model's `estimation_budgets`.
    pub budgets: EstimationBudgets,
    /// Surcharges of the tools that will be sent.
    pub surcharges: ToolSurcharges,
}

// ── Output types ─────────────────────────────────────────────────────────

/// A message of the assembled context, before the current user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedMessage {
    pub role: ContextRole,
    pub content: String,
}

/// The assembled, budget-checked context of one turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPlan {
    /// System prompt followed by the tool guards, joined with a blank line.
    pub instructions: String,
    /// Thread summary (as a `user` message with the preamble) if kept, then
    /// the kept recent messages in chronological order. The current user
    /// message is not included: the caller appends it with its images.
    pub messages: Vec<PlannedMessage>,
    /// Stored token estimate of the summary when it was kept.
    pub summary_applied: Option<u32>,
    /// Estimate of the assembled request: instructions, kept summary, kept
    /// recent messages, the current user message and its images
    /// (`assembled_context_tokens` of DESIGN 3.6). Never above `token_budget`.
    pub assembled_tokens: u64,
    /// At least one recent message was dropped (urgent summary trigger).
    pub messages_truncated: bool,
    /// `min(max_input_tokens, context_window - max_output_tokens_applied)`,
    /// the base of the proactive summary threshold.
    pub effective_budget: u64,
    /// `effective_budget` minus surcharges and `fixed_overhead_tokens`; the
    /// budget the context was truncated to.
    pub token_budget: u64,
}

/// Thread summary trigger decision (DESIGN 3.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryTrigger {
    /// No summary work.
    None,
    /// First summary: the assembled context reached the threshold.
    Proactive,
    /// Re-summarize: context assembly dropped messages.
    Urgent,
}

// ── Estimation ───────────────────────────────────────────────────────────

/// Per-item token estimate (module docs): the 5.5.4 formula without
/// `fixed_overhead_tokens`, which the budget deducts once per request.
#[must_use]
pub fn estimate_item_tokens(text: &str, budgets: &EstimationBudgets) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let per_item = EstimationBudgets {
        fixed_overhead_tokens: 0,
        ..*budgets
    };
    estimate_text_tokens(byte_len(text), &per_item)
}

fn byte_len(text: &str) -> u64 {
    u64::try_from(text.len()).unwrap_or(u64::MAX)
}

fn image_tokens(image_count: u32, budgets: &EstimationBudgets) -> u64 {
    u64::from(image_count) * u64::from(budgets.image_token_budget)
}

const fn context_budget_exceeded() -> DomainError {
    DomainError::out_of_range(OutOfRangeReason::ContextBudgetExceeded)
}

// ── Checks ───────────────────────────────────────────────────────────────

/// `INPUT_TOO_LONG` check of the current message against the effective
/// model's `max_input_tokens` (`0` = no limit). The message is estimated with
/// the full 5.5.4 formula ([`estimate_text_tokens`]); an estimate equal to
/// the limit passes.
///
/// # Errors
/// [`DomainError::OutOfRange`] with [`OutOfRangeReason::InputTooLong`] when
/// the estimate exceeds a non-zero `max_input_tokens`.
pub fn check_input_length(
    user_message: &str,
    max_input_tokens: u32,
    budgets: &EstimationBudgets,
) -> Result<(), DomainError> {
    if max_input_tokens == 0 {
        return Ok(());
    }
    let estimate = estimate_text_tokens(byte_len(user_message), budgets);
    if estimate > u64::from(max_input_tokens) {
        return Err(DomainError::out_of_range(OutOfRangeReason::InputTooLong));
    }
    Ok(())
}

/// Truncation budget: [`effective_budget`] minus surcharges and
/// `fixed_overhead_tokens`. Always at least 1 on success.
///
/// # Errors
/// [`DomainError::OutOfRange`] with [`OutOfRangeReason::ContextBudgetExceeded`]
/// when `max_output_tokens_applied >= context_window` or the deductions reach
/// the input limit.
pub fn token_budget(
    limits: ModelLimits,
    surcharges: ToolSurcharges,
    budgets: &EstimationBudgets,
) -> Result<u64, DomainError> {
    let input_limit = limits
        .effective_budget()
        .ok_or_else(context_budget_exceeded)?;
    let deductions = surcharges.total() + u64::from(budgets.fixed_overhead_tokens);
    if deductions >= input_limit {
        return Err(context_budget_exceeded());
    }
    Ok(input_limit - deductions)
}

// ── Guards ───────────────────────────────────────────────────────────────

/// Tool guard texts for the tools that will be sent, in the order
/// `file_search` (`context.file_search_guard`), `web_search`
/// (`context.web_search_guard`), `search_knowledge` (`knowledge_search.guard`).
/// Blank guards are skipped.
#[must_use]
pub fn select_guards(
    send_file_search: bool,
    send_web_search: bool,
    send_knowledge: bool,
    context: &ContextConfig,
    knowledge: &KnowledgeSearchConfig,
) -> Vec<String> {
    [
        (send_file_search, &context.file_search_guard),
        (send_web_search, &context.web_search_guard),
        (send_knowledge, &knowledge.guard),
    ]
    .into_iter()
    .filter(|(send, guard)| *send && !guard.trim().is_empty())
    .map(|(_, guard)| guard.clone())
    .collect()
}

/// System prompt and guards joined with [`SECTION_SEPARATOR`]; empty parts
/// are skipped.
fn build_instructions(system_prompt: String, guards: Vec<String>) -> String {
    std::iter::once(system_prompt)
        .chain(guards)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(SECTION_SEPARATOR)
}

/// Content of the summary message: preamble, blank line, summary text.
#[must_use]
pub fn summary_message_content(summary_text: &str) -> String {
    format!("{THREAD_SUMMARY_PREAMBLE}{SECTION_SEPARATOR}{summary_text}")
}

// ── Assembly ─────────────────────────────────────────────────────────────

/// Chronological ordering key `(created_at ASC, id ASC)`.
fn chronological(a: &HistoryMessage, b: &HistoryMessage) -> Ordering {
    a.created_at
        .cmp(&b.created_at)
        .then_with(|| a.id.cmp(&b.id))
}

/// Keeps the newest messages that fit `remaining` (walking newest to oldest,
/// stopping at the first that does not fit), then drops leading assistant
/// messages. Returns the kept messages (chronological) and their tokens.
fn fit_recent(
    mut recent: Vec<HistoryMessage>,
    remaining: u64,
    budgets: &EstimationBudgets,
) -> (Vec<HistoryMessage>, u64) {
    recent.sort_by(chronological);
    let mut left = remaining;
    let mut first_kept = recent.len();
    for message in recent.iter().rev() {
        let cost = estimate_item_tokens(&message.content, budgets);
        if cost > left {
            break;
        }
        left -= cost;
        first_kept -= 1;
    }
    let mut kept = recent.split_off(first_kept);
    let leading_assistants = kept
        .iter()
        .take_while(|message| message.role == ContextRole::Assistant)
        .count();
    kept.drain(..leading_assistants);
    let used = kept
        .iter()
        .map(|message| estimate_item_tokens(&message.content, budgets))
        .sum();
    (kept, used)
}

/// Assembles the turn's [`ContextPlan`] (DESIGN "Context Plan Truncation
/// Algorithm"):
///
/// 1. budget per [`token_budget`];
/// 2. mandatory items: instructions (system prompt + guards), the current
///    user message and `image_token_budget` per image; above the budget:
///    `CONTEXT_BUDGET_EXCEEDED`;
/// 3. the thread summary (with the B.5.5 preamble, as one `user` message) is
///    kept if it fits the remaining budget, otherwise dropped entirely;
/// 4. recent messages, ordered by `(created_at, id)`, are walked newest to
///    oldest and kept while they fit; the first one that does not fit and all
///    older ones are dropped;
/// 5. leading assistant messages of the kept range are dropped, so an answer
///    is never sent without its question.
///
/// `messages_truncated` is set when any recent message was dropped in steps 4
/// or 5. Identical inputs give an identical plan.
///
/// # Errors
/// [`DomainError::OutOfRange`] with [`OutOfRangeReason::ContextBudgetExceeded`]
/// when the budget is empty (see [`token_budget`]) or the mandatory items do
/// not fit it.
pub fn assemble(input: ContextInput) -> Result<ContextPlan, DomainError> {
    let ContextInput {
        system_prompt,
        guards,
        summary,
        recent,
        user_message,
        image_count,
        limits,
        budgets,
        surcharges,
    } = input;

    let token_budget = token_budget(limits, surcharges, &budgets)?;
    let effective_budget = limits
        .effective_budget()
        .ok_or_else(context_budget_exceeded)?;

    let instructions = build_instructions(system_prompt, guards);
    let mandatory = estimate_item_tokens(&instructions, &budgets)
        .saturating_add(estimate_item_tokens(&user_message, &budgets))
        .saturating_add(image_tokens(image_count, &budgets));
    if mandatory > token_budget {
        return Err(context_budget_exceeded());
    }
    let mut remaining = token_budget - mandatory;

    let mut messages = Vec::with_capacity(recent.len() + 1);
    let mut summary_applied = None;
    if let Some(summary) = summary {
        let content = summary_message_content(&summary.text);
        let cost = estimate_item_tokens(&content, &budgets);
        if cost <= remaining {
            remaining -= cost;
            summary_applied = Some(summary.token_estimate);
            messages.push(PlannedMessage {
                role: ContextRole::User,
                content,
            });
        }
    }

    let recent_count = recent.len();
    let (kept, recent_tokens) = fit_recent(recent, remaining, &budgets);
    remaining -= recent_tokens;
    let messages_truncated = kept.len() < recent_count;
    messages.extend(kept.into_iter().map(|message| PlannedMessage {
        role: message.role,
        content: message.content,
    }));

    Ok(ContextPlan {
        instructions,
        messages,
        summary_applied,
        assembled_tokens: token_budget - remaining,
        messages_truncated,
        effective_budget,
        token_budget,
    })
}

// ── Summary trigger ──────────────────────────────────────────────────────

/// Thread summary trigger (DESIGN 3.6), evaluated on the assembled estimate:
///
/// - disabled worker: [`SummaryTrigger::None`];
/// - `messages_truncated`: [`SummaryTrigger::Urgent`] (also with a summary);
/// - no summary and `assembled_tokens >= compression_threshold_pct% of
///   effective_budget` (exact: `assembled * 100 >= budget * pct`):
///   [`SummaryTrigger::Proactive`];
/// - otherwise (in particular a summary exists and nothing was truncated):
///   [`SummaryTrigger::None`].
///
/// `effective_budget` is [`effective_budget`] (no surcharges, no overhead).
#[must_use]
pub fn summary_trigger(
    assembled_tokens: u64,
    effective_budget: u64,
    compression_threshold_pct: u32,
    summary_exists: bool,
    messages_truncated: bool,
    enabled: bool,
) -> SummaryTrigger {
    if !enabled {
        return SummaryTrigger::None;
    }
    if messages_truncated {
        return SummaryTrigger::Urgent;
    }
    if summary_exists {
        return SummaryTrigger::None;
    }
    let assembled_scaled = u128::from(assembled_tokens) * 100;
    let threshold_scaled = u128::from(effective_budget) * u128::from(compression_threshold_pct);
    if assembled_scaled >= threshold_scaled {
        SummaryTrigger::Proactive
    } else {
        SummaryTrigger::None
    }
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
