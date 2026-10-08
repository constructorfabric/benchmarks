//! Azure `OpenAI` Files / Vector Stores paths:
//! `/{alias}/openai/...?api-version=V`.

/// `/{alias}/openai{tail}?api-version={version}` (`tail` starts with `/`).
pub(super) fn uri(alias: &str, api_version: &str, tail: &str) -> String {
    format!("/{alias}/openai{tail}?api-version={api_version}")
}
