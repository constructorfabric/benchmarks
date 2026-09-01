// Created: 2026-08-29 by Constructor Tech
//! Alias derivation from the upstream endpoint set.
//!
//! Alias behaviour is determined entirely by endpoint type (DESIGN §3.2
//! "Alias Resolution"). Aliases are not arbitrary labels: they are derived from
//! the endpoints whenever derivation is possible.

use std::collections::BTreeSet;

use super::error::OagwError;
use super::model::{Endpoint, validate_hostname_like};

/// Outcome of alias derivation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DerivedAlias {
    /// Derivation succeeded.
    Derived(String),
    /// Derivation is impossible; the caller must supply an alias.
    NotDerivable(NotDerivable),
}

/// Why derivation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotDerivable {
    /// Hosts have no common registrable suffix under the Public Suffix List.
    NoCommonSuffix,
    /// The only common suffix is a bare public suffix (e.g. `co.uk`).
    BarePublicSuffix,
    /// Endpoints are IP literals.
    IpEndpoints,
    /// Multiple endpoints on different ports defeat the `host:port` form.
    MixedPorts,
}

impl NotDerivable {
    /// Stable, user-facing description of the failure.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NoCommonSuffix => "the endpoints share no registrable common suffix",
            Self::BarePublicSuffix => "the common suffix is a bare public suffix",
            Self::IpEndpoints => "the endpoints are IP literals",
            Self::MixedPorts => "the endpoints use different ports",
        }
    }
}

/// Compute the derived alias for an endpoint set.
///
/// # Rules
///
/// * Single endpoint, standard port (`http` → 80, everything else → 443):
///   the host name.
/// * Single endpoint, non-standard port: `host:port`.
/// * Multiple endpoints: the common suffix across all hostnames when it is at
///   least two labels **and** a registrable domain under the Public Suffix List
///   (i.e. not a bare public suffix). When all endpoints share a non-standard
///   port the alias is `suffix:port`.
/// * IP endpoints, bare public suffixes and heterogeneous hostnames are not
///   derivable.
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> DerivedAlias {
    let Some(first) = endpoints.first() else {
        return DerivedAlias::NotDerivable(NotDerivable::NoCommonSuffix);
    };
    // IP endpoints are never derivable, not even a single one: the operator
    // must name the upstream explicitly (DESIGN "Alias Enforcement Rules").
    if first.is_ip() {
        return DerivedAlias::NotDerivable(NotDerivable::IpEndpoints);
    }
    if endpoints.len() == 1 {
        return DerivedAlias::Derived(single_endpoint_alias(first));
    }
    let hosts: Vec<String> = endpoints
        .iter()
        .map(|e| e.normalized_host())
        .collect::<Vec<_>>();
    if hosts.iter().any(|h| h.is_empty()) {
        return DerivedAlias::NotDerivable(NotDerivable::NoCommonSuffix);
    }
    let Some(suffix) = common_suffix(&hosts) else {
        return DerivedAlias::NotDerivable(NotDerivable::NoCommonSuffix);
    };
    let Some(common_port) = common_port(endpoints) else {
        return DerivedAlias::NotDerivable(NotDerivable::MixedPorts);
    };
    if !suffix.contains('.') {
        // A shared TLD ("com") is not a meaningful common suffix.
        return DerivedAlias::NotDerivable(NotDerivable::NoCommonSuffix);
    }
    if !is_registrable_domain(&suffix) {
        return DerivedAlias::NotDerivable(NotDerivable::BarePublicSuffix);
    }
    if common_port == first.standard_port() {
        DerivedAlias::Derived(suffix)
    } else {
        DerivedAlias::Derived(format!("{suffix}:{common_port}"))
    }
}

fn single_endpoint_alias(endpoint: &Endpoint) -> String {
    let host = endpoint.normalized_host();
    if endpoint.port == endpoint.standard_port() {
        return host;
    }
    format!("{host}:{}", endpoint.port)
}

/// Longest common dot-separated suffix across `hosts`, or `None` when one host
/// equals the whole suffix (i.e. the suffix would swallow an entire host).
fn common_suffix(hosts: &[String]) -> Option<String> {
    let label_sets: Vec<Vec<&str>> = hosts.iter().map(|h| h.split('.').collect()).collect();
    let shortest = label_sets.iter().map(Vec::len).min().unwrap_or(0);
    if shortest < 2 {
        return None;
    }
    let mut shared: Vec<String> = Vec::new();
    for index in 1..=shortest {
        let label = label_sets[0][label_sets[0].len() - index];
        if label_sets
            .iter()
            .all(|labels| labels[labels.len() - index] == label)
        {
            shared.push(label.to_owned());
        } else {
            break;
        }
    }
    if shared.len() < 2 {
        return None;
    }
    shared.reverse();
    let suffix = shared.join(".");
    // A suffix equal to one of the hosts means the pool is a subdomain of
    // itself; the suffix must be strictly shorter than every host.
    if hosts.iter().any(|h| h == &suffix) {
        return None;
    }
    Some(suffix)
}

/// The shared port of all endpoints, or `None` when they differ.
fn common_port(endpoints: &[Endpoint]) -> Option<u16> {
    let ports: BTreeSet<u16> = endpoints.iter().map(|e| e.port).collect();
    if ports.len() != 1 {
        return None;
    }
    ports.into_iter().next()
}

/// `true` when `suffix` is a registrable domain: at least two labels and not a
/// bare public suffix under the Public Suffix List.
///
/// `psl::domain_str` returns the registrable domain of its input, which is the
/// input itself only when the input already is a registrable domain.
fn is_registrable_domain(suffix: &str) -> bool {
    psl::domain_str(suffix) == Some(suffix)
}

/// Enforce the alias rules for a create or replace operation.
///
/// # Semantics
///
/// * Derivable endpoints: the alias **must** equal the derived value. A
///   differing explicit alias is rejected with `400`; the exact derived value is
///   accepted as an idempotent no-op.
/// * Non-derivable endpoints: an explicit alias is **required** (`400` when
///   missing) and is used as-is.
/// * `existing_alias` is set for replaces: the alias is immutable, so any
///   operation that would change it is rejected.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] with a message naming the conflicting or
/// missing alias.
pub fn resolve_alias(
    endpoints: &[Endpoint],
    supplied: Option<&str>,
    existing_alias: Option<&str>,
) -> Result<(String, bool), OagwError> {
    let derived = compute_derived_alias(endpoints);
    let resolved = match (&derived, supplied) {
        (DerivedAlias::Derived(value), Some(supplied)) => {
            let normalized = validate_hostname_like(supplied)?;
            if normalized != *value {
                return Err(OagwError::Validation(format!(
                    "alias '{normalized}' does not match the derived alias '{value}'; \
                     hostname based endpoints cannot be re-aliased"
                )));
            }
            (normalized.clone(), true)
        }
        (DerivedAlias::Derived(value), None) => (value.clone(), true),
        (DerivedAlias::NotDerivable(_), Some(supplied)) => {
            (validate_hostname_like(supplied)?, false)
        }
        (DerivedAlias::NotDerivable(reason), None) => {
            return Err(OagwError::Validation(format!(
                "an explicit alias is required for these endpoints ({}); \
                 endpoints are IP based or share no registrable common suffix",
                reason.as_str()
            )));
        }
    };
    if let Some(existing) = existing_alias
        && existing != resolved.0
    {
        return Err(OagwError::Validation(format!(
            "alias is immutable: existing alias '{existing}' cannot be changed to '{}'; \
             delete and re-create the upstream instead",
            resolved.0
        )));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(scheme: &str, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: scheme.to_owned(),
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn single_host_standard_port() {
        let endpoints = [ep("https", "api.openai.com", 443)];
        assert_eq!(
            compute_derived_alias(&endpoints),
            DerivedAlias::Derived("api.openai.com".to_owned())
        );
    }

    #[test]
    fn single_host_non_standard_port() {
        let endpoints = [ep("https", "api.openai.com", 8443)];
        assert_eq!(
            compute_derived_alias(&endpoints),
            DerivedAlias::Derived("api.openai.com:8443".to_owned())
        );
    }

    #[test]
    fn common_suffix_multi_host() {
        let endpoints = [
            ep("https", "us.vendor.com", 443),
            ep("https", "eu.vendor.com", 443),
        ];
        assert_eq!(
            compute_derived_alias(&endpoints),
            DerivedAlias::Derived("vendor.com".to_owned())
        );
    }

    #[test]
    fn common_suffix_with_port() {
        let endpoints = [
            ep("https", "us.vendor.com", 8443),
            ep("https", "eu.vendor.com", 8443),
        ];
        assert_eq!(
            compute_derived_alias(&endpoints),
            DerivedAlias::Derived("vendor.com:8443".to_owned())
        );
    }

    #[test]
    fn bare_public_suffix_not_derivable() {
        let endpoints = [ep("https", "foo.co.uk", 443), ep("https", "bar.co.uk", 443)];
        assert_eq!(
            compute_derived_alias(&endpoints),
            DerivedAlias::NotDerivable(NotDerivable::BarePublicSuffix)
        );
    }

    #[test]
    fn no_common_suffix_not_derivable() {
        let endpoints = [
            ep("https", "us.foo.com", 443),
            ep("https", "eu.bar.com", 443),
        ];
        assert_eq!(
            compute_derived_alias(&endpoints),
            DerivedAlias::NotDerivable(NotDerivable::NoCommonSuffix)
        );
    }

    #[test]
    fn ip_endpoints_not_derivable() {
        let endpoints = [ep("https", "10.0.1.1", 443), ep("https", "10.0.1.2", 443)];
        assert_eq!(
            compute_derived_alias(&endpoints),
            DerivedAlias::NotDerivable(NotDerivable::IpEndpoints)
        );
    }

    #[test]
    fn normalizes_case_and_trailing_dot() {
        let endpoints = [ep("https", "Api.OpenAI.COM.", 443)];
        assert_eq!(
            compute_derived_alias(&endpoints),
            DerivedAlias::Derived("api.openai.com".to_owned())
        );
    }

    #[test]
    fn supplied_alias_mismatch_rejected() {
        let endpoints = [ep("https", "api.openai.com", 443)];
        let err = resolve_alias(&endpoints, Some("my-service"), None)
            .expect_err("mismatching alias must be rejected");
        assert!(matches!(err, OagwError::Validation(_)));
    }

    #[test]
    fn supplied_alias_equal_is_idempotent() {
        let endpoints = [ep("https", "api.openai.com", 443)];
        let (alias, derived) =
            resolve_alias(&endpoints, Some("api.openai.com"), None).expect("accepted");
        assert_eq!(alias, "api.openai.com");
        assert!(derived);
    }

    #[test]
    fn non_derivable_requires_alias() {
        let endpoints = [ep("https", "10.0.1.1", 443)];
        assert!(resolve_alias(&endpoints, None, None).is_err());
        let (alias, _) = resolve_alias(&endpoints, Some("my-service"), None).expect("accepted");
        assert_eq!(alias, "my-service");
    }

    #[test]
    fn alias_is_immutable() {
        let endpoints = [ep("https", "api.openai.com", 443)];
        assert!(resolve_alias(&endpoints, None, Some("api.openai.com")).is_ok());
        assert!(resolve_alias(&endpoints, None, Some("other.example.com")).is_err());
    }

    #[test]
    fn rfc1123_rejections() {
        assert!(validate_hostname_like("-bad.example.com").is_err());
        assert!(validate_hostname_like("bad-.example.com").is_err());
        assert!(validate_hostname_like("a..b").is_err());
        assert!(validate_hostname_like(&"a".repeat(64)).is_err());
        assert!(validate_hostname_like("").is_err());
    }
}
