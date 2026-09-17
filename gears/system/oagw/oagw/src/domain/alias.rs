//! Alias derivation, normalization and update enforcement.
//!
//! The alias is the routing key in `/oagw/v1/proxy/{alias}/...` and is
//! therefore immutable once set. Derivation is PSL-aware: a shared suffix only
//! counts as a registrable domain when it has at least two labels and is not a
//! bare public suffix (`foo.co.uk` + `bar.co.uk` is *not* derivable).

use super::error::{DomainError, ErrorKind};
use super::model::Endpoint;

/// Longest hostname accepted by RFC 1123 (excluding a trailing dot).
pub const MAX_HOSTNAME_LEN: usize = 253;
/// Longest single DNS label.
pub const MAX_LABEL_LEN: usize = 63;

/// Normalizes an alias to its canonical comparison form: ASCII lowercase,
/// trailing dot stripped, surrounding whitespace trimmed.
#[must_use]
pub fn normalize_alias(value: &str) -> String {
    let trimmed = value.trim();
    let no_trailing_dot = trimmed.strip_suffix('.').unwrap_or(trimmed);
    no_trailing_dot.to_ascii_lowercase()
}

/// `true` when `host` is an IPv4 or IPv6 literal.
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    let candidate = host.trim().trim_start_matches('[').trim_end_matches(']');
    if candidate.contains(':') {
        return candidate.parse::<std::net::Ipv6Addr>().is_ok();
    }
    candidate.parse::<std::net::Ipv4Addr>().is_ok()
}

/// Validates a hostname per RFC 1123 (IPv4/IPv6 literals are accepted as-is).
///
/// # Errors
///
/// Returns a 400 `ValidationError` describing the first rule the host breaks.
pub fn validate_hostname(host: &str) -> Result<(), DomainError> {
    let trimmed = host.trim().trim_end_matches('.');
    if trimmed.is_empty() {
        return Err(DomainError::new(
            ErrorKind::Validation,
            "host must not be empty",
        ));
    }
    if is_ip_literal(trimmed) {
        return Ok(());
    }
    if trimmed.len() > MAX_HOSTNAME_LEN {
        return Err(DomainError::new(
            ErrorKind::Validation,
            format!("host exceeds {MAX_HOSTNAME_LEN} characters"),
        ));
    }
    for label in trimmed.split('.') {
        validate_label(label, trimmed)?;
    }
    Ok(())
}

fn validate_label(label: &str, whole: &str) -> Result<(), DomainError> {
    if label.is_empty() || label.len() > MAX_LABEL_LEN {
        return Err(DomainError::new(
            ErrorKind::Validation,
            format!("invalid DNS label length in host '{whole}'"),
        ));
    }
    if !label
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(DomainError::new(
            ErrorKind::Validation,
            format!("host '{whole}' contains characters outside RFC 1123 label set"),
        ));
    }
    if label.starts_with('-') || label.ends_with('-') {
        return Err(DomainError::new(
            ErrorKind::Validation,
            format!("label in host '{whole}' starts or ends with a hyphen"),
        ));
    }
    Ok(())
}

/// Derives the routing alias from the endpoint pool.
///
/// Returns `None` when the pool is not derivable: IP literals, heterogeneous
/// hostnames with no common suffix, hostname pools whose only common suffix is
/// a bare public suffix, or a pool whose endpoints disagree on scheme/port.
#[must_use]
pub fn derive_alias(endpoints: &[Endpoint]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }
    if !pool_is_homogeneous(endpoints) {
        return None;
    }
    let aliases: Vec<String> = endpoints.iter().map(Endpoint::alias_host).collect();

    if aliases.len() == 1 {
        let host = &endpoints[0].host;
        return if is_ip_literal(host) {
            None
        } else {
            Some(normalize_alias(&aliases[0]))
        };
    }

    let distinct: std::collections::BTreeSet<&String> = aliases.iter().collect();
    if distinct.len() == 1 {
        let host = &endpoints[0].host;
        return if is_ip_literal(host) {
            None
        } else {
            Some(normalize_alias(&aliases[0]))
        };
    }

    // Multi-host pool: every endpoint must be a hostname.
    if endpoints.iter().any(|e| is_ip_literal(&e.host)) {
        return None;
    }

    let port = endpoints[0].effective_port();
    let standard = port == endpoints[0].scheme.default_port();
    let suffix = common_suffix(&aliases.iter().map(String::as_str).collect::<Vec<_>>())?;
    if psl::domain_str(&suffix) != Some(suffix.as_str()) {
        // Bare public suffix (e.g. `co.uk`) — not registrable.
        return None;
    }
    Some(if standard {
        suffix
    } else {
        format!("{suffix}:{port}")
    })
}

fn pool_is_homogeneous(endpoints: &[Endpoint]) -> bool {
    let first = &endpoints[0];
    endpoints
        .iter()
        .all(|e| e.scheme == first.scheme && e.effective_port() == first.effective_port())
}

/// Longest label-wise common suffix across the values (without ports).
fn common_suffix(values: &[&str]) -> Option<String> {
    let label_sets: Vec<Vec<&str>> = values
        .iter()
        .map(|v| {
            v.rsplit_once(':')
                .map_or(*v, |(host, _)| host)
                .split('.')
                .rev()
                .collect()
        })
        .collect();
    let min_len = label_sets.iter().map(Vec::len).min()?;
    let mut shared: Vec<&str> = Vec::new();
    for idx in 0..min_len {
        let candidate = label_sets[0][idx];
        if !label_sets.iter().all(|labels| labels[idx] == candidate) {
            break;
        }
        shared.push(candidate);
    }
    if shared.len() < 2 {
        return None;
    }
    shared.reverse();
    Some(shared.join("."))
}

/// Outcome of an alias update attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasUpdate {
    /// Keep the existing alias.
    Retain,
}

/// Enforces the alias update transition table (DESIGN "Alias Update Behavior").
///
/// # Errors
///
/// Returns a 400 `ValidationError` when the transition would change the alias.
pub fn enforce_alias_update(
    current_endpoints: &[Endpoint],
    current_alias: &str,
    new_endpoints: &[Endpoint],
    requested_alias: Option<&str>,
) -> Result<AliasUpdate, DomainError> {
    let reject = |reason: String| Err(DomainError::new(ErrorKind::Validation, reason));

    if new_endpoints.is_empty() {
        return reject("at least one endpoint is required".to_owned());
    }
    if let Some(requested) = requested_alias {
        let normalized = normalize_alias(requested);
        if normalized != current_alias {
            return reject(format!(
                "alias '{normalized}' would change the routing key; the alias is immutable (current: '{current_alias}') — delete and re-create the upstream"
            ));
        }
    }

    let endpoints_unchanged = current_endpoints == new_endpoints;
    if endpoints_unchanged {
        return Ok(AliasUpdate::Retain);
    }

    let current_derived = derive_alias(current_endpoints);
    let new_derived = derive_alias(new_endpoints);

    match (current_derived, new_derived) {
        (_, Some(derived)) => {
            if normalize_alias(&derived) == normalize_alias(current_alias) {
                Ok(AliasUpdate::Retain)
            } else {
                reject(format!(
                    "endpoint change would alter the derived alias from '{current_alias}' to '{derived}' — delete and re-create the upstream"
                ))
            }
        }
        (Some(_), None) => reject(
            "transitioning a derivable upstream to non-derivable endpoints is rejected — delete and re-create the upstream"
                .to_owned(),
        ),
        (None, None) => Ok(AliasUpdate::Retain),
    }
}

/// Validates a user-supplied alias value against the wire pattern
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
///
/// # Errors
///
/// Returns a 400 `ValidationError` when the value does not match.
pub fn validate_alias_shape(alias: &str) -> Result<(), DomainError> {
    let first_invalid = alias.chars().find(|c| {
        !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '.' || *c == ':' || *c == '-')
    });
    if let Some(bad) = first_invalid {
        return Err(DomainError::new(
            ErrorKind::Validation,
            format!("alias contains invalid character '{bad}'"),
        ));
    }
    let starts_ok = alias
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let ends_ok = alias
        .chars()
        .next_back()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !starts_ok || !ends_ok {
        return Err(DomainError::new(
            ErrorKind::Validation,
            "alias must start and end with an ASCII letter or digit",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::super::model::EndpointScheme;
    use super::*;

    fn ep(scheme: EndpointScheme, host: &str, port: Option<u16>) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn normalizes_case_and_trailing_dots() {
        assert_eq!(normalize_alias("Api.OpenAI.COM."), "api.openai.com");
        assert_eq!(normalize_alias("  VENDOR.com "), "vendor.com");
    }

    #[test]
    fn single_hostname_derives_alias() {
        let endpoints = [ep(EndpointScheme::Https, "api.openai.com", Some(443))];
        assert_eq!(derive_alias(&endpoints).as_deref(), Some("api.openai.com"));

        let custom = [ep(EndpointScheme::Https, "api.openai.com", Some(8443))];
        assert_eq!(
            derive_alias(&custom).as_deref(),
            Some("api.openai.com:8443")
        );

        let plain_http = [ep(EndpointScheme::Http, "upstream", Some(80))];
        assert_eq!(derive_alias(&plain_http).as_deref(), Some("upstream"));
    }

    #[test]
    fn ip_literals_are_not_derivable() {
        let single = [ep(EndpointScheme::Https, "10.0.1.1", None)];
        assert_eq!(derive_alias(&single), None);

        let pool = [
            ep(EndpointScheme::Https, "10.0.1.1", None),
            ep(EndpointScheme::Https, "10.0.1.2", None),
        ];
        assert_eq!(derive_alias(&pool), None);
    }

    #[test]
    fn common_suffix_requires_registrable_domain() {
        let good = [
            ep(EndpointScheme::Https, "us.vendor.com", None),
            ep(EndpointScheme::Https, "eu.vendor.com", None),
        ];
        assert_eq!(derive_alias(&good).as_deref(), Some("vendor.com"));

        let bare_public_suffix = [
            ep(EndpointScheme::Https, "foo.co.uk", None),
            ep(EndpointScheme::Https, "bar.co.uk", None),
        ];
        assert_eq!(derive_alias(&bare_public_suffix), None);

        let unrelated = [
            ep(EndpointScheme::Https, "us.foo.com", None),
            ep(EndpointScheme::Https, "eu.bar.com", None),
        ];
        assert_eq!(derive_alias(&unrelated), None);
    }

    #[test]
    fn common_suffix_preserves_nonstandard_port() {
        let pool = [
            ep(EndpointScheme::Https, "us.vendor.com", Some(8443)),
            ep(EndpointScheme::Https, "eu.vendor.com", Some(8443)),
        ];
        assert_eq!(derive_alias(&pool).as_deref(), Some("vendor.com:8443"));
    }

    #[test]
    fn heterogeneous_pool_requires_explicit_alias() {
        let mixed_scheme = [
            ep(EndpointScheme::Https, "a.vendor.com", None),
            ep(EndpointScheme::Http, "b.vendor.com", None),
        ];
        assert_eq!(derive_alias(&mixed_scheme), None);
    }

    #[test]
    fn rfc1123_validation_rejects_bad_labels() {
        assert!(validate_hostname("api.openai.com").is_ok());
        assert!(validate_hostname("api.openai.com.").is_ok());
        assert!(validate_hostname("-bad.example").is_err());
        assert!(validate_hostname("bad-.example").is_err());
        assert!(validate_hostname("ba d.example").is_err());
        assert!(validate_hostname("").is_err());
        assert!(validate_hostname("10.0.0.1").is_ok());
        let long_label = "a".repeat(64);
        assert!(validate_hostname(&long_label).is_err());
    }

    #[test]
    fn alias_update_table_is_enforced() {
        let host = [ep(EndpointScheme::Https, "a.vendor.com", None)];
        let host_same = [ep(EndpointScheme::Https, "a.vendor.com", None)];
        let host_other = [ep(EndpointScheme::Https, "b.vendor.com", None)];
        let ip = [ep(EndpointScheme::Https, "10.0.0.1", None)];
        let ip_other = [ep(EndpointScheme::Https, "10.0.0.2", None)];

        // Derivable → Derivable, same derived alias.
        assert!(
            enforce_alias_update(&host, "a.vendor.com", &host_same, Some("a.vendor.com")).is_ok()
        );
        // Derivable → Derivable, alias would change.
        assert!(enforce_alias_update(&host, "a.vendor.com", &host_other, None).is_err());
        // Derivable → Non-derivable: always rejected.
        assert!(enforce_alias_update(&host, "a.vendor.com", &ip, Some("a.vendor.com")).is_err());
        // Non-derivable → Non-derivable, endpoints change but alias retained.
        assert!(matches!(
            enforce_alias_update(&ip, "my-service", &ip_other, Some("my-service")),
            Ok(AliasUpdate::Retain)
        ));
        // Non-derivable → Non-derivable with a differing explicit alias.
        assert!(enforce_alias_update(&ip, "my-service", &ip_other, Some("other")).is_err());
        // Non-derivable → Derivable, derived equals existing.
        assert!(matches!(
            enforce_alias_update(&ip, "a.vendor.com", &host, None),
            Ok(AliasUpdate::Retain)
        ));
        // No endpoint change, exact-match alias tolerated.
        assert!(matches!(
            enforce_alias_update(&ip, "my-service", &ip, Some("my-service")),
            Ok(AliasUpdate::Retain)
        ));
    }

    #[test]
    fn alias_shape_is_restricted() {
        assert!(validate_alias_shape("api.openai.com").is_ok());
        assert!(validate_alias_shape("vendor.com:8443").is_ok());
        assert!(validate_alias_shape("-vendor.com").is_err());
        assert!(validate_alias_shape("vendor.com-").is_err());
        assert!(validate_alias_shape("Vendor.com").is_err());
    }
}
