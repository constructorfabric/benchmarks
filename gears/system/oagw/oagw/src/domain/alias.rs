//! Alias derivation and update enforcement.
//!
//! Rules from `DESIGN.md` §Alias Enforcement:
//!
//! * single hostname → hostname (non-standard port → `hostname:port`)
//! * multiple hostnames with a registrable common suffix (≥2 labels, not a
//!   bare public suffix) → the suffix (`:port` preserved when non-standard)
//! * IP addresses, heterogeneous hostnames and bare-public-suffix pools are
//!   not derivable — an explicit alias is required

use psl::Psl as _;

use crate::domain::model::{Endpoint, normalize_hostname};
use crate::gts;

/// Standard ports omitted from derived aliases.
fn is_standard_port(scheme: crate::domain::model::Scheme, port: u16) -> bool {
    port == scheme.standard_port()
}

/// Longest registrable common suffix of a set of hostnames, or `None`.
///
/// The suffix must contain at least two labels and must not be a bare public
/// suffix (PSL-validated).
#[must_use]
pub fn common_domain_suffix(hosts: &[String]) -> Option<String> {
    if hosts.len() < 2 {
        return None;
    }

    let mut parts: Vec<Vec<&str>> = Vec::with_capacity(hosts.len());
    for host in hosts {
        parts.push(host.split('.').rev().collect());
    }

    let first = &parts[0];
    let mut shared = 0_usize;
    for (i, label) in first.iter().enumerate() {
        if parts.iter().all(|p| p.get(i) == Some(label)) {
            shared += 1;
        } else {
            break;
        }
    }

    // `shared` counts every label the hosts agree on, including the
    // registrable ones: `us.vendor.com` / `eu.vendor.com` share `com` and
    // `vendor`, and the derivable alias is exactly `vendor.com`. A pool that
    // only agrees on the public suffix itself is caught by the check below.
    if shared == 0 {
        return None;
    }
    let suffix: Vec<&str> = first[..shared].to_vec();
    let candidate = suffix.iter().rev().copied().collect::<Vec<_>>().join(".");

    // A bare public suffix (`com`, `co.uk`) is not registrable; the candidate
    // must be a public suffix plus at least one more label.
    let public_suffix = psl::List.suffix(candidate.as_bytes());
    if public_suffix.map(|s| s.as_bytes()) == Some(candidate.as_bytes()) {
        return None;
    }
    Some(candidate)
}

/// Computes the alias an upstream's endpoints imply, when one can be derived.
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }

    // All endpoints must share a scheme/port profile, otherwise no single alias
    // can identify the pool.
    let first = &endpoints[0];
    let port = first.effective_port();
    let scheme = first.scheme;
    if endpoints
        .iter()
        .any(|e| e.scheme != scheme || e.effective_port() != port)
    {
        return None;
    }

    let mut hosts: Vec<String> = Vec::with_capacity(endpoints.len());
    let mut ip_count = 0_usize;
    for endpoint in endpoints {
        if endpoint.ip().is_some() {
            ip_count += 1;
        }
        {
            let h = normalize_hostname(&endpoint.host)?;
            hosts.push(h)
        }
    }
    if ip_count > 0 {
        return None;
    }

    let port_suffix = if is_standard_port(scheme, port) {
        None
    } else {
        Some(port)
    };

    if hosts.len() == 1 {
        return Some(match port_suffix {
            Some(p) => format!("{}:{p}", hosts[0]),
            None => hosts[0].clone(),
        });
    }

    let unique: std::collections::BTreeSet<&String> = hosts.iter().collect();
    if unique.len() == 1 {
        // A single distinct hostname repeated with different ports is not
        // representable as one alias.
        return None;
    }

    let suffix = common_domain_suffix(&hosts)?;
    Some(match port_suffix {
        Some(p) => format!("{suffix}:{p}"),
        None => suffix,
    })
}

/// Result of alias reconciliation on create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasDecision {
    /// The caller supplied no alias and the endpoints derive one.
    Derived(String),
    /// The caller supplied the exact derived alias (idempotent no-op).
    SuppliedMatchesDerived(String),
    /// The caller supplied an explicit alias for a non-derivable endpoint pool.
    Explicit(String),
}

/// Reconciles a user-provided alias with the endpoints on create.
///
/// # Errors
///
/// Returns a 400 problem when a hostname-based pool is given a different
/// alias, or when a non-derivable pool omits one.
pub fn enforce_alias_create(
    endpoints: &[Endpoint],
    alias: Option<&str>,
) -> Result<AliasDecision, crate::domain::error::OagwError> {
    let normalized = alias.map(str::trim).filter(|a| !a.is_empty());
    let derived = compute_derived_alias(endpoints);

    let supplied = normalized.and_then(normalized_alias);

    match (derived, supplied) {
        (Some(d), None) => Ok(AliasDecision::Derived(d)),
        (Some(d), Some(supplied)) => {
            if supplied == d {
                Ok(AliasDecision::SuppliedMatchesDerived(d))
            } else {
                Err(crate::domain::error::OagwError::validation(format!(
                    "alias {supplied:?} does not match the derived alias {d:?} for these endpoints; \
                     omit the alias or match it exactly"
                )))
            }
        }
        (None, Some(supplied)) => Ok(AliasDecision::Explicit(supplied)),
        (None, None) => Err(crate::domain::error::OagwError::validation(
            "alias is required: these endpoints do not imply one (IP addresses, heterogeneous \
             hostnames, or a bare public suffix)",
        )),
    }
}

/// Normalizes a caller-provided alias.
fn normalized_alias(alias: &str) -> Option<String> {
    let trimmed = alias.trim().trim_end_matches('.').to_ascii_lowercase();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Enforces the alias transition table when endpoints change on update.
///
/// * derivable → derivable is allowed only when the recomputed alias equals the
///   existing one
/// * non-derivable → non-derivable retains the existing alias
/// * derivable → non-derivable is rejected outright
///
/// # Errors
///
/// Returns a 400 problem describing which transition was rejected.
pub fn enforce_alias_update(
    existing_alias: &str,
    endpoints: &[Endpoint],
    previous_derivable: bool,
    supplied_alias: Option<&str>,
) -> Result<(), crate::domain::error::OagwError> {
    if let Some(supplied) = normalized_alias(supplied_alias.unwrap_or(existing_alias))
        && supplied != existing_alias
    {
        return Err(crate::domain::error::OagwError::validation(format!(
            "alias is immutable once set (routing key {existing_alias:?}); delete and re-create \
             the upstream to change it"
        )));
    }

    match compute_derived_alias(endpoints) {
        Some(derived) if derived == existing_alias => Ok(()),
        Some(derived) => Err(crate::domain::error::OagwError::validation(format!(
            "endpoint change would change the derived alias from {existing_alias:?} to \
             {derived:?}; the alias is the routing key, so delete and re-create the upstream"
        ))),
        None if !previous_derivable => Ok(()),
        None => Err(crate::domain::error::OagwError::validation(
            "endpoint change leaves the upstream non-derivable; delete and re-create it",
        )),
    }
}

/// Formats the full GTS identifier of an upstream from its UUID.
#[must_use]
pub fn upstream_id(uuid: &uuid::Uuid) -> String {
    format!("{}{uuid}", gts::UPSTREAM_TYPE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::Scheme;

    fn endpoint(scheme: Scheme, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port: Some(port),
        }
    }

    #[test]
    fn a_single_hostname_derives_its_own_name_on_a_standard_port() {
        let endpoints = [endpoint(Scheme::Https, "api.openai.com", 443)];
        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("api.openai.com")
        );
    }

    #[test]
    fn a_non_standard_port_is_kept_in_the_alias() {
        let endpoints = [endpoint(Scheme::Https, "api.openai.com", 8443)];
        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("api.openai.com:8443")
        );
    }

    #[test]
    fn the_standard_port_depends_on_the_scheme() {
        let http = [endpoint(Scheme::Http, "api.openai.com", 80)];
        assert_eq!(
            compute_derived_alias(&http).as_deref(),
            Some("api.openai.com")
        );
        let http_on_tls_port = [endpoint(Scheme::Http, "api.openai.com", 443)];
        assert_eq!(
            compute_derived_alias(&http_on_tls_port).as_deref(),
            Some("api.openai.com:443")
        );
    }

    #[test]
    fn a_hostname_pool_derives_the_registrable_common_suffix() {
        let endpoints = [
            endpoint(Scheme::Https, "us.vendor.com", 443),
            endpoint(Scheme::Https, "eu.vendor.com", 443),
        ];
        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("vendor.com")
        );
    }

    #[test]
    fn a_deeper_pool_derives_the_longest_common_suffix() {
        let endpoints = [
            endpoint(Scheme::Https, "api.eu.vendor.com", 443),
            endpoint(Scheme::Https, "web.eu.vendor.com", 443),
        ];
        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("eu.vendor.com")
        );
    }

    #[test]
    fn a_pool_sharing_only_a_public_suffix_is_not_derivable() {
        let endpoints = [
            endpoint(Scheme::Https, "foo.co.uk", 443),
            endpoint(Scheme::Https, "bar.co.uk", 443),
        ];
        assert_eq!(compute_derived_alias(&endpoints), None);
    }

    #[test]
    fn a_pool_without_a_common_suffix_is_not_derivable() {
        let endpoints = [
            endpoint(Scheme::Https, "us.foo.com", 443),
            endpoint(Scheme::Https, "eu.bar.com", 443),
        ];
        assert_eq!(compute_derived_alias(&endpoints), None);
    }

    #[test]
    fn ip_endpoints_are_never_derivable() {
        let endpoints = [endpoint(Scheme::Http, "10.0.1.1", 8080)];
        assert_eq!(compute_derived_alias(&endpoints), None);
    }

    #[test]
    fn a_mixed_pool_is_not_derivable() {
        let endpoints = [
            endpoint(Scheme::Https, "us.vendor.com", 443),
            endpoint(Scheme::Https, "eu.vendor.com", 8443),
        ];
        assert_eq!(compute_derived_alias(&endpoints), None);
    }

    #[test]
    fn an_empty_endpoint_list_derives_nothing() {
        assert_eq!(compute_derived_alias(&[]), None);
    }

    #[test]
    fn a_supplied_alias_must_equal_the_derived_one() {
        let endpoints = [endpoint(Scheme::Https, "api.openai.com", 443)];
        let exact = enforce_alias_create(&endpoints, Some("api.openai.com")).expect("exact");
        assert_eq!(
            exact,
            AliasDecision::SuppliedMatchesDerived("api.openai.com".to_owned())
        );
        assert!(enforce_alias_create(&endpoints, Some("other")).is_err());
        let derived = enforce_alias_create(&endpoints, None).expect("derived");
        assert_eq!(derived, AliasDecision::Derived("api.openai.com".to_owned()));
    }

    #[test]
    fn a_non_derivable_pool_takes_an_explicit_alias_and_rejects_nothing_else() {
        let endpoints = [endpoint(Scheme::Http, "127.0.0.1", 8080)];
        assert_eq!(
            enforce_alias_create(&endpoints, Some("Loopback.")).expect("explicit"),
            AliasDecision::Explicit("loopback".to_owned())
        );
        assert!(enforce_alias_create(&endpoints, None).is_err());
    }
}
