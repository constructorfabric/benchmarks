//! Summary request prompt building, fitting and response parsing (DESIGN §3.6, B.5.5).

use mini_chat_sdk::{ModelCatalogEntry, UsageTokens};

use crate::config::{DEFAULT_SUMMARY_SYSTEM_PROMPT, ThreadSummaryWorkerConfig};
use crate::domain::context::HistoryMessage;

/// Opening of the summary request when no summary exists yet.
pub const OPENING_NEW: &str = "Summarize the following conversation:";

/// Opening of the summary request when a summary already exists (followed by the
/// `<existing_summary>` block and [`NEW_MESSAGES_HEADER`]).
pub const OPENING_EXISTING: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";

/// Header of the new messages after the existing summary block.
pub const NEW_MESSAGES_HEADER: &str = "New messages to incorporate:";

/// Analysis instruction at the end of the summary request.
pub const ANALYSIS_INSTRUCTION: &str = "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:
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

/// System prompt: catalog `thread_summary_prompt`, else the configured prompt, else the built-in one.
#[must_use]
pub fn system_prompt(model: &ModelCatalogEntry, cfg: &ThreadSummaryWorkerConfig) -> String {
    if !model.thread_summary_prompt.trim().is_empty() {
        model.thread_summary_prompt.clone()
    } else if !cfg.summary_system_prompt.trim().is_empty() {
        cfg.summary_system_prompt.clone()
    } else {
        DEFAULT_SUMMARY_SYSTEM_PROMPT.to_owned()
    }
}

/// Cuts `content` to `limit` characters followed by `...` when it is longer (0 = no limit).
#[must_use]
pub fn truncate_content(content: &str, limit: usize) -> String {
    if limit == 0 || content.chars().count() <= limit {
        return content.to_owned();
    }
    let mut out: String = content.chars().take(limit).collect();
    out.push_str("...");
    out
}

/// Prompt entries (`User: ...` / `Assistant: ...`) of the non-system messages, in order.
#[must_use]
pub fn entries(messages: &[HistoryMessage], content_limit: usize) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| {
            let label = match m.role.as_str() {
                "user" => "User",
                "assistant" => "Assistant",
                _ => return None,
            };
            Some(format!("{label}: {}", truncate_content(&m.content, content_limit)))
        })
        .collect()
}

/// User prompt: opening (+ existing summary block), entries separated by blank lines, analysis instruction.
#[must_use]
pub fn user_prompt(existing_summary: Option<&str>, entries: &[String]) -> String {
    let mut out = String::new();
    match existing_summary {
        Some(s) => {
            out.push_str(OPENING_EXISTING);
            out.push_str("\n\n<existing_summary>\n");
            out.push_str(s);
            out.push_str("\n</existing_summary>\n\n");
            out.push_str(NEW_MESSAGES_HEADER);
        }
        None => out.push_str(OPENING_NEW),
    }
    out.push_str("\n\n");
    out.push_str(&entries.join("\n\n"));
    out.push_str("\n\n");
    out.push_str(ANALYSIS_INSTRUCTION);
    out
}

/// Input budget of the summary model: `context_window - max_output_tokens`, capped by
/// `max_input_tokens` when > 0. `None` when the catalog `context_window` is 0 (no fitting).
#[must_use]
pub fn input_budget(model: &ModelCatalogEntry) -> Option<i64> {
    if model.context_window == 0 {
        return None;
    }
    let b = i64::from(model.context_window) - i64::from(model.max_output_tokens);
    Some(if model.max_input_tokens > 0 { b.min(i64::from(model.max_input_tokens)) } else { b })
}

/// Prompt size estimate (`bytes / bytes_per_token_conservative`, rounded up).
#[must_use]
pub fn estimate_prompt(system: &str, user: &str, model: &ModelCatalogEntry) -> i64 {
    let bpt = u64::from(model.estimation_budgets.bytes_per_token_conservative.max(1));
    let bytes = u64::try_from(system.len().saturating_add(user.len())).unwrap_or(u64::MAX);
    i64::try_from(bytes.div_ceil(bpt)).unwrap_or(i64::MAX)
}

/// Number of oldest entries to drop in one step: `ceil(n/5)`, keeping at least `keep` entries.
#[must_use]
pub fn drop_step(n: usize, keep: usize) -> usize {
    n.div_ceil(5).min(n.saturating_sub(keep))
}

/// Drops the oldest entries (`ceil(n/5)` per step, keeping at least two) until the prompt fits the
/// summary model's input budget. Returns the number of dropped entries.
#[must_use]
pub fn fit_entries(
    entries: &mut Vec<String>,
    existing_summary: Option<&str>,
    system: &str,
    model: &ModelCatalogEntry,
) -> usize {
    let Some(budget) = input_budget(model) else { return 0 };
    let mut dropped = 0;
    loop {
        let prompt = user_prompt(existing_summary, entries);
        if estimate_prompt(system, &prompt, model) <= budget {
            break;
        }
        let step = drop_step(entries.len(), 2);
        if step == 0 {
            break;
        }
        entries.drain(..step);
        dropped += step;
    }
    dropped
}

/// Removes every complete `<analysis>...</analysis>` block.
fn remove_analysis(text: &str) -> String {
    const OPEN: &str = "<analysis>";
    const CLOSE: &str = "</analysis>";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        let Some(len) = rest[start..].find(CLOSE) else { break };
        out.push_str(&rest[..start]);
        rest = &rest[start + len + CLOSE.len()..];
    }
    out.push_str(rest);
    out
}

/// Text inside the first complete `<summary>...</summary>` block.
fn summary_block(text: &str) -> Option<&str> {
    const OPEN: &str = "<summary>";
    const CLOSE: &str = "</summary>";
    let start = text.find(OPEN)? + OPEN.len();
    let len = text[start..].find(CLOSE)?;
    Some(&text[start..start + len])
}

/// Collapses runs of blank lines into one blank line and trims the result.
fn collapse_blank_lines(text: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    let mut previous_blank = false;
    for line in text.lines() {
        let blank = line.trim().is_empty();
        if blank && previous_blank {
            continue;
        }
        lines.push(if blank { "" } else { line });
        previous_blank = blank;
    }
    lines.join("\n").trim().to_owned()
}

/// Extracts the summary from the model output: the `<analysis>` block is removed and the text of the
/// `<summary>` block is returned with runs of blank lines collapsed. Without a `<summary>` block the
/// whole remaining text is returned, unless it still contains `<analysis` / `<summary` markup (empty).
#[must_use]
pub fn parse_summary(output: &str) -> String {
    let without_analysis = remove_analysis(output);
    if let Some(inner) = summary_block(&without_analysis) {
        return collapse_blank_lines(inner);
    }
    if without_analysis.contains("<analysis") || without_analysis.contains("<summary") {
        return String::new();
    }
    collapse_blank_lines(&without_analysis)
}

/// Stored `token_estimate`: `output_tokens - reasoning_tokens` when positive, else `ceil(bytes / 4)`.
#[must_use]
pub fn token_estimate(usage: Option<&UsageTokens>, summary: &str) -> i64 {
    let from_usage = usage.map_or(0, |u| u.output_tokens - u.reasoning_tokens);
    if from_usage > 0 {
        from_usage
    } else {
        i64::try_from(summary.len().div_ceil(4)).unwrap_or(i64::MAX)
    }
}
