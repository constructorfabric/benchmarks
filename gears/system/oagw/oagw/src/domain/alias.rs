//! Alias derivation, normalization and validation (`PRD.md` § 5.5, `DESIGN.md`
//! § 3.2 "Alias Resolution").
//!
//! An alias is the human-readable routing identifier in the proxy URL. It is
//! *enforced* by endpoint type: hostname endpoints always derive it, IP or
//! non-derivable endpoints require it explicitly.

use std::collections::BTreeSet;
use std::net::IpAddr;

use crate::domain::model::Endpoint;

/// Standard port for plaintext HTTP.
pub const HTTP_STANDARD_PORT: u16 = 80;

/// Standard port for every TLS-carrying scheme.
pub const TLS_STANDARD_PORT: u16 = 443;

/// Maximum length of a fully qualified domain name.
const MAX_HOSTNAME_LEN: usize = 253;

/// Maximum length of a single DNS label.
const MAX_LABEL_LEN: usize = 63;

/// Minimum number of labels in a derivable common suffix.
const MIN_COMMON_LABELS: usize = 2;

/// Standard port for a scheme: 80 for `http`, 443 for every other scheme.
#[must_use]
pub fn is_standard_port(scheme: crate::domain::model::Scheme, port: u16) -> bool {
    match scheme {
        crate::domain::model::Scheme::Http => port == HTTP_STANDARD_PORT,
        _ => port == TLS_STANDARD_PORT,
    }
}

/// Normalizes an alias: ASCII lowercase, trailing dots stripped, whitespace
/// trimmed. Returns `None` when the result is empty.
#[must_use]
pub fn normalize_alias(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('.').to_ascii_lowercase();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Validates a hostname per RFC 1123.
///
/// # Errors
/// Returns a description of the first rule the host violates.
pub fn validate_hostname(raw: &str) -> Result<(), String> {
    let host = raw.trim().trim_end_matches('.');
    if host.is_empty() {
        return Err("host must not be empty".to_owned());
    }
    if host.len() > MAX_HOSTNAME_LEN {
        return Err("host exceeds the 253 character limit".to_owned());
    }
    for label in host.split('.') {
        if label.is_empty() {
            return Err(format!("host '{raw}' has an empty label"));
        }
        if label.len() > MAX_LABEL_LEN {
            return Err(format!(
                "host '{raw}' has a label longer than 63 characters"
            ));
        }
        for ch in label.chars() {
            if !(ch.is_ascii_alphanumeric() || ch == '-') {
                return Err(format!(
                    "host '{raw}' contains the invalid character '{ch}'"
                ));
            }
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!(
                "label in host '{raw}' starts or ends with a hyphen"
            ));
        }
    }
    Ok(())
}

/// Whether a host is a literal IPv4 or IPv6 address.
#[must_use]
pub fn is_ip_address(host: &str) -> bool {
    let candidate = host.trim_end_matches('.');
    candidate.parse::<IpAddr>().is_ok()
}

/// Derives the alias for a pool of endpoints, or `None` when the pool is not
/// derivable and an explicit alias is required.
///
/// * a single hostname uses the hostname, or `hostname:port` on a non-standard
///   port;
/// * a pool of hostnames uses the longest common suffix of at least two labels
///   that is a registrable domain, with the port appended on a non-standard
///   port;
/// * a bare public suffix (e.g. `co.uk`), a pool with no common suffix, a pool
///   with mixed ports, or any IP endpoint is not derivable.
#[must_use]
pub fn derive_alias(endpoints: &[Endpoint]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }

    let mut hosts: BTreeSet<String> = BTreeSet::new();
    let mut ports: BTreeSet<u16> = BTreeSet::new();
    for endpoint in endpoints {
        if is_ip_address(&endpoint.host) {
            return None;
        }
        if validate_hostname(&endpoint.host).is_err() {
            return None;
        }
        hosts.insert(normalize_alias(&endpoint.host)?);
        ports.insert(endpoint.port());
    }

    if ports.len() > 1 {
        // Mixed ports cannot produce a single unambiguous alias.
        return None;
    }
    let port = ports.into_iter().next()?;

    if hosts.len() == 1 {
        let host = hosts.into_iter().next()?;
        return Some(with_port(host, port, endpoints.first()?));
    }

    let suffix = common_suffix(&hosts)?;
    if !is_registrable(&suffix) {
        // A bare public suffix (e.g. `co.uk`) is not a usable alias.
        return None;
    }
    Some(with_port(suffix, port, endpoints.first()?))
}

fn with_port(host: String, port: u16, endpoint: &Endpoint) -> String {
    if is_standard_port(endpoint.scheme, port) {
        host
    } else {
        format!("{host}:{port}")
    }
}

/// Longest common suffix of `hosts` made of at least two labels.
fn common_suffix(hosts: &BTreeSet<String>) -> Option<String> {
    let mut candidate: Option<String> = None;
    for host in hosts {
        match candidate {
            None => candidate = Some(host.clone()),
            Some(ref current) => {
                let shared = shared_suffix_labels(current, host);
                if shared < MIN_COMMON_LABELS {
                    return None;
                }
                let next = tail_labels(host, shared);
                candidate = Some(next);
            }
        }
    }
    candidate
}

/// Number of labels shared at the tail of two hosts.
fn shared_suffix_labels(a: &str, b: &str) -> usize {
    let a: Vec<&str> = a.split('.').collect();
    let b: Vec<&str> = b.split('.').collect();
    a.iter()
        .rev()
        .zip(b.iter().rev())
        .take_while(|(x, y)| x == y)
        .count()
        .min(a.len())
        .min(b.len())
}

/// Last `count` labels of a host, joined with dots.
fn tail_labels(host: &str, count: usize) -> String {
    let labels: Vec<&str> = host.split('.').collect();
    let start = labels.len().saturating_sub(count);
    labels[start..].join(".")
}

/// Whether a host is a registrable domain: the public suffix plus at least one
/// more label. A bare public suffix (`co.uk`) is not registrable.
fn is_registrable(host: &str) -> bool {
    psl::domain_str(host).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::Scheme;

    fn ep(host: &str, port: u16, scheme: Scheme) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port: Some(port),
        }
    }

    #[test]
    fn single_hostname_on_the_standard_port_uses_the_hostname() {
        let endpoints = [ep("api.openai.com", 443, Scheme::Https)];
        assert_eq!(derive_alias(&endpoints).as_deref(), Some("api.openai.com"));
    }

    #[test]
    fn single_hostname_on_a_nonstandard_port_includes_the_port() {
        let endpoints = [ep("api.openai.com", 8443, Scheme::Https)];
        assert_eq!(
            derive_alias(&endpoints).as_deref(),
            Some("api.openai.com:8443")
        );
    }

    #[test]
    fn plaintext_http_uses_port_80_as_its_standard_port() {
        assert_eq!(
            derive_alias(&[ep("svc.internal", 80, Scheme::Http)]).as_deref(),
            Some("svc.internal")
        );
        assert_eq!(
            derive_alias(&[ep("svc.internal", 8080, Scheme::Http)]).as_deref(),
            Some("svc.internal:8080")
        );
    }

    #[test]
    fn multi_region_pool_derives_the_common_registrable_suffix() {
        let endpoints = [
            ep("us.vendor.com", 443, Scheme::Https),
            ep("eu.vendor.com", 443, Scheme::Https),
        ];
        assert_eq!(derive_alias(&endpoints).as_deref(), Some("vendor.com"));
    }

    #[test]
    fn multi_region_pool_with_a_nonstandard_port_derives_suffix_and_port() {
        let endpoints = [
            ep("us.vendor.com", 8443, Scheme::Https),
            ep("eu.vendor.com", 8443, Scheme::Https),
        ];
        assert_eq!(derive_alias(&endpoints).as_deref(), Some("vendor.com:8443"));
    }

    #[test]
    fn a_pool_whose_common_suffix_is_a_bare_public_suffix_is_not_derivable() {
        let endpoints = [
            ep("a.co.uk", 443, Scheme::Https),
            ep("b.co.uk", 443, Scheme::Https),
        ];
        assert_eq!(derive_alias(&endpoints), None);
    }

    #[test]
    fn a_pool_with_no_common_suffix_is_not_derivable() {
        let endpoints = [
            ep("alpha.one.com", 443, Scheme::Https),
            ep("beta.two.org", 443, Scheme::Https),
        ];
        assert_eq!(derive_alias(&endpoints), None);
    }

    #[test]
    fn a_single_label_host_is_not_a_derivable_pool_suffix() {
        // A two-label pool sharing only one label is below the minimum.
        let endpoints = [
            ep("api.com", 443, Scheme::Https),
            ep("web.com", 443, Scheme::Https),
        ];
        assert_eq!(derive_alias(&endpoints), None);
    }

    #[test]
    fn ip_endpoints_are_not_derivable() {
        let endpoints = [
            ep("10.0.1.1", 443, Scheme::Https),
            ep("10.0.1.2", 443, Scheme::Https),
        ];
        assert_eq!(derive_alias(&endpoints), None);
        assert_eq!(derive_alias(&[ep("10.0.1.1", 443, Scheme::Https)]), None);
    }

    #[test]
    fn ipv6_endpoints_are_not_derivable() {
        let endpoints = [ep("::1", 443, Scheme::Https)];
        assert_eq!(derive_alias(&endpoints), None);
    }

    #[test]
    fn mixed_ports_are_not_derivable() {
        let endpoints = [
            ep("us.vendor.com", 443, Scheme::Https),
            ep("eu.vendor.com", 8443, Scheme::Https),
        ];
        assert_eq!(derive_alias(&endpoints), None);
    }

    #[test]
    fn normalization_is_lowercase_and_strips_trailing_dots() {
        assert_eq!(
            normalize_alias("API.OpenAI.COM.").as_deref(),
            Some("api.openai.com")
        );
        assert_eq!(normalize_alias("   "), None);
        assert_eq!(normalize_alias("."), None);
    }

    #[test]
    fn hostname_validation_follows_rfc_1123() {
        assert!(validate_hostname("api.openai.com").is_ok());
        assert!(validate_hostname("a-b.c").is_ok());
        assert!(validate_hostname("10.0.0.1").is_ok());
        assert!(validate_hostname("").is_err());
        assert!(validate_hostname("under_score.example").is_err());
        assert!(validate_hostname("-leading.example").is_err());
        assert!(validate_hostname("trailing-.example").is_err());
        assert!(validate_hostname("a..b").is_err());
        let long_label = "a".repeat(64);
        assert!(validate_hostname(&long_label).is_err());
        let long_host = format!("{}.example", "b".repeat(250));
        assert!(validate_hostname(&long_host).is_err());
    }

    #[test]
    fn psl_distinguishes_registrable_domains_from_public_suffixes() {
        assert!(is_registrable("vendor.com"));
        assert!(!is_registrable("co.uk"));
        assert!(is_registrable("api.openai.com"));
    }

    #[test]
    fn three_label_pools_derive_the_two_label_suffix() {
        let endpoints = [
            ep("a.us.vendor.com", 443, Scheme::Https),
            ep("b.eu.vendor.com", 443, Scheme::Https),
        ];
        assert_eq!(derive_alias(&endpoints).as_deref(), Some("vendor.com"));
    }
}
