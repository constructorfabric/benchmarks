//! Upstream alias derivation and enforcement (PRD §5.5, DESIGN §3.5).
//!
//! A derived alias is computed from the upstream's endpoints. The alias is
//! immutable once created: an update may only keep an existing alias when it
//! continues to match the (possibly re-derived) value; changing a derived
//! alias requires deleting and recreating the upstream.

use std::collections::HashSet;

use url::Url;

use crate::domain::dto::{Endpoint, EndpointScheme};
use crate::domain::error::DomainError;

/// Normalize a hostname: trim a single trailing dot, lowercase ASCII and
/// trim surrounding whitespace.
#[must_use]
pub fn normalize_hostname(value: &str) -> String {
    let mut out = value.trim().to_ascii_lowercase();
    if out.len() > 1 && out.ends_with('.') {
        out.pop();
    }
    out
}

/// Validate an RFC 1123 hostname (labels of alphanumerics and hyphens, each
/// label not starting/ending with a hyphen, total length ≤ 253).
#[must_use]
pub fn is_valid_hostname(hostname: &str) -> bool {
    if hostname.is_empty() || hostname.len() > 253 {
        return false;
    }
    let hostname = hostname.trim_end_matches('.');
    if hostname.is_empty() {
        return false;
    }
    hostname
        .split('.')
        .all(|label| !label.is_empty() && label.len() <= 63 && valid_label(label))
}

fn valid_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
}

/// An IPv4 address in dotted-quad form (used to short-circuit hostname
/// derivation).
#[must_use]
pub fn is_ipv4(host: &str) -> bool {
    let parts: Vec<&str> = host.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && p.bytes().all(|b| b.is_ascii_digit())
                && p.parse::<u16>().is_ok_and(|v| v <= 255)
        })
}

/// `true` when the host looks like an IPv6 literal (contains `:`).
#[must_use]
pub fn is_ipv6_candidate(host: &str) -> bool {
    host.contains(':')
}

/// Validate an alias string for the wire format: lowercase RFC 1123
/// hostname, optionally followed by `:port` for non-standard ports.
#[must_use]
pub fn is_valid_alias(alias: &str) -> bool {
    let host = match alias.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => h,
        Some(_) => return false,
        None => alias,
    };
    if host.contains(':') {
        return false;
    }
    is_valid_hostname(host)
}

/// The alias derivation decision for a set of endpoints.
#[derive(Debug)]
pub enum AliasDecision {
    /// A value was derived and must be used.
    Derived(String),
    /// Endpoints do not permit derivation; the caller must supply an alias.
    ExplicitRequired,
}

/// Derive the alias for a set of endpoints, following the DESIGN §3.5
/// matrix:
///
/// - single hostname with a standard port → the hostname
/// - single hostname with a non-standard port → `hostname:port`
/// - multiple hostnames sharing a common registered-domain suffix (≥ 2
///   labels, not a bare public suffix) → `suffix[:port]`
/// - anything else (IPs, mixed hosts without a usable common suffix) →
///   `ExplicitRequired`
pub fn derive_alias(endpoints: &[Endpoint]) -> AliasDecision {
    if endpoints.is_empty() {
        return AliasDecision::ExplicitRequired;
    }

    let hosts: Vec<String> = endpoints.iter().map(Endpoint::normalized_host).collect();
    let distinct: Vec<&str> = hosts
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    if distinct.iter().all(|h| is_ipv4(h) || is_ipv6_candidate(h)) {
        return AliasDecision::ExplicitRequired;
    }

    // Ensure every hostname is a valid RFC 1123 hostname before deriving.
    // IPs mixed with hostnames are deliberately not derived from.
    if distinct.iter().any(|h| !is_valid_hostname(h)) {
        return AliasDecision::ExplicitRequired;
    }

    let port = endpoints[0].port;
    let scheme = endpoints[0].scheme;
    let standard = is_standard_port(scheme, port);

    if distinct.len() == 1 {
        let host = distinct[0].to_owned();
        return if standard {
            AliasDecision::Derived(host)
        } else {
            AliasDecision::Derived(format!("{host}:{port}"))
        };
    }

    // Multiple distinct hostnames — compute the common label suffix.
    match common_registered_suffix(&distinct) {
        Some(suffix) => {
            if standard {
                AliasDecision::Derived(suffix)
            } else {
                AliasDecision::Derived(format!("{suffix}:{port}"))
            }
        }
        None => AliasDecision::ExplicitRequired,
    }
}

fn is_standard_port(scheme: EndpointScheme, port: u16) -> bool {
    match scheme {
        // Plain HTTP uses port 80; HTTPS, WSS and gRPC all use 443
        // (DESIGN §3.5 standard-port matrix).
        EndpointScheme::Http => port == 80,
        _ => port == 443,
    }
}

/// Longest common suffix (by dot-separated labels) shared by all hosts that
/// is a registrable domain (≥ 2 labels, not a bare public suffix per the
/// PSL). Returns `None` when no usable suffix exists.
fn common_registered_suffix(hosts: &[&str]) -> Option<String> {
    let label_sets: Vec<Vec<&str>> = hosts
        .iter()
        .map(|h| h.split('.').collect::<Vec<&str>>())
        .collect();

    // Iterate labels from the right; grow the shared suffix.
    let first = &label_sets[0];
    let mut end = first.len();
    'labels: loop {
        if end == 0 {
            return None;
        }
        for other in label_sets.iter().skip(1) {
            if other.len() < end || other[other.len() - end..] != first[first.len() - end..] {
                end -= 1;
                continue 'labels;
            }
        }
        break;
    }
    if end == 0 {
        return None;
    }
    let suffix = first[first.len() - end..].join(".");
    if suffix.len() < 2 || suffix.split('.').count() < 2 {
        return None;
    }
    // Reject bare public suffixes (e.g. `co.uk`, `com`) — a registrable
    // domain, not a public suffix, is required.
    if !is_registrable(&suffix) {
        // Try removing labels one at a time until we find a registrable
        // domain that is still a common suffix.
        let mut labels: Vec<&str> = suffix.split('.').collect();
        while labels.len() >= 2 {
            let candidate = labels.join(".");
            if is_registrable(&candidate) {
                return Some(candidate);
            }
            labels.remove(0);
        }
        return None;
    }
    Some(suffix)
}

/// `true` when `domain` is a registrable domain (per the PSL) and not itself
/// a bare public suffix.
fn is_registrable(domain: &str) -> bool {
    let Some(registrable) = psl::domain_str(domain) else {
        return false;
    };
    // domain_str returns the registrable domain; we need it to be exactly
    // `domain` (i.e. `domain` is not a public suffix and has a usable parent).
    registrable == domain
}

/// Validate that a user-provided alias correctly matches the endpoints.
///
/// Used during validation: an explicit alias is accepted only when it equals
/// the derived value (when one can be derived).
#[must_use]
pub fn matches_derived(endpoints: &[Endpoint], alias: &str) -> bool {
    match derive_alias(endpoints) {
        AliasDecision::Derived(derived) => derived == alias,
        AliasDecision::ExplicitRequired => true,
    }
}

/// Validate a fully-qualified endpoint URL (scheme must match the endpoint
/// scheme, host must be present). Returns the endpoint with normalized host.
///
/// # Errors
///
/// Returns a [`DomainError::Validation`] when the URL is malformed, carries
/// no host, or the host is not a valid RFC 1123 hostname.
///
/// [`DomainError::Validation`]: crate::domain::error::DomainError::Validation
#[allow(clippy::result_large_err)] // DomainError is a broad domain error enum; boxing would ripple through every caller
pub fn validate_endpoint_url(scheme: EndpointScheme, raw: &str) -> Result<Endpoint, DomainError> {
    let url = Url::parse(raw)
        .map_err(|e| DomainError::validation(format!("invalid endpoint URL: {e}")))?;
    let Some(host) = url.host_str() else {
        return Err(DomainError::validation("endpoint URL must carry a host"));
    };
    let host_norm = normalize_hostname(host);
    if !is_valid_hostname(&host_norm) {
        return Err(DomainError::validation(format!(
            "endpoint host is not a valid hostname: {host}"
        )));
    }
    let port = match url.port_or_known_default() {
        Some(p) => p,
        None => scheme.default_port(),
    };
    Ok(Endpoint {
        scheme,
        host: host_norm,
        port,
    })
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn ep(host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: EndpointScheme::Https,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn single_hostname_standard_port() {
        match derive_alias(&[ep("api.example.com", 443)]) {
            AliasDecision::Derived(a) => assert_eq!(a, "api.example.com"),
            AliasDecision::ExplicitRequired => panic!("expected derived"),
        }
    }

    #[test]
    fn single_hostname_nonstandard_port() {
        match derive_alias(&[ep("api.example.com", 8443)]) {
            AliasDecision::Derived(a) => assert_eq!(a, "api.example.com:8443"),
            AliasDecision::ExplicitRequired => panic!("expected derived"),
        }
    }

    #[test]
    fn multi_hostname_common_suffix() {
        match derive_alias(&[ep("us.api.example.com", 443), ep("eu.api.example.com", 443)]) {
            AliasDecision::Derived(a) => assert_eq!(a, "example.com"),
            AliasDecision::ExplicitRequired => panic!("expected derived"),
        }
    }

    #[test]
    fn multi_hostname_common_suffix_is_not_public_suffix() {
        // `com` alone would be a bare public suffix — rejected.
        match derive_alias(&[ep("a.com", 443), ep("b.com", 443)]) {
            AliasDecision::ExplicitRequired => {}
            other @ AliasDecision::Derived(_) => {
                panic!("expected explicit required, got {other:?}")
            }
        }
    }

    #[test]
    fn ip_address_requires_explicit_alias() {
        match derive_alias(&[ep("192.168.0.1", 443)]) {
            AliasDecision::ExplicitRequired => {}
            AliasDecision::Derived(_) => panic!("expected explicit required"),
        }
    }

    #[test]
    fn hostname_validation() {
        assert!(is_valid_hostname("api.example.com"));
        assert!(is_valid_hostname("a-b.example.com"));
        assert!(!is_valid_hostname("-bad.example.com"));
        assert!(!is_valid_hostname("bad-.example.com"));
        assert!(!is_valid_hostname("under_score.example.com"));
        assert!(!is_valid_hostname(""));
    }

    #[test]
    fn alias_format_validation() {
        // Structure is checked; case is normalized separately (DESIGN §3.5).
        assert!(is_valid_alias("api.example.com"));
        assert!(is_valid_alias("api.example.com:8443"));
        assert!(is_valid_alias("Api.Example.com"));
        assert!(!is_valid_alias("api.example.com:notaport"));
        assert!(!is_valid_alias("api.example.com/v1"));
        assert!(!is_valid_alias("api.example.com:"));
    }

    #[test]
    fn alias_normalization_lowercases_and_strips_dot() {
        let a = super::super::management::normalize_alias("Api.OpenAI.COM.");
        assert_eq!(a, "api.openai.com");
    }
}
