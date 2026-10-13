//! Provider identifier non-exposure (S§9.4, D "Provider identifier
//! non-exposure invariant") and the provider `user` field.

use std::sync::LazyLock;

use regex::Regex;
use uuid::Uuid;

/// `(pattern, replacement)` applied in order: URLs first (they may contain
/// ids), then credentials, then provider ids.
static RULES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (r"https?://[^\s)\]}>,;]+", "[url]"),
        (r"\bBearer\s+[A-Za-z0-9\-._~+/]+=*", "[credential]"),
        (r"\bsk-(?:[A-Za-z0-9]+-)*[A-Za-z0-9]{10,}", "[credential]"),
        (
            r"\b(?:resp_|chatcmpl-|cmpl-|msg_)[A-Za-z0-9]+",
            "[provider_id]",
        ),
        (
            r"\b(?:file-|file_|assistant-|vs_)[A-Za-z0-9]{12,}",
            "[provider_id]",
        ),
    ]
    .into_iter()
    // Constant patterns, all exercised by the unit tests.
    .filter_map(|(p, r)| Regex::new(p).ok().map(|re| (re, r)))
    .collect()
});

/// Replace provider ids with `[provider_id]`, URLs with `[url]`, and `sk-`
/// keys / `Bearer` tokens with `[credential]`; the rest is left as is.
#[must_use]
pub fn sanitize_provider_message(msg: &str) -> String {
    let mut out = msg.to_owned();
    for (re, replacement) in RULES.iter() {
        if re.is_match(&out) {
            out = re.replace_all(&out, *replacement).into_owned();
        }
    }
    out
}

/// Provider `user` field: tenant and user UUIDs in simple form, tenant first
/// (64 lowercase hex characters).
#[must_use]
pub fn provider_user_field(tenant: Uuid, user: Uuid) -> String {
    format!("{}{}", tenant.simple(), user.simple())
}

#[cfg(test)]
#[path = "sanitize_tests.rs"]
mod tests;
