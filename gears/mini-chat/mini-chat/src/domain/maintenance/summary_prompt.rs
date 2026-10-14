//! The summary prompt (`cpt-cf-mini-chat-algo-background-maintenance-summary-prompt`): the system
//! prompt, the user prompt with the optional `<existing_summary>` block, the `User:` and
//! `Assistant:` entries cut at `message_content_limit`, the analysis instruction and the fitting
//! to the summary model's input budget. The texts are those of DESIGN B.5.5.

use mini_chat_sdk::ModelCatalogEntry;
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::maintenance::settings::SummaryWorkerSettings;

/// The built-in system prompt of the summary request (B.5.5).
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Given a conversation (and optionally an existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent information not present in the conversation.";

/// The opening of the user prompt when there is no summary yet.
pub const OPENING_NEW: &str = "Summarize the following conversation:";

/// The opening of the user prompt when a summary exists (the `<existing_summary>` block and
/// `New messages to incorporate:` follow).
pub const OPENING_MERGE: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";

/// The line that introduces the messages after an `<existing_summary>` block.
pub const NEW_MESSAGES_HEADER: &str = "New messages to incorporate:";

/// The analysis instruction that ends the user prompt.
pub const ANALYSIS_INSTRUCTION: &str = "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:\n1. Chronologically review each exchange, identifying:\n   - The user's requests and questions\n   - Key decisions, answers, and information shared\n   - Any follow-up actions or commitments\n   - Specific names, dates, numbers, URLs, or references mentioned\n2. Verify accuracy and completeness.\n\nYour summary MUST include these sections:\n\n1. Conversation Purpose: The user's primary goals and recurring themes\n2. Key Information Exchanged: Important facts, decisions, recommendations, and answers\n3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections\n4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit\n5. Current Topic: What was being discussed most recently, with enough detail to continue naturally\n\nRespond with an <analysis> block followed by a <summary> block.";

/// One message of the frozen range.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeMessage {
    pub id: Uuid,
    /// `user`, `assistant` or `system`.
    pub role: String,
    pub content: String,
    pub created_at: OffsetDateTime,
}

/// The prompts of one summary request and the messages they carry.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryPrompt {
    pub system: String,
    pub user: String,
    /// The messages the user prompt was built from, in range order (system messages included:
    /// they are skipped by the user prompt, not by the range).
    pub kept: Vec<RangeMessage>,
}

/// The system prompt: the catalog `thread_summary_prompt` when it is not empty, else
/// `thread_summary_worker.summary_system_prompt` when it is not empty, else the built-in default.
#[must_use]
pub fn system_prompt(model: &ModelCatalogEntry, settings: &SummaryWorkerSettings) -> String {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-system
    if !model.thread_summary_prompt.is_empty() {
        model.thread_summary_prompt.clone()
    } else if !settings.summary_system_prompt.is_empty() {
        settings.summary_system_prompt.clone()
    } else {
        DEFAULT_SYSTEM_PROMPT.to_owned()
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-system
}

/// The user prompt of `kept` (the oldest first): the opening, the `<existing_summary>` block of
/// `base_summary` when there is one, one entry per non-system message and the analysis
/// instruction.
#[must_use]
pub fn render_user_prompt(
    base_summary: Option<&str>,
    kept: &[RangeMessage],
    content_limit: usize,
) -> String {
    let mut sections = Vec::new();
    sections.push(match base_summary {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-no-summary
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-open-new
        None => OPENING_NEW.to_owned(),
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-open-new
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-no-summary
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-has-summary
        // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-open-merge
        Some(summary) => format!(
            "{OPENING_MERGE}\n\n<existing_summary>\n{summary}\n</existing_summary>\n\n{NEW_MESSAGES_HEADER}"
        ),
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-open-merge
        // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-has-summary
    });
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-each
    let entries: Vec<String> = kept
        .iter()
        .filter_map(|message| {
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-entry
            let label = match message.role.as_str() {
                "user" => "User",
                "assistant" => "Assistant",
                _ => return None,
            };
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-entry
            // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-truncate
            let content = if content_limit > 0 && message.content.chars().count() > content_limit {
                let cut: String = message.content.chars().take(content_limit).collect();
                format!("{cut}...")
            } else {
                message.content.clone()
            };
            // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-truncate
            Some(format!("{label}: {content}"))
        })
        .collect();
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-each
    if !entries.is_empty() {
        // the entries are separated by a blank line
        sections.push(entries.join("\n\n"));
    }
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-analysis
    sections.push(ANALYSIS_INSTRUCTION.to_owned());
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-analysis
    sections.join("\n\n")
}

/// The input budget in tokens: `context_window - max_output_tokens`, capped by `max_input_tokens`
/// when that is greater than 0; `None` when `context_window` is 0 (no fitting).
#[must_use]
pub fn input_budget(model: &ModelCatalogEntry) -> Option<u64> {
    if model.context_window == 0 {
        return None;
    }
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-budget
    let budget = u64::from(model.context_window.saturating_sub(model.max_output_tokens));
    Some(if model.max_input_tokens > 0 {
        budget.min(u64::from(model.max_input_tokens))
    } else {
        budget
    })
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-budget
}

/// The estimated size of a prompt: UTF-8 bytes of both prompts divided by
/// `bytes_per_token_conservative` (at least 1), rounded up.
#[must_use]
pub fn estimate_tokens(system: &str, user: &str, bytes_per_token: u32) -> u64 {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-estimate
    let bytes = (system.len() + user.len()) as u64;
    bytes.div_ceil(u64::from(bytes_per_token.max(1)))
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-estimate
}

/// `kept` without its oldest `ceil(n / 5)` messages, never keeping fewer than two (a slice of two
/// or fewer messages is returned unchanged).
#[must_use]
pub fn drop_oldest_fifth(kept: &[RangeMessage]) -> &[RangeMessage] {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-fit-drop
    let count = kept.len();
    if count <= 2 {
        return kept;
    }
    let dropped = count.div_ceil(5).min(count - 2);
    &kept[dropped..]
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-fit-drop
}

/// Build and fit the prompt of the range `messages` over the optional `base_summary`: while the
/// estimate exceeds the input budget and more than two messages are kept, drop the oldest fifth.
#[must_use]
pub fn build_prompt(
    model: &ModelCatalogEntry,
    settings: &SummaryWorkerSettings,
    base_summary: Option<&str>,
    messages: &[RangeMessage],
) -> SummaryPrompt {
    let system = system_prompt(model, settings);
    let limit = settings.message_content_limit;
    let mut kept = messages;
    let mut user = render_user_prompt(base_summary, kept, limit);
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-no-fit
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-return-no-fit
    let Some(budget) = input_budget(model) else {
        // no context window is known: the prompts and all messages are returned without fitting
        return SummaryPrompt {
            system,
            user,
            kept: kept.to_vec(),
        };
    };
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-return-no-fit
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-no-fit
    let bytes_per_token = model.estimation_budgets.bytes_per_token_conservative;
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-fit-loop
    while estimate_tokens(&system, &user, bytes_per_token) > budget && kept.len() > 2 {
        kept = drop_oldest_fifth(kept);
        user = render_user_prompt(base_summary, kept, limit);
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-fit-loop
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-range-unchanged
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-return
    // the whole frozen range stays the range to mark compressed even when fitting dropped its
    // oldest messages; the dropped messages leave the context
    SummaryPrompt {
        system,
        user,
        kept: kept.to_vec(),
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-return
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-prompt:p1:inst-pr-range-unchanged
}

#[cfg(test)]
#[path = "summary_prompt_tests.rs"]
mod summary_prompt_tests;
