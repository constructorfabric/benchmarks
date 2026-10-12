//! The summary request: user prompt format and fitting to the summary model's input budget
//! (DESIGN "Request format", Appendix B.5.5).

use std::borrow::Cow;

use super::model::RangeMessage;
use crate::domain::llm::InputRole;

/// Opening of a request without an existing summary.
const OPENING_NEW: &str = "Summarize the following conversation:";

/// Opening of a request that updates an existing summary.
const OPENING_UPDATE: &str = "The existing summary below covers the earlier conversation. \
Incorporate it with the new messages into a single updated summary.\n\n\
IMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: \
current topic and recent decisions > user preferences and corrections > older facts. Compress or \
drop the least relevant older details rather than letting the summary grow unboundedly.";

/// Marker in front of the messages of an update request.
const NEW_MESSAGES_MARKER: &str = "New messages to incorporate:";

/// The analysis instruction that ends every summary request.
const ANALYSIS_INSTRUCTION: &str = "Before providing your final summary, wrap your analysis in \
<analysis> tags. In your analysis:
1. Chronologically review each exchange, identifying:
   - The user's requests and questions
   - Key decisions, answers, and information shared
   - Any follow-up actions or commitments
   - Specific names, dates, numbers, URLs, or references mentioned
2. Verify accuracy and completeness.

Your summary MUST include these sections:

1. Conversation Purpose: The user's primary goals and recurring themes
2. Key Information Exchanged: Important facts, decisions, recommendations, and answers
3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections
4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit
5. Current Topic: What was being discussed most recently, with enough detail to continue naturally

Respond with an <analysis> block followed by a <summary> block.";

/// Marker appended to a message that was cut to the content limit.
const ELLIPSIS: &str = "...";

/// The fewest messages a prompt keeps when it is fitted or retried.
const MIN_KEPT_MESSAGES: usize = 2;

/// The content of a message cut to `limit` characters (`0`: no limit); a cut message ends with
/// `...`.
fn limited(content: &str, limit: usize) -> Cow<'_, str> {
    if limit == 0 || content.chars().count() <= limit {
        return Cow::Borrowed(content);
    }
    let mut cut: String = content.chars().take(limit).collect();
    cut.push_str(ELLIPSIS);
    Cow::Owned(cut)
}

fn entry(message: &RangeMessage, limit: usize) -> String {
    let label = match message.role {
        InputRole::User => "User",
        InputRole::Assistant => "Assistant",
    };
    format!("{label}: {}", limited(&message.content, limit))
}

/// The user prompt: the opening (with the existing summary of an update), one entry per message
/// separated by a blank line, then the analysis instruction.
#[must_use]
pub fn user_prompt(
    existing_summary: Option<&str>,
    messages: &[RangeMessage],
    content_limit: usize,
) -> String {
    let entries = messages
        .iter()
        .map(|message| entry(message, content_limit))
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut prompt = String::new();
    match existing_summary {
        None => {
            prompt.push_str(OPENING_NEW);
            prompt.push_str("\n\n");
        }
        Some(summary) => {
            prompt.push_str(OPENING_UPDATE);
            prompt.push_str("\n\n<existing_summary>\n");
            prompt.push_str(summary);
            prompt.push_str("\n</existing_summary>\n\n");
            prompt.push_str(NEW_MESSAGES_MARKER);
            prompt.push_str("\n\n");
        }
    }
    prompt.push_str(&entries);
    prompt.push_str("\n\n");
    prompt.push_str(ANALYSIS_INSTRUCTION);
    prompt
}

/// How many of the oldest messages one step drops: `ceil(n / 5)`, never below two kept.
#[must_use]
pub fn drop_step(count: usize) -> usize {
    if count <= MIN_KEPT_MESSAGES {
        0
    } else {
        count.div_ceil(5).min(count - MIN_KEPT_MESSAGES)
    }
}

/// The input budget of the summary model in tokens: `context_window - max_output_tokens`, capped
/// by `max_input_tokens` when it is positive; `None` (no fitting) when the window is unknown (`0`).
#[must_use]
pub fn input_budget(
    context_window: u32,
    max_output_tokens: u32,
    max_input_tokens: u32,
) -> Option<i64> {
    if context_window == 0 {
        return None;
    }
    let room = i64::from(context_window) - i64::from(max_output_tokens);
    Some(if max_input_tokens > 0 {
        room.min(i64::from(max_input_tokens))
    } else {
        room
    })
}

/// How many of the oldest messages to drop so that the prompt (system prompt plus user prompt,
/// `bytes_per_token` bytes per token) fits `budget`: whole [`drop_step`]s until it fits or only two
/// messages remain.
#[must_use]
#[allow(
    clippy::integer_division,
    reason = "the prompt size is bytes / bytes_per_token_conservative (DESIGN 3.6)"
)]
pub fn fit(
    system_prompt: &str,
    existing_summary: Option<&str>,
    messages: &[RangeMessage],
    content_limit: usize,
    budget: Option<i64>,
    bytes_per_token: u32,
) -> usize {
    let Some(budget) = budget else {
        return 0;
    };
    let per_token = usize::try_from(bytes_per_token.max(1)).unwrap_or(1);
    let mut dropped = 0;
    loop {
        let remaining = &messages[dropped..];
        let bytes =
            system_prompt.len() + user_prompt(existing_summary, remaining, content_limit).len();
        let tokens = i64::try_from(bytes / per_token).unwrap_or(i64::MAX);
        let step = drop_step(remaining.len());
        if tokens <= budget || step == 0 {
            return dropped;
        }
        dropped += step;
    }
}

#[cfg(test)]
#[path = "prompt_tests.rs"]
mod tests;
