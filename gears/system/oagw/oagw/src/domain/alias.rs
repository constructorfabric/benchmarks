//! Alias derivation, normalization and hostname validation (DESIGN §3.2
//! "Alias Enforcement Rules").
//!
//! Alias behaviour is determined entirely by the endpoint set:
//!
//! | Endpoint set | Alias |
//! |---|---|
//! | single hostname, standard port | `hostname` |
//! | single hostname, non-standard port | `hostname:port` |
//! | several hostnames, registrable common suffix | `suffix[:port]` |
//! | several hostnames, suffix is a bare public suffix | explicit required |
//! | several hostnames, no common suffix | explicit required |
//! | IP endpoints | explicit required |

use crate::domain::model::{Endpoint, Scheme};

/// Maximum length of a fully qualified hostname (RFC 1123).
pub const MAX_HOSTNAME_LENGTH: usize = 253;

/// Maximum length of a single hostname label (RFC 1123).
pub const MAX_LABEL_LENGTH: usize = 63;

/// Normalizes an alias or hostname: ASCII lowercase, trailing dots stripped.
#[must_use]
pub fn normalize_alias(value: &str) -> String {
    let lowered = value.to_ascii_lowercase();
    let trimmed = lowered.trim_end_matches('.');
    trimmed.to_owned()
}

/// Validates a hostname per RFC 1123.
///
/// Returns `Err` with a human-readable reason when the host is not a valid
/// hostname. A trailing dot (FQDN notation) is tolerated and stripped.
///
/// # Errors
///
/// Returns the validation reason when the hostname violates RFC 1123 label or
/// length rules.
pub fn validate_hostname(host: &str) -> Result<String, String> {
    let trimmed = host.trim_end_matches('.');
    if trimmed.is_empty() {
        return Err("hostname must not be empty".to_owned());
    }
    if trimmed.len() > MAX_HOSTNAME_LENGTH {
        return Err(format!(
            "hostname exceeds the maximum length of {MAX_HOSTNAME_LENGTH} characters"
        ));
    }
    if trimmed.parse::<std::net::IpAddr>().is_ok() {
        return Ok(trimmed.to_owned());
    }
    for label in trimmed.split('.') {
        validate_label(label)?;
    }
    Ok(trimmed.to_ascii_lowercase())
}

fn validate_label(label: &str) -> Result<(), String> {
    if label.is_empty() {
        return Err("hostname labels must not be empty".to_owned());
    }
    if label.len() > MAX_LABEL_LENGTH {
        return Err(format!(
            "hostname labels are limited to {MAX_LABEL_LENGTH} characters"
        ));
    }
    if !label
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "hostname label contains invalid characters: {label:?}"
        ));
    }
    if label.starts_with('-') || label.ends_with('-') {
        return Err("hostname labels must not start or end with a hyphen".to_owned());
    }
    Ok(())
}

/// Outcome of alias derivation for an endpoint set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DerivedAlias {
    /// Derivation succeeded.
    Derived(String),
    /// Derivation failed; an explicit alias is required.
    NotDerivable,
}

/// Computes the alias derived from the endpoint set.
///
/// * A single hostname endpoint derives from the hostname (plus `:port` when
///   the port is non-standard for the scheme).
/// * Several hostname endpoints derive from their registrable common suffix
///   when one exists (PSL-validated), preserving `:port` for non-standard
///   ports.
/// * IP endpoints, heterogeneous hostnames and pools whose only common suffix
///   is a bare public suffix are not derivable.
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> DerivedAlias {
    if endpoints.is_empty() {
        return DerivedAlias::NotDerivable;
    }
    let first_port = endpoints[0].port;
    let port_suffix = port_suffix(endpoints[0].scheme, first_port);

    if endpoints.len() == 1 {
        let endpoint = &endpoints[0];
        if endpoint.is_ip() {
            return DerivedAlias::NotDerivable;
        }
        let Ok(host) = validate_hostname(&endpoint.host) else {
            return DerivedAlias::NotDerivable;
        };
        return DerivedAlias::Derived(format!("{host}{port_suffix}"));
    }

    let mut hosts = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        if endpoint.is_ip() {
            return DerivedAlias::NotDerivable;
        }
        let Ok(host) = validate_hostname(&endpoint.host) else {
            return DerivedAlias::NotDerivable;
        };
        if endpoint.port != first_port || endpoint.scheme != endpoints[0].scheme {
            // Heterogeneous pools must not derive a shared suffix alias.
            return DerivedAlias::NotDerivable;
        }
        hosts.push(host);
    }
    match common_registrable_suffix(&hosts) {
        Some(suffix) => DerivedAlias::Derived(format!("{suffix}{port_suffix}")),
        None => DerivedAlias::NotDerivable,
    }
}

fn port_suffix(scheme: Scheme, port: u16) -> String {
    if port == scheme.standard_port() {
        String::new()
    } else {
        format!(":{port}")
    }
}

/// Returns the registrable domain shared by every host, when one exists.
///
/// `us.vendor.com` / `eu.vendor.com` share `vendor.com`; `foo.co.uk` /
/// `bar.co.uk` do not because `co.uk` is a bare public suffix.
#[must_use]
pub fn common_registrable_suffix(hosts: &[String]) -> Option<String> {
    let mut registrable: Option<String> = None;
    for host in hosts {
        let candidate = psl::domain_str(host)?;
        match &registrable {
            Some(existing) if existing == candidate => {}
            Some(_) => return None,
            None => registrable = Some(candidate.to_owned()),
        }
    }
    registrable.filter(|suffix| suffix.split('.').count() >= 2)
}

/// `true` when the alias is already the derived value (idempotent no-op).
#[must_use]
pub fn alias_matches_derived(provided: &str, endpoints: &[Endpoint]) -> bool {
    match compute_derived_alias(endpoints) {
        DerivedAlias::Derived(derived) => normalize_alias(provided) == derived,
        DerivedAlias::NotDerivable => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(host: &str, port: u16, scheme: Scheme) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn single_hostname_standard_port_derives_hostname() {
        let endpoints = vec![endpoint("api.openai.com", 443, Scheme::Https)];
        assert_eq!(
            compute_derived_alias(&endpoints),
            DerivedAlias::Derived("api.openai.com".to_owned())
        );
    }

    #[test]
    fn single_hostname_non_standard_port_keeps_port() {
        let endpoints = vec![endpoint("api.openai.com", 8443, Scheme::Https)];
        assert_eq!(
            compute_derived_alias(&endpoints),
            DerivedAlias::Derived("api.openai.com:8443".to_owned())
        );
    }

    #[test]
    fn plaintext_scheme_uses_http_standard_port() {
        let endpoints = vec![endpoint("mock.internal", 80, Scheme::Http)];
        assert_eq!(
            compute_derived_alias(&endpoints),
            DerivedAlias::Derived("mock.internal".to_owned())
        );
    }

    #[test]
    fn shared_registrable_suffix_derives_suffix() {
        let endpoints = vec![
            endpoint("us.vendor.com", 443, Scheme::Https),
            endpoint("eu.vendor.com", 443, Scheme::Https),
        ];
        assert_eq!(
            compute_derived_alias(&endpoints),
            DerivedAlias::Derived("vendor.com".to_owned())
        );
    }

    #[test]
    fn shared_suffix_with_non_standard_port_keeps_port() {
        let endpoints = vec![
            endpoint("us.vendor.com", 8443, Scheme::Https),
            endpoint("eu.vendor.com", 8443, Scheme::Https),
        ];
        assert_eq!(
            compute_derived_alias(&endpoints),
            DerivedAlias::Derived("vendor.com:8443".to_owned())
        );
    }

    #[test]
    fn bare_public_suffix_is_not_derivable() {
        let endpoints = vec![
            endpoint("foo.co.uk", 443, Scheme::Https),
            endpoint("bar.co.uk", 443, Scheme::Https),
        ];
        assert_eq!(
            compute_derived_alias(&endpoints),
            DerivedAlias::NotDerivable
        );
    }

    #[test]
    fn heterogeneous_hostnames_are_not_derivable() {
        let endpoints = vec![
            endpoint("us.foo.com", 443, Scheme::Https),
            endpoint("eu.bar.com", 443, Scheme::Https),
        ];
        assert_eq!(
            compute_derived_alias(&endpoints),
            DerivedAlias::NotDerivable
        );
    }

    #[test]
    fn ip_endpoints_are_not_derivable() {
        let endpoints = vec![
            endpoint("10.0.1.1", 443, Scheme::Https),
            endpoint("10.0.1.2", 443, Scheme::Https),
        ];
        assert_eq!(
            compute_derived_alias(&endpoints),
            DerivedAlias::NotDerivable
        );
        let single = vec![endpoint("127.0.0.1", 8080, Scheme::Http)];
        assert_eq!(compute_derived_alias(&single), DerivedAlias::NotDerivable);
    }

    #[test]
    fn normalization_lowercases_and_strips_trailing_dot() {
        assert_eq!(normalize_alias("Api.OpenAI.COM."), "api.openai.com");
        assert_eq!(normalize_alias("MY-SERVICE"), "my-service");
    }

    #[test]
    fn hostname_validation_enforces_rfc1123() {
        assert!(validate_hostname("api.openai.com").is_ok());
        assert!(validate_hostname("api.openai.com.").is_ok());
        assert!(validate_hostname("127.0.0.1").is_ok());
        assert!(validate_hostname("-bad.label").is_err());
        assert!(validate_hostname("bad..label").is_err());
        assert!(validate_hostname(&"a".repeat(64)).is_err());
        assert!(validate_hostname(&format!("{}.example.com", "a".repeat(64))).is_err());
        assert!(validate_hostname("").is_err());
    }

    #[test]
    fn idempotent_alias_check_compares_normalized_values() {
        let endpoints = vec![endpoint("api.openai.com", 443, Scheme::Https)];
        assert!(alias_matches_derived("api.openai.com", &endpoints));
        assert!(alias_matches_derived("API.OPENAI.COM", &endpoints));
        assert!(!alias_matches_derived("other", &endpoints));
    }
}
