//! Alias derivation and enforcement (`DESIGN.md` §3.1 "Alias Resolution").
//!
//! Aliases are not arbitrary labels: they are derived from the endpoint pool,
//! validated per RFC 1123, normalized to ASCII lowercase and are **immutable**
//! once set. The derivation matrix is covered by unit tests in
//! `alias_tests.rs`.

use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, EndpointScheme};

/// Ports omitted from a derived alias, keyed by scheme.
const STANDARD_PORTS: [(EndpointScheme, u16); 5] = [
    (EndpointScheme::Https, 443),
    (EndpointScheme::Wss, 443),
    (EndpointScheme::Wt, 443),
    (EndpointScheme::Grpc, 443),
    (EndpointScheme::Http, 80),
];

/// Maximum length of a hostname per RFC 1123.
pub const MAX_HOSTNAME_LENGTH: usize = 253;
/// Maximum length of a single hostname label per RFC 1123.
pub const MAX_LABEL_LENGTH: usize = 63;

/// Normalizes a hostname or alias: ASCII lowercase, trailing dot stripped.
#[must_use]
pub fn normalize(input: &str) -> String {
    input
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase()
        .to_owned()
}

/// Returns `true` when `value` is an IPv4 dotted quad or an IPv6 literal
/// (containing `:`).
#[must_use]
pub fn is_ip_address(host: &str) -> bool {
    let host = host.trim();
    if host.contains(':') {
        return host.parse::<std::net::Ipv6Addr>().is_ok();
    }
    host.parse::<std::net::Ipv4Addr>().is_ok()
}

/// Validates a hostname per RFC 1123.
///
/// Max 253 characters total, each label 1-63 characters, ASCII alphanumeric
/// plus hyphen only, no leading or trailing hyphen. A single trailing dot
/// (FQDN notation) is tolerated.
///
/// # Errors
///
/// Returns a 400 [`DomainError::Validation`] describing the first violated
/// rule.
pub fn validate_hostname(host: &str) -> Result<(), DomainError> {
    let candidate = host.trim().trim_end_matches('.');
    if candidate.is_empty() {
        return Err(DomainError::Validation("endpoint host is empty".to_owned()));
    }
    if candidate.len() > MAX_HOSTNAME_LENGTH {
        return Err(DomainError::Validation(format!(
            "hostname '{host}' exceeds {MAX_HOSTNAME_LENGTH} characters"
        )));
    }
    if candidate.contains(':') && !is_ip_address(candidate) {
        return Err(DomainError::Validation(format!(
            "hostname '{host}' contains invalid characters"
        )));
    }
    // IPv4 and IPv6 literals are not host names, but they are legal endpoint
    // hosts (an explicit alias is then required).
    if is_ip_address(candidate) {
        return Ok(());
    }
    for label in candidate.split('.') {
        if label.is_empty() {
            return Err(DomainError::Validation(format!(
                "hostname '{host}' has an empty label"
            )));
        }
        if label.len() > MAX_LABEL_LENGTH {
            return Err(DomainError::Validation(format!(
                "hostname '{host}' has a label longer than {MAX_LABEL_LENGTH} characters"
            )));
        }
        let valid = label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if !valid {
            return Err(DomainError::Validation(format!(
                "hostname '{host}' contains characters outside [a-zA-Z0-9.-]"
            )));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(DomainError::Validation(format!(
                "hostname '{host}' has a label starting or ending with a hyphen"
            )));
        }
    }
    Ok(())
}

/// Validates an alias against the wire pattern
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
///
/// # Errors
///
/// Returns a 400 [`DomainError::Validation`] when the alias is empty, not
/// ASCII lowercase, or contains characters outside `[a-z0-9.:-]`.
pub fn validate_alias(alias: &str) -> Result<(), DomainError> {
    if alias.is_empty() {
        return Err(DomainError::Validation(
            "alias must not be empty".to_owned(),
        ));
    }
    let first = alias.as_bytes()[0];
    if !first.is_ascii_digit() && !first.is_ascii_lowercase() {
        return Err(DomainError::Validation(format!(
            "alias '{alias}' must start with a lowercase letter or digit"
        )));
    }
    let last = alias.as_bytes()[alias.len() - 1];
    if !last.is_ascii_digit() && !last.is_ascii_lowercase() {
        return Err(DomainError::Validation(format!(
            "alias '{alias}' must end with a lowercase letter or digit"
        )));
    }
    if !alias
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b':' || b == b'-')
    {
        return Err(DomainError::Validation(format!(
            "alias '{alias}' may only contain lowercase letters, digits, '.', ':' and '-'"
        )));
    }
    Ok(())
}

/// Standard (non-suffixed) port for a scheme.
#[must_use]
pub fn is_standard_port(scheme: EndpointScheme, port: u16) -> bool {
    STANDARD_PORTS
        .iter()
        .any(|(s, p)| *s == scheme && *p == port)
}

/// True when every endpoint shares the same scheme and port.
#[must_use]
pub fn endpoints_are_homogeneous(endpoints: &[Endpoint]) -> bool {
    endpoints
        .iter()
        .all(|e| e.scheme == endpoints[0].scheme && e.port == endpoints[0].port)
}

/// Longest common label suffix (case-insensitive) of a non-empty host list.
///
/// Returns the number of trailing labels shared by every host, plus the
/// normalized suffix string. Never returns more labels than the shortest host.
#[must_use]
fn common_label_suffix(hosts: &[&str]) -> Option<(usize, String)> {
    let normalized: Vec<Vec<&str>> = hosts.iter().map(|h| h.split('.').collect()).collect();
    let shortest = normalized.iter().map(Vec::len).min()?;
    let mut shared = 0usize;
    while shared < shortest {
        let label = normalized[0][normalized[0].len() - 1 - shared];
        if normalized
            .iter()
            .all(|labels| labels[labels.len() - 1 - shared] == label)
        {
            shared += 1;
        } else {
            break;
        }
    }
    if shared == 0 {
        return None;
    }
    let suffix = normalized[0][normalized[0].len() - shared..].join(".");
    Some((shared, suffix))
}

/// Derives the alias for a single-endpoint upstream.
///
/// Hostname endpoints derive `hostname` (standard port) or `hostname:port`
/// (non-standard port). IP-based endpoints are not derivable — an explicit
/// alias is required.
#[must_use]
pub fn derive_single_alias(endpoint: &Endpoint) -> Option<String> {
    let host = normalize(&endpoint.host);
    if host.is_empty() || is_ip_address(&host) {
        return None;
    }
    if is_standard_port(endpoint.scheme, endpoint.port) {
        Some(host)
    } else {
        Some(format!("{host}:{}", endpoint.port))
    }
}

/// Derives the alias for an upstream endpoint pool.
///
/// * single hostname endpoint → `hostname[:port]`
/// * multiple hostnames with a registrable common suffix (at least two labels
///   and not a bare public suffix) → `suffix[:port]`
/// * IP endpoints, heterogeneous pools or pools whose only common suffix is a
///   bare public suffix (e.g. `co.uk`) → `None`, an explicit alias is required.
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    let first = endpoints.first()?;
    if endpoints.len() == 1 {
        return derive_single_alias(first);
    }
    if !endpoints_are_homogeneous(endpoints) {
        return None;
    }
    let hosts: Vec<String> = endpoints
        .iter()
        .map(|e| normalize(&e.host))
        .collect::<Vec<_>>();
    if hosts.iter().any(|h| is_ip_address(h)) {
        return None;
    }
    let refs: Vec<&str> = hosts.iter().map(String::as_str).collect();
    let (labels, suffix) = common_label_suffix(&refs)?;
    if labels < 2 {
        return None;
    }
    // A bare public suffix (e.g. `co.uk`) is not a registrable domain: the PSL
    // reports it as its own suffix, so we reject the derivation.
    psl::domain_str(&suffix)?;
    if psl::suffix_str(&suffix).is_some_and(|public| public == suffix.as_str()) {
        return None;
    }
    if is_standard_port(first.scheme, first.port) {
        Some(suffix)
    } else {
        Some(format!("{suffix}:{}", first.port))
    }
}

/// Resolves the alias of a new upstream, enforcing the derivation matrix.
///
/// # Errors
///
/// * endpoints invalid (RFC 1123) → 400
/// * endpoints mixed scheme / port → 400
/// * derived alias missing while no explicit alias is supplied → 400
/// * explicit alias differing from the derived value → 400
pub fn enforce_alias_on_create(
    endpoints: &[Endpoint],
    provided: Option<&str>,
) -> Result<String, DomainError> {
    validate_endpoints(endpoints)?;
    let derived = compute_derived_alias(endpoints);
    match (derived, provided.map(normalize)) {
        (Some(derived), None) => Ok(derived),
        (Some(derived), Some(provided)) if provided == derived => Ok(derived),
        (Some(derived), Some(provided)) => Err(DomainError::Validation(format!(
            "alias '{provided}' does not match the derived alias '{derived}' for these endpoints"
        ))),
        (None, Some(provided)) => {
            validate_alias(&provided)?;
            Ok(provided)
        }
        (None, None) => Err(DomainError::Validation(
            "alias is required for IP-based or non-derivable endpoints".to_owned(),
        )),
    }
}

/// Re-resolves the alias on endpoint change, enforcing alias immutability.
///
/// `existing` is the alias the upstream currently carries and `provided` the
/// alias from the replacement payload (if any). Because the alias is the
/// routing key in `/v1/proxy/{alias}/...`, any endpoint change that would
/// alter the derived alias is rejected.
///
/// # Errors
///
/// Returns a 400 [`DomainError::Validation`] whenever the recomputed alias
/// differs from `existing`, including the derivable → non-derivable and
/// non-derivable → derivable transitions which are always rejected.
pub fn enforce_alias_update_with(
    existing: &str,
    endpoints: &[Endpoint],
    provided: Option<&str>,
) -> Result<String, DomainError> {
    validate_endpoints(endpoints)?;
    let derived = compute_derived_alias(endpoints);
    let normalized_provided = provided.map(normalize);

    match (derived.as_ref(), normalized_provided.as_ref()) {
        // Derivable → derivable: the recomputed alias must equal the existing one.
        (Some(derived), _) if *derived == existing => Ok(existing.to_owned()),
        (Some(derived), _) => Err(DomainError::Validation(format!(
            "endpoint change would change the derived alias to '{derived}'; the alias \
             is immutable - delete and re-create the upstream"
        ))),
        // Non-derivable → non-derivable: existing alias retained.
        (None, Some(provided)) if provided == existing => Ok(existing.to_owned()),
        (None, Some(provided)) => Err(DomainError::Validation(format!(
            "alias '{provided}' does not match the existing alias '{existing}'; the alias \
             is immutable - delete and re-create the upstream"
        ))),
        // Non-derivable, no alias supplied → keep the current alias.
        (None, None) => Ok(existing.to_owned()),
    }
}

/// Validates an endpoint pool: RFC 1123 hosts and a homogeneous scheme/port.
///
/// # Errors
///
/// Returns a 400 [`DomainError::Validation`] on the first violated rule.
pub fn validate_endpoints(endpoints: &[Endpoint]) -> Result<(), DomainError> {
    if endpoints.is_empty() {
        return Err(DomainError::Validation(
            "at least one endpoint is required".to_owned(),
        ));
    }
    for endpoint in endpoints {
        validate_hostname(&endpoint.host)?;
        if endpoint.port == 0 {
            return Err(DomainError::Validation(format!(
                "endpoint '{}' port must be between 1 and 65535",
                endpoint.host
            )));
        }
    }
    if !endpoints_are_homogeneous(endpoints) {
        return Err(DomainError::Validation(
            "all endpoints must share the same scheme and port".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "alias_tests.rs"]
mod tests;
