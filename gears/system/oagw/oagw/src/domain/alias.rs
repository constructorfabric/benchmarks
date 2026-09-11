//! Alias derivation, normalization and hostname validation.
//!
//! Implements `docs/DESIGN.md` §"Alias Enforcement Rules" and the alias update
//! transition table.

use crate::domain::error::DomainError;
use crate::domain::model::EndpointScheme;
use crate::domain::model::{Endpoint, Upstream};

/// Maximum length of an RFC 1123 hostname (trailing dot excluded).
const MAX_HOST_LEN: usize = 253;
/// Maximum length of a single hostname label.
const MAX_LABEL_LEN: usize = 63;
/// Minimum number of labels a derived common suffix must have.
const MIN_COMMON_SUFFIX_LABELS: usize = 2;

/// Result of alias derivation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasDerivation {
    /// A value was derived.
    Derived(&'static str),
    /// No value could be derived; an explicit alias is required.
    #[allow(dead_code, reason = "variant is matched on, never constructed")]
    NotDerivable,
}

/// Whether a host string is an IPv4 or IPv6 literal.
#[must_use]
pub fn is_ip_address(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
}

/// Whether a host string is a valid RFC 1123 hostname (IPs are not hostnames).
#[must_use]
pub fn is_valid_hostname(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() || host.len() > MAX_HOST_LEN {
        return false;
    }
    // A hostname never contains a scheme separator, a port or whitespace.
    if host.contains([':', '/', ' ', '\t', '@', '%']) {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= MAX_LABEL_LEN
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

/// Whether a host is acceptable as an upstream endpoint host.
#[must_use]
pub fn is_valid_host(host: &str) -> bool {
    is_valid_hostname(host) || is_ip_address(host)
}

/// Normalize an alias or host: ASCII lowercase, trailing dots stripped.
#[must_use]
pub fn normalize(input: &str) -> String {
    input
        .trim()
        .to_ascii_lowercase()
        .trim_end_matches('.')
        .to_owned()
}

/// Port considered standard for a scheme and therefore omitted from aliases.
#[must_use]
pub fn standard_port(scheme: EndpointScheme) -> u16 {
    match scheme {
        EndpointScheme::Http => 80,
        EndpointScheme::Https | EndpointScheme::Wss | EndpointScheme::Wt | EndpointScheme::Grpc => {
            443
        }
    }
}

/// Longest common domain suffix of two hosts, or `None` when they share less
/// than two labels.
fn common_suffix(a: &str, b: &str) -> Option<String> {
    let a: Vec<&str> = a.split('.').collect();
    let b: Vec<&str> = b.split('.').collect();
    let mut shared = 0;
    while shared < a.len() && shared < b.len() {
        let la = a[a.len() - 1 - shared];
        let lb = b[b.len() - 1 - shared];
        if !la.eq_ignore_ascii_case(lb) {
            break;
        }
        shared += 1;
    }
    if shared < MIN_COMMON_SUFFIX_LABELS {
        return None;
    }
    Some(a[a.len() - shared..].join("."))
}

/// Whether a dotted suffix is a registrable domain per the public suffix list.
///
/// `vendor.com` is registrable; `co.uk` is a bare public suffix and is not.
fn is_registrable(suffix: &str) -> bool {
    psl::domain_str(suffix).is_some()
}

/// Derive the alias for a pool of endpoints.
///
/// Returns `None` when the endpoints are IP-based, heterogeneous or only share
/// a bare public suffix — those require an explicit alias.
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    let first = endpoints.first()?;
    if endpoints.is_empty() {
        return None;
    }
    let port = first.port;
    let scheme = first.scheme;
    if endpoints
        .iter()
        .any(|e| e.port != port || e.scheme != scheme)
    {
        return None;
    }

    let hosts: Vec<String> = endpoints.iter().map(|e| normalize(&e.host)).collect();
    if hosts.iter().any(|h| is_ip_address(h)) {
        return None;
    }

    if hosts.len() == 1 {
        let host = &hosts[0];
        if !is_valid_hostname(host) {
            return None;
        }
        return Some(if port == standard_port(scheme) {
            host.clone()
        } else {
            format!("{host}:{port}")
        });
    }

    let mut suffix: Option<String> = None;
    for pair in hosts.windows(2) {
        let candidate = common_suffix(&pair[0], &pair[1])?;
        // The longest common suffix wins; ties keep the newer candidate.
        if suffix
            .as_ref()
            .is_none_or(|existing| existing.len() <= candidate.len())
        {
            suffix = Some(candidate);
        }
    }
    let suffix = suffix?;
    if !is_registrable(&suffix) {
        return None;
    }
    Some(if port == standard_port(scheme) {
        suffix
    } else {
        format!("{suffix}:{port}")
    })
}

/// Enforce the alias rules for a create or replace.
///
/// * A derived alias always wins; a user alias must match it exactly.
/// * A non-derivable pool requires an explicit alias.
///
/// # Errors
///
/// Returns [`DomainError::Validation`] when the user-supplied alias disagrees
/// with the derived value, or when none can be derived and none was supplied.
pub fn enforce_alias_update_with(
    derived: Option<String>,
    user_alias: Option<&str>,
    context: &str,
) -> Result<String, DomainError> {
    match (derived, user_alias) {
        (Some(derived), None) => Ok(derived),
        (Some(derived), Some(provided)) => {
            let provided = normalize(provided);
            if provided == derived {
                Ok(derived)
            } else {
                Err(DomainError::validation(format!(
                    "{context}: alias must be the derived value '{derived}', got '{provided}'"
                )))
            }
        }
        (None, Some(provided)) => {
            let provided = normalize(provided);
            if provided.is_empty() {
                Err(DomainError::validation(format!(
                    "{context}: an explicit alias is required for these endpoints"
                )))
            } else {
                Ok(provided)
            }
        }
        (None, None) => Err(DomainError::validation(format!(
            "{context}: an explicit alias is required for these endpoints"
        ))),
    }
}

/// Enforce alias immutability on replace, per the DESIGN transition table.
///
/// # Errors
///
/// Returns [`DomainError::Validation`] whenever the recomputed alias differs
/// from the stored one, regardless of any user-supplied alias.
pub fn enforce_alias_update_derived(
    existing: &Upstream,
    new_endpoints: &[Endpoint],
    user_alias: Option<&str>,
) -> Result<String, DomainError> {
    let derived = compute_derived_alias(new_endpoints);
    let existing_alias = normalize(&existing.alias);
    match derived {
        Some(derived) if normalize(&derived) == existing_alias => {
            if let Some(provided) = user_alias {
                let provided = normalize(provided);
                if !provided.is_empty() && provided != existing_alias {
                    return Err(DomainError::validation(format!(
                        "alias is immutable; derived alias is '{existing_alias}'"
                    )));
                }
            }
            Ok(existing_alias)
        }
        Some(derived) => Err(DomainError::validation(format!(
            "alias is immutable: endpoints would change the alias to '{}' (existing '{}'); \
             delete and re-create the upstream instead",
            normalize(&derived),
            existing_alias
        ))),
        None => Err(DomainError::validation(format!(
            "alias is immutable: these endpoints have no derivable alias \
             (existing '{existing_alias}'); delete and re-create the upstream instead"
        ))),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn ep(host: &str, port: u16, scheme: EndpointScheme) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn single_hostname_on_standard_port_derives_the_host() {
        let eps = [ep("api.openai.com", 443, EndpointScheme::Https)];
        assert_eq!(
            compute_derived_alias(&eps).as_deref(),
            Some("api.openai.com")
        );
    }

    #[test]
    fn non_standard_port_is_appended() {
        let eps = [ep("api.openai.com", 8443, EndpointScheme::Https)];
        assert_eq!(
            compute_derived_alias(&eps).as_deref(),
            Some("api.openai.com:8443")
        );
    }

    #[test]
    fn http_uses_port_80_as_standard() {
        let eps = [ep("svc.local", 80, EndpointScheme::Http)];
        assert_eq!(compute_derived_alias(&eps).as_deref(), Some("svc.local"));
    }

    #[test]
    fn multi_host_pool_derives_the_common_suffix() {
        let eps = [
            ep("us.vendor.com", 443, EndpointScheme::Https),
            ep("eu.vendor.com", 443, EndpointScheme::Https),
        ];
        assert_eq!(compute_derived_alias(&eps).as_deref(), Some("vendor.com"));
    }

    #[test]
    fn bare_public_suffix_is_not_derivable() {
        let eps = [
            ep("foo.co.uk", 443, EndpointScheme::Https),
            ep("bar.co.uk", 443, EndpointScheme::Https),
        ];
        assert_eq!(compute_derived_alias(&eps), None);
    }

    #[test]
    fn heterogeneous_hosts_are_not_derivable() {
        let eps = [
            ep("us.foo.com", 443, EndpointScheme::Https),
            ep("eu.bar.com", 443, EndpointScheme::Https),
        ];
        assert_eq!(compute_derived_alias(&eps), None);
    }

    #[test]
    fn ip_endpoints_are_never_derived() {
        let eps = [
            ep("10.0.1.1", 443, EndpointScheme::Https),
            ep("10.0.1.2", 443, EndpointScheme::Https),
        ];
        assert_eq!(compute_derived_alias(&eps), None);
    }

    #[test]
    fn mixed_ports_break_derivation() {
        let eps = [
            ep("us.vendor.com", 443, EndpointScheme::Https),
            ep("eu.vendor.com", 8443, EndpointScheme::Https),
        ];
        assert_eq!(compute_derived_alias(&eps), None);
    }

    #[test]
    fn multi_host_pool_preserves_the_port() {
        let eps = [
            ep("us.vendor.com", 8443, EndpointScheme::Https),
            ep("eu.vendor.com", 8443, EndpointScheme::Https),
        ];
        assert_eq!(
            compute_derived_alias(&eps).as_deref(),
            Some("vendor.com:8443")
        );
    }

    #[test]
    fn normalization_lowercases_and_strips_trailing_dots() {
        assert_eq!(normalize("Api.OpenAI.COM."), "api.openai.com");
        assert_eq!(normalize("  EXAMPLE.com  "), "example.com");
    }

    #[test]
    fn hostname_validation_accepts_and_rejects() {
        assert!(is_valid_host("api.openai.com"));
        assert!(is_valid_host("api.openai.com."));
        assert!(is_valid_host("a-b.c-d.example"));
        assert!(is_valid_host("10.0.1.1"));
        assert!(is_valid_host("::1"));
        assert!(!is_valid_host("-bad.example.com"));
        assert!(!is_valid_host("bad-.example.com"));
        assert!(!is_valid_host("bad_host.example.com"));
        assert!(!is_valid_host(""));
        assert!(!is_valid_host("host:443"));
        assert!(!is_valid_host("host/path"));
    }

    #[test]
    fn label_length_limit_is_enforced() {
        let long = "a".repeat(64);
        assert!(!is_valid_host(&format!("{long}.example.com")));
        let ok = "a".repeat(63);
        assert!(is_valid_host(&format!("{ok}.example.com")));
    }

    #[test]
    fn total_length_limit_is_enforced() {
        let label = "a".repeat(63);
        let host = ["x"; 5]
            .iter()
            .map(|_| label.clone())
            .collect::<Vec<_>>()
            .join(".");
        assert!(host.len() > MAX_HOST_LEN);
        assert!(!is_valid_host(&host));
    }
}
