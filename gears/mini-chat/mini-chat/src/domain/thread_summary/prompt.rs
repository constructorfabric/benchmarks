//! The request of the summary model and its answer (DESIGN 3.6 "Execution
//! stages and invariants": request format, prompt fitting, response parsing;
//! B.5.5 for the texts).

use crate::domain::context::HistoryRole;

/// Opening of the request when the chat has no summary yet (B.5.5).
const FRESH_OPENING: &str = "Summarize the following conversation:";

/// Opening of the request when a summary exists (B.5.5).
const MERGE_OPENING: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";

/// Introduces the messages after the existing summary.
const NEW_MESSAGES: &str = "New messages to incorporate:";

/// Analysis instruction at the end of the request (B.5.5).
const ANALYSIS_INSTRUCTION: &str = "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:\n1. Chronologically review each exchange, identifying:\n   - The user's requests and questions\n   - Key decisions, answers, and information shared\n   - Any follow-up actions or commitments\n   - Specific names, dates, numbers, URLs, or references mentioned\n2. Verify accuracy and completeness.\n\nYour summary MUST include these sections:\n\n1. Conversation Purpose: The user's primary goals and recurring themes\n2. Key Information Exchanged: Important facts, decisions, recommendations, and answers\n3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections\n4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit\n5. Current Topic: What was being discussed most recently, with enough detail to continue naturally\n\nRespond with an <analysis> block followed by a <summary> block.";

/// Marks a cut message.
const ELLIPSIS: &str = "...";

/// One message of the summarized range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Entry {
    pub(super) role: HistoryRole,
    pub(super) content: String,
}

impl Entry {
    /// `User: <content>` / `Assistant: <content>`; content longer than `limit`
    /// characters (0 = unlimited) is cut to that length and ends with `...`.
    fn render(&self, limit: usize) -> String {
        let label = match self.role {
            HistoryRole::User => "User",
            HistoryRole::Assistant => "Assistant",
        };
        format!("{label}: {}", cut(&self.content, limit))
    }
}

fn cut(content: &str, limit: usize) -> String {
    if limit == 0 || content.chars().count() <= limit {
        return content.to_owned();
    }
    let kept: String = content
        .chars()
        .take(limit.saturating_sub(ELLIPSIS.len()))
        .collect();
    format!("{kept}{ELLIPSIS}")
}

/// The user prompt: the opening (with the existing summary, when there is one),
/// the entries one blank line apart, the analysis instruction.
pub(super) fn build_prompt(existing: Option<&str>, entries: &[Entry], limit: usize) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(entries.len() + 4);
    match existing {
        None => parts.push(FRESH_OPENING.to_owned()),
        Some(summary) => {
            parts.push(MERGE_OPENING.to_owned());
            parts.push(format!(
                "<existing_summary>\n{summary}\n</existing_summary>"
            ));
            parts.push(NEW_MESSAGES.to_owned());
        }
    }
    parts.extend(entries.iter().map(|entry| entry.render(limit)));
    parts.push(ANALYSIS_INSTRUCTION.to_owned());
    parts.join("\n\n")
}

/// How the prompt is fitted to the summary model.
#[derive(Debug, Clone, Copy)]
pub(super) struct FitBudget {
    /// Input budget in tokens.
    pub(super) tokens: u64,
    /// Bytes per token of the estimate.
    pub(super) bytes_per_token: u64,
}

/// Drops the oldest `ceil(n / 5)` entries per step while the prompt (system
/// prompt plus user prompt) is over the budget, keeping at least two.
pub(super) fn fit(
    mut entries: Vec<Entry>,
    existing: Option<&str>,
    system: &str,
    limit: usize,
    budget: FitBudget,
) -> Vec<Entry> {
    while entries.len() > 2 {
        let bytes = system.len() + build_prompt(existing, &entries, limit).len();
        let size = u64::try_from(bytes)
            .unwrap_or(u64::MAX)
            .div_ceil(budget.bytes_per_token.max(1));
        if size <= budget.tokens {
            break;
        }
        entries.drain(..oldest_fifth(entries.len()));
    }
    entries
}

/// How many of the oldest entries a step drops: `ceil(n / 5)`, never below two
/// entries left.
pub(super) fn oldest_fifth(len: usize) -> usize {
    len.div_ceil(5).min(len.saturating_sub(2))
}

/// The text to store from the answer of the model: the `<analysis>` blocks are
/// removed, the first closed `<summary>` block is kept (the whole remaining
/// text without one), runs of blank lines collapse to one. Empty when nothing
/// usable is left: blank, or the text still holds `<analysis` / `<summary`
/// markup (an unterminated block, a stray tag).
pub(super) fn parse_summary(answer: &str) -> String {
    let without_analysis = strip_blocks(answer, "analysis");
    let stored = closed_block(&without_analysis, "summary").unwrap_or(without_analysis.as_str());
    if stored.contains("<analysis") || stored.contains("<summary") {
        return String::new();
    }
    collapse_blank_lines(stored)
}

/// Removes every closed `<tag>...</tag>` block; an unclosed one stays.
fn strip_blocks(text: &str, tag: &str) -> String {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut rest = text;
    let mut kept = String::with_capacity(text.len());
    while let Some(start) = rest.find(&open) {
        let after_open = start + open.len();
        let Some(length) = rest[after_open..].find(&close) else {
            break;
        };
        kept.push_str(&rest[..start]);
        rest = &rest[after_open + length + close.len()..];
    }
    kept.push_str(rest);
    kept
}

/// The text inside the first closed `<tag>...</tag>` block.
fn closed_block<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let start = text.find(&open)? + open.len();
    let length = text[start..].find(&close)?;
    Some(&text[start..start + length])
}

/// Runs of blank lines become one blank line; the result is trimmed.
fn collapse_blank_lines(text: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        let blank = line.trim().is_empty();
        if blank && lines.last().is_some_and(|last| last.is_empty()) {
            continue;
        }
        lines.push(if blank { "" } else { line });
    }
    lines.join("\n").trim().to_owned()
}

#[cfg(test)]
#[path = "prompt_tests.rs"]
mod tests;
