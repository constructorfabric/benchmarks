//! Alias derivation and normalization.
//!
//! An upstream is identified by an *alias* used in proxy URLs. Aliases are derived from
//! the endpoint set when it is derivable, and must be supplied explicitly when it is
//! not. Rules follow the component contract:
//!
//! - ASCII-lowercase, trailing dot stripped.
//! - A single hostname endpoint derives the hostname itself, suffixed with `:port` when
//!   the port is non-standard for the endpoint's scheme.
//! - A multi-endpoint pool derives the longest common registrable suffix across the
//!   endpoints (again suffixed with `:port` when non-standard).
//! - A bare public suffix (`co.uk`) is not derivable.
//! - IP literals and non-derivable sets require an explicit alias.

use crate::error::{ErrorKind, OagwError};

/// Derive the alias for an endpoint set, returning `None` when no alias can be derived
/// and an explicit one must be supplied.
///
/// Each endpoint is a `(host, port, scheme)` triple.
#[must_use]
pub fn derive(endpoints: &[(String, u16, &str)]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }

    // A single endpoint derives its host (plus port when non-standard).
    if endpoints.len() == 1 {
        let (host, port, scheme) = &endpoints[0];
        if is_ip_literal(host) {
            return None;
        }
        return Some(with_port(host, *port, scheme));
    }

    // Multi-endpoint: the longest label-aligned common suffix, which must be a
    // registrable domain. `psl::domain_str` rejects both bare public suffixes
    // (`co.uk`) and single labels (`com`), so one check covers both rules.
    let mut suffix = normalize(&endpoints[0].0);
    for (host, _, _) in endpoints.iter().skip(1) {
        suffix = common_suffix(&suffix, &normalize(host))?;
    }

    psl::domain_str(&suffix)?;

    // All endpoints in a derivable pool share the port, so use the first one.
    let (_, port, scheme) = &endpoints[0];
    Some(with_port(&suffix, *port, scheme))
}

/// Returns `true` when the two hosts are the same name modulo case and a trailing dot.
#[must_use]
pub fn same_host(a: &str, b: &str) -> bool {
    normalize(a) == normalize(b)
}

/// Normalizes an alias or hostname: ASCII lowercase, trailing dot stripped.
#[must_use]
pub fn normalize(value: &str) -> String {
    value.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Returns `true` when `value` is an IPv4 or IPv6 literal.
#[must_use]
pub fn is_ip_literal(value: &str) -> bool {
    let candidate = value.trim_end_matches('.');
    if candidate.parse::<std::net::Ipv4Addr>().is_ok() {
        return true;
    }
    if candidate.parse::<std::net::Ipv6Addr>().is_ok() {
        return true;
    }
    // Bracketed IPv6 (`[::1]`) is also an IP literal.
    candidate.starts_with('[') && candidate.ends_with(']')
}

/// Appends `:port` when `port` is non-standard for `scheme`.
#[must_use]
pub fn with_port(host: &str, port: u16, scheme: &str) -> String {
    if is_standard_port(port, scheme) {
        normalize(host)
    } else {
        format!("{}:{}", normalize(host), port)
    }
}

/// Returns `true` when `port` is the scheme's default port.
#[must_use]
pub fn is_standard_port(port: u16, scheme: &str) -> bool {
    matches!(
        (scheme, port),
        ("http" | "ws", 80) | ("https" | "wss" | "wt" | "grpc", 443)
    )
}

/// Longest dot-separated label suffix shared by `a` and `b`.
#[must_use]
pub fn common_suffix(a: &str, b: &str) -> Option<String> {
    let a_labels: Vec<&str> = a.split('.').collect();
    let b_labels: Vec<&str> = b.split('.').collect();
    let mut shared: Vec<&str> = Vec::new();
    let mut i = a_labels.len();
    let mut j = b_labels.len();
    while i > 0 && j > 0 {
        i -= 1;
        j -= 1;
        if !a_labels[i].eq_ignore_ascii_case(b_labels[j]) {
            break;
        }
        shared.push(a_labels[i]);
    }
    if shared.is_empty() {
        None
    } else {
        shared.reverse();
        Some(shared.join("."))
    }
}

/// Validate a caller-supplied alias against the pattern `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
///
/// # Errors
///
/// Returns a validation error when the alias is empty, starts or ends with a separator,
/// or carries characters outside the allowed set.
pub fn validate_alias(alias: &str) -> Result<(), OagwError> {
    let valid = !alias.is_empty()
        && alias.len() <= 253
        && alias
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && alias
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && alias.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | ':' | '-')
        });
    if valid {
        Ok(())
    } else {
        Err(OagwError::new(
            ErrorKind::ValidationError,
            format!("alias `{alias}` does not match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$"),
        ))
    }
}

/// Validate a tag against the pattern `^[a-z0-9_-]+$`.
///
/// # Errors
///
/// Returns a validation error when the tag is empty or carries other characters.
pub fn validate_tag(tag: &str) -> Result<(), OagwError> {
    if !tag.is_empty()
        && tag.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        Ok(())
    } else {
        Err(OagwError::new(
            ErrorKind::ValidationError,
            format!("tag `{tag}` does not match ^[a-z0-9_-]+$"),
        ))
    }
}

/// Validate a hostname per RFC 1123 (labels of 1–63 alphanumeric-or-hyphen characters
/// not starting or ending with a hyphen, total length ≤ 253) or an IP literal.
///
/// # Errors
///
/// Returns a validation error when the host is malformed.
pub fn validate_host(host: &str) -> Result<(), OagwError> {
    if is_ip_literal(host) {
        return Ok(());
    }
    let candidate = normalize(host);
    if candidate.is_empty() || candidate.len() > 253 {
        return Err(OagwError::new(
            ErrorKind::ValidationError,
            "host must be between 1 and 253 characters",
        ));
    }
    let ok = candidate.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    });
    if ok {
        Ok(())
    } else {
        Err(OagwError::new(
            ErrorKind::ValidationError,
            format!("host `{host}` is not a valid RFC 1123 hostname or IP literal"),
        ))
    }
}
