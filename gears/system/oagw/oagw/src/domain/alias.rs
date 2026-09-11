// @cpt-begin:cpt-cf-oagw-dod-resource-model-alias-derivation:p1:inst-alias
//! Alias derivation, normalization and update rules.
//!
//! An alias is the routing key in `/oagw/v1/proxy/{alias}/...`. It is derived
//! from hostname endpoints and must be supplied explicitly for endpoints the
//! gateway cannot derive from.

use super::error::{DomainError, DomainResult};
use super::model::{Endpoint, Scheme};

/// Normalize an alias to lowercase with any trailing dot removed.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias.trim_end_matches('.').to_ascii_lowercase()
}

/// Validate a hostname per RFC 1123.
///
/// # Errors
/// Returns a validation error when the hostname is empty, too long, or has a
/// label that is empty, over-long, or hyphen-terminated.
pub fn validate_hostname(host: &str) -> DomainResult<()> {
    let host = host.trim_end_matches('.');
    if host.is_empty() {
        return Err(DomainError::validation("endpoint host must not be empty"));
    }
    if host.len() > 253 {
        return Err(DomainError::validation(
            "endpoint host must not exceed 253 characters",
        ));
    }
    if is_ip_literal(host) {
        return Ok(());
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(DomainError::validation(format!(
                "endpoint host label `{label}` must be 1 to 63 characters"
            )));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(DomainError::validation(format!(
                "endpoint host label `{label}` must not start or end with a hyphen"
            )));
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(DomainError::validation(format!(
                "endpoint host label `{label}` must be alphanumeric or hyphen"
            )));
        }
    }
    Ok(())
}

/// Whether a host string is an IP literal rather than a hostname.
///
/// Trims a trailing dot before testing, because every caller means "is this
/// endpoint host, once normalized, an IP address" -- an IP literal such as
/// `10.0.0.1.` or `[::1].` must not be mistaken for a hostname and silently
/// given a derived alias, which would bypass the rule that an IP-based pool
/// requires an explicit alias.
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    let host = host.trim_end_matches('.');
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.parse::<std::net::IpAddr>().is_ok()
}

/// Render the alias suffix for a port, omitting the scheme's standard port.
fn port_suffix(scheme: Scheme, port: u16) -> String {
    if port == scheme.standard_port() {
        String::new()
    } else {
        format!(":{port}")
    }
}

/// Derive the alias implied by an endpoint pool.
///
/// Returns `None` when derivation is not possible, which is the case for IP
/// endpoints, for heterogeneous hostnames with no common suffix, and for a pool
/// whose only common suffix is a bare public suffix.
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    let first = endpoints.first()?;
    if endpoints.iter().any(|e| is_ip_literal(&e.host)) {
        return None;
    }
    let suffix = port_suffix(first.scheme, first.port);

    if endpoints.len() == 1 {
        let host = normalize_alias(&first.host);
        return Some(format!("{host}{suffix}"));
    }

    let hosts: Vec<String> = endpoints.iter().map(|e| normalize_alias(&e.host)).collect();
    if hosts.iter().all(|h| *h == hosts[0]) {
        return Some(format!("{}{suffix}", hosts[0]));
    }
    let common = common_domain_suffix(&hosts)?;
    Some(format!("{common}{suffix}"))
}

/// Longest common registrable domain suffix of a set of hostnames.
///
/// Returns `None` when the hosts share fewer than two labels, or when the
/// shared suffix is itself a bare public suffix such as `co.uk`.
fn common_domain_suffix(hosts: &[String]) -> Option<String> {
    let mut label_lists: Vec<Vec<&str>> = hosts
        .iter()
        .map(|h| h.split('.').rev().collect::<Vec<_>>())
        .collect();
    let shortest = label_lists.iter().map(Vec::len).min()?;
    let mut shared: Vec<&str> = Vec::new();
    for index in 0..shortest {
        let candidate = label_lists[0][index];
        if label_lists.iter().all(|labels| labels[index] == candidate) {
            shared.push(candidate);
        } else {
            break;
        }
    }
    label_lists.clear();
    if shared.len() < 2 {
        return None;
    }
    shared.reverse();
    let suffix = shared.join(".");
    if is_bare_public_suffix(&suffix) {
        return None;
    }
    Some(suffix)
}

/// Whether a domain is a bare public suffix and so not registrable.
fn is_bare_public_suffix(domain: &str) -> bool {
    let Some(found) = psl::suffix(domain.as_bytes()) else {
        return false;
    };
    let Ok(suffix) = std::str::from_utf8(found.as_bytes()) else {
        return false;
    };
    suffix.eq_ignore_ascii_case(domain)
}

/// Resolve the alias a create request should store.
///
/// # Errors
/// Returns a validation error when a derivable pool is given a conflicting
/// explicit alias, or when a non-derivable pool is given none.
pub fn resolve_create_alias(
    endpoints: &[Endpoint],
    requested: Option<&str>,
) -> DomainResult<String> {
    let derived = compute_derived_alias(endpoints);
    match (derived, requested) {
        (Some(derived), None) => Ok(derived),
        (Some(derived), Some(requested)) => {
            let requested = normalize_alias(requested);
            if requested == derived {
                // Exact match is tolerated so a create is idempotent.
                Ok(derived)
            } else {
                Err(DomainError::validation(format!(
                    "alias is derived from hostname endpoints as `{derived}`; \
                     remove the alias field or supply that exact value"
                )))
            }
        }
        (None, Some(requested)) => {
            let requested = normalize_alias(requested);
            validate_alias_pattern(&requested)?;
            Ok(requested)
        }
        (None, None) => Err(DomainError::validation(
            "alias is required because it cannot be derived from these endpoints",
        )),
    }
}

/// Validate an alias against the pattern the schema declares.
///
/// # Errors
/// Returns a validation error when the alias does not match
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
pub fn validate_alias_pattern(alias: &str) -> DomainResult<()> {
    let invalid = alias.is_empty()
        || !alias
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | ':' | '-'))
        || !starts_and_ends_alphanumeric(alias);
    if invalid {
        return Err(DomainError::validation(format!(
            "alias `{alias}` must match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$"
        )));
    }
    Ok(())
}

fn starts_and_ends_alphanumeric(alias: &str) -> bool {
    let first_ok = alias
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let last_ok = alias
        .chars()
        .next_back()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    first_ok && last_ok
}

/// Enforce the alias-update rules on a replacement.
///
/// The alias is immutable once set, because it is the routing key. Any change
/// that would alter it is rejected; the operator must delete and re-create.
///
/// # Errors
/// Returns a validation error when the replacement would change the alias.
pub fn enforce_alias_update(
    existing_alias: &str,
    endpoints: &[Endpoint],
    requested: Option<&str>,
) -> DomainResult<String> {
    let derived = compute_derived_alias(endpoints);
    match derived {
        Some(derived) if derived == existing_alias => Ok(derived),
        Some(derived) => Err(DomainError::validation(format!(
            "alias is immutable; these endpoints derive `{derived}` but the upstream is \
             `{existing_alias}`. Delete and re-create the upstream instead"
        ))),
        None => {
            // Non-derivable endpoints retain the existing alias. A differing
            // explicit alias is rejected.
            match requested {
                None => Ok(existing_alias.to_owned()),
                Some(requested) if normalize_alias(requested) == existing_alias => {
                    Ok(existing_alias.to_owned())
                }
                Some(_) => Err(DomainError::validation(format!(
                    "alias is immutable; the upstream keeps `{existing_alias}`. \
                     Delete and re-create the upstream instead"
                ))),
            }
        }
    }
}
// @cpt-end:cpt-cf-oagw-dod-resource-model-alias-derivation:p1:inst-alias

#[cfg(test)]
mod tests {
    use super::{
        compute_derived_alias, enforce_alias_update, is_ip_literal, normalize_alias,
        resolve_create_alias, validate_alias_pattern, validate_hostname,
    };
    use crate::domain::model::{Endpoint, Scheme};

    fn endpoint(host: &str, scheme: Scheme, port: u16) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn single_hostname_on_standard_port_derives_bare_host() {
        let eps = vec![endpoint("api.openai.com", Scheme::Https, 443)];
        assert_eq!(
            compute_derived_alias(&eps).as_deref(),
            Some("api.openai.com")
        );
    }

    #[test]
    fn single_hostname_on_non_standard_port_keeps_the_port() {
        let eps = vec![endpoint("api.openai.com", Scheme::Https, 8443)];
        assert_eq!(
            compute_derived_alias(&eps).as_deref(),
            Some("api.openai.com:8443")
        );
    }

    #[test]
    fn plaintext_endpoint_on_port_eighty_derives_bare_host() {
        let eps = vec![endpoint("stub.internal", Scheme::Http, 80)];
        assert_eq!(
            compute_derived_alias(&eps).as_deref(),
            Some("stub.internal")
        );
    }

    #[test]
    fn multiple_hostnames_derive_the_common_registrable_suffix() {
        let eps = vec![
            endpoint("us.vendor.com", Scheme::Https, 443),
            endpoint("eu.vendor.com", Scheme::Https, 443),
        ];
        assert_eq!(compute_derived_alias(&eps).as_deref(), Some("vendor.com"));
    }

    #[test]
    fn common_suffix_pool_keeps_a_non_standard_port() {
        let eps = vec![
            endpoint("us.vendor.com", Scheme::Https, 8443),
            endpoint("eu.vendor.com", Scheme::Https, 8443),
        ];
        assert_eq!(
            compute_derived_alias(&eps).as_deref(),
            Some("vendor.com:8443")
        );
    }

    #[test]
    fn bare_public_suffix_is_not_derivable() {
        let eps = vec![
            endpoint("foo.co.uk", Scheme::Https, 443),
            endpoint("bar.co.uk", Scheme::Https, 443),
        ];
        assert_eq!(compute_derived_alias(&eps), None);
    }

    #[test]
    fn heterogeneous_hostnames_are_not_derivable() {
        let eps = vec![
            endpoint("us.foo.com", Scheme::Https, 443),
            endpoint("eu.bar.com", Scheme::Https, 443),
        ];
        assert_eq!(compute_derived_alias(&eps), None);
    }

    #[test]
    fn ip_endpoints_are_not_derivable() {
        let eps = vec![endpoint("10.0.1.1", Scheme::Https, 443)];
        assert_eq!(compute_derived_alias(&eps), None);
        assert!(is_ip_literal("10.0.1.1"));
        assert!(!is_ip_literal("api.openai.com"));
    }

    #[test]
    fn create_rejects_an_alias_that_differs_from_the_derived_one() {
        let eps = vec![endpoint("api.openai.com", Scheme::Https, 443)];
        assert!(resolve_create_alias(&eps, Some("something-else")).is_err());
    }

    #[test]
    fn create_tolerates_the_exact_derived_alias() {
        let eps = vec![endpoint("api.openai.com", Scheme::Https, 443)];
        let alias = resolve_create_alias(&eps, Some("API.OpenAI.COM")).expect("idempotent");
        assert_eq!(alias, "api.openai.com");
    }

    #[test]
    fn create_requires_an_explicit_alias_for_ip_endpoints() {
        let eps = vec![endpoint("10.0.1.1", Scheme::Https, 443)];
        assert!(resolve_create_alias(&eps, None).is_err());
        let alias = resolve_create_alias(&eps, Some("my-service")).expect("explicit alias");
        assert_eq!(alias, "my-service");
    }

    #[test]
    fn replacement_rejects_an_endpoint_change_that_alters_the_alias() {
        let eps = vec![endpoint("api.other.com", Scheme::Https, 443)];
        assert!(enforce_alias_update("api.openai.com", &eps, None).is_err());
    }

    #[test]
    fn replacement_allows_endpoints_that_derive_the_same_alias() {
        let eps = vec![endpoint("api.openai.com", Scheme::Https, 443)];
        let alias = enforce_alias_update("api.openai.com", &eps, None).expect("unchanged");
        assert_eq!(alias, "api.openai.com");
    }

    #[test]
    fn replacement_of_ip_pool_retains_the_existing_alias() {
        let eps = vec![endpoint("10.0.1.9", Scheme::Https, 443)];
        let alias = enforce_alias_update("my-service", &eps, None).expect("retained");
        assert_eq!(alias, "my-service");
        assert!(enforce_alias_update("my-service", &eps, Some("renamed")).is_err());
    }

    #[test]
    fn ip_literal_detection_trims_a_trailing_dot() {
        assert!(is_ip_literal("10.0.0.1."));
        assert!(is_ip_literal("[::1]."));
        // A hostname that legitimately ends in a dot (a fully-qualified DNS
        // name) is still not an IP literal.
        assert!(!is_ip_literal("api.openai.com."));
    }

    #[test]
    fn an_ip_endpoint_with_a_trailing_dot_still_requires_an_explicit_alias() {
        let eps = vec![endpoint("10.0.0.1.", Scheme::Https, 443)];
        assert_eq!(compute_derived_alias(&eps), None);
        assert!(resolve_create_alias(&eps, None).is_err());
        let alias = resolve_create_alias(&eps, Some("my-service")).expect("explicit alias");
        assert_eq!(alias, "my-service");
    }

    #[test]
    fn replacement_of_a_trailing_dot_ip_pool_retains_the_existing_alias() {
        let eps = vec![endpoint("10.0.0.1.", Scheme::Https, 443)];
        let alias = enforce_alias_update("my-service", &eps, None).expect("retained");
        assert_eq!(alias, "my-service");
        assert!(enforce_alias_update("my-service", &eps, Some("renamed")).is_err());
    }

    #[test]
    fn hostname_validation_follows_rfc_1123() {
        assert!(validate_hostname("api.openai.com").is_ok());
        assert!(validate_hostname("api.openai.com.").is_ok());
        assert!(validate_hostname("10.0.0.1").is_ok());
        assert!(validate_hostname("").is_err());
        assert!(validate_hostname("-bad.example.com").is_err());
        assert!(validate_hostname("bad-.example.com").is_err());
        assert!(validate_hostname("under_score.example.com").is_err());
    }

    #[test]
    fn alias_pattern_is_enforced_for_explicit_values() {
        assert!(validate_alias_pattern("my-service").is_ok());
        assert!(validate_alias_pattern("api.openai.com:8443").is_ok());
        assert!(validate_alias_pattern("-leading").is_err());
        assert!(validate_alias_pattern("trailing-").is_err());
        assert!(validate_alias_pattern("Upper").is_err());
    }

    #[test]
    fn normalization_lowercases_and_strips_a_trailing_dot() {
        assert_eq!(normalize_alias("API.OpenAI.COM."), "api.openai.com");
    }
}
