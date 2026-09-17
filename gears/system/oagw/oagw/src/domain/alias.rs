//! Alias derivation, normalization and validation (DESIGN "Alias Resolution").
//!
//! Alias behavior is determined entirely by endpoint type:
//!
//! * single hostname, standard port → `hostname`
//! * single hostname, non-standard port → `hostname:port`
//! * multiple hostnames sharing a registrable common suffix (≥2 labels, not a
//!   bare public suffix) → `common-suffix[:port]`
//! * IP-based or non-derivable endpoint pools → explicit alias required
//!
//! Aliases are normalized to ASCII lowercase with trailing dots stripped and
//! resolution is case-insensitive.

use std::net::IpAddr;

use crate::domain::models::{Endpoint, Scheme, Upstream};

#[cfg(test)]
use crate::domain::models::ServerConfig;

/// Whether `host` parses as an IP address.
#[must_use]
pub fn is_ip(host: &str) -> bool {
    host.trim_end_matches('.').parse::<IpAddr>().is_ok()
}

/// Validate a hostname per RFC 1123: max 253 characters, each label 1–63
/// characters, ASCII alphanumeric/hyphen, labels cannot start or end with a
/// hyphen. A single trailing dot (FQDN notation) is tolerated and stripped.
#[must_use]
pub fn validate_hostname(host: &str) -> bool {
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    let trimmed = host.trim_end_matches('.');
    if trimmed.is_empty() {
        return false;
    }
    trimmed.split('.').all(|label| {
        let bytes = label.as_bytes();
        !bytes.is_empty()
            && bytes.len() <= 63
            && bytes[0].is_ascii_alphanumeric()
            && bytes[bytes.len() - 1].is_ascii_alphanumeric()
            && bytes
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
    })
}

/// Normalize an alias to ASCII lowercase with trailing dots stripped.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    let trimmed = alias.trim().trim_end_matches('.');
    // ASCII lowercase only — alias grammar is `[a-z0-9.:-]`.
    trimmed
        .bytes()
        .map(|b| b.to_ascii_lowercase() as char)
        .collect()
}

/// Standard port for the given scheme (omitted from derived aliases).
#[must_use]
pub fn is_standard_port(scheme: Scheme, port: u16) -> bool {
    port == scheme.default_port()
}

/// Common domain suffix shared by all hosts, if any (≥2 labels, and not a
/// bare public suffix per the PSL). Returns the suffix itself on success.
#[must_use]
pub fn common_domain_suffix(hosts: &[&str]) -> Option<String> {
    let hostset: Vec<&str> = hosts
        .iter()
        .map(|h| h.trim_end_matches('.'))
        .filter(|h| !is_ip(h) && validate_hostname(h))
        .collect();
    if hostset.len() < 2 || hostset.len() != hosts.len() {
        return None;
    }

    // Longest trailing label sequence common to all hostnames.
    let first_labels: Vec<&str> = hostset[0].split('.').collect();
    let mut candidate: Option<Vec<&str>> = None;
    for i in 0..first_labels.len() {
        let suffix_labels = &first_labels[first_labels.len() - 1 - i..];
        let suffix = suffix_labels.join(".");
        let shared = hostset
            .iter()
            .all(|h| h == &suffix || format!("{h}").ends_with(&format!(".{suffix}")));
        if shared {
            candidate = Some(suffix_labels.to_vec());
        } else {
            break;
        }
    }

    let labels = candidate?;
    if labels.len() < 2 {
        return None;
    }
    let suffix = labels.join(".");
    // Reject bare public suffixes (e.g. `co.uk`): the candidate must itself be
    // a registrable domain, i.e. its public suffix must be strictly shorter.
    if psl::suffix_str(&suffix) == Some(suffix.as_str()) {
        return None;
    }
    Some(suffix)
}

/// Result of endpoint-to-alias analysis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasDerivation {
    /// Derivable automatically (single hostname or registrable common suffix).
    Derived(String),
    /// Not derivable — explicit alias is required (IP-based, heterogeneous,
    /// or bare-public-suffix pools).
    ExplicitRequired,
}

/// Compute the derived alias for an endpoint pool, applying the standard-port
/// rule and the multi-host common-suffix rule.
#[must_use]
pub fn derive_alias(endpoints: &[Endpoint]) -> AliasDerivation {
    if endpoints.is_empty() {
        return AliasDerivation::ExplicitRequired;
    }

    let first_port = endpoints[0].port;
    let first_scheme = endpoints[0].scheme;
    // All endpoints must share scheme and port (DESIGN "Multi-Endpoint
    // Load Balancing"); a pool violating this falls back to explicit - the
    // management validation layer rejects it with a clearer message.
    let homogeneous = endpoints
        .iter()
        .all(|e| e.scheme == first_scheme && e.port == first_port);
    if !homogeneous {
        return AliasDerivation::ExplicitRequired;
    }

    let hosts: Vec<&str> = endpoints.iter().map(|e| e.host.as_str()).collect();

    // Single endpoint (or a homogeneous pool resolved as one host): hostname
    // derivative. IP-only pools always require an explicit alias.
    if hosts.iter().all(|h| h == &hosts[0]) {
        let host = hosts[0].trim_end_matches('.');
        if is_ip(host) {
            return AliasDerivation::ExplicitRequired;
        }
        if !validate_hostname(host) {
            return AliasDerivation::ExplicitRequired;
        }
        let derived = if is_standard_port(first_scheme, first_port) {
            normalize_alias(host)
        } else {
            normalize_alias(host) + &format!(":{first_port}")
        };
        return AliasDerivation::Derived(derived);
    }

    // Multiple distinct hostnames: registrable common suffix (≥2 labels).
    if let Some(suffix) = common_domain_suffix(&hosts) {
        let derived = if is_standard_port(first_scheme, first_port) {
            normalize_alias(&suffix)
        } else {
            normalize_alias(&suffix) + &format!(":{first_port}")
        };
        return AliasDerivation::Derived(derived);
    }

    AliasDerivation::ExplicitRequired
}

/// Validate a user-provided alias against its endpoint pool, enforcing the
/// DESIGN alias rules.
///
/// Returns `Ok(alias)` with the *normalized* alias on success, or a
/// descriptive validation error.
///
/// # Errors
///
/// Returns a `String` describing the validation failure.
pub fn enforce_alias(endpoints: &[Endpoint], user_alias: Option<&str>) -> Result<String, String> {
    let derivation = derive_alias(endpoints);

    match derivation {
        AliasDerivation::Derived(derived) => {
            match user_alias {
                None => Ok(derived),
                Some(user) => {
                    let normalized = normalize_alias(user);
                    if normalized == derived {
                        // Exact-match tolerated silently (idempotent no-op).
                        Ok(derived)
                    } else {
                        Err(format!(
                            "user-provided alias {user:?} differs from the auto-derived value \
                             {derived:?}; alias is enforced for hostname-based endpoints"
                        ))
                    }
                }
            }
        }
        AliasDerivation::ExplicitRequired => match user_alias {
            None => {
                Err("explicit alias is required for IP-based or non-derivable endpoints".to_owned())
            }
            Some(user) => {
                let normalized = normalize_alias(user);
                if normalized.is_empty() {
                    Err("alias must not be empty".to_owned())
                } else {
                    Ok(normalized)
                }
            }
        },
    }
}

/// Enforce the DESIGN "Alias Update Behavior" table during `PUT upstream`.
///
/// Aliases are immutable routing keys; only the cases where the effective
/// alias provably stays the same are allowed. On success the effective
/// (unchanged) alias is returned.
///
/// # Errors
///
/// Returns a `String` describing the validation failure.
pub fn enforce_alias_update(
    existing_alias: &str,
    existing_endpoints: &[Endpoint],
    new_upstream: &Upstream,
) -> Result<String, String> {
    use AliasDerivation::{Derived, ExplicitRequired};

    let old_alias = normalize_alias(existing_alias);
    let old_derivation = derive_alias(existing_endpoints);
    let new_derivation = derive_alias(&new_upstream.server.endpoints);
    let user_alias = new_upstream.alias.as_deref();

    let endpoints_unchanged = existing_endpoints == new_upstream.server.endpoints.as_slice();
    if endpoints_unchanged {
        return match user_alias {
            None => Ok(old_alias),
            Some(user) => {
                let normalized = normalize_alias(user);
                if normalized == old_alias {
                    Ok(old_alias)
                } else {
                    Err(format!(
                        "alias is immutable; existing alias {old_alias:?} must be retained, got {normalized:?}"
                    ))
                }
            }
        };
    }

    match (&old_derivation, &new_derivation) {
        // Derivable → Derivable: allowed only when the recomputed alias is unchanged.
        (Derived(_), Derived(new_derived)) | (ExplicitRequired, Derived(new_derived)) => {
            if *new_derived == old_alias {
                Ok(old_alias)
            } else {
                Err(format!(
                    "endpoint change would alter the routing alias from {old_alias:?} to {new_derived:?}; \
                     delete and re-create the upstream instead"
                ))
            }
        }
        // Derivable → Non-derivable: rejected always, even with an explicit alias.
        (Derived(_), ExplicitRequired) => Err(
            "alias transition from derivable to non-derivable is rejected; \
             delete and re-create the upstream"
                .to_owned(),
        ),
        // Non-derivable → Non-derivable (endpoints changed): existing alias retained.
        (ExplicitRequired, ExplicitRequired) => match user_alias {
            None => Ok(old_alias),
            Some(user) => {
                let normalized = normalize_alias(user);
                if normalized == old_alias {
                    Ok(old_alias)
                } else {
                    Err(format!(
                        "alias is immutable; existing alias {old_alias:?} is retained"
                    ))
                }
            }
        },
    }
}

/// Validate the alias grammar (`^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`).
#[must_use]
pub fn valid_alias_characters(alias: &str) -> bool {
    if alias.is_empty() {
        return false;
    }
    let bytes = alias.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'.' || *b == b':' || *b == b'-')
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn ep(scheme: Scheme, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn single_hostname_standard_port() {
        let eps = vec![ep(Scheme::Https, "api.openai.com", 443)];
        assert_eq!(
            derive_alias(&eps),
            AliasDerivation::Derived("api.openai.com".to_owned())
        );
    }

    #[test]
    fn single_hostname_nonstandard_port() {
        let eps = vec![ep(Scheme::Https, "api.openai.com", 8443)];
        assert_eq!(
            derive_alias(&eps),
            AliasDerivation::Derived("api.openai.com:8443".to_owned())
        );
    }

    #[test]
    fn multi_hostname_common_suffix() {
        let eps = vec![
            ep(Scheme::Https, "us.vendor.com", 443),
            ep(Scheme::Https, "eu.vendor.com", 443),
        ];
        assert_eq!(
            derive_alias(&eps),
            AliasDerivation::Derived("vendor.com".to_owned())
        );
    }

    #[test]
    fn multi_hostname_common_suffix_nonstandard_port() {
        let eps = vec![
            ep(Scheme::Https, "us.vendor.com", 8443),
            ep(Scheme::Https, "eu.vendor.com", 8443),
        ];
        assert_eq!(
            derive_alias(&eps),
            AliasDerivation::Derived("vendor.com:8443".to_owned())
        );
    }

    #[test]
    fn multi_hostname_bare_public_suffix_is_not_derivable() {
        let eps = vec![
            ep(Scheme::Https, "foo.co.uk", 443),
            ep(Scheme::Https, "bar.co.uk", 443),
        ];
        assert_eq!(derive_alias(&eps), AliasDerivation::ExplicitRequired);
    }

    #[test]
    fn ip_based_requires_explicit_alias() {
        let eps = vec![
            ep(Scheme::Https, "10.0.1.1", 443),
            ep(Scheme::Https, "10.0.1.2", 443),
        ];
        assert_eq!(derive_alias(&eps), AliasDerivation::ExplicitRequired);
        assert!(enforce_alias(&eps, None).is_err());
        assert_eq!(
            enforce_alias(&eps, Some("my-internal-service")).unwrap(),
            "my-internal-service"
        );
    }

    #[test]
    fn hostname_alias_is_enforced() {
        let eps = vec![ep(Scheme::Https, "api.openai.com", 443)];
        assert!(enforce_alias(&eps, Some("different.example")).is_err());
        // Exact-match accepted as a no-op.
        assert_eq!(
            enforce_alias(&eps, Some("API.OpenAI.COM.")).unwrap(),
            "api.openai.com"
        );
    }

    #[test]
    fn hostname_validation_rules() {
        assert!(validate_hostname("api.openai.com"));
        assert!(validate_hostname("api.openai.com."));
        assert!(!validate_hostname("-bad.example"));
        assert!(!validate_hostname("bad-.example"));
        assert!(!validate_hostname("has space.example"));
    }

    #[test]
    fn explicit_alias_for_heterogeneous_pool() {
        let eps = vec![
            ep(Scheme::Https, "us.foo.com", 443),
            ep(Scheme::Https, "eu.bar.com", 443),
        ];
        assert_eq!(derive_alias(&eps), AliasDerivation::ExplicitRequired);
    }

    fn upstream_with(endpoints: Vec<Endpoint>, alias: Option<&str>) -> Upstream {
        let mut u = Upstream {
            server: ServerConfig { endpoints },
            ..Default::default()
        };
        u.alias = alias.map(str::to_owned);
        u
    }

    #[test]
    fn alias_update_unchanged_endpoints_retains_alias() {
        let old = vec![ep(Scheme::Https, "api.openai.com", 443)];
        let mut new_up = upstream_with(old.clone(), Some("api.openai.com"));
        new_up.enabled = false; // non-endpoint field change
        assert_eq!(
            enforce_alias_update("api.openai.com", &old, &new_up).unwrap(),
            "api.openai.com"
        );
        // Differing user alias rejected on no endpoint change.
        let renamed = upstream_with(old.clone(), Some("different.example"));
        assert!(enforce_alias_update("api.openai.com", &old, &renamed).is_err());
    }

    #[test]
    fn alias_update_derivable_to_derivable_requires_same_alias() {
        let old = vec![ep(Scheme::Https, "api.openai.com", 443)];
        let changed = vec![ep(Scheme::Https, "api.openai.com", 8443)];
        // Derived alias would become api.openai.com:8443 -> rejected.
        assert!(
            enforce_alias_update("api.openai.com", &old, &upstream_with(changed, None)).is_err()
        );
        // Pointing to another host changes the alias -> rejected.
        let other = vec![ep(Scheme::Https, "api.anthropic.com", 443)];
        assert!(enforce_alias_update("api.openai.com", &old, &upstream_with(other, None)).is_err());
        // Same alias preserved across a harmless port-only change? Port changes
        // alter the derived alias, so it must be rejected (delete-and-recreate).
    }

    #[test]
    fn alias_update_ip_to_ip_retains_explicit_alias() {
        let old = vec![ep(Scheme::Https, "10.0.1.1", 443)];
        let changed = vec![ep(Scheme::Https, "10.0.1.2", 443)];
        let kept = upstream_with(changed.clone(), Some("my-service"));
        assert_eq!(
            enforce_alias_update("my-service", &old, &kept).unwrap(),
            "my-service"
        );
        let renamed = upstream_with(changed, Some("other-service"));
        assert!(enforce_alias_update("my-service", &old, &renamed).is_err());
    }

    #[test]
    fn alias_update_derivable_to_ip_rejected() {
        let old = vec![ep(Scheme::Https, "api.openai.com", 443)];
        let ip = vec![ep(Scheme::Https, "10.0.1.9", 443)];
        assert!(
            enforce_alias_update("api.openai.com", &old, &upstream_with(ip, Some("x"))).is_err()
        );
    }
}
