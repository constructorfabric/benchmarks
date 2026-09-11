//! Alias derivation, normalization and validation.
//!
//! The alias is the routing key in `/oagw/v1/proxy/{alias}/...`, so it is
//! derived from the endpoint pool when possible and validated as an RFC 1123
//! hostname. IP endpoints and pools with no common domain suffix require the
//! operator to supply one.

use crate::domain::error::DomainError;
use crate::domain::model::Endpoint;

/// A validated alias: lowercase host, optionally `host:port`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alias {
    value: String,
}

impl Alias {
    /// The normalized alias string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Consume the alias into its string form.
    #[must_use]
    pub fn into_string(self) -> String {
        self.value
    }
}

impl std::fmt::Display for Alias {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.value)
    }
}

/// Normalize an alias candidate: trim, strip trailing dots, ASCII lowercase.
#[must_use]
pub fn normalize(raw: &str) -> String {
    raw.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Validate an alias against RFC 1123 host labels, with an optional port.
///
/// # Errors
/// Returns [`ErrorKind::Validation`] when the alias is not a valid host label
/// sequence, or its port is outside 1..=65535.
pub fn validate_rfc1123(candidate: &str) -> Result<(), DomainError> {
    let (host, port) = split_host_port(candidate);
    if host.is_empty() {
        return Err(DomainError::validation("alias must not be empty"));
    }
    if host.len() > 253 {
        return Err(DomainError::validation("alias exceeds 253 characters"));
    }
    for label in host.split('.') {
        validate_label(label)?;
    }
    if let Some(port) = port
        && !(1..=65_535).contains(&port)
    {
        return Err(DomainError::validation(format!(
            "alias port {port} is outside 1..=65535"
        )));
    }
    Ok(())
}

fn validate_label(label: &str) -> Result<(), DomainError> {
    if label.is_empty() || label.len() > 63 {
        return Err(DomainError::validation(format!(
            "alias label '{label}' must be 1..=63 characters"
        )));
    }
    if label.starts_with('-') || label.ends_with('-') {
        return Err(DomainError::validation(format!(
            "alias label '{label}' must not start or end with a hyphen"
        )));
    }
    if !label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(DomainError::validation(format!(
            "alias label '{label}' contains characters outside RFC 1123"
        )));
    }
    Ok(())
}

/// Split `host:port`, leaving bracketed IPv6 intact.
#[must_use]
pub fn split_host_port(candidate: &str) -> (String, Option<u16>) {
    let candidate = normalize(candidate);
    if candidate.starts_with('[')
        && let Some(end) = candidate.find(']')
    {
        let host = candidate[1..end].to_owned();
        let port = candidate[end + 1..]
            .strip_prefix(':')
            .and_then(|p| p.parse::<u16>().ok());
        return (host, port);
    }
    match candidate.rsplit_once(':') {
        Some((host, port)) => match port.parse::<u16>() {
            Ok(port) => (host.to_owned(), Some(port)),
            Err(_) => (candidate, None),
        },
        None => (candidate, None),
    }
}

/// Build a validated alias from a candidate string.
///
/// # Errors
/// Returns [`ErrorKind::Validation`] when the candidate is not a valid host.
pub fn from_string(candidate: &str) -> Result<Alias, DomainError> {
    let value = normalize(candidate);
    validate_rfc1123(&value)?;
    Ok(Alias { value })
}

/// Derive an alias from the endpoint pool.
///
/// Returns `None` when the pool cannot be summarized — the operator must then
/// supply an explicit alias.
///
/// | endpoints | result |
/// |---|---|
/// | single hostname | `host` |
/// | single host, non-standard port | `host:port` |
/// | pool of hosts sharing a registrable suffix | the suffix |
/// | pool with a shared suffix and a shared non-standard port | `suffix:port` |
/// | pool with no common suffix, or any IP endpoint | `None` |
///
/// # Errors
/// Returns [`ErrorKind::Validation`] when the derived candidate is not a valid
/// host, including when it is a bare public suffix such as `co.uk`.
pub fn derive(endpoints: &[Endpoint]) -> Result<Option<Alias>, DomainError> {
    if endpoints.is_empty() {
        return Err(DomainError::validation(
            "at least one endpoint is required to derive an alias",
        ));
    }
    if endpoints.iter().any(Endpoint::is_ip) {
        return Ok(None);
    }

    let mut hosts: Vec<String> = endpoints.iter().map(|e| normalize(&e.host)).collect();
    hosts.sort();
    hosts.dedup();

    let alias = if hosts.len() == 1 {
        hosts[0].clone()
    } else {
        match common_domain_suffix(&hosts) {
            Some(suffix) => suffix,
            None => return Ok(None),
        }
    };

    // Preserve a port only when every endpoint carries the same explicit
    // non-standard port; a mixed pool cannot be addressed by one authority.
    let explicit_ports: Vec<Option<u16>> = endpoints.iter().map(|e| e.port).collect();
    let all_same = explicit_ports.windows(2).all(|w| w[0] == w[1]);
    let port = if all_same { explicit_ports[0] } else { None };

    let candidate = match port {
        Some(p) if endpoints.iter().any(|e| e.scheme.default_port() != p) => {
            format!("{alias}:{p}")
        }
        _ => alias,
    };

    if is_bare_public_suffix(&candidate) {
        return Err(DomainError::validation(format!(
            "derived alias '{candidate}' is a bare public suffix; supply an explicit alias"
        )));
    }
    from_string(&candidate).map(Some)
}

/// Whether `candidate` is a bare public suffix such as `co.uk`.
#[must_use]
pub fn is_bare_public_suffix(candidate: &str) -> bool {
    let (host, _) = split_host_port(candidate);
    !host.is_empty() && !host.contains(':') && psl::domain(host.as_bytes()).is_none()
}

/// Longest common registrable domain suffix shared by every host.
///
/// Returns `None` when the hosts share nothing below their public suffix, so
/// that `a.partner.com` and `b.other.com` do not derive an alias.
#[must_use]
pub fn common_domain_suffix(hosts: &[String]) -> Option<String> {
    let labels: Vec<Vec<&str>> = hosts
        .iter()
        .map(|h| h.trim_end_matches('.').split('.').rev().collect())
        .collect();
    let shortest = labels.iter().map(Vec::len).min()?;
    let mut common: Vec<&str> = Vec::new();
    for index in 0..shortest {
        let first = labels[0][index];
        if labels.iter().all(|l| l[index] == first) {
            common.push(first);
        } else {
            break;
        }
    }
    if common.len() < 2 {
        return None;
    }
    let joined: Vec<&str> = common.iter().rev().copied().collect();
    // A bare public suffix is reported back so the caller can reject it: `co.uk`
    // alone is not addressable, and silently returning `None` would hide why.
    Some(joined.join("."))
}

#[cfg(test)]
mod alias_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn endpoint(host: &str, port: Option<u16>) -> Endpoint {
        Endpoint {
            scheme: crate::domain::model::Scheme::Https,
            host: host.to_owned(),
            port,
            ..Endpoint::default()
        }
    }

    #[test]
    fn single_hostname_derives_its_host() {
        let alias = derive(&[endpoint("api.partner.com", None)])
            .unwrap()
            .expect("derivable");
        assert_eq!(alias.as_str(), "api.partner.com");
    }

    #[test]
    fn single_host_with_nonstandard_port_appends_the_port() {
        let alias = derive(&[endpoint("api.partner.com", Some(8443))])
            .unwrap()
            .expect("derivable");
        assert_eq!(alias.as_str(), "api.partner.com:8443");
    }

    #[test]
    fn single_host_with_standard_port_omits_the_port() {
        let alias = derive(&[endpoint("api.partner.com", Some(443))])
            .unwrap()
            .expect("derivable");
        assert_eq!(alias.as_str(), "api.partner.com");
    }

    #[test]
    fn common_suffix_pool_derives_the_suffix() {
        let alias = derive(&[
            endpoint("a.partner.com", None),
            endpoint("b.partner.com", None),
        ])
        .unwrap()
        .expect("derivable");
        assert_eq!(alias.as_str(), "partner.com");
    }

    #[test]
    fn common_suffix_pool_preserves_a_shared_port() {
        let alias = derive(&[
            endpoint("a.partner.com", Some(8443)),
            endpoint("b.partner.com", Some(8443)),
        ])
        .unwrap()
        .expect("derivable");
        assert_eq!(alias.as_str(), "partner.com:8443");
    }

    #[test]
    fn unrelated_hosts_require_an_explicit_alias() {
        let derived = derive(&[
            endpoint("a.partner.com", None),
            endpoint("b.other.com", None),
        ])
        .unwrap();
        assert!(derived.is_none(), "no common suffix means no derivation");
    }

    #[test]
    fn ip_endpoints_require_an_explicit_alias() {
        let derived = derive(&[endpoint("127.0.0.1", None)]).unwrap();
        assert!(derived.is_none());
        let derived = derive(&[endpoint("10.0.0.1", None), endpoint("10.0.0.2", None)]).unwrap();
        assert!(derived.is_none());
    }

    #[test]
    fn bare_public_suffixes_are_rejected() {
        let err = derive(&[endpoint("a.co.uk", None), endpoint("b.co.uk", None)]).unwrap_err();
        assert_eq!(err.kind(), crate::domain::error::ErrorKind::Validation);
    }

    #[test]
    fn normalization_lowercases_and_strips_trailing_dots() {
        assert_eq!(normalize("API.Partner.COM."), "api.partner.com");
        assert_eq!(normalize("  LocalHost  "), "localhost");
        let alias = from_string("API.Partner.COM.").unwrap();
        assert_eq!(alias.as_str(), "api.partner.com");
    }

    #[test]
    fn rfc1123_labels_are_enforced() {
        assert!(validate_rfc1123("api.partner.com").is_ok());
        assert!(validate_rfc1123("localhost").is_ok());
        assert!(validate_rfc1123("my-host_1.partner.com").is_ok());
        assert!(validate_rfc1123("").is_err(), "empty");
        assert!(
            validate_rfc1123("-leading.partner.com").is_err(),
            "leading hyphen"
        );
        assert!(
            validate_rfc1123("trailing-.partner.com").is_err(),
            "trailing hyphen"
        );
        assert!(validate_rfc1123("bad host.partner.com").is_err(), "space");
        assert!(validate_rfc1123("a/?.com").is_err(), "special characters");
        let long = "x".repeat(64);
        assert!(validate_rfc1123(&long).is_err(), "label over 63 characters");
    }

    #[test]
    fn ports_outside_range_are_rejected() {
        assert!(validate_rfc1123("api.partner.com:70000").is_err());
        assert!(validate_rfc1123("api.partner.com:0").is_err());
        assert!(validate_rfc1123("api.partner.com:8443").is_ok());
    }

    #[test]
    fn bracketed_ipv6_is_handled() {
        let (host, port) = split_host_port("[fe80::1]:8443");
        assert_eq!(host, "fe80::1");
        assert_eq!(port, Some(8443));
    }

    #[test]
    fn empty_endpoint_pool_is_a_validation_error() {
        let err = derive(&[]).unwrap_err();
        assert_eq!(err.kind(), crate::domain::error::ErrorKind::Validation);
    }

    #[test]
    fn http_scheme_port_is_respected() {
        let mut e = endpoint("api.partner.com", Some(80));
        e.scheme = crate::domain::model::Scheme::Http;
        let alias = derive(&[e]).unwrap().expect("derivable");
        assert_eq!(alias.as_str(), "api.partner.com");
    }
}
