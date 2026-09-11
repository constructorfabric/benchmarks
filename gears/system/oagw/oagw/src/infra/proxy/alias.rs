// Updated: 2026-09-01 by Constructor Tech
//! Alias derivation and enforcement.
//!
//! An alias is the stable name a caller uses in `/proxy/{alias}`. OAGW derives
//! it from the endpoints so that an ordinary single-host upstream needs no
//! configuration, and requires it explicitly where the endpoints cannot be
//! summarized by one name.
//!
//! Derivation rules (upstream schema, `alias` field):
//!
//! | Endpoints | Derived alias |
//! |---|---|
//! | one hostname, standard port | `hostname` |
//! | one hostname, non-standard port | `hostname:port` |
//! | ≥2 hostnames sharing a registrable suffix (≥2 labels), standard port | `suffix` |
//! | ≥2 hostnames sharing a registrable suffix (≥2 labels), non-standard port | `suffix:port` |
//! | any IP literal | explicit alias required |
//! | bare public suffix (`co.uk`) | explicit alias required |
//! | no common suffix, or mixed ports | explicit alias required |

use crate::domain::dto::Endpoint;
use crate::domain::error::DomainError;

/// Result of attempting to derive an alias from a set of endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasDerivation {
    /// A name was derivable.
    Derived(String),
    /// The endpoints cannot be summarized; the caller must supply an alias.
    RequiresExplicit(&'static str),
}

/// Normalize a candidate alias: ASCII-lowercase, trailing dots stripped.
#[must_use]
pub fn normalize(raw: &str) -> String {
    raw.trim().trim_end_matches('.').to_ascii_lowercase()
}

impl AliasDerivation {
    /// Whether a name was derivable.
    #[must_use]
    pub fn is_derived(&self) -> bool {
        matches!(self, Self::Derived(_))
    }

    /// Whether the endpoints require an explicit alias.
    #[must_use]
    pub fn is_requires_explicit(&self) -> bool {
        matches!(self, Self::RequiresExplicit(_))
    }
}

fn labels(host: &str) -> Vec<&str> {
    host.split('.').filter(|l| !l.is_empty()).collect()
}

/// Longest suffix of labels shared by every host, or `None` when the hosts
/// share nothing.
fn common_suffix(hosts: &[String]) -> Option<String> {
    // Labels are collected once: the comparison walks every host on every
    // depth step, so an iterator consumed on the first pass would silently
    // compare against nothing and report the whole first host as shared.
    let lists: Vec<Vec<&str>> = hosts.iter().map(|h| labels(h)).collect();
    let first = lists.first()?;
    let mut depth = 0usize;
    'outer: for i in 0..first.len() {
        let expected = first[first.len() - 1 - i];
        for other in &lists[1..] {
            if other.len() <= i || other[other.len() - 1 - i] != expected {
                break 'outer;
            }
        }
        depth += 1;
    }
    if depth == 0 {
        return None;
    }
    Some(first[first.len() - depth..].join("."))
}

/// Attempt to derive an alias for a set of normalized, non-empty endpoints.
#[must_use]
pub fn derive(endpoints: &[Endpoint]) -> AliasDerivation {
    if endpoints.is_empty() {
        return AliasDerivation::RequiresExplicit("no endpoints configured");
    }

    let mut hosts = Vec::with_capacity(endpoints.len());
    for ep in endpoints {
        let host = ep.normalized_host();
        if host.parse::<std::net::IpAddr>().is_ok() {
            return AliasDerivation::RequiresExplicit("IP endpoints require an explicit alias");
        }
        if host.is_empty() {
            return AliasDerivation::RequiresExplicit("endpoints have no host");
        }
        hosts.push(host);
    }

    // Ports must agree before a name can stand for the whole set.
    let ports: Vec<u16> = endpoints.iter().map(Endpoint::effective_port).collect();
    let Some(port) = ports.first().copied() else {
        return AliasDerivation::RequiresExplicit("no endpoints configured");
    };
    if ports.iter().any(|p| *p != port) {
        return AliasDerivation::RequiresExplicit("endpoints use different ports");
    }
    let standard = endpoints
        .iter()
        .all(|e| e.effective_port() == e.scheme.default_port());

    if hosts.len() == 1 {
        let host = &hosts[0];
        // A bare public suffix (`co.uk`) is not a usable routing name. A
        // single label is never treated as one: internal service names have no
        // registrable suffix at all, and the PSL wildcard rule would otherwise
        // make them indistinguishable from `co.uk`.
        if labels(host).len() >= 2 && psl::suffix_str(host) == Some(host.as_str()) {
            return AliasDerivation::RequiresExplicit(
                "host is a bare public suffix; an explicit alias is required",
            );
        }
        return AliasDerivation::Derived(if standard {
            host.clone()
        } else {
            format!("{host}:{port}")
        });
    }

    let Some(suffix) = common_suffix(&hosts) else {
        return AliasDerivation::RequiresExplicit(
            "endpoints do not share a common domain suffix; an explicit alias is required",
        );
    };
    if labels(&suffix).len() < 2 {
        return AliasDerivation::RequiresExplicit(
            "endpoints share only a single-label suffix; an explicit alias is required",
        );
    }
    // The suffix must itself be a registrable domain: at least the public
    // suffix plus one label. `co.uk` alone is not.
    if labels(&suffix).len() >= 2 && psl::suffix_str(&suffix) == Some(suffix.as_str()) {
        return AliasDerivation::RequiresExplicit(
            "endpoints share only a bare public suffix; an explicit alias is required",
        );
    }
    AliasDerivation::Derived(if standard {
        suffix
    } else {
        format!("{suffix}:{port}")
    })
}

/// Resolve the alias an upstream must be stored under.
///
/// * A derived alias is used when the caller supplied none.
/// * A caller-supplied alias must equal the derived value exactly — a
///   mismatch is rejected rather than silently overridden, because the alias
///   is the routing key and two operators must not route to the same upstream
///   under different names.
/// * When nothing is derivable the caller must have supplied one.
///
/// # Errors
///
/// [`DomainError::InvalidField`] on the `alias` field.
pub fn resolve_alias(
    explicit: Option<&str>,
    endpoints: &[Endpoint],
) -> Result<String, DomainError> {
    let derivation = derive(endpoints);
    match (explicit, derivation) {
        (Some(provided), AliasDerivation::Derived(derived)) => {
            let provided = normalize(provided);
            if provided == derived {
                Ok(derived)
            } else {
                Err(DomainError::invalid(
                    "alias",
                    format!(
                        "'{provided}' does not match the alias derived from the endpoints ('{derived}'); omit it or use the derived value"
                    ),
                ))
            }
        }
        (Some(provided), AliasDerivation::RequiresExplicit(_reason)) => {
            let normalized = normalize(provided);
            if !crate::domain::dto::validate_alias_shape(&normalized) {
                return Err(DomainError::invalid(
                    "alias",
                    format!("'{provided}' must match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$"),
                ));
            }
            Ok(normalized)
        }
        (None, AliasDerivation::Derived(derived)) => Ok(derived),
        (None, AliasDerivation::RequiresExplicit(reason)) => Err(DomainError::invalid(
            "alias",
            format!("an explicit alias is required: {reason}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ep(v: serde_json::Value) -> Endpoint {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn single_hostname_standard_port() {
        let eps = vec![ep(json!({ "scheme": "https", "host": "Api.Example.com." }))];
        assert_eq!(
            derive(&eps),
            AliasDerivation::Derived("api.example.com".to_owned())
        );
    }

    #[test]
    fn single_hostname_non_standard_port() {
        let eps = vec![ep(
            json!({ "scheme": "https", "host": "api.example.com", "port": 8443 }),
        )];
        assert_eq!(
            derive(&eps),
            AliasDerivation::Derived("api.example.com:8443".to_owned())
        );
    }

    #[test]
    fn single_http_host_on_port_80() {
        let eps = vec![ep(
            json!({ "scheme": "http", "host": "api.example.com", "port": 80 }),
        )];
        assert_eq!(
            derive(&eps),
            AliasDerivation::Derived("api.example.com".to_owned())
        );
    }

    #[test]
    fn multi_hostname_common_suffix() {
        let eps = vec![
            ep(json!({ "scheme": "https", "host": "us.vendor.com" })),
            ep(json!({ "scheme": "https", "host": "eu.vendor.com" })),
        ];
        assert_eq!(
            derive(&eps),
            AliasDerivation::Derived("vendor.com".to_owned())
        );
    }

    #[test]
    fn multi_hostname_common_suffix_non_standard_port() {
        let eps = vec![
            ep(json!({ "scheme": "https", "host": "us.vendor.com", "port": 8443 })),
            ep(json!({ "scheme": "https", "host": "eu.vendor.com", "port": 8443 })),
        ];
        assert_eq!(
            derive(&eps),
            AliasDerivation::Derived("vendor.com:8443".to_owned())
        );
    }

    #[test]
    fn multi_hostname_with_public_suffix_only() {
        let eps = vec![
            ep(json!({ "scheme": "https", "host": "a.co.uk" })),
            ep(json!({ "scheme": "https", "host": "b.co.uk" })),
        ];
        assert!(matches!(derive(&eps), AliasDerivation::RequiresExplicit(_)));
    }

    #[test]
    fn multi_hostname_without_common_suffix() {
        let eps = vec![
            ep(json!({ "scheme": "https", "host": "a.vendor.com" })),
            ep(json!({ "scheme": "https", "host": "b.other.com" })),
        ];
        assert!(matches!(derive(&eps), AliasDerivation::RequiresExplicit(_)));
    }

    #[test]
    fn bare_public_suffix_host_requires_explicit() {
        let eps = vec![ep(json!({ "scheme": "https", "host": "co.uk" }))];
        assert!(matches!(derive(&eps), AliasDerivation::RequiresExplicit(_)));
    }

    #[test]
    fn ip_endpoints_require_explicit() {
        let eps = vec![ep(json!({ "scheme": "https", "host": "93.184.216.34" }))];
        assert!(matches!(derive(&eps), AliasDerivation::RequiresExplicit(_)));
    }

    #[test]
    fn internal_single_label_hosts_derive() {
        let eps = vec![ep(
            json!({ "scheme": "http", "host": "openai-backend", "port": 9000 }),
        )];
        assert_eq!(
            derive(&eps),
            AliasDerivation::Derived("openai-backend:9000".to_owned())
        );
    }

    #[test]
    fn deeper_common_suffix_is_used() {
        let eps = vec![
            ep(json!({ "scheme": "https", "host": "a.b.vendor.co.uk" })),
            ep(json!({ "scheme": "https", "host": "c.d.vendor.co.uk" })),
        ];
        assert_eq!(
            derive(&eps),
            AliasDerivation::Derived("vendor.co.uk".to_owned())
        );
    }

    #[test]
    fn resolve_accepts_the_derived_alias() {
        let eps = vec![ep(json!({ "scheme": "https", "host": "api.example.com" }))];
        assert_eq!(
            resolve_alias(Some("api.example.com"), &eps).unwrap(),
            "api.example.com"
        );
    }

    #[test]
    fn resolve_rejects_a_mismatched_alias() {
        let eps = vec![ep(json!({ "scheme": "https", "host": "api.example.com" }))];
        let err = resolve_alias(Some("my-upstream"), &eps).unwrap_err();
        assert!(err.to_string().contains("does not match"), "{err}");
    }

    #[test]
    fn resolve_requires_an_alias_for_ips() {
        let eps = vec![ep(json!({ "scheme": "https", "host": "93.184.216.34" }))];
        assert!(resolve_alias(None, &eps).is_err());
        assert_eq!(resolve_alias(Some("vendor-x"), &eps).unwrap(), "vendor-x");
    }

    #[test]
    fn resolve_derives_when_omitted() {
        let eps = vec![ep(json!({ "scheme": "https", "host": "api.example.com" }))];
        assert_eq!(resolve_alias(None, &eps).unwrap(), "api.example.com");
    }

    #[test]
    fn resolve_normalizes_the_input() {
        let eps = vec![ep(json!({ "scheme": "https", "host": "93.184.216.34" }))];
        assert_eq!(
            resolve_alias(Some("  Vendor-X. "), &eps).unwrap(),
            "vendor-x"
        );
    }

    #[test]
    fn empty_endpoints_require_explicit() {
        assert!(matches!(derive(&[]), AliasDerivation::RequiresExplicit(_)));
    }
}
