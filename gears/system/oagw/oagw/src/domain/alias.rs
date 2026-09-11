//! Alias derivation and hostname validation (DESIGN §3.2 "Alias Resolution").

use crate::domain::model::{Endpoint, Scheme};

/// Labels that are omitted from a derived alias.
///
/// HTTP: 80; HTTPS / WSS / WebTransport / gRPC: 443.
#[must_use]
pub fn is_standard_port(scheme: Scheme, port: u16) -> bool {
    match scheme {
        Scheme::Http => port == 80,
        Scheme::Https | Scheme::Wss | Scheme::Wt | Scheme::Grpc => port == 443,
    }
}

/// Normalize a host the way aliases are normalized: ASCII lowercase with any
/// trailing dot (FQDN notation) stripped.
#[must_use]
pub fn normalize(host: &str) -> String {
    let lower = host.trim().to_ascii_lowercase();
    lower.strip_suffix('.').unwrap_or(&lower).to_owned()
}

/// Validate a hostname per RFC 1123 as DESIGN §3.2 requires.
///
/// # Errors
///
/// Returns the reason the host is not acceptable.
pub fn validate_hostname(host: &str) -> Result<(), String> {
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() {
        return Err("must not be empty".to_owned());
    }
    if host.len() > 253 {
        return Err("exceeds the 253 character limit".to_owned());
    }
    for label in host.split('.') {
        if label.is_empty() {
            return Err("contains an empty label".to_owned());
        }
        if label.len() > 63 {
            return Err("contains a label longer than 63 characters".to_owned());
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err("contains a label that starts or ends with a hyphen".to_owned());
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err("contains characters outside [a-zA-Z0-9-]".to_owned());
        }
    }
    Ok(())
}

/// `true` when the host is an IPv4 or IPv6 literal.
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    let candidate = host.strip_suffix('.').unwrap_or(host);
    let candidate = candidate.trim_start_matches('[').trim_end_matches(']');
    candidate.parse::<std::net::IpAddr>().is_ok()
}

/// `true` when `candidate` is a bare public suffix such as `co.uk`.
fn is_public_suffix(candidate: &str) -> bool {
    psl::domain_str(candidate).is_none()
}

/// Derive the alias an endpoint set implies, or `None` when the endpoints do
/// not determine one and an explicit alias is required.
///
/// * one hostname endpoint → the hostname (plus `:port` when non-standard)
/// * several hostname endpoints → the longest common label suffix that is a
///   registrable domain (never a bare public suffix), plus `:port`
/// * anything containing an IP literal, or with no common suffix → `None`
#[must_use]
pub fn derive(endpoints: &[Endpoint]) -> Option<String> {
    let first = endpoints.first()?;
    if endpoints
        .iter()
        .any(|endpoint| is_ip_literal(&endpoint.host))
    {
        return None;
    }

    let port_suffix = if is_standard_port(first.scheme, first.port) {
        String::new()
    } else {
        format!(":{}", first.port)
    };

    let normalized: Vec<String> = endpoints
        .iter()
        .map(|endpoint| normalize(&endpoint.host))
        .collect();
    if normalized.len() == 1 {
        return Some(format!("{}{port_suffix}", normalized[0]));
    }

    let common = common_domain_suffix(&normalized)?;
    Some(format!("{common}{port_suffix}"))
}

/// Whether `alias` was derived from the common suffix of a multi-endpoint
/// pool — the configuration in which `X-OAGW-Target-Host` is mandatory.
///
/// A pool whose endpoints have no registrable common suffix (or that is a bare
/// public suffix) carries an *explicit* alias, and the header stays optional.
#[must_use]
pub fn needs_target_host(endpoints: &[Endpoint], alias: &str) -> bool {
    if endpoints.len() < 2 {
        return false;
    }
    let hosts: Vec<String> = endpoints
        .iter()
        .map(|endpoint| normalize(&endpoint.host))
        .collect();
    let Some(suffix) = common_domain_suffix(&hosts) else {
        return false;
    };
    let port_suffix = if is_standard_port(endpoints[0].scheme, endpoints[0].port) {
        String::new()
    } else {
        format!(":{}", endpoints[0].port)
    };
    alias.eq_ignore_ascii_case(&format!("{suffix}{port_suffix}"))
}

/// Longest common label suffix of the given normalized hosts that is a
/// registrable domain (≥ 2 labels, not a bare public suffix).
#[must_use]
pub(crate) fn common_domain_suffix(hosts: &[String]) -> Option<String> {
    let min_labels = hosts.iter().map(|host| host.split('.').count()).min()?;
    let first: Vec<&str> = hosts[0].split('.').collect();

    for take in (2..=min_labels).rev() {
        let candidate_labels = &first[first.len() - take..];
        let candidate = candidate_labels.join(".");
        let matches = hosts.iter().all(|host| {
            let labels: Vec<&str> = host.split('.').collect();
            labels[labels.len() - take..] == *candidate_labels
        });
        if matches && !is_public_suffix(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(scheme: Scheme, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn single_hostname_standard_port_derives_bare_hostname() {
        let endpoints = vec![endpoint(Scheme::Https, "api.openai.com", 443)];
        assert_eq!(derive(&endpoints).as_deref(), Some("api.openai.com"));
    }

    #[test]
    fn single_hostname_non_standard_port_appends_port() {
        let endpoints = vec![endpoint(Scheme::Https, "api.openai.com", 8443)];
        assert_eq!(derive(&endpoints).as_deref(), Some("api.openai.com:8443"));
    }

    #[test]
    fn http_standard_port_is_80() {
        let endpoints = vec![endpoint(Scheme::Http, "example.test", 80)];
        assert_eq!(derive(&endpoints).as_deref(), Some("example.test"));
        let endpoints = vec![endpoint(Scheme::Http, "example.test", 8080)];
        assert_eq!(derive(&endpoints).as_deref(), Some("example.test:8080"));
    }

    #[test]
    fn multi_endpoint_pools_derive_common_suffix() {
        let endpoints = vec![
            endpoint(Scheme::Https, "us.vendor.com", 443),
            endpoint(Scheme::Https, "eu.vendor.com", 443),
        ];
        assert_eq!(derive(&endpoints).as_deref(), Some("vendor.com"));
    }

    #[test]
    fn multi_endpoint_non_standard_port_keeps_port_in_alias() {
        let endpoints = vec![
            endpoint(Scheme::Https, "us.vendor.com", 8443),
            endpoint(Scheme::Https, "eu.vendor.com", 8443),
        ];
        assert_eq!(derive(&endpoints).as_deref(), Some("vendor.com:8443"));
    }

    #[test]
    fn bare_public_suffix_is_not_derivable() {
        let endpoints = vec![
            endpoint(Scheme::Https, "foo.co.uk", 443),
            endpoint(Scheme::Https, "bar.co.uk", 443),
        ];
        assert_eq!(derive(&endpoints), None);
    }

    #[test]
    fn heterogeneous_hosts_are_not_derivable() {
        let endpoints = vec![
            endpoint(Scheme::Https, "us.foo.com", 443),
            endpoint(Scheme::Https, "eu.bar.com", 443),
        ];
        assert_eq!(derive(&endpoints), None);
    }

    #[test]
    fn ip_endpoints_require_explicit_alias() {
        let endpoints = vec![
            endpoint(Scheme::Https, "10.0.1.1", 443),
            endpoint(Scheme::Https, "10.0.1.2", 443),
        ];
        assert_eq!(derive(&endpoints), None);
    }

    #[test]
    fn single_ip_endpoint_requires_explicit_alias() {
        let endpoints = vec![endpoint(Scheme::Https, "10.0.1.1", 443)];
        assert_eq!(derive(&endpoints), None);
    }

    #[test]
    fn normalizes_case_and_trailing_dot() {
        assert_eq!(normalize("Api.OpenAI.COM."), "api.openai.com");
    }

    #[test]
    fn rejects_malformed_hostnames() {
        assert!(validate_hostname("").is_err());
        assert!(validate_hostname("-bad.example.com").is_err());
        assert!(validate_hostname("bad..example.com").is_err());
        assert!(validate_hostname("bad_.example.com").is_err());
        assert!(validate_hostname(&"a".repeat(300)).is_err());
        assert!(validate_hostname("api.openai.com").is_ok());
        assert!(validate_hostname("api.openai.com.").is_ok());
    }

    #[test]
    fn detects_ip_literals_including_ipv6() {
        assert!(is_ip_literal("10.0.1.1"));
        assert!(is_ip_literal("::1"));
        assert!(is_ip_literal("[::1]"));
        assert!(!is_ip_literal("api.openai.com"));
    }
}
