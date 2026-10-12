//! Small validation helpers shared by the config sections.

use std::fmt::Display;

/// `value` must lie in `lo..=hi`.
pub(super) fn range<T: PartialOrd + Display + Copy>(
    name: &str,
    value: T,
    lo: T,
    hi: T,
) -> Result<(), String> {
    if value < lo || value > hi {
        return Err(format!("{name} must be in {lo}..={hi}, got {value}"));
    }
    Ok(())
}

/// `value` must be greater than zero.
pub(super) fn positive<T: PartialOrd + Default + Copy>(name: &str, value: T) -> Result<(), String> {
    if value <= T::default() {
        return Err(format!("{name} must be > 0"));
    }
    Ok(())
}

/// `value` must be non-blank.
pub(super) fn non_empty(name: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("{name} must be non-empty"));
    }
    Ok(())
}

/// Host / alias characters: the host becomes the OAGW alias in `/{alias}/...`,
/// so `/`, `?`, `#`, `@` or spaces would change the proxied path.
pub(super) fn host(name: &str, value: &str) -> Result<(), String> {
    non_empty(name, value)?;
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '[' | ']'))
    {
        return Err(format!(
            "{name} may only contain letters, digits and . - _ : [ ]"
        ));
    }
    Ok(())
}

/// REST route prefix: empty (routes at the root) or `/segment[/segment]*`
/// without a trailing `/`. Every segment must be non-empty, not `.`/`..`, and
/// made of `[A-Za-z0-9._~-]` only, because the prefix is spliced into route
/// templates (so `:`/`*`/`{`/`}`/`%`/`?`/`#`/whitespace are rejected).
pub(super) fn url_prefix(name: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Ok(());
    }
    if !value.starts_with('/') {
        return Err(format!("{name} must be empty or start with '/'"));
    }
    if value.ends_with('/') {
        return Err(format!("{name} must not end with '/'"));
    }
    for segment in value[1..].split('/') {
        let clean = !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '-'));
        if !clean {
            return Err(format!(
                "{name} segment {segment:?} is invalid: each '/'-separated segment must be \
                 non-empty, not '.' or '..', and contain only letters, digits and . _ ~ -"
            ));
        }
    }
    Ok(())
}
