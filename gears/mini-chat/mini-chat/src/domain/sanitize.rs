//! Provider error message sanitization (DESIGN §3.3 "Provider identifier
//! non-exposure invariant").

use std::sync::LazyLock;

use regex::Regex;

struct Rules {
    url: Regex,
    bearer: Regex,
    sk: Regex,
    short_ids: Regex,
    long_ids: Regex,
}

static RULES: LazyLock<Option<Rules>> = LazyLock::new(|| {
    Some(Rules {
        url: Regex::new(r#"(?i)\b(?:https?|wss?)://[^\s"'<>]+"#).ok()?,
        bearer: Regex::new(r"(?i)\bBearer\s+[A-Za-z0-9._~+/=\-]+").ok()?,
        sk: Regex::new(r"\bsk-[A-Za-z0-9_\-]*(?:[A-Za-z0-9][A-Za-z0-9_\-]*){10,}").ok()?,
        short_ids: Regex::new(r"\b(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+").ok()?,
        long_ids: Regex::new(r"\b(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,}").ok()?,
    })
});

/// Replace provider ids with `[provider_id]`, URLs with `[url]` and API keys /
/// bearer tokens with `[credential]`; everything else is kept.
#[must_use]
pub fn sanitize_provider_message(input: &str) -> String {
    let Some(r) = RULES.as_ref() else {
        return "Provider error".to_owned();
    };
    let s = r.url.replace_all(input, "[url]");
    let s = r.bearer.replace_all(&s, "[credential]");
    let s = r.sk.replace_all(&s, "[credential]");
    let s = r.short_ids.replace_all(&s, "[provider_id]");
    let s = r.long_ids.replace_all(&s, "[provider_id]");
    s.into_owned()
}

#[cfg(test)]
#[path = "sanitize_tests.rs"]
mod tests;
