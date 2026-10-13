//! Summary prompt construction and fitting
//! (`cpt-cf-mini-chat-algo-background-workers-build-summary-prompt`, DESIGN B.5.5).

use mini_chat_sdk::ModelCatalogEntry;
use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

/// The built-in default system prompt (DESIGN B.5.5).
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are a conversation summarizer. Given a conversation (and optionally an existing summary), produce a detailed structured summary. Respond with an <analysis> block (your reasoning) followed by a <summary> block (the final summary). Only the <summary> content will be stored. Do not invent information not present in the conversation.";

/// Opening of a first summary request.
pub const OPENING_FIRST: &str = "Summarize the following conversation:";

/// Opening of a merge request (followed by the `<existing_summary>` block and
/// `New messages to incorporate:`).
pub const OPENING_MERGE: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";

/// The analysis instruction at the end of the user prompt.
pub const ANALYSIS_INSTRUCTION: &str = "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:\n1. Chronologically review each exchange, identifying:\n   - The user's requests and questions\n   - Key decisions, answers, and information shared\n   - Any follow-up actions or commitments\n   - Specific names, dates, numbers, URLs, or references mentioned\n2. Verify accuracy and completeness.\n\nYour summary MUST include these sections:\n\n1. Conversation Purpose: The user's primary goals and recurring themes\n2. Key Information Exchanged: Important facts, decisions, recommendations, and answers\n3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections\n4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit\n5. Current Topic: What was being discussed most recently, with enough detail to continue naturally\n\nRespond with an <analysis> block followed by a <summary> block.";

/// A message of the summarized range.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeMessage {
    /// Message id.
    pub id: Uuid,
    /// `user`, `assistant` or another role (skipped in the prompt).
    pub role: String,
    /// Content.
    pub content: String,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

/// The built prompts and the messages they carry.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryPrompt {
    /// The system prompt.
    pub system: String,
    /// The user prompt.
    pub user: String,
    /// The kept messages (the commit still covers the whole frozen range).
    pub kept: Vec<RangeMessage>,
}

/// Step 1: the catalog `thread_summary_prompt` when it is not empty, otherwise
/// `thread_summary_worker.summary_system_prompt` when it is not empty,
/// otherwise the built-in default.
#[must_use]
pub fn select_system_prompt(catalog_prompt: &str, configured_prompt: &str) -> String {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-build-summary-prompt:p1:inst-select-system-prompt
    if !catalog_prompt.is_empty() {
        catalog_prompt.to_owned()
    } else if !configured_prompt.is_empty() {
        configured_prompt.to_owned()
    } else {
        DEFAULT_SYSTEM_PROMPT.to_owned()
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-build-summary-prompt:p1:inst-select-system-prompt
}

/// Cuts `content` to `limit` characters (Unicode scalar values) and ends it with
/// `...` when it is longer; `limit` 0 means no cut.
fn cut(content: &str, limit: usize) -> String {
    if limit > 0 && content.chars().count() > limit {
        let mut cut: String = content.chars().take(limit).collect();
        cut.push_str("...");
        cut
    } else {
        content.to_owned()
    }
}

/// Steps 2 to 4: the user prompt of `kept` (messages of a role other than
/// `user` and `assistant` are skipped).
#[must_use]
pub fn user_prompt(kept: &[RangeMessage], existing: Option<&str>, content_limit: usize) -> String {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-build-summary-prompt:p1:inst-prompt-opening
    let mut prompt = match existing {
        None => OPENING_FIRST.to_owned(),
        Some(summary) => format!(
            "{OPENING_MERGE}\n\n<existing_summary>\n{summary}\n</existing_summary>\n\nNew messages to incorporate:"
        ),
    };
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-build-summary-prompt:p1:inst-prompt-opening
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-build-summary-prompt:p1:inst-prompt-entries
    let entries: Vec<String> = kept
        .iter()
        .filter_map(|message| {
            let label = match message.role.as_str() {
                "user" => "User",
                "assistant" => "Assistant",
                _ => return None,
            };
            Some(format!("{label}: {}", cut(&message.content, content_limit)))
        })
        .collect();
    if !entries.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(&entries.join("\n\n"));
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-build-summary-prompt:p1:inst-prompt-entries
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-build-summary-prompt:p1:inst-prompt-analysis
    prompt.push_str("\n\n");
    prompt.push_str(ANALYSIS_INSTRUCTION);
    prompt
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-build-summary-prompt:p1:inst-prompt-analysis
}

/// Step 5: `context_window` minus `max_output_tokens`, lowered to
/// `max_input_tokens` when that is above 0 and smaller. `None` when
/// `context_window` is 0 (no fitting).
#[must_use]
pub fn input_budget(entry: &ModelCatalogEntry) -> Option<i64> {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-build-summary-prompt:p1:inst-summary-budget
    if entry.context_window == 0 {
        return None;
    }
    let mut budget = i64::from(entry.context_window) - i64::from(entry.max_output_tokens);
    if entry.max_input_tokens > 0 {
        budget = budget.min(i64::from(entry.max_input_tokens));
    }
    Some(budget)
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-build-summary-prompt:p1:inst-summary-budget
}

/// The estimate of the prompts: their UTF-8 bytes divided by
/// `bytes_per_token_conservative`, rounded up.
#[must_use]
pub fn estimate_prompt_tokens(entry: &ModelCatalogEntry, system: &str, user: &str) -> i64 {
    let ratio = u64::from(entry.estimation_budgets.bytes_per_token_conservative.max(1));
    let bytes = (system.len() + user.len()) as u64;
    i64::try_from(bytes.div_ceil(ratio)).unwrap_or(i64::MAX)
}

/// Drops the oldest `ceil(n / 5)` of `kept` (keeping at least 2); `false` when
/// at most 2 messages are kept.
pub fn drop_oldest_fifth(kept: &mut Vec<RangeMessage>) -> bool {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-call-summary-model:p1:inst-ptl-retry
    if kept.len() <= 2 {
        return false;
    }
    let drop = kept.len().div_ceil(5).min(kept.len() - 2);
    kept.drain(..drop);
    true
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-call-summary-model:p1:inst-ptl-retry
}

/// The prompts of `range`, fitted to the input budget of the summary model by
/// dropping the oldest messages (steps 5 to 7).
#[must_use]
pub fn build_prompt(
    entry: &ModelCatalogEntry,
    range: &[RangeMessage],
    existing: Option<&str>,
    system: &str,
    content_limit: usize,
) -> SummaryPrompt {
    let mut kept = range.to_vec();
    let budget = input_budget(entry);
    loop {
        let user = user_prompt(&kept, existing, content_limit);
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-build-summary-prompt:p1:inst-fit-prompt
        let over =
            budget.is_some_and(|budget| estimate_prompt_tokens(entry, system, &user) > budget);
        if !over || !drop_oldest_fifth(&mut kept) {
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-build-summary-prompt:p1:inst-return-prompt
            return SummaryPrompt {
                system: system.to_owned(),
                user,
                kept,
            };
            // @cpt-end:cpt-cf-mini-chat-algo-background-workers-build-summary-prompt:p1:inst-return-prompt
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-build-summary-prompt:p1:inst-fit-prompt
    }
}

/// Rebuilds the prompts of `kept` after a context-length retry dropped messages.
#[must_use]
pub fn rebuild_prompt(
    kept: Vec<RangeMessage>,
    existing: Option<&str>,
    system: &str,
    content_limit: usize,
) -> SummaryPrompt {
    SummaryPrompt {
        system: system.to_owned(),
        user: user_prompt(&kept, existing, content_limit),
        kept,
    }
}

#[cfg(test)]
#[path = "summary_prompt_tests.rs"]
mod tests;
