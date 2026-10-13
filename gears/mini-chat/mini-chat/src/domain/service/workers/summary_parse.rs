//! Parsing of the summary answer and the `token_estimate` rule
//! (`cpt-cf-mini-chat-algo-background-workers-call-summary-model`,
//! `inst-parse-summary`; `cpt-cf-mini-chat-algo-background-workers-handle-thread-summary`,
//! `inst-token-estimate`).

use mini_chat_sdk::UsageTokens;

/// The summary text of a model answer: every `<analysis>...</analysis>` block
/// removed; the inside of the `<summary>...</summary>` block when there is one;
/// else the remaining text unless it still contains `<analysis` or `<summary`
/// (then nothing). Runs of blank lines are collapsed and the result is
/// trimmed. `None` when the result is empty.
#[must_use]
pub fn parse_summary(answer: &str) -> Option<String> {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-call-summary-model:p1:inst-parse-summary
    let mut text = answer.to_owned();
    while let Some(start) = text.find("<analysis>") {
        let Some(length) = text[start..].find("</analysis>") else {
            break;
        };
        let end = start + length + "</analysis>".len();
        text.replace_range(start..end, "");
    }
    let kept = match text.find("<summary>") {
        Some(start) => {
            let inner = start + "<summary>".len();
            text[inner..]
                .find("</summary>")
                .map(|length| text[inner..inner + length].to_owned())
        }
        None => None,
    };
    let kept = kept
        .or_else(|| (!text.contains("<analysis") && !text.contains("<summary")).then_some(text))?;
    let collapsed = collapse_blank_lines(&kept);
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-call-summary-model:p1:inst-parse-summary
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-call-summary-model:p1:inst-if-summary-empty
    (!collapsed.is_empty()).then_some(collapsed)
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-call-summary-model:p1:inst-if-summary-empty
}

/// Runs of blank (empty or whitespace-only) lines become one blank line; the
/// result is trimmed.
fn collapse_blank_lines(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut previous_blank = false;
    for line in text.lines() {
        let blank = line.trim().is_empty();
        if blank && previous_blank {
            continue;
        }
        out.push(if blank { "" } else { line });
        previous_blank = blank;
    }
    out.join("\n").trim().to_owned()
}

/// `token_estimate` of a committed summary: `output_tokens` minus
/// `reasoning_tokens` when that difference is above 0, otherwise the UTF-8 byte
/// length of the text divided by 4, rounded up.
#[must_use]
pub fn token_estimate(usage: &UsageTokens, summary_text: &str) -> i32 {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-token-estimate
    let visible = usage.output_tokens.saturating_sub(usage.reasoning_tokens);
    let estimate = if visible > 0 {
        visible
    } else {
        (summary_text.len() as u64).div_ceil(4)
    };
    i32::try_from(estimate).unwrap_or(i32::MAX)
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-handle-thread-summary:p1:inst-token-estimate
}

#[cfg(test)]
#[path = "summary_parse_tests.rs"]
mod tests;
