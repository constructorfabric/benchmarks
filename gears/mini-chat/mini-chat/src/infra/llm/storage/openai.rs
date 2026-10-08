//! `OpenAI` Files / Vector Stores paths: `/{alias}/v1/...`.

/// `/{alias}/v1{tail}` (`tail` starts with `/`).
pub(super) fn uri(alias: &str, tail: &str) -> String {
    format!("/{alias}/v1{tail}")
}
