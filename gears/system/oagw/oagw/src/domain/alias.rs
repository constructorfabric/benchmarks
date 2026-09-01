//! Alias derivation and validation engine.
//!
//! Alias rules (DESIGN §"Alias Resolution" + upstream.v1.schema.json):
//! * single hostname + standard port  -> hostname
//! * single hostname + non-standard port -> `hostname:port`
//! * multiple hostnames sharing a registrable common suffix (>= 2 labels,
//!   not a bare public suffix) -> `suffix[:port]`
//! * IP addresses or non-derivable pools -> explicit alias required

use std::net::IpAddr;

use crate::domain::model::Endpoint;

/// Regex-free alias pattern check (`^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`).
#[must_use]
pub fn validate_alias_chars(alias: &str) -> bool {
    if alias.is_empty() || alias.len() > 255 {
        return false;
    }
    let bytes = alias.as_bytes();
    let first = *bytes.first().unwrap_or(&0);
    let last = *bytes.last().unwrap_or(&0);
    if !first.is_ascii_alphanumeric() || !last.is_ascii_alphanumeric() {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b':' | b'-'))
}

/// Whether `host` is an IP literal (v4 or v6).
#[must_use]
pub fn is_ip(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok()
}

/// Normalize a host: ASCII lowercase, strip a single trailing dot (FQDN form).
#[must_use]
pub fn normalize_host(host: &str) -> String {
    let trimmed = if host.len() > 1 && host.ends_with('.') {
        &host[..host.len() - 1]
    } else {
        host
    };
    trimmed.to_ascii_lowercase()
}

/// Validate a hostname per RFC 1123: total length <= 253, each label 1..=63
/// chars, ASCII alphanumerics and hyphens, labels do not start/end with a
/// hyphen. IP literals are also accepted (they are not hostnames but are
/// valid endpoint hosts).
///
/// # Errors
///
/// Returns a human-readable reason when the host fails validation.
pub fn validate_host(host: &str) -> Result<(), String> {
    if host.is_empty() {
        return Err("host must not be empty".to_owned());
    }
    if host.len() > 253 {
        return Err(format!("host exceeds 253 characters: {host}"));
    }
    if is_ip(host) {
        return Ok(());
    }
    for label in host.split('.') {
        if label.is_empty() {
            return Err(format!("host contains an empty label: {host}"));
        }
        if label.len() > 63 {
            return Err(format!("label exceeds 63 characters: {label}"));
        }
        let is_valid = label.bytes().enumerate().all(|(idx, b)| {
            b.is_ascii_alphanumeric() || (b == b'-' && idx != 0 && idx != label.len() - 1)
        });
        if !is_valid {
            return Err(format!("invalid hostname label: {label}"));
        }
        let first = *label.as_bytes().first().unwrap_or(&0);
        let last = *label.as_bytes().last().unwrap_or(&0);
        if first == b'-' || last == b'-' {
            return Err(format!(
                "label must not start or end with a hyphen: {label}"
            ));
        }
    }
    Ok(())
}

/// Whether a port is "standard" (omitted from derived aliases).
#[must_use]
pub fn is_standard_port(endpoint: &Endpoint) -> bool {
    endpoint.effective_port() == endpoint.scheme.default_port()
}

/// Derive the alias for a single endpoint pool.
///
/// Returns `None` when the pool is non-derivable (IP endpoints, or hostname
/// pools whose only common suffix is a bare public suffix or shorter than two
/// labels) — in that case the operator must supply an explicit alias.
#[must_use]
pub fn derive_alias(endpoints: &[Endpoint]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }
    let hosts: Vec<String> = endpoints.iter().map(|e| normalize_host(&e.host)).collect();
    if hosts.iter().any(|h| is_ip(h)) {
        return None;
    }
    let ports: Vec<u16> = endpoints.iter().map(Endpoint::effective_port).collect();
    if endpoints.len() == 1 {
        let host = &hosts[0];
        return Some(if is_standard_port(&endpoints[0]) {
            host.clone()
        } else {
            format!("{host}:{}", ports[0])
        });
    }
    // Multi-endpoint pool: common label suffix, >= 2 labels, registrable.
    let suffix = common_label_suffix(&hosts)?;
    if psl::domain_str(&suffix) != Some(suffix.as_str()) {
        // `suffix` is itself the registrable domain; when the PSL derives a
        // shorter registrable domain the suffix is a bare public suffix.
        return None;
    }
    let suffix_label_count = suffix.split('.').count();
    if suffix_label_count < 2 {
        return None;
    }
    // Pools must share one port (enforced during validation); include it only
    // when non-standard.
    let port_part = match ports.first() {
        Some(p) if *p != 443 => format!(":{p}"),
        _ => String::new(),
    };
    Some(format!("{suffix}{port_part}"))
}

/// Longest common suffix (by dot-separated labels) shared by all hosts.
#[must_use]
fn common_label_suffix(hosts: &[String]) -> Option<String> {
    let label_sets: Vec<Vec<&str>> = hosts
        .iter()
        .map(|h| h.split('.').collect::<Vec<&str>>())
        .collect();
    let shortest = label_sets.iter().map(Vec::len).min()?;
    let mut common: Vec<&str> = Vec::new();
    for offset in 1..=shortest {
        let candidate = label_sets[0].get(label_sets[0].len() - offset)?;
        let all_match = label_sets[1..]
            .iter()
            .all(|labels| labels.get(labels.len() - offset) == Some(candidate));
        if !all_match {
            break;
        }
        common.push(candidate);
    }
    if common.is_empty() {
        return None;
    }
    common.reverse();
    let joined = common.join(".");
    if joined.len() > 253 {
        return None;
    }
    Some(joined)
}

/// Normalize a user-supplied alias (lowercase, trailing dot stripped) and
/// reject characters outside the documented pattern.
///
/// # Errors
///
/// Returns a reason when the alias contains invalid characters.
pub fn normalize_alias(alias: &str) -> Result<String, String> {
    let normalized = {
        let trimmed = if alias.len() > 1 && alias.ends_with('.') {
            &alias[..alias.len() - 1]
        } else {
            alias
        };
        trimmed.to_ascii_lowercase()
    };
    if !validate_alias_chars(&normalized) {
        return Err(format!(
            "alias must match [a-z0-9]([a-z0-9.:-]*[a-z0-9])? (got {alias:?})"
        ));
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, EndpointScheme};

    fn ep(scheme: EndpointScheme, host: &str, port: Option<u16>) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn single_hostname_standard_port_derives_host() {
        let endpoints = vec![ep(EndpointScheme::Https, "api.openai.com", Some(443))];
        assert_eq!(derive_alias(&endpoints).as_deref(), Some("api.openai.com"));
    }

    #[test]
    fn single_hostname_without_explicit_port_uses_default() {
        let endpoints = vec![ep(EndpointScheme::Https, "api.openai.com", None)];
        assert_eq!(derive_alias(&endpoints).as_deref(), Some("api.openai.com"));
    }

    #[test]
    fn single_hostname_nonstandard_port_includes_port() {
        let endpoints = vec![ep(EndpointScheme::Https, "api.openai.com", Some(8443))];
        assert_eq!(
            derive_alias(&endpoints).as_deref(),
            Some("api.openai.com:8443")
        );
    }

    #[test]
    fn multi_hostname_registrable_suffix_derives() {
        let endpoints = vec![
            ep(EndpointScheme::Https, "us.vendor.com", Some(443)),
            ep(EndpointScheme::Https, "eu.vendor.com", Some(443)),
        ];
        assert_eq!(derive_alias(&endpoints).as_deref(), Some("vendor.com"));
    }

    #[test]
    fn multi_hostname_bare_public_suffix_does_not_derive() {
        let endpoints = vec![
            ep(EndpointScheme::Https, "foo.co.uk", Some(443)),
            ep(EndpointScheme::Https, "bar.co.uk", Some(443)),
        ];
        assert_eq!(derive_alias(&endpoints), None);
    }

    #[test]
    fn multi_hostname_heterogeneous_does_not_derive() {
        let endpoints = vec![
            ep(EndpointScheme::Https, "us.foo.com", Some(443)),
            ep(EndpointScheme::Https, "eu.bar.com", Some(443)),
        ];
        assert_eq!(derive_alias(&endpoints), None);
    }

    #[test]
    fn ip_never_derives() {
        assert_eq!(
            derive_alias(&[ep(EndpointScheme::Https, "10.0.1.1", Some(443))]),
            None
        );
        assert_eq!(
            derive_alias(&[
                ep(EndpointScheme::Https, "10.0.1.1", Some(443)),
                ep(EndpointScheme::Https, "10.0.1.2", Some(443)),
            ]),
            None
        );
    }

    #[test]
    fn hostname_validation() {
        assert!(validate_host("api.openai.com").is_ok());
        assert!(validate_host("10.0.1.1").is_ok());
        assert!(validate_host("2001:db8::1").is_ok());
        assert!(validate_host("-bad.example.com").is_err());
        assert!(validate_host("bad-.example.com").is_err());
        assert!(validate_host("a_b.example.com").is_err());
    }

    #[test]
    fn alias_normalization() {
        assert_eq!(normalize_alias("Api.OpenAI.COM").unwrap(), "api.openai.com");
        assert_eq!(
            normalize_alias("api.openai.com.").unwrap(),
            "api.openai.com"
        );
        assert!(normalize_alias("bad alias!").is_err());
        assert!(validate_alias_chars("my-service:8443"));
    }
}
