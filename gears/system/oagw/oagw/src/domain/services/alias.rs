//! Alias derivation and validation (DOCS §2).
//!
//! Alias rules are determined by the upstream endpoints:
//!
//! * single hostname + standard port → alias = hostname
//! * single hostname + non-standard port → alias = `hostname:port`
//! * multiple hostnames, common registrable suffix (PSL-validated,
//!   ≥2 labels) → alias = suffix (`:port` when non-standard)
//! * multiple hostnames whose common suffix is a bare public suffix, or
//!   no common registrable suffix → explicit alias required
//! * any IP endpoint → explicit alias required
//!
//! Aliases are normalized to ASCII lowercase with trailing dots stripped;
//! resolution is case-insensitive.

use std::net::IpAddr;
use std::str::FromStr;

use psl::Psl;

use crate::domain::error::DomainError;
use crate::domain::models::{Endpoint, EndpointScheme};

/// Whether `alias` matches the documented pattern `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
#[must_use]
pub fn is_valid_alias(alias: &str) -> bool {
    let bytes = alias.as_bytes();
    if bytes.is_empty() || bytes.len() > 253 {
        return false;
    }
    let is_alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let is_ok = |b: u8| is_alnum(b) || matches!(b, b'.' | b':' | b'-');
    // First and last characters must be alphanumeric.
    if !is_alnum(bytes[0]) || !is_alnum(bytes[bytes.len() - 1]) {
        return false;
    }
    bytes.iter().all(|&b| is_ok(b))
}

/// Normalize an alias for storage/resolution: ASCII lowercase, strip
/// trailing dots.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias.trim_end_matches('.').to_ascii_lowercase()
}

/// Validate a hostname per RFC 1123: max 253 chars, 1–63 per label,
/// ASCII alphanumerics + hyphen, labels do not start/end with a hyphen.
/// A trailing dot (FQDN) is tolerated and stripped.
#[must_use]
pub fn validate_hostname(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        if label.is_empty() || label.len() > 63 {
            return false;
        }
        let bytes = label.as_bytes();
        let valid_char = |b: u8| b.is_ascii_alphanumeric() || b == b'-';
        bytes.iter().all(|&b| valid_char(b)) && bytes[0] != b'-' && bytes[bytes.len() - 1] != b'-'
    })
}

/// Whether the host literal is an IP address (v4 or v6).
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    IpAddr::from_str(host).is_ok()
}

/// Outcome of alias derivation for an upstream's endpoint set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Derivation {
    /// The alias is auto-derived from the endpoints.
    Derived(String),
    /// No alias can be derived; an explicit alias is required.
    RequiresExplicit,
}

/// Standard port for a scheme — omitted from derived aliases.
#[must_use]
pub fn standard_port(scheme: EndpointScheme) -> u16 {
    match scheme {
        EndpointScheme::Http => 80,
        _ => 443,
    }
}

/// Derive the alias for an endpoint set following the DOCS §2 rules.
/// Endpoints are assumed already validated (non-empty, consistent
/// schemes).
#[must_use]
pub fn derive_alias(endpoints: &[Endpoint]) -> Derivation {
    // Group endpoints by shape: hostname vs IP.
    let hostnames: Vec<&Endpoint> = endpoints
        .iter()
        .filter(|e| !is_ip_literal(&e.host))
        .collect();

    // Any IP endpoint (or a scheme mix we cannot reason about) makes
    // derivation impossible — explicit alias required.
    if hostnames.len() != endpoints.len() {
        return Derivation::RequiresExplicit;
    }

    let hostname_strs: Vec<String> = hostnames
        .iter()
        .map(|e| normalize_alias(&e.host))
        .collect();

    // All endpoints must be reachable on the same port for a shared alias.
    let first_port = endpoints[0].effective_port();
    let standard = standard_port(endpoints[0].scheme);
    let ports_consistent = hostnames
        .iter()
        .all(|e| e.effective_port() == first_port);

    if hostname_strs.len() == 1 {
        let host = &hostname_strs[0];
        return if ports_consistent && first_port != standard {
            Derivation::Derived(format!("{host}:{first_port}"))
        } else if ports_consistent {
            Derivation::Derived(host.clone())
        } else {
            Derivation::RequiresExplicit
        };
    }

    // Multiple hostnames: find the longest common suffix with ≥2 labels
    // that is a registrable (non-public) domain.
    match common_registrable_suffix(&hostname_strs) {
        Some(suffix) => {
            if ports_consistent && first_port != standard {
                Derivation::Derived(format!("{suffix}:{first_port}"))
            } else if ports_consistent {
                Derivation::Derived(suffix)
            } else {
                Derivation::RequiresExplicit
            }
        }
        None => Derivation::RequiresExplicit,
    }
}

/// Longest common suffix of the label sequences that (a) has at least two
/// labels and (b) is not a bare public suffix (PSL), i.e. is registrable.
fn common_registrable_suffix(hostnames: &[String]) -> Option<String> {
    let label_lists: Vec<Vec<&str>> = hostnames
        .iter()
        .map(|h| h.split('.').collect())
        .collect();
    if label_lists.is_empty() {
        return None;
    }

    // Longest common suffix of the label arrays, comparing from the end.
    // A host `h` has `h.len()` labels; the k-th-from-last label is at
    // `h[h.len() - 1 - k]`. `i` walks the first host's labels from its
    // end; `distance = first.len() - 1 - i` is the offset from the end.
    let first = &label_lists[0];
    let mut common_rev: Vec<&str> = Vec::new();
    'outer: for i in (0..first.len()).rev() {
        let distance = first.len() - 1 - i;
        let label = first[i];
        for labels in label_lists.iter().skip(1) {
            if labels.len() <= distance || labels[labels.len() - 1 - distance] != label {
                break 'outer;
            }
        }
        common_rev.push(label);
    }

    if common_rev.len() < 2 {
        return None;
    }

    let candidate = common_rev.iter().rev().copied().collect::<Vec<_>>().join(".");
    // Reject a candidate that is itself a public suffix, i.e. has no
    // label before the suffix (e.g. `co.uk` from foo.co.uk + bar.co.uk).
    // `List::domain` returns `None` exactly when the name is a bare
    // public suffix, so it is the precise test for "registrable".
    psl::List.domain(candidate.as_bytes())?;
    Some(candidate)
}

/// Validate the endpoints of an upstream as a unit, and resolve the
/// effective alias given an optional user-provided one.
///
/// Returns `(alias, changed)` where `changed` is `true` when the caller
/// provided an alias different from the derived value.
///
/// # Errors
/// * `DomainError::Validation` when endpoints are empty, mix schemes or
///   ports inconsistently, or the alias rules are violated.
pub fn resolve_alias(
    endpoints: &[Endpoint],
    provided: Option<&str>,
) -> Result<(String, bool), DomainError> {
    if endpoints.is_empty() {
        return Err(DomainError::validation(
            "upstream must declare at least one endpoint",
        ));
    }

    // Scheme / port consistency across the pool (DOCS §4.3).
    let scheme = endpoints[0].scheme;
    let port = endpoints[0].effective_port();
    for e in endpoints {
        if e.scheme != scheme {
            return Err(DomainError::validation(
                "all endpoints must use the same scheme",
            ));
        }
        if e.effective_port() != port {
            return Err(DomainError::validation(
                "all endpoints must use the same port",
            ));
        }
    }
    for e in endpoints {
        if !is_ip_literal(&e.host) && !validate_hostname(&e.host) {
            return Err(DomainError::validation(format!(
                "endpoint host '{}' is not a valid hostname or IP",
                e.host
            )));
        }
    }

    match derive_alias(endpoints) {
        Derivation::Derived(derived) => match provided {
            None => Ok((derived, false)),
            Some(p) => {
                let normalized = normalize_alias(p);
                if normalized == derived {
                    // Providing the exact derived value is a tolerated
                    // idempotent no-op.
                    Ok((derived, false))
                } else {
                    Err(DomainError::validation(format!(
                        "alias '{normalized}' does not match the derived alias '{derived}' \
                         for the declared endpoints"
                    )))
                }
            }
        },
        Derivation::RequiresExplicit => match provided {
            Some(p) => {
                let normalized = normalize_alias(p);
                if !is_valid_alias(&normalized) {
                    return Err(DomainError::validation(format!(
                        "alias '{normalized}' contains invalid characters; \
                         only [a-z0-9.:-] are allowed and the alias must start/end alphanumeric"
                    )));
                }
                Ok((normalized, true))
            }
            None => Err(DomainError::validation(
                "an explicit alias is required for this endpoint set (IP endpoints or \
                 no common registrable domain suffix)",
            )),
        },
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn ep(host: &str, port: Option<u16>) -> Endpoint {
        Endpoint {
            scheme: EndpointScheme::Https,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn alias_pattern_rules() {
        assert!(is_valid_alias("api.openai.com"));
        assert!(is_valid_alias("api.openai.com:8443"));
        assert!(is_valid_alias("my-internal-service"));
        assert!(is_valid_alias("a"));
        assert!(!is_valid_alias(""));
        assert!(!is_valid_alias("-leading"));
        assert!(!is_valid_alias("trailing-"));
        assert!(!is_valid_alias(".dotstart"));
        assert!(!is_valid_alias("has space"));
        assert!(!is_valid_alias("UPPER"));
    }

    #[test]
    fn normalize_lowercases_and_strips_trailing_dots() {
        assert_eq!(normalize_alias("Api.OpenAI.COM."), "api.openai.com");
    }

    #[test]
    fn single_hostname_standard_port_derives_hostname() {
        // https://api.openai.com:443 → api.openai.com
        assert_eq!(
            derive_alias(&[ep("api.openai.com", Some(443))]),
            Derivation::Derived("api.openai.com".into())
        );
    }

    #[test]
    fn single_hostname_non_standard_port_derives_hostname_port() {
        assert_eq!(
            derive_alias(&[ep("api.openai.com", Some(8443))]),
            Derivation::Derived("api.openai.com:8443".into())
        );
    }

    #[test]
    fn multi_hostname_common_suffix_derives_registrable_domain() {
        // us.vendor.com + eu.vendor.com → vendor.com
        assert_eq!(
            derive_alias(&[ep("us.vendor.com", None), ep("eu.vendor.com", None)]),
            Derivation::Derived("vendor.com".into())
        );
    }

    #[test]
    fn multi_hostname_common_suffix_preserves_non_standard_port() {
        assert_eq!(
            derive_alias(&[
                ep("us.vendor.com", Some(8443)),
                ep("eu.vendor.com", Some(8443))
            ]),
            Derivation::Derived("vendor.com:8443".into())
        );
    }

    #[test]
    fn bare_public_suffix_common_requires_explicit() {
        // foo.co.uk + bar.co.uk → co.uk is a bare public suffix.
        assert_eq!(
            derive_alias(&[ep("foo.co.uk", None), ep("bar.co.uk", None)]),
            Derivation::RequiresExplicit
        );
    }

    #[test]
    fn no_common_suffix_requires_explicit() {
        assert_eq!(
            derive_alias(&[ep("us.foo.com", None), ep("eu.bar.com", None)]),
            Derivation::RequiresExplicit
        );
    }

    #[test]
    fn ip_endpoints_require_explicit() {
        assert_eq!(
            derive_alias(&[ep("10.0.1.1", None), ep("10.0.1.2", None)]),
            Derivation::RequiresExplicit
        );
        assert_eq!(
            derive_alias(&[ep("10.0.1.1", None)]),
            Derivation::RequiresExplicit
        );
    }

    #[test]
    fn resolve_provided_alias_matching_derived_is_noop() {
        let endpoints = [ep("api.openai.com", None)];
        let (alias, changed) = resolve_alias(&endpoints, Some("api.openai.com")).unwrap();
        assert_eq!(alias, "api.openai.com");
        assert!(!changed);
    }

    #[test]
    fn resolve_rejects_mismatched_alias_for_hostname_endpoints() {
        let endpoints = [ep("api.openai.com", None)];
        let err = resolve_alias(&endpoints, Some("my-alias")).unwrap_err();
        assert!(matches!(err, DomainError::Validation { .. }));
    }

    #[test]
    fn resolve_accepts_explicit_alias_for_ips() {
        let endpoints = [ep("10.0.1.1", None), ep("10.0.1.2", None)];
        let (alias, changed) = resolve_alias(&endpoints, Some("my-internal-service")).unwrap();
        assert_eq!(alias, "my-internal-service");
        assert!(changed);
    }

    #[test]
    fn resolve_requires_explicit_alias_for_ips() {
        let endpoints = [ep("10.0.1.1", None)];
        let err = resolve_alias(&endpoints, None).unwrap_err();
        assert!(matches!(err, DomainError::Validation { .. }));
    }

    #[test]
    fn hostname_validation() {
        assert!(validate_hostname("api.openai.com"));
        assert!(validate_hostname("localhost")); // single label
        assert!(validate_hostname("api.openai.com.")); // FQDN tolerated
        assert!(!validate_hostname("-bad.com"));
        assert!(!validate_hostname("bad-.com"));
        assert!(!validate_hostname("has space.com"));
        assert!(!validate_hostname(""));
    }

    #[test]
    fn ip_literal_detection() {
        assert!(is_ip_literal("10.0.1.1"));
        assert!(is_ip_literal("2001:db8::1"));
        assert!(!is_ip_literal("api.openai.com"));
    }
}
