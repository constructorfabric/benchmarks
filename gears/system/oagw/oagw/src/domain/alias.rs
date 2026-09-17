//! Alias derivation and normalization (DESIGN "Alias Resolution").
//!
//! Derivation rules:
//!
//! | Endpoints | Alias |
//! |---|---|
//! | single hostname, standard port | `hostname` |
//! | single hostname, non-standard port | `hostname:port` |
//! | multiple hostnames, PSL-validated common suffix, standard port | common suffix |
//! | multiple hostnames, common public suffix only, or no common suffix | explicit alias required |
//! | IP addresses (single or multiple) | explicit alias required |

use crate::domain::error::{OagwError, OagwResult};

/// Standard ports per scheme (`http`→80, everything else→443).
#[must_use]
pub fn standard_port(scheme: crate::domain::model::EndpointScheme, port: u16) -> bool {
    port == scheme.default_port()
}

/// Normalizes an alias or hostname: ASCII lowercase, trailing dot stripped.
#[must_use]
pub fn normalize(input: &str) -> String {
    let mut out = input.trim().to_ascii_lowercase();
    while out.ends_with('.') {
        out.pop();
    }
    out
}

/// `true` when `host` is an IPv4 or IPv6 literal.
#[must_use]
pub fn is_ip(host: &str) -> bool {
    host.parse::<std::net::IpAddr>().is_ok()
}

/// Validates an alias against the schema pattern
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$` after normalization.
#[must_use]
pub fn is_valid_alias(alias: &str) -> bool {
    let bytes = alias.as_bytes();
    if bytes.is_empty() || bytes.len() > 253 {
        return false;
    }
    let first = bytes[0];
    let last = bytes[bytes.len() - 1];
    let is_edge = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if !is_edge(first) || !is_edge(last) {
        return false;
    }
    bytes.iter().all(|&b| {
        is_edge(b) || b == b'.' || b == b':' || b == b'-'
    })
}

/// Longest common suffix shared by all hosts, in label terms.
///
/// Returns `None` when the hosts do not share at least one full label.
#[must_use]
pub fn common_suffix(hosts: &[&str]) -> Option<String> {
    let mut hosts = hosts.iter().map(|h| normalize(h)).collect::<Vec<_>>();
    hosts.sort();
    let first = hosts.first()?;
    let last = hosts.last()?;
    if first == last {
        return Some(first.clone());
    }
    let first_labels: Vec<&str> = first.split('.').rev().collect();
    let last_labels: Vec<&str> = last.split('.').collect::<Vec<_>>().into_iter().rev().collect();
    let mut shared: Vec<&str> = Vec::new();
    for (a, b) in first_labels.iter().zip(last_labels.iter()) {
        if a.eq_ignore_ascii_case(b) {
            shared.push(a);
        } else {
            break;
        }
    }
    if shared.is_empty() {
        return None;
    }
    shared.reverse();
    Some(shared.join("."))
}

/// `true` when `candidate` is a registrable domain (at least two labels and
/// not a bare public suffix), validated against the embedded PSL.
#[must_use]
pub fn is_registrable_suffix(candidate: &str) -> bool {
    let candidate = normalize(candidate);
    if candidate.contains(':') {
        return false;
    }
    let labels = candidate.split('.').count();
    if labels < 2 {
        return false;
    }
    // `psl::suffix_str` returns the longest public suffix (e.g. `co.uk`).
    // A candidate that *is* a public suffix is not registrable.
    match psl::suffix_str(&candidate) {
        Some(public) => !public.eq_ignore_ascii_case(&candidate),
        None => false,
    }
}

/// Derives the alias for a set of endpoints.
///
/// Returns `Ok(None)` when no alias can be derived (explicit alias required)
/// and `Err(_)` when a multi-host common suffix is a bare public suffix
/// (derivation rejected).
///
/// # Errors
///
/// [`crate::domain::error::OagwError::Validation`] when the endpoints carry a
/// common public suffix that is not registrable.
pub fn derive_alias(endpoints: &[crate::domain::model::Endpoint]) -> OagwResult<Option<String>> {
    if endpoints.is_empty() {
        return Ok(None);
    }

    let hosts: Vec<&str> = endpoints.iter().map(|e| e.host.as_str()).collect();
    if hosts.iter().all(|h| is_ip(h)) {
        // IP endpoints never derive an alias.
        return Ok(None);
    }

    if endpoints.len() == 1 {
        let endpoint = &endpoints[0];
        let host = normalize(&endpoint.host);
        if !is_valid_alias(&host) {
            return Ok(None);
        }
        if standard_port(endpoint.scheme, endpoint.port) {
            return Ok(Some(host));
        }
        return Ok(Some(format!("{host}:{}", endpoint.port)));
    }

    let Some(suffix) = common_suffix(&hosts) else {
        return Ok(None);
    };
    if suffix.split('.').count() < 2 {
        // `us.foo.com` + `eu.bar.com` only share the TLD: no registrable
        // suffix to derive from, so an explicit alias is required.
        return Ok(None);
    }
    if !is_registrable_suffix(&suffix) {
        // `foo.co.uk` + `bar.co.uk`: `co.uk` is a public suffix, derivation is
        // rejected rather than silently colliding across tenants.
        return Err(crate::domain::error::OagwError::Validation(format!(
            "common suffix '{suffix}' is a public suffix; provide an explicit alias"
        )));
    }

    let first_port = endpoints[0].port;
    let same_port = endpoints.iter().all(|e| e.port == first_port);
    if same_port && !standard_port(endpoints[0].scheme, first_port) {
        return Ok(Some(format!("{suffix}:{first_port}")));
    }
    Ok(Some(suffix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::EndpointScheme;

    fn ep(host: &str, port: u16) -> crate::domain::model::Endpoint {
        crate::domain::model::Endpoint {
            scheme: EndpointScheme::Https,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn single_hostname_standard_port_derives_hostname() {
        let derived = derive_alias(&[ep("api.openai.com", 443)]).unwrap();
        assert_eq!(derived.as_deref(), Some("api.openai.com"));
    }

    #[test]
    fn single_hostname_nonstandard_port_appends_port() {
        let derived = derive_alias(&[ep("api.openai.com", 8443)]).unwrap();
        assert_eq!(derived.as_deref(), Some("api.openai.com:8443"));
    }

    #[test]
    fn http_scheme_standard_port_is_80() {
        let endpoint = crate::domain::model::Endpoint {
            scheme: EndpointScheme::Http,
            host: "localhost".to_owned(),
            port: 80,
        };
        assert_eq!(
            derive_alias(std::slice::from_ref(&endpoint)).unwrap().as_deref(),
            Some("localhost")
        );
    }

    #[test]
    fn common_registrable_suffix_derives() {
        let derived = derive_alias(&[ep("us.vendor.com", 443), ep("eu.vendor.com", 443)]).unwrap();
        assert_eq!(derived.as_deref(), Some("vendor.com"));
    }

    #[test]
    fn common_suffix_with_nonstandard_port_derives_with_port() {
        let derived = derive_alias(&[ep("us.vendor.com", 8443), ep("eu.vendor.com", 8443)]).unwrap();
        assert_eq!(derived.as_deref(), Some("vendor.com:8443"));
    }

    #[test]
    fn bare_public_suffix_is_rejected() {
        let err = derive_alias(&[ep("foo.co.uk", 443), ep("bar.co.uk", 443)]).unwrap_err();
        assert!(matches!(err, crate::domain::error::OagwError::Validation(_)));
    }

    #[test]
    fn ip_endpoints_require_explicit_alias() {
        let derived = derive_alias(&[ep("10.0.1.1", 443)]).unwrap();
        assert_eq!(derived, None);
    }

    #[test]
    fn no_common_suffix_requires_explicit_alias() {
        let derived = derive_alias(&[ep("us.foo.com", 443), ep("eu.bar.com", 443)]).unwrap();
        assert_eq!(derived, None);
    }

    #[test]
    fn normalize_strips_trailing_dots() {
        assert_eq!(normalize("API.OpenAI.COM."), "api.openai.com");
    }

    #[test]
    fn valid_alias_pattern() {
        assert!(is_valid_alias("api.openai.com"));
        assert!(is_valid_alias("my-service:8443"));
        assert!(!is_valid_alias("-lead"));
        assert!(!is_valid_alias("trail-"));
        assert!(!is_valid_alias(""));
    }
}

/// Resolves the effective alias for an upstream input: the explicit alias when
/// supplied, otherwise the derived one.
///
/// # Errors
///
/// [`OagwError::Validation`] when no alias can be determined.
pub fn resolve_alias(input: &crate::domain::model::UpstreamInput) -> OagwResult<String> {
    if let Some(explicit) = &input.alias {
        return Ok(normalize(explicit));
    }
    derive_alias(&input.server.endpoints)?.ok_or_else(|| {
        OagwError::Validation(
            "alias is required: endpoints are IP-based or not derivable".to_owned(),
        )
    })
}
