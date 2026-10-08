//! Thread summary prompt construction and response parsing (DESIGN
//! "Thread Summary Update", B.5.5).

/// Opening of a first summary request.
pub const OPENING_FIRST: &str = "Summarize the following conversation:";
/// Opening when a summary already exists.
pub const OPENING_MERGE: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";
/// Analysis instruction closing the request.
pub const ANALYSIS_INSTRUCTION: &str = "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:\n1. Chronologically review each exchange, identifying:\n   - The user's requests and questions\n   - Key decisions, answers, and information shared\n   - Any follow-up actions or commitments\n   - Specific names, dates, numbers, URLs, or references mentioned\n2. Verify accuracy and completeness.\n\nYour summary MUST include these sections:\n\n1. Conversation Purpose: The user's primary goals and recurring themes\n2. Key Information Exchanged: Important facts, decisions, recommendations, and answers\n3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections\n4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit\n5. Current Topic: What was being discussed most recently, with enough detail to continue naturally\n\nRespond with an <analysis> block followed by a <summary> block.";

/// One message of the summarized range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeMessage {
    pub role: String,
    pub content: String,
}

fn cut(content: &str, limit: usize) -> String {
    if limit == 0 || content.chars().count() <= limit {
        return content.to_owned();
    }
    let mut s: String = content.chars().take(limit).collect();
    s.push_str("...");
    s
}

/// Build the user prompt of the summary request.
#[must_use]
pub fn build_user_prompt(existing: Option<&str>, messages: &[RangeMessage], content_limit: usize) -> String {
    let mut out = String::new();
    if let Some(s) = existing {
        out.push_str(OPENING_MERGE);
        out.push_str("\n\n<existing_summary>\n");
        out.push_str(s);
        out.push_str("\n</existing_summary>\n\nNew messages to incorporate:\n\n");
    } else {
        out.push_str(OPENING_FIRST);
        out.push_str("\n\n");
    }
    let entries: Vec<String> = messages
        .iter()
        .filter(|m| m.role != "system")
        .map(|m| {
            let who = if m.role == "assistant" { "Assistant" } else { "User" };
            format!("{who}: {}", cut(&m.content, content_limit))
        })
        .collect();
    out.push_str(&entries.join("\n\n"));
    out.push_str("\n\n");
    out.push_str(ANALYSIS_INSTRUCTION);
    out
}

fn collapse_blank_lines(s: &str) -> String {
    let mut out = Vec::new();
    let mut blank = false;
    for line in s.lines() {
        let is_blank = line.trim().is_empty();
        if is_blank && blank {
            continue;
        }
        blank = is_blank;
        out.push(line);
    }
    out.join("\n").trim().to_owned()
}

/// Extract the stored summary from the model response.
#[must_use]
pub fn parse_response(text: &str) -> String {
    let mut s = text.to_owned();
    while let Some(start) = s.find("<analysis>") {
        if let Some(end) = s[start..].find("</analysis>") {
            s.replace_range(start..start + end + "</analysis>".len(), "");
        } else {
            s.truncate(start);
            break;
        }
    }
    if let Some(start) = s.find("<summary>") {
        let body = &s[start + "<summary>".len()..];
        let body = body.find("</summary>").map_or(body, |end| &body[..end]);
        return collapse_blank_lines(body);
    }
    if s.contains("<analysis") || s.contains("<summary") {
        return String::new();
    }
    collapse_blank_lines(&s)
}

/// Fit the range into `budget_tokens` (bytes / `bpt`): drop the oldest
/// `ceil(n/5)` messages per step, keeping at least two.
#[must_use]
pub fn fit_messages(
    system_prompt: &str,
    existing: Option<&str>,
    mut messages: Vec<RangeMessage>,
    content_limit: usize,
    budget_tokens: Option<u64>,
    bpt: u32,
) -> Vec<RangeMessage> {
    let Some(budget) = budget_tokens else {
        return messages;
    };
    let bpt = u64::from(bpt.max(1));
    loop {
        let size = (system_prompt.len() + build_user_prompt(existing, &messages, content_limit).len()) as u64;
        if size.div_ceil(bpt) <= budget || messages.len() <= 2 {
            return messages;
        }
        let drop = messages.len().div_ceil(5).min(messages.len() - 2);
        messages.drain(0..drop);
    }
}

/// Drop ~20% of the oldest messages (context-length retry), keeping two.
#[must_use]
pub fn drop_fifth(mut messages: Vec<RangeMessage>) -> Vec<RangeMessage> {
    if messages.len() > 2 {
        let drop = messages.len().div_ceil(5).min(messages.len() - 2);
        messages.drain(0..drop);
    }
    messages
}

/// Stored token estimate of a summary.
#[must_use]
pub fn token_estimate(output_tokens: i64, reasoning_tokens: i64, summary: &str) -> i32 {
    let d = output_tokens - reasoning_tokens;
    let v = if d > 0 { d } else { i64::try_from(summary.len().div_ceil(4)).unwrap_or(i64::MAX) };
    i32::try_from(v).unwrap_or(i32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(role: &str, c: &str) -> RangeMessage {
        RangeMessage { role: role.into(), content: c.into() }
    }

    #[test]
    fn prompt_shapes() {
        let p = build_user_prompt(None, &[m("user", "hi"), m("assistant", "hello"), m("system", "x")], 0);
        assert!(p.starts_with(OPENING_FIRST));
        assert!(p.contains("User: hi\n\nAssistant: hello"));
        assert!(!p.contains("x\n"));
        assert!(p.ends_with("Respond with an <analysis> block followed by a <summary> block."));
        let p = build_user_prompt(Some("old"), &[m("user", "abcdef")], 3);
        assert!(p.contains("<existing_summary>\nold\n</existing_summary>"));
        assert!(p.contains("New messages to incorporate:"));
        assert!(p.contains("User: abc..."));
    }

    #[test]
    fn response_parsing() {
        assert_eq!(parse_response("<analysis>think</analysis>\n<summary>\nA\n\n\n\nB\n</summary>"), "A\n\nB");
        assert_eq!(parse_response("plain text"), "plain text");
        assert_eq!(parse_response("<analysis>unterminated"), "");
        assert_eq!(parse_response("<summary bad"), "");
        assert_eq!(parse_response("<analysis>x</analysis>"), "");
    }

    #[test]
    fn fitting_keeps_two() {
        let msgs: Vec<_> = (0..10).map(|i| m("user", &"x".repeat(100 + i))).collect();
        let out = fit_messages("sys", None, msgs, 0, Some(10), 4);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].content.len(), 109);
    }

    #[test]
    fn estimates() {
        assert_eq!(token_estimate(100, 20, "x"), 80);
        assert_eq!(token_estimate(0, 0, "abcdefgh1"), 3);
    }
}
