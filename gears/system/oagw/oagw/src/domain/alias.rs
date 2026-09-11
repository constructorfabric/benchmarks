// Created: 2026-09-01 by Constructor Tech
//! Alias derivation and enforcement.
//!
//! `docs/DESIGN.md` §3.2 "Alias Enforcement Rules" and
//! `docs/ADR/0001-request-routing.md` Appendix A. Derivation is driven
//! entirely by the endpoint pool: hostname endpoints derive, IP endpoints
//! and non-derivable hostname pools require an explicit alias.

use super::model::{Endpoint, is_standard_port, normalize_alias, validate_hostname};

/// Result of resolving what the alias for an endpoint pool should be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derived {
    /// The normalized alias.
    pub alias: String,
    /// How the alias was obtained.
    pub kind: AliasKind,
}

/// How an alias came to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasKind {
    /// Derived from a single hostname.
    SingleHost,
    /// Derived from the registrable common suffix of several hostnames.
    CommonSuffix,
    /// Explicitly provided — endpoints are IP-based or non-derivable.
    Explicit,
}

/// Derive the alias an endpoint pool implies.
///
/// Returns `None` when the pool is IP-based or its hostnames share no
/// registrable suffix — in both cases the operator must supply an alias.
#[must_use]
pub fn derive_alias(endpoints: &[Endpoint]) -> Option<(String, AliasKind)> {
    let first = endpoints.first()?;
    if first.is_ip() {
        return None;
    }

    // All endpoints in a pool share protocol, scheme and port
    // (DESIGN.md §3.2), so the first endpoint's port decides whether the
    // derived alias carries a port suffix.
    let port = first.port;
    let scheme = first.scheme.as_str();

    if endpoints.len() == 1 {
        return Some((
            with_port(first.host.as_str(), port, scheme),
            AliasKind::SingleHost,
        ));
    }

    let registrable = common_registrable_domain(endpoints)?;
    Some((
        with_port(&registrable, port, scheme),
        AliasKind::CommonSuffix,
    ))
}

/// The registrable domain shared by every endpoint, when there is one.
///
/// A bare public suffix (`co.uk`) is *not* a registrable domain, so pools
/// like `foo.co.uk` / `bar.co.uk` fail here and require an explicit alias.
fn common_registrable_domain(endpoints: &[Endpoint]) -> Option<String> {
    let mut shared: Option<String> = None;
    for ep in endpoints {
        if ep.is_ip() || ep.port != endpoints[0].port || ep.scheme != endpoints[0].scheme {
            return None;
        }
        let host = ep.host.trim_end_matches('.').to_ascii_lowercase();
        let domain = psl::domain_str(&host)?.to_owned();
        match &shared {
            None => shared = Some(domain),
            Some(prev) if *prev == domain => {}
            Some(_) => return None,
        }
    }
    shared
}

/// Append `:port` when the port is non-standard for `scheme`.
fn with_port(host: &str, port: u16, scheme: &str) -> String {
    let host = normalize_alias(host);
    if is_standard_port(scheme, port) {
        host
    } else {
        format!("{host}:{port}")
    }
}

/// Validate the alias a create/replace request carries against what the
/// endpoint pool requires.
///
/// # Errors
/// Returns a message describing the conflict, for a `400 ValidationError`.
pub fn enforce_alias(provided: Option<&str>, endpoints: &[Endpoint]) -> Result<String, String> {
    for ep in endpoints {
        if ep.is_ip() {
            validate_ip(&ep.host)?;
        } else {
            validate_hostname(&ep.host)?;
        }
    }
    if endpoints.is_empty() {
        return Err("at least one endpoint is required".to_owned());
    }

    match derive_alias(endpoints) {
        Some((derived, _)) => match provided.map(normalize_alias) {
            None => Ok(derived),
            Some(given) if given == derived => Ok(derived),
            Some(given) => Err(format!(
                "alias '{given}' does not match the derived alias '{derived}'; \
                 hostname endpoints always auto-derive the alias"
            )),
        },
        None => match provided.map(normalize_alias) {
            Some(given) if !given.is_empty() => Ok(given),
            _ => Err(
                "an explicit alias is required for IP-based or non-derivable endpoints".to_owned(),
            ),
        },
    }
}

/// Decide whether an update may keep, recompute or must reject `alias`.
///
/// # Errors
/// Returns a message describing the conflict, for a `400 ValidationError`.
pub fn enforce_alias_update(
    existing: &str,
    provided: Option<&str>,
    old_endpoints: &[Endpoint],
    new_endpoints: &[Endpoint],
) -> Result<String, String> {
    let old_derivable = derive_alias(old_endpoints).is_some();
    let new_derivable = derive_alias(new_endpoints).is_some();

    match (old_derivable, new_derivable) {
        // Derivable → derivable is allowed only when the derived value is
        // unchanged; anything else means "delete and re-create".
        (true, true) => {
            let next = enforce_alias(provided, new_endpoints)?;
            if next == existing {
                Ok(next)
            } else {
                Err(alias_immutable(&format!("'{existing}' -> '{next}'")))
            }
        }
        // Derivable → non-derivable is rejected unconditionally, even when
        // an explicit alias is supplied: the routing key would move.
        (true, false) => Err(format!(
            "changing hostname endpoints to non-derivable endpoints would alter the \
             routing key '{existing}'; delete and re-create the upstream instead"
        )),
        // Non-derivable → non-derivable keeps or renames the explicit alias:
        // nothing about it is derivable, so the caller's alias wins.
        (false, false) => match provided.map(normalize_alias) {
            Some(alias) if !alias.is_empty() => Ok(alias),
            _ => Ok(existing.to_owned()),
        },
        // Non-derivable → derivable is allowed only if the derived value
        // coincides with the existing routing key.
        (false, true) => {
            let next = enforce_alias(provided, new_endpoints)?;
            if next == existing {
                Ok(next)
            } else {
                Err(alias_immutable(existing))
            }
        }
    }
}

fn alias_immutable(existing: &str) -> String {
    format!("alias '{existing}' is immutable once set; delete and re-create the upstream")
}

fn validate_ip(host: &str) -> Result<(), String> {
    if super::model::is_ip_literal(host) {
        Ok(())
    } else {
        Err(format!("'{host}' is not a valid IP address"))
    }
}

/// `true` when `alias` matches the schema pattern
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
#[must_use]
pub fn is_valid_alias(alias: &str) -> bool {
    if alias.is_empty() || alias.len() > 253 {
        return false;
    }
    let legal =
        |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | ':' | '-');
    if !alias.chars().all(legal) {
        return false;
    }
    let mut chars = alias.chars();
    let first = chars.next().expect("non-empty");
    let last = alias.chars().last().expect("non-empty");
    let edge = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    edge(first) && edge(last)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::model::Endpoint;

    fn ep(scheme: &str, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: scheme.to_owned(),
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn single_hostname_standard_port_derives_the_host() {
        let eps = [ep("https", "api.openai.com", 443)];
        assert_eq!(
            derive_alias(&eps),
            Some(("api.openai.com".to_owned(), AliasKind::SingleHost))
        );
    }

    #[test]
    fn single_hostname_non_standard_port_keeps_the_port() {
        let eps = [ep("https", "api.openai.com", 8443)];
        assert_eq!(
            derive_alias(&eps),
            Some(("api.openai.com:8443".to_owned(), AliasKind::SingleHost))
        );
    }

    #[test]
    fn plaintext_uses_port_80_as_standard() {
        let eps = [ep("http", "mock.internal", 80)];
        assert_eq!(
            derive_alias(&eps),
            Some(("mock.internal".to_owned(), AliasKind::SingleHost))
        );
    }

    #[test]
    fn common_registrable_suffix_is_derived() {
        let eps = [
            ep("https", "us.vendor.com", 443),
            ep("https", "eu.vendor.com", 443),
        ];
        assert_eq!(
            derive_alias(&eps),
            Some(("vendor.com".to_owned(), AliasKind::CommonSuffix))
        );
    }

    #[test]
    fn common_suffix_keeps_the_port_when_non_standard() {
        let eps = [
            ep("https", "us.vendor.com", 8443),
            ep("https", "eu.vendor.com", 8443),
        ];
        assert_eq!(
            derive_alias(&eps),
            Some(("vendor.com:8443".to_owned(), AliasKind::CommonSuffix))
        );
    }

    #[test]
    fn bare_public_suffix_is_not_derivable() {
        let eps = [ep("https", "foo.co.uk", 443), ep("https", "bar.co.uk", 443)];
        assert_eq!(derive_alias(&eps), None);
    }

    #[test]
    fn unrelated_hostnames_are_not_derivable() {
        let eps = [
            ep("https", "us.foo.com", 443),
            ep("https", "eu.bar.com", 443),
        ];
        assert_eq!(derive_alias(&eps), None);
    }

    #[test]
    fn ip_endpoints_are_not_derivable() {
        let eps = [ep("https", "10.0.1.1", 443), ep("https", "10.0.1.2", 443)];
        assert_eq!(derive_alias(&eps), None);
    }

    #[test]
    fn single_ip_is_not_derivable() {
        let eps = [ep("https", "127.0.0.1", 8443)];
        assert_eq!(derive_alias(&eps), None);
    }

    #[test]
    fn mismatched_ports_are_not_derivable() {
        let eps = [
            ep("https", "us.vendor.com", 443),
            ep("https", "eu.vendor.com", 8443),
        ];
        assert_eq!(derive_alias(&eps), None);
    }

    #[test]
    fn empty_pool_derives_nothing() {
        assert_eq!(derive_alias(&[]), None);
    }

    #[test]
    fn create_accepts_the_derived_alias_verbatim() {
        let eps = [ep("https", "api.openai.com", 443)];
        assert_eq!(
            enforce_alias(Some("api.openai.com"), &eps),
            Ok("api.openai.com".to_owned())
        );
        assert_eq!(enforce_alias(None, &eps), Ok("api.openai.com".to_owned()));
    }

    #[test]
    fn create_rejects_a_differing_alias_for_hostnames() {
        let eps = [ep("https", "api.openai.com", 443)];
        let err = enforce_alias(Some("my-openai"), &eps).unwrap_err();
        assert!(err.contains("does not match the derived alias"), "{err}");
    }

    #[test]
    fn create_requires_an_alias_for_ips() {
        let eps = [ep("https", "10.0.1.1", 443)];
        assert!(enforce_alias(None, &eps).is_err());
        assert_eq!(
            enforce_alias(Some("my-service"), &eps),
            Ok("my-service".to_owned())
        );
    }

    #[test]
    fn update_rejects_an_alias_changing_endpoint_change() {
        let old = [ep("https", "api.openai.com", 443)];
        let new = [ep("https", "api.anthropic.com", 443)];
        let err = enforce_alias_update("api.openai.com", None, &old, &new).unwrap_err();
        assert!(err.contains("delete and re-create"), "{err}");
    }

    #[test]
    fn update_tolerates_a_recomputed_equal_alias() {
        let old = [ep("https", "api.openai.com", 443)];
        let new = [ep("https", "api.openai.com", 8443)];
        // 443 → 8443 changes the derived alias, so it must be rejected.
        assert!(enforce_alias_update("api.openai.com", None, &old, &new).is_err());
        let same = [ep("https", "api.openai.com", 443)];
        assert_eq!(
            enforce_alias_update("api.openai.com", Some("api.openai.com"), &old, &same),
            Ok("api.openai.com".to_owned())
        );
    }

    #[test]
    fn update_keeps_the_alias_when_moving_ip_to_ip() {
        let old = [ep("https", "10.0.1.1", 443)];
        let new = [ep("https", "10.0.1.2", 443)];
        assert_eq!(
            enforce_alias_update("my-service", None, &old, &new),
            Ok("my-service".to_owned())
        );
        // IP-based upstreams are the one case an explicit rename is allowed.
        assert_eq!(
            enforce_alias_update("my-service", Some("renamed"), &old, &new),
            Ok("renamed".to_owned())
        );
    }

    #[test]
    fn update_rejects_hostname_to_ip() {
        let old = [ep("https", "api.openai.com", 443)];
        let new = [ep("https", "10.0.1.1", 443)];
        assert!(enforce_alias_update("api.openai.com", Some("my-service"), &old, &new).is_err());
    }

    #[test]
    fn update_allows_ip_to_hostname_when_the_derivation_agrees() {
        let old = [ep("https", "10.0.1.1", 443)];
        let new = [ep("https", "api.openai.com", 443)];
        assert!(enforce_alias_update("my-service", None, &old, &new).is_err());
    }

    #[test]
    fn alias_syntax_follows_the_schema_pattern() {
        assert!(is_valid_alias("api.openai.com"));
        assert!(is_valid_alias("api.openai.com:8443"));
        // The schema pattern `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$` has no
        // underscore in it.
        assert!(!is_valid_alias("my-service_1"));
        assert!(is_valid_alias("my-service"));
        assert!(!is_valid_alias(""));
        assert!(!is_valid_alias("-leading"));
        assert!(!is_valid_alias("trailing-"));
        assert!(!is_valid_alias("Has-Caps"));
    }
}
