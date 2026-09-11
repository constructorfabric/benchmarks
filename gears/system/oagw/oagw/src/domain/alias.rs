//! Alias derivation and resolution.
//!
//! Realizes `cpt-cf-oagw-algo-um-derive-alias`.
//!
//! An alias is derived from the endpoint pool when every endpoint names a
//! hostname: the host is ASCII-lowercased and a trailing dot stripped. When the
//! pool is not derivable — an IP literal, or hosts that disagree — the caller
//! must supply the alias. A caller-supplied alias that differs from a derivable
//! value is rejected; one that matches exactly is an idempotent no-op.

use crate::domain::error::DomainError;
use crate::domain::model::Endpoint;

/// Normalize a hostname for use as an alias.
#[must_use]
pub fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Whether a host is an IP literal rather than a name.
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.parse::<std::net::IpAddr>().is_ok()
}

/// Whether an alias satisfies the schema's pattern
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
#[must_use]
pub fn is_valid_alias(alias: &str) -> bool {
    if alias.is_empty() {
        return false;
    }
    let ok_inner = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | ':' | '-');
    let ok_edge = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let mut chars = alias.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !ok_edge(first) {
        return false;
    }
    if alias.len() == 1 {
        return true;
    }
    let last = alias.chars().last().unwrap_or(first);
    if !ok_edge(last) {
        return false;
    }
    alias.chars().all(ok_inner)
}

/// The alias derivable from an endpoint pool, when one exists.
///
/// Returns `None` when the pool contains an IP literal or the hosts disagree,
/// in which case the caller must supply the alias explicitly.
#[must_use]
pub fn derive(endpoints: &[Endpoint]) -> Option<String> {
    let first = endpoints.first()?;
    if is_ip_literal(&first.host) {
        return None;
    }
    let candidate = normalize_host(&first.host);
    if candidate.is_empty() {
        return None;
    }
    for e in endpoints.iter().skip(1) {
        if is_ip_literal(&e.host) || normalize_host(&e.host) != candidate {
            return None;
        }
    }
    Some(candidate)
}

/// Resolve the effective alias for a write.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the pool is not derivable and no
/// alias was supplied, when a supplied alias contradicts a derivable one, or
/// when a supplied alias is malformed.
// @cpt-begin:cpt-cf-oagw-dod-um-alias-derivation:p1:inst-full
pub fn resolve(endpoints: &[Endpoint], supplied: Option<&str>) -> Result<String, DomainError> {
    let derived = derive(endpoints);
    match (derived, supplied) {
        // Derivable, nothing supplied: use the derived value.
        (Some(d), None) => Ok(d),
        // Derivable and supplied: an exact match is an idempotent no-op,
        // anything else is rejected.
        (Some(d), Some(s)) => {
            let s = normalize_host(s);
            if s == d {
                Ok(d)
            } else {
                Err(DomainError::validation(
                    "alias",
                    format!(
                        "alias is derived from the endpoint host and cannot be overridden; \
                         expected `{d}`, got `{s}`"
                    ),
                ))
            }
        }
        // Not derivable: the caller must supply a well-formed alias.
        (None, Some(s)) => {
            let s = normalize_host(s);
            if is_valid_alias(&s) {
                Ok(s)
            } else {
                Err(DomainError::validation(
                    "alias",
                    "alias must match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$",
                ))
            }
        }
        (None, None) => Err(DomainError::validation(
            "alias",
            "alias is required when it cannot be derived from the endpoint host",
        )),
    }
}
// @cpt-end:cpt-cf-oagw-dod-um-alias-derivation:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::Scheme;

    fn ep(host: &str) -> Endpoint {
        Endpoint {
            scheme: Scheme::Https,
            host: host.to_owned(),
            port: None,
        }
    }

    #[test]
    fn derives_from_a_hostname_endpoint() {
        assert_eq!(derive(&[ep("Example.COM.")]).as_deref(), Some("example.com"));
    }

    #[test]
    fn an_ip_literal_pool_is_not_derivable() {
        assert!(derive(&[ep("10.0.0.1")]).is_none());
        assert!(derive(&[ep("::1")]).is_none());
    }

    #[test]
    fn disagreeing_hosts_are_not_derivable() {
        assert!(derive(&[ep("a.example.com"), ep("b.example.com")]).is_none());
    }

    #[test]
    fn identical_hosts_across_endpoints_still_derive() {
        assert_eq!(
            derive(&[ep("example.com"), ep("example.com")]).as_deref(),
            Some("example.com")
        );
    }

    #[test]
    fn resolve_uses_the_derived_value_when_none_supplied() {
        assert_eq!(resolve(&[ep("example.com")], None).unwrap(), "example.com");
    }

    #[test]
    fn an_exactly_matching_supplied_alias_is_accepted() {
        assert_eq!(
            resolve(&[ep("example.com")], Some("example.com")).unwrap(),
            "example.com"
        );
        // Case and trailing dot normalize before the comparison.
        assert_eq!(
            resolve(&[ep("example.com")], Some("Example.COM.")).unwrap(),
            "example.com"
        );
    }

    #[test]
    fn a_contradicting_supplied_alias_is_rejected() {
        let err = resolve(&[ep("example.com")], Some("other")).unwrap_err();
        match err {
            DomainError::Validation { field, .. } => assert_eq!(field, "alias"),
            other => panic!("expected validation error, got {other:?}"),
        }
    }

    #[test]
    fn an_ip_pool_requires_a_supplied_alias() {
        assert!(resolve(&[ep("10.0.0.1")], None).is_err());
        assert_eq!(resolve(&[ep("10.0.0.1")], Some("billing")).unwrap(), "billing");
    }

    #[test]
    fn a_malformed_supplied_alias_is_rejected() {
        assert!(resolve(&[ep("10.0.0.1")], Some("-bad-")).is_err());
        assert!(resolve(&[ep("10.0.0.1")], Some("UPPER")).is_ok()); // normalized first
    }

    #[test]
    fn alias_pattern_accepts_documented_shapes() {
        assert!(is_valid_alias("a"));
        assert!(is_valid_alias("example.com"));
        assert!(is_valid_alias("host-1:8080"));
        assert!(!is_valid_alias(""));
        assert!(!is_valid_alias("-lead"));
        assert!(!is_valid_alias("trail-"));
        assert!(!is_valid_alias("Upper"));
        assert!(!is_valid_alias("has space"));
    }
}
