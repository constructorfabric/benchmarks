//! Alias derivation, normalization and update enforcement.
//!
//! An alias is the routing key in `/oagw/v1/proxy/{alias}/...`, not a free
//! label: hostname-based pools always auto-derive it, IP-based and otherwise
//! non-derivable pools must state it explicitly, and once set it never changes
//! (`cpt-cf-oagw-fr-alias-resolution`).

use std::net::IpAddr;

use crate::domain::error::{OagwError, OagwResult};
use crate::domain::model::Endpoint;

/// Longest hostname accepted, per RFC 1123.
const MAX_HOSTNAME_LEN: usize = 253;
/// Longest single label, per RFC 1123.
const MAX_LABEL_LEN: usize = 63;

/// Normalize an alias or hostname: ASCII lowercase, trailing dots stripped.
#[must_use]
pub fn normalize(raw: &str) -> String {
    raw.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Whether `host` parses as an IP literal (v4, or v6 with or without brackets).
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    let bare = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    bare.parse::<IpAddr>().is_ok()
}

/// Validate a hostname per RFC 1123. IP literals are accepted as-is.
///
/// # Errors
///
/// Returns a `ValidationError` describing the first rule violated.
pub fn validate_host(raw: &str) -> OagwResult<String> {
    let host = normalize(raw);
    if host.is_empty() {
        return Err(OagwError::validation("endpoint host must not be empty"));
    }
    if is_ip_literal(&host) {
        return Ok(host);
    }
    if host.len() > MAX_HOSTNAME_LEN {
        return Err(OagwError::validation(format!(
            "endpoint host exceeds {MAX_HOSTNAME_LEN} characters: {host}"
        )));
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > MAX_LABEL_LEN {
            return Err(OagwError::validation(format!(
                "endpoint host label must be 1-{MAX_LABEL_LEN} characters: {host}"
            )));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(OagwError::validation(format!(
                "endpoint host label may only contain ASCII letters, digits and hyphens: {host}"
            )));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(OagwError::validation(format!(
                "endpoint host label must not start or end with a hyphen: {host}"
            )));
        }
    }
    Ok(host)
}

/// Validate an explicit, user-supplied alias against the schema pattern
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$` (applied after normalization).
///
/// # Errors
///
/// Returns a `ValidationError` when the alias is empty or contains characters
/// outside the permitted set.
pub fn validate_alias(raw: &str) -> OagwResult<String> {
    let alias = normalize(raw);
    if alias.is_empty() {
        return Err(OagwError::validation("alias must not be empty"));
    }
    let bytes = alias.as_bytes();
    let is_edge_ok = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if !is_edge_ok(bytes[0]) || !is_edge_ok(bytes[bytes.len() - 1]) {
        return Err(OagwError::validation(format!(
            "alias must start and end with a lowercase letter or digit: {alias}"
        )));
    }
    if !bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b':' | b'-'))
    {
        return Err(OagwError::validation(format!(
            "alias may only contain lowercase letters, digits and the characters '.', ':', '-': {alias}"
        )));
    }
    Ok(alias)
}

/// Longest common domain suffix of `hosts` that is a *registrable* domain.
///
/// Requires at least two labels and rejects a bare public suffix
/// (`co.uk`), which is what the PSL check is for.
#[must_use]
pub fn common_domain_suffix(hosts: &[String]) -> Option<String> {
    let first = hosts.first()?;
    let mut common: Vec<&str> = first.split('.').collect();
    for host in hosts.iter().skip(1) {
        let labels: Vec<&str> = host.split('.').collect();
        let mut shared = Vec::new();
        for (a, b) in common.iter().rev().zip(labels.iter().rev()) {
            if a == b {
                shared.push(*a);
            } else {
                break;
            }
        }
        shared.reverse();
        common = shared;
        if common.is_empty() {
            return None;
        }
    }
    if common.len() < 2 {
        return None;
    }
    let candidate = common.join(".");
    // A shared suffix that is itself a public suffix is not a routing target.
    if psl::suffix_str(&candidate).is_some_and(|s| s == candidate) {
        return None;
    }
    Some(candidate)
}

/// Derive the alias for an endpoint pool, or `None` when derivation fails.
///
/// Derivation fails for IP-based pools, for heterogeneous hostnames with no
/// registrable common suffix, and for pools whose only common suffix is a bare
/// public suffix.
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    let first = endpoints.first()?;
    let hosts: Vec<String> = endpoints.iter().map(|e| normalize(&e.host)).collect();
    if hosts.iter().any(|h| is_ip_literal(h)) {
        return None;
    }

    let mut distinct: Vec<String> = Vec::new();
    for host in &hosts {
        if !distinct.contains(host) {
            distinct.push(host.clone());
        }
    }

    let base = if distinct.len() == 1 {
        distinct[0].clone()
    } else {
        common_domain_suffix(&distinct)?
    };

    // Non-standard ports stay in the alias so pools that share a domain suffix
    // on different ports do not collide.
    if first.port == first.scheme.standard_port() {
        Some(base)
    } else {
        Some(format!("{}:{}", base, first.port))
    }
}

/// Decide the alias a *newly created* upstream gets.
///
/// * Derivable pool — the derived value wins. A user-supplied alias is
///   rejected unless it is exactly the derived value (tolerated as an
///   idempotent no-op).
/// * Non-derivable pool — an explicit alias is mandatory.
///
/// # Errors
///
/// Returns a `ValidationError` when a user alias contradicts the derived one,
/// or when a non-derivable pool omits the alias.
pub fn resolve_alias_for_create(
    endpoints: &[Endpoint],
    requested: Option<&str>,
) -> OagwResult<String> {
    let derived = compute_derived_alias(endpoints);
    match (derived, requested) {
        (Some(derived), None) => Ok(derived),
        (Some(derived), Some(requested)) => {
            let requested = validate_alias(requested)?;
            if requested == derived {
                Ok(derived)
            } else {
                Err(OagwError::validation(format!(
                    "alias is auto-derived for hostname endpoints and cannot be overridden \
                     (derived '{derived}', requested '{requested}')"
                )))
            }
        }
        (None, Some(requested)) => validate_alias(requested),
        (None, None) => Err(OagwError::validation(
            "alias is required for IP-based or non-derivable endpoints",
        )),
    }
}

/// Decide the alias a *replaced* upstream keeps.
///
/// The alias is immutable: any endpoint change that would alter the derived
/// alias is rejected, and a differing user-supplied alias is never accepted.
/// The operator must delete and re-create instead.
///
/// # Errors
///
/// Returns a `ValidationError` when the update would change the alias.
pub fn enforce_alias_update(
    existing_alias: &str,
    endpoints: &[Endpoint],
    requested: Option<&str>,
) -> OagwResult<String> {
    if let Some(requested) = requested {
        let requested = validate_alias(requested)?;
        if requested != existing_alias {
            return Err(OagwError::validation(format!(
                "alias is immutable once set (current '{existing_alias}', requested \
                 '{requested}'); delete and re-create the upstream to change it"
            )));
        }
    }

    match compute_derived_alias(endpoints) {
        Some(derived) if derived == existing_alias => Ok(derived),
        Some(derived) => Err(OagwError::validation(format!(
            "endpoint change would move the alias from '{existing_alias}' to '{derived}'; \
             the alias is immutable — delete and re-create the upstream"
        ))),
        // Non-derivable pools keep the alias they were created with.
        None => Ok(existing_alias.to_owned()),
    }
}

#[cfg(test)]
#[path = "alias_tests.rs"]
mod tests;
