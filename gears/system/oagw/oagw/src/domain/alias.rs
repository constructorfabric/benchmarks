//! Alias derivation and update enforcement (`DESIGN.md` §3.3).
//!
//! Aliases are the proxy routing key, so they are not free-form labels: a
//! hostname upstream *must* carry the alias its endpoints derive, and an
//! upstream whose endpoints cannot be derived (IP literals, heterogeneous
//! pools, bare-public-suffix pools) *must* be given an explicit alias.
//! Derivation of a multi-host pool uses the Public Suffix List to find a
//! registrable common suffix.
use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, validate_alias};

/// Suffix separator between a host and its port inside an alias.
const PORT_SEP: char = ':';

/// Derive the alias an endpoint pool implies, or `None` when the pool is not
/// derivable and an explicit alias is required.
///
/// * one hostname endpoint → the hostname itself (`api.openai.com:443` →
///   `api.openai.com`, `api.openai.com:8443` → `api.openai.com:8443`);
/// * several hostname endpoints → their registrable common suffix, validated
///   against the Public Suffix List (`us.vendor.com` + `eu.vendor.com` →
///   `vendor.com`);
/// * anything else (any IP literal, mixed pools, pools whose only common
///   suffix is a bare public suffix such as `co.uk`) → `None`.
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }
    if endpoints.iter().any(|endpoint| endpoint.is_ip_literal()) {
        return None;
    }
    let host = endpoints[0].host.as_str();
    let port = endpoints[0].port;
    if endpoints.len() == 1 {
        return Some(with_port(host, port, endpoints[0].scheme));
    }
    // All endpoints of a pool must agree on scheme and port, so any one of
    // them carries the port the alias must preserve.
    let mut registrable: Vec<&str> = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        if endpoint.port != port {
            return None;
        }
        // A bare public suffix (`co.uk`) yields no registrable domain at all,
        // which is exactly the "derivation rejected" case from the table.
        registrable.push(psl::domain_str(&endpoint.host)?);
    }
    let first = registrable[0];
    if !registrable.iter().all(|domain| *domain == first) {
        return None;
    }
    Some(with_port(first, port, endpoints[0].scheme))
}

/// Append `:port` when the port is not the scheme's standard one.
fn with_port(host: &str, port: u16, scheme: crate::domain::model::EndpointScheme) -> String {
    if port == scheme.standard_port() {
        host.to_owned()
    } else {
        format!("{host}{PORT_SEP}{port}")
    }
}

/// `true` when an alias was derived rather than supplied by the operator.
#[must_use]
pub fn is_derived(upstream: &crate::domain::model::Upstream) -> bool {
    compute_derived_alias(&upstream.server.endpoints)
        .is_some_and(|derived| derived == upstream.alias)
}

/// Resolve the alias of a *new* upstream from its endpoints and the caller's
/// optional alias.
///
/// * derivable endpoints: the derived value wins; an exact match is tolerated
///   for idempotency, any other value is a 400;
/// * non-derivable endpoints: an explicit alias is required.
///
/// # Errors
///
/// [`DomainError::AliasRule`] when the caller's alias disagrees with the
/// derivation, and [`DomainError::validation`] when an alias is required but
/// absent or malformed.
pub fn resolve_alias_for_create(
    endpoints: &[Endpoint],
    provided: Option<&str>,
) -> Result<String, DomainError> {
    let derived = compute_derived_alias(endpoints);
    match (derived, provided.map(str::trim)) {
        (Some(derived), None) => Ok(derived),
        (Some(derived), Some(provided)) => {
            let normalized = normalize(provided);
            if normalized == derived {
                Ok(derived)
            } else {
                Err(DomainError::field(
                    "alias",
                    crate::domain::reason::ALIAS_FORMAT,
                    format!(
                        "alias is auto-derived for hostname-based upstreams: expected '{derived}'"
                    ),
                ))
            }
        }
        (None, Some(provided)) => {
            let normalized = normalize(provided);
            validate_alias(&normalized)?;
            Ok(normalized)
        }
        (None, None) => Err(DomainError::field(
            "alias",
            crate::domain::reason::MISSING,
            "alias is required for IP-based or non-derivable endpoint pools",
        )),
    }
}

/// Enforce the alias update table from `DESIGN.md` §3.3.
///
/// `existing` carries the stored upstream (and therefore its current alias
/// and endpoints); `endpoints` are the replacement pool; `provided` is the
/// caller-supplied alias, when any.
///
/// # Errors
///
/// [`DomainError::AliasRule`] whenever the transition would change the
/// routing key.
pub fn enforce_alias_update(
    existing: &crate::domain::model::Upstream,
    endpoints: &[Endpoint],
    provided: Option<&str>,
) -> Result<(), DomainError> {
    let was_derivable = compute_derived_alias(&existing.server.endpoints).is_some();
    let derived_now = compute_derived_alias(endpoints);
    match (was_derivable, derived_now) {
        // Derivable -> derivable, and the derivation still matches.
        (true, Some(derived)) if derived == existing.alias => Ok(()),
        // Derivable -> derivable with a different result: the routing key
        // would move, so the operator must delete and re-create.
        (true, Some(derived)) => Err(DomainError::AliasRule {
            detail: format!(
                "alias is immutable: changing the endpoints would change the derived alias from '{}' to '{}'; delete and re-create the upstream",
                existing.alias, derived
            ),
        }),
        // Derivable -> non-derivable: rejected always, even with an alias.
        (true, None) => Err(DomainError::AliasRule {
            detail: format!(
                "alias is immutable: '{}' was derived from hostname endpoints and cannot survive an IP-based or non-derivable endpoint pool; delete and re-create the upstream",
                existing.alias
            ),
        }),
        // Non-derivable -> derivable: allowed only when the derivation lands
        // on the stored alias.
        (false, Some(derived)) if derived == existing.alias => Ok(()),
        (false, Some(derived)) => Err(DomainError::AliasRule {
            detail: format!(
                "alias is immutable: the new endpoint pool derives '{}' but this upstream is registered as '{}'; delete and re-create the upstream",
                derived, existing.alias
            ),
        }),
        // Non-derivable -> non-derivable: the stored alias is retained and a
        // differing caller-supplied alias is refused.
        (false, None) => {
            if let Some(provided) = provided.map(str::trim).map(normalize)
                && provided != existing.alias
            {
                return Err(DomainError::AliasRule {
                    detail: format!(
                        "alias is immutable for IP-based endpoints: '{}' cannot be renamed to '{provided}'",
                        existing.alias
                    ),
                });
            }
            // Same class of endpoints (none of them derivable), so the
            // routing key is unaffected and the pool may be repointed.
            Ok(())
        }
    }
}

/// Normalize an alias: ASCII lower-case, trailing dots stripped, whitespace
/// trimmed. Resolution is case-insensitive (`DESIGN.md` §3.3).
#[must_use]
pub fn normalize(alias: &str) -> String {
    alias
        .trim()
        .trim_end_matches('.')
        .trim()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::EndpointScheme;

    fn endpoint(scheme: EndpointScheme, host: &str, port: Option<u16>) -> Endpoint {
        Endpoint::new(scheme, host, port).expect("valid endpoint")
    }

    #[test]
    fn derives_single_hostname() {
        let endpoints = vec![endpoint(EndpointScheme::Https, "api.openai.com", None)];
        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("api.openai.com")
        );
    }

    #[test]
    fn derives_single_hostname_with_port() {
        let endpoints = vec![endpoint(
            EndpointScheme::Https,
            "api.openai.com",
            Some(8443),
        )];
        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("api.openai.com:8443")
        );
    }

    #[test]
    fn derives_common_registrable_suffix() {
        let endpoints = vec![
            endpoint(EndpointScheme::Https, "us.vendor.com", None),
            endpoint(EndpointScheme::Https, "eu.vendor.com", None),
        ];
        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("vendor.com")
        );
    }

    #[test]
    fn preserves_port_in_multi_host_suffix() {
        let endpoints = vec![
            endpoint(EndpointScheme::Https, "us.vendor.com", Some(8443)),
            endpoint(EndpointScheme::Https, "eu.vendor.com", Some(8443)),
        ];
        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("vendor.com:8443")
        );
    }

    #[test]
    fn rejects_bare_public_suffix_pool() {
        let endpoints = vec![
            endpoint(EndpointScheme::Https, "foo.co.uk", None),
            endpoint(EndpointScheme::Https, "bar.co.uk", None),
        ];
        assert_eq!(compute_derived_alias(&endpoints), None);
    }

    #[test]
    fn rejects_unrelated_hostnames() {
        let endpoints = vec![
            endpoint(EndpointScheme::Https, "us.foo.com", None),
            endpoint(EndpointScheme::Https, "eu.bar.com", None),
        ];
        assert_eq!(compute_derived_alias(&endpoints), None);
    }

    #[test]
    fn rejects_ip_endpoints() {
        let endpoints = vec![endpoint(EndpointScheme::Https, "10.0.1.1", None)];
        assert_eq!(compute_derived_alias(&endpoints), None);
        let endpoints = vec![
            endpoint(EndpointScheme::Https, "10.0.1.1", None),
            endpoint(EndpointScheme::Https, "10.0.1.2", None),
        ];
        assert_eq!(compute_derived_alias(&endpoints), None);
    }

    #[test]
    fn create_derives_when_absent() {
        let endpoints = vec![endpoint(EndpointScheme::Https, "api.openai.com", None)];
        assert_eq!(
            resolve_alias_for_create(&endpoints, None).expect("derived"),
            "api.openai.com"
        );
    }

    #[test]
    fn create_tolerates_exact_derived_alias() {
        let endpoints = vec![endpoint(EndpointScheme::Https, "api.openai.com", None)];
        assert_eq!(
            resolve_alias_for_create(&endpoints, Some("api.openai.com")).expect("derived"),
            "api.openai.com"
        );
    }

    #[test]
    fn create_rejects_override_of_derived_alias() {
        let endpoints = vec![endpoint(EndpointScheme::Https, "api.openai.com", None)];
        let error = resolve_alias_for_create(&endpoints, Some("my-service"))
            .expect_err("override must be rejected");
        assert!(error.to_string().contains("auto-derived"), "{error}");
    }

    #[test]
    fn create_requires_alias_for_ip_pool() {
        let endpoints = vec![endpoint(EndpointScheme::Https, "10.0.1.1", None)];
        let error = resolve_alias_for_create(&endpoints, None).expect_err("alias required");
        assert!(error.to_string().contains("alias is required"), "{error}");
        assert_eq!(
            resolve_alias_for_create(&endpoints, Some("My-Service")).expect("explicit"),
            "my-service"
        );
    }

    #[test]
    fn update_allows_unchanged_derivation() {
        let endpoints = vec![endpoint(EndpointScheme::Https, "api.openai.com", None)];
        let upstream = crate::domain::model::Upstream {
            id: uuid::Uuid::new_v4(),
            tenant_id: uuid::Uuid::new_v4(),
            alias: "api.openai.com".to_owned(),
            enabled: true,
            tags: Vec::new(),
            server: crate::domain::model::ServerConfig {
                endpoints: endpoints.clone(),
            },
            protocol: crate::domain::model::Protocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            created_at: 0,
            updated_at: 0,
        };
        assert!(enforce_alias_update(&upstream, &endpoints, None).is_ok());
        assert!(enforce_alias_update(&upstream, &endpoints, Some("api.openai.com")).is_ok());
    }

    #[test]
    fn update_rejects_alias_would_change() {
        let endpoints = vec![endpoint(EndpointScheme::Https, "api.openai.com", None)];
        let mut upstream = crate::domain::model::Upstream {
            id: uuid::Uuid::new_v4(),
            tenant_id: uuid::Uuid::new_v4(),
            alias: "api.openai.com".to_owned(),
            enabled: true,
            tags: Vec::new(),
            server: crate::domain::model::ServerConfig {
                endpoints: endpoints.clone(),
            },
            protocol: crate::domain::model::Protocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            created_at: 0,
            updated_at: 0,
        };
        let replacement = vec![endpoint(EndpointScheme::Https, "api.anthropic.com", None)];
        let error = enforce_alias_update(&upstream, &replacement, None)
            .expect_err("derived alias would change");
        assert!(
            error.to_string().contains("delete and re-create"),
            "{error}"
        );

        // Derivable -> non-derivable is rejected even with an alias.
        let ips = vec![endpoint(EndpointScheme::Https, "10.0.1.1", None)];
        let error = enforce_alias_update(&upstream, &ips, Some("api.openai.com"))
            .expect_err("derivable to non-derivable");
        assert!(error.to_string().contains("immutable"), "{error}");

        // Non-derivable -> derivable with a mismatching derivation.
        upstream.alias = "my-service".to_owned();
        upstream.server = crate::domain::model::ServerConfig {
            endpoints: ips.clone(),
        };
        let error = enforce_alias_update(&upstream, &endpoints, None)
            .expect_err("derived alias differs from stored alias");
        assert!(
            error.to_string().contains("delete and re-create"),
            "{error}"
        );
    }

    #[test]
    fn update_retains_alias_for_ip_pool() {
        let ips = vec![endpoint(EndpointScheme::Https, "10.0.1.1", None)];
        let upstream = crate::domain::model::Upstream {
            id: uuid::Uuid::new_v4(),
            tenant_id: uuid::Uuid::new_v4(),
            alias: "my-service".to_owned(),
            enabled: true,
            tags: Vec::new(),
            server: crate::domain::model::ServerConfig {
                endpoints: ips.clone(),
            },
            protocol: crate::domain::model::Protocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            created_at: 0,
            updated_at: 0,
        };
        assert!(enforce_alias_update(&upstream, &ips, None).is_ok());
        // A differing alias is refused even for an IP pool.
        assert!(enforce_alias_update(&upstream, &ips, Some("other")).is_err());
    }

    #[test]
    fn normalizes_case_and_trailing_dot() {
        assert_eq!(normalize("Api.OpenAI.COM."), "api.openai.com");
    }
}
