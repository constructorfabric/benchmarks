//! Parsing of the summary answer and the stored token estimate (DESIGN "Response parsing").

use mini_chat_sdk::TokenUsage;

const ANALYSIS_OPEN: &str = "<analysis>";
const ANALYSIS_CLOSE: &str = "</analysis>";
const SUMMARY_OPEN: &str = "<summary>";
const SUMMARY_CLOSE: &str = "</summary>";

/// Removes every complete `<analysis>...</analysis>` block.
fn strip_analysis(text: &str) -> String {
    let mut rest = text;
    let mut kept = String::with_capacity(text.len());
    while let Some(open) = rest.find(ANALYSIS_OPEN) {
        let after_open = &rest[open + ANALYSIS_OPEN.len()..];
        let Some(close) = after_open.find(ANALYSIS_CLOSE) else {
            break;
        };
        kept.push_str(&rest[..open]);
        rest = &after_open[close + ANALYSIS_CLOSE.len()..];
    }
    kept.push_str(rest);
    kept
}

/// The text inside the first complete `<summary>...</summary>` block.
fn summary_block(text: &str) -> Option<&str> {
    let open = text.find(SUMMARY_OPEN)?;
    let after_open = &text[open + SUMMARY_OPEN.len()..];
    let close = after_open.find(SUMMARY_CLOSE)?;
    Some(&after_open[..close])
}

/// Collapses every run of blank lines into one blank line and trims the text.
fn collapse_blank_lines(text: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    let mut previous_blank = false;
    for line in text.lines() {
        let blank = line.trim().is_empty();
        if blank && previous_blank {
            continue;
        }
        previous_blank = blank;
        lines.push(if blank { "" } else { line });
    }
    lines.join("\n").trim().to_owned()
}

/// The summary text of a provider answer: the `<analysis>` blocks are removed and the text inside
/// `<summary>` is kept; without a `<summary>` block the remaining text is kept unless it still
/// contains `<analysis` or `<summary` markup. Runs of blank lines are collapsed. `None` when
/// nothing usable is left.
#[must_use]
pub fn parse_summary(answer: &str) -> Option<String> {
    let without_analysis = strip_analysis(answer);
    let text = match summary_block(&without_analysis) {
        Some(inner) => inner,
        None if without_analysis.contains("<analysis") || without_analysis.contains("<summary") => {
            return None;
        }
        None => without_analysis.as_str(),
    };
    let collapsed = collapse_blank_lines(text);
    (!collapsed.is_empty()).then_some(collapsed)
}

/// The stored `token_estimate`: `output_tokens - reasoning_tokens` when positive, otherwise
/// `ceil(bytes / 4)` of the summary.
#[must_use]
pub fn token_estimate(summary: &str, usage: Option<&TokenUsage>) -> i32 {
    let reported = usage.map_or(0, |usage| {
        usage.output_tokens.saturating_sub(usage.reasoning_tokens)
    });
    let tokens = if reported > 0 {
        reported
    } else {
        i64::try_from(summary.len().div_ceil(4)).unwrap_or(i64::MAX)
    };
    i32::try_from(tokens).unwrap_or(i32::MAX)
}

#[cfg(test)]
#[path = "parse_tests.rs"]
mod tests;
