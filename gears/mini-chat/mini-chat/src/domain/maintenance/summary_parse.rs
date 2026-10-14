//! The summary response parsing
//! (`cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing`): the `<analysis>`
//! block removed, the `<summary>` content taken, blank-line runs collapsed and the stored
//! `token_estimate`.

use mini_chat_sdk::UsageTokens;
use toolkit_macros::domain_model;

/// The summary text to store and its estimate.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSummary {
    pub text: String,
    pub token_estimate: i32,
}

/// Parse the response text of a successful call; `None` when the result is empty.
#[must_use]
pub fn parse_summary_text(text: &str, usage: &UsageTokens) -> Option<ParsedSummary> {
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-strip-analysis
    let rest = remove_blocks(text, "<analysis>", "</analysis>");
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-strip-analysis
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-summary-block
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-take-summary
    let taken = if let Some(inner) = block(&rest, "<summary>", "</summary>") {
        inner.to_owned()
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-take-summary
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-summary-block
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-leftover-markup
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-empty-markup
    } else if rest.contains("<analysis") || rest.contains("<summary") {
        return None;
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-empty-markup
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-leftover-markup
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-take-rest
    } else {
        rest
    };
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-take-rest
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-collapse
    let mut lines: Vec<&str> = Vec::new();
    for line in taken.lines() {
        let blank = line.trim().is_empty();
        if blank && lines.last().is_some_and(|last| last.is_empty()) {
            continue;
        }
        lines.push(if blank { "" } else { line });
    }
    let summary = lines.join("\n").trim().to_owned();
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-collapse
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-empty
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-return-empty
    if summary.is_empty() {
        return None;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-return-empty
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-empty
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-token-estimate
    let token_estimate = if usage.output_tokens > usage.reasoning_tokens {
        usage.output_tokens - usage.reasoning_tokens
    } else {
        (summary.len() as u64).div_ceil(4)
    };
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-token-estimate
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-no-quality-gate
    // @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-return
    // no length or entropy validation: the quality gate is P2+
    Some(ParsedSummary {
        text: summary,
        token_estimate: i32::try_from(token_estimate).unwrap_or(i32::MAX),
    })
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-return
    // @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-summary-response-parsing:p1:inst-rp-no-quality-gate
}

/// The text with every complete `open ... close` block removed.
fn remove_blocks(text: &str, open: &str, close: &str) -> String {
    let mut rest = text;
    let mut out = String::new();
    while let Some(start) = rest.find(open) {
        let Some(end) = rest[start + open.len()..].find(close) else {
            break;
        };
        out.push_str(&rest[..start]);
        rest = &rest[start + open.len() + end + close.len()..];
    }
    out.push_str(rest);
    out
}

/// The text inside the first complete `open ... close` block.
fn block<'a>(text: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = text.find(open)? + open.len();
    let end = text[start..].find(close)?;
    Some(&text[start..start + end])
}

#[cfg(test)]
#[path = "summary_parse_tests.rs"]
mod summary_parse_tests;
