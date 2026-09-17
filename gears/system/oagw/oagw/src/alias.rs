// Created: 2026-09-03 by Constructor Tech
//! Alias derivation and normalization.
//!
//! Aliases are the routing keys of the data plane
//! (`{METHOD} /oagw/v1/proxy/{alias}/...`). Their value is enforced by the
//! endpoint type rather than being a free-form label; see `DESIGN.md`
//! "Alias Enforcement Rules".

use std::net::IpAddr;

use crate::error::{ErrorKind, OagwError};
use crate::model::{Endpoint, EndpointScheme};

/// Standard ports that are omitted from a derived alias.
///
/// HTTP dials 80; every TLS-family scheme dials 443.
const HTTP_STANDARD_PORT: u16 = 80;
const TLS_STANDARD_PORT: u16 = 443;

/// Maximum length of a fully qualified domain name (RFC 1123).
const MAX_HOSTNAME_LEN: usize = 253;
/// Maximum length of a single DNS label (RFC 1123).
const MAX_LABEL_LEN: usize = 63;

/// Normalizes a hostname or alias: ASCII lowercase, trailing dots stripped.
#[must_use]
pub fn normalize(value: &str) -> String {
    value.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Whether `host` is an IPv4 or IPv6 literal.
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok()
}

/// Validates and normalizes an endpoint hostname per RFC 1123.
///
/// A trailing dot (FQDN notation) is tolerated and stripped. IP literals are
/// accepted as-is.
///
/// # Errors
/// Returns a 400 `ValidationError` when the value is not a valid hostname or
/// IP literal.
pub fn validate_host(host: &str) -> Result<String, OagwError> {
    let normalized = normalize(host);
    if normalized.is_empty() {
        return Err(OagwError::new(
            ErrorKind::Validation,
            "endpoint host must not be empty",
        ));
    }
    if is_ip_literal(&normalized) {
        return Ok(normalized);
    }
    if normalized.len() > MAX_HOSTNAME_LEN {
        return Err(OagwError::new(
            ErrorKind::Validation,
            format!("endpoint host exceeds {MAX_HOSTNAME_LEN} characters"),
        ));
    }
    if normalized.contains("..") {
        return Err(OagwError::new(
            ErrorKind::Validation,
            "endpoint host must not contain empty labels",
        ));
    }
    for label in normalized.split('.') {
        if label.is_empty() || label.len() > MAX_LABEL_LEN {
            return Err(OagwError::new(
                ErrorKind::Validation,
                "endpoint host labels must be between 1 and 63 characters",
            ));
        }
        if !label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(OagwError::new(
                ErrorKind::Validation,
                "endpoint host labels may only contain ASCII letters, digits and hyphens",
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(OagwError::new(
                ErrorKind::Validation,
                "endpoint host labels must not start or end with a hyphen",
            ));
        }
    }
    Ok(normalized)
}

/// Whether `scheme` dials the default port omitted from a derived alias.
#[must_use]
pub fn is_standard_port(scheme: EndpointScheme, port: u16) -> bool {
    match scheme {
        EndpointScheme::Http => port == HTTP_STANDARD_PORT,
        EndpointScheme::Https | EndpointScheme::Wss | EndpointScheme::Wt | EndpointScheme::Grpc => {
            port == TLS_STANDARD_PORT
        }
    }
}

/// The registrable domain of `host` according to the public suffix list, when
/// the host is not an IP literal.
#[must_use]
fn registrable_domain(host: &str) -> Option<String> {
    psl::domain_str(host).map(str::to_owned)
}

/// Derives the alias from a set of endpoints.
///
/// Returns `None` when the endpoints are not auto-derivable: IP literals,
/// heterogeneous hostnames with no registrable common suffix, or a hostname
/// pool whose only common suffix is a bare public suffix (such as `co.uk`).
#[must_use]
pub fn derive_alias(endpoints: &[Endpoint]) -> Option<String> {
    let first = endpoints.first()?;
    if endpoints.iter().any(|e| is_ip_literal(&e.host)) {
        return None;
    }
    let hosts: Vec<String> = endpoints.iter().map(|e| e.host.clone()).collect();

    let base = if hosts.iter().all(|h| h == &hosts[0]) {
        hosts[0].clone()
    } else {
        let common = registrable_domain(&hosts[0])?;
        let all_shared = hosts
            .iter()
            .all(|host| registrable_domain(host).is_some_and(|d| d == common));
        if !all_shared {
            return None;
        }
        common
    };

    let port = first.port();
    if !is_standard_port(first.scheme, port) {
        Some(format!("{base}:{port}"))
    } else {
        Some(base)
    }
}

/// Derives the alias for a new upstream, applying the create-time rules.
///
/// A user-provided alias that matches the derived value is tolerated as an
/// idempotent no-op; a differing value is rejected.
///
/// # Errors
/// Returns a 400 `ValidationError` when the alias is missing for a
/// non-derivable endpoint set, or when a provided alias contradicts the
/// derived one.
pub fn enforce_alias_create(
    provided: Option<&str>,
    endpoints: &[Endpoint],
) -> Result<String, OagwError> {
    let derived = derive_alias(endpoints);
    match (provided, derived) {
        (Some(provided), Some(derived)) => {
            let normalized = normalize(provided);
            if normalized == derived {
                Ok(derived)
            } else {
                Err(OagwError::new(
                    ErrorKind::Validation,
                    format!(
                        "alias must be auto-derived from the endpoints: expected '{derived}'"
                    ),
                ))
            }
        }
        (Some(provided), None) => Ok(normalize(provided)),
        (None, Some(derived)) => Ok(derived),
        (None, None) => Err(OagwError::new(
            ErrorKind::Validation,
            "alias is required for IP-based or non-derivable endpoint pools",
        )),
    }
}

/// Enforces alias immutability on replacement, per the alias update matrix.
///
/// `previous` is the stored endpoint set and `endpoints` the submitted one; a
/// provided alias that does not match the stored one is always rejected.
///
/// # Errors
/// Returns a 400 `ValidationError` for alias overrides and for any transition
/// that would re-key the routing table, except non-derivable → non-derivable,
/// which retains the stored alias.
pub fn enforce_alias_update(
    existing: &str,
    provided: Option<&str>,
    previous: &[Endpoint],
    endpoints: &[Endpoint],
) -> Result<(), OagwError> {
    if let Some(provided) = provided {
        let normalized = normalize(provided);
        if normalized != existing {
            return Err(OagwError::new(
                ErrorKind::Validation,
                format!(
                    "alias '{existing}' is immutable; an override to '{normalized}' is not allowed"
                ),
            ));
        }
    }
    if previous == endpoints {
        return Ok(());
    }
    match (derive_alias(previous), derive_alias(endpoints)) {
        // The derivation reproduces the routing key, so nothing moves.
        (Some(before), Some(derived)) if before == derived => Ok(()),
        // An explicit-alias pool replaced by a derivable one keeps the stored
        // key when the derivation reproduces it.
        (None, Some(derived)) if derived == existing => Ok(()),
        (Some(_), Some(derived)) | (None, Some(derived)) => Err(OagwError::new(
            ErrorKind::Validation,
            format!(
                "alias '{existing}' is immutable and the new endpoints would derive '{derived}'; \
                 delete and re-create the upstream"
            ),
        )),
        // A derivable pool replaced by an IP pool changes the derivation
        // result, so it is refused regardless of the alias submitted.
        (Some(_), None) => Err(OagwError::new(
            ErrorKind::Validation,
            format!(
                "alias '{existing}' is immutable and the new endpoints are not derivable; \
                 delete and re-create the upstream"
            ),
        )),
        // Non-derivable → non-derivable keeps the routing key stable, so the
        // stored alias is retained.
        (None, None) => Ok(()),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn endpoint(host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: EndpointScheme::Https,
            host: host.to_owned(),
            port: Some(port),
        }
    }

    #[test]
    fn single_hostname_with_standard_port_derives_hostname() {
        let derived = derive_alias(&[endpoint("api.openai.com", 443)]);
        assert_eq!(derived.as_deref(), Some("api.openai.com"));
    }

    #[test]
    fn single_hostname_with_non_standard_port_appends_port() {
        let derived = derive_alias(&[endpoint("api.openai.com", 8443)]);
        assert_eq!(derived.as_deref(), Some("api.openai.com:8443"));
    }

    #[test]
    fn multi_hostname_pool_derives_registrable_common_suffix() {
        let derived = derive_alias(&[endpoint("us.vendor.com", 443), endpoint("eu.vendor.com", 443)]);
        assert_eq!(derived.as_deref(), Some("vendor.com"));
    }

    #[test]
    fn non_standard_port_pool_preserves_port_in_suffix() {
        let derived = derive_alias(&[endpoint("us.vendor.com", 8443), endpoint("eu.vendor.com", 8443)]);
        assert_eq!(derived.as_deref(), Some("vendor.com:8443"));
    }

    #[test]
    fn bare_public_suffix_pool_is_not_derivable() {
        let derived = derive_alias(&[endpoint("foo.co.uk", 443), endpoint("bar.co.uk", 443)]);
        assert_eq!(derived, None);
    }

    #[test]
    fn heterogeneous_hostnames_are_not_derivable() {
        let derived = derive_alias(&[endpoint("us.foo.com", 443), endpoint("eu.bar.com", 443)]);
        assert_eq!(derived, None);
    }

    #[test]
    fn ip_pools_are_not_derivable() {
        let derived = derive_alias(&[endpoint("10.0.1.1", 443), endpoint("10.0.1.2", 443)]);
        assert_eq!(derived, None);
    }

    #[test]
    fn non_derivables_require_explicit_alias_on_create() {
        let endpoints = [endpoint("10.0.1.1", 443)];
        assert!(enforce_alias_create(None, &endpoints).is_err());
        assert_eq!(
            enforce_alias_create(Some("My-Service"), &endpoints).expect("alias"),
            "my-service"
        );
    }

    #[test]
    fn mismatching_alias_on_derivable_endpoints_is_rejected() {
        let endpoints = [endpoint("api.openai.com", 443)];
        assert!(enforce_alias_create(Some("openai"), &endpoints).is_err());
        assert!(enforce_alias_create(Some("api.openai.com"), &endpoints).is_ok());
    }

    #[test]
    fn update_rejects_alias_override_and_alias_changing_endpoints() {
        let old = [endpoint("api.openai.com", 443)];
        assert!(enforce_alias_update("api.openai.com", Some("other"), &old, &old).is_err());

        let same = [endpoint("api.openai.com", 443)];
        assert!(enforce_alias_update("api.openai.com", None, &same, &same).is_ok());

        let changed = [endpoint("api.other.com", 443)];
        assert!(enforce_alias_update("api.openai.com", None, &old, &changed).is_err());
    }

    #[test]
    fn update_matrix_retains_the_alias_when_both_pools_are_non_derivable() {
        // Non-derivable → non-derivable: the routing key does not move.
        let ip = [endpoint("10.0.1.1", 443)];
        let other_ip = [endpoint("10.0.1.2", 443)];
        assert!(enforce_alias_update("my-service", None, &ip, &other_ip).is_ok());

        // Derivable → non-derivable is refused regardless of the alias.
        let host = [endpoint("api.openai.com", 443)];
        assert!(enforce_alias_update("api.openai.com", None, &host, &ip).is_err());

        // Non-derivable → derivable is allowed only when the derivation
        // reproduces the stored key.
        assert!(enforce_alias_update("api.openai.com", None, &ip, &host).is_ok());
        let renamed = [endpoint("api.other.com", 443)];
        assert!(enforce_alias_update("api.openai.com", None, &ip, &renamed).is_err());
    }

    #[test]
    fn hostname_validation_rejects_bad_labels() {
        assert!(validate_host("api.openai.com.").is_ok());
        assert!(validate_host("-bad.example.com").is_err());
        assert!(validate_host("bad..example.com").is_err());
        assert!(validate_host("bad_example.com").is_err());
        assert!(validate_host(&"a".repeat(254)).is_err());
        assert!(validate_host("10.0.0.1").is_ok());
        assert!(validate_host("2001:db8::1").is_ok());
    }
}
