//! Alias derivation, normalization and enforcement (DESIGN §3.2
//! "Alias Resolution", ADR-0001).
//!
//! Aliases are not arbitrary labels: for hostname endpoints they are
//! auto-derived (single hostname, or the registrable common suffix of a
//! hostname pool), while IP-based or non-derivable endpoint pools require an
//! explicit alias. Aliases are immutable after creation (the routing key in
//! `/v1/proxy/{alias}/...`); endpoint changes that would alter the derived
//! alias are rejected with 400.

use std::net::IpAddr;

use crate::domain::model::Endpoint;

/// Normalize an alias or hostname: ASCII lowercase, trailing dots stripped.
#[must_use]
pub fn normalize(value: &str) -> String {
    value.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// True when `host` parses as an IP literal (v4 or v6).
#[must_use]
pub fn is_ip(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok()
}

/// Validate a hostname per RFC 1123: max 253 chars total, each label 1–63,
/// labels of ASCII alphanumerics and hyphens, no leading/trailing hyphen.
/// A single trailing dot (FQDN notation) is tolerated.
#[must_use]
pub fn validate_hostname(host: &str) -> Result<(), String> {
    let trimmed = host.strip_suffix('.').unwrap_or(host);
    if trimmed.is_empty() || trimmed.len() > 253 {
        return Err(format!("invalid hostname '{host}': bad length"));
    }
    for label in trimmed.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(format!(
                "invalid hostname '{host}': label must be 1..63 chars"
            ));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(format!(
                "invalid hostname '{host}': labels may contain only ASCII alphanumerics and hyphens"
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!(
                "invalid hostname '{host}': labels cannot start or end with hyphen"
            ));
        }
    }
    Ok(())
}

/// Validate an endpoint's `host`: either an IP literal or a valid hostname.
#[must_use]
pub fn validate_host(host: &str) -> Result<(), String> {
    if is_ip(host) {
        return Ok(());
    }
    validate_hostname(host)
}

/// Longest common domain suffix across the given hostnames (by dot labels).
/// Returns `None` when hosts have no labels in common. Hostnames are
/// assumed normalized (lowercase, no trailing dot) by callers.
fn common_domain_suffix(hosts: &[String]) -> Option<String> {
    let split: Vec<Vec<&str>> = hosts
        .iter()
        .map(|h| h.split('.').collect::<Vec<_>>())
        .collect();
    let max_depth = split.iter().map(|labels| labels.len()).min().unwrap_or(0);
    if max_depth == 0 {
        return None;
    }
    let mut common = 0usize;
    'outer: for depth in 1..=max_depth {
        let candidate = split[0][split[0].len() - depth];
        for labels in &split[1..] {
            if labels[labels.len() - depth] != candidate {
                break 'outer;
            }
        }
        common = depth;
    }
    if common == 0 {
        None
    } else {
        let suffix: Vec<&str> = split[0][split[0].len() - common..].to_vec();
        Some(suffix.join("."))
    }
}

/// True when `domain` is itself a bare public suffix (e.g. `co.uk`) per the
/// Public Suffix List (`psl` crate). Such a suffix is not registrable and
/// cannot serve as a derived alias.
#[must_use]
pub fn is_bare_public_suffix(domain: &str) -> bool {
    match psl::suffix_str(domain) {
        Some(suffix) => suffix.eq_ignore_ascii_case(domain),
        None => false,
    }
}

/// Result of deriving an alias from an endpoint pool.
pub enum DeriveOutcome {
    /// Alias auto-derived from hostname(s).
    Derived(String),
    /// The pool is not derivable (IP-based, heterogeneous hosts, or only a
    /// bare public suffix common) — an explicit alias is required.
    NotDerivable,
}

/// Compute the derived alias for an endpoint pool.
///
/// - Single hostname endpoint → `hostname` (or `hostname:port` when the port
///   is non-standard).
/// - Multiple hostnames → registrable common domain suffix (≥2 labels, not a
///   bare public suffix), preserving `:port` when the pool shares a
///   non-standard port.
/// - IP-based pools, heterogeneous hostnames without a registrable common
///   suffix → not derivable.
#[must_use]
pub fn derive_alias(endpoints: &[Endpoint]) -> DeriveOutcome {
    if endpoints.is_empty() {
        return DeriveOutcome::NotDerivable;
    }

    let all_ips = endpoints.iter().all(|e| is_ip(&e.host));
    if all_ips {
        return DeriveOutcome::NotDerivable;
    }
    // Mixed IP + hostname pools are not derivable.
    if endpoints.iter().any(|e| is_ip(&e.host)) {
        return DeriveOutcome::NotDerivable;
    }

    // Normalize hosts (RFC1123 trailing dots tolerated at parse time).
    let hosts: Vec<String> = endpoints.iter().map(|e| normalize(&e.host)).collect();

    if endpoints.len() == 1 {
        let host = &hosts[0];
        let port = non_standard_suffix_port(endpoints);
        return DeriveOutcome::Derived(match port {
            Some(p) => format!("{host}:{p}"),
            None => host.clone(),
        });
    }

    let Some(suffix) = common_domain_suffix(&hosts) else {
        return DeriveOutcome::NotDerivable;
    };
    if suffix.split('.').count() < 2 {
        return DeriveOutcome::NotDerivable;
    }
    if is_bare_public_suffix(&suffix) {
        return DeriveOutcome::NotDerivable;
    }
    let port = non_standard_suffix_port(endpoints);
    DeriveOutcome::Derived(match port {
        Some(p) => format!("{suffix}:{p}"),
        None => suffix,
    })
}

/// Returns the shared non-standard port across a pool, or `None` when all
/// endpoints use their scheme's standard port (or differ).
fn non_standard_suffix_port(endpoints: &[Endpoint]) -> Option<u16> {
    let ports: Vec<u16> = endpoints.iter().map(Endpoint::effective_port).collect();
    if ports.iter().all(|p| *p == 80 || *p == 443) {
        return None;
    }
    let first = ports[0];
    if ports.iter().all(|p| *p == first) {
        Some(first)
    } else {
        // Non-uniform ports: keep the alias port-free to avoid ambiguity;
        // endpoint selection via X-OAGW-Target-Host remains available.
        None
    }
}

/// `(alias, derived)` resolved for a create request.
pub struct AliasResolution {
    pub alias: String,
    pub derived: bool,
}

/// Apply the alias enforcement rules for a **create** operation.
///
/// - Hostname-only pools: alias is auto-derived. A user-provided alias that
///   differs from the derived value is rejected (400); matching the derived
///   value is tolerated (idempotency).
/// - IP-based / non-derivable pools: explicit alias required.
pub fn resolve_create_alias(
    requested: Option<&str>,
    endpoints: &[Endpoint],
) -> Result<AliasResolution, String> {
    // Validate every host regardless of alias path.
    for ep in endpoints {
        validate_host(&ep.host).map_err(|e| format!("invalid endpoint host '{}': {e}", ep.host))?;
    }

    match derive_alias(endpoints) {
        DeriveOutcome::Derived(derived) => match requested {
            Some(provided) if normalize(provided) == derived => Ok(AliasResolution {
                alias: derived,
                derived: true,
            }),
            Some(provided) if provided.trim().is_empty() => Ok(AliasResolution {
                alias: derived,
                derived: true,
            }),
            Some(provided) => Err(format!(
                "alias '{provided}' does not match the auto-derived alias '{derived}' for hostname endpoints; \
                 omit the alias or provide the exact derived value"
            )),
            None => Ok(AliasResolution {
                alias: derived,
                derived: true,
            }),
        },
        DeriveOutcome::NotDerivable => match requested {
            Some(provided) if !provided.trim().is_empty() => {
                let alias = normalize(provided);
                if alias.is_empty() || alias.contains('/') || alias.contains('?') {
                    return Err("invalid alias".to_owned());
                }
                Ok(AliasResolution {
                    alias,
                    derived: false,
                })
            }
            _ => Err(
                "explicit alias is required for IP-based or non-derivable endpoint pools"
                    .to_owned(),
            ),
        },
    }
}

/// Apply the alias enforcement rules for an **update** (PUT) operation,
/// given the existing stored alias (`existing`) and whether it was derived.
///
/// The alias is immutable once set: any endpoint change that would alter a
/// derived alias is rejected; a differing user-provided alias is rejected;
/// the exact-match alias is tolerated as a no-op.
pub fn resolve_update_alias(
    existing: &str,
    existing_derived: bool,
    requested: Option<&str>,
    endpoints: &[Endpoint],
) -> Result<String, String> {
    for ep in endpoints {
        validate_host(&ep.host).map_err(|e| format!("invalid endpoint host '{}': {e}", ep.host))?;
    }

    let current = match derive_alias(endpoints) {
        DeriveOutcome::Derived(d) => Some(d),
        DeriveOutcome::NotDerivable => None,
    };

    match (current, requested) {
        (Some(derived), Some(provided)) => {
            if normalize(provided) == existing {
                // Explicit alias matching the existing one: allowed, provided
                // the recomputed derived alias also equals it.
                if derived == existing {
                    Ok(existing.to_owned())
                } else {
                    Err(
                        "alias is immutable: the recomputed derived alias would change; \
                         delete and re-create the upstream instead"
                            .to_owned(),
                    )
                }
            } else {
                // The alias is immutable: a differing user-provided alias is
                // rejected even when the endpoints still yield the same
                // derived alias.
                Err(
                    "alias is immutable: a differing user-provided alias is not accepted"
                        .to_owned(),
                )
            }
        }
        (Some(derived), None) => {
            if derived == existing {
                Ok(existing.to_owned())
            } else if existing_derived {
                Err(
                    "alias is immutable: changing the endpoints would change the derived alias; \
                     delete and re-create the upstream instead"
                        .to_owned(),
                )
            } else {
                Err(
                    "alias is immutable: non-derivable → derivable transitions that change \
                     the alias are rejected; delete and re-create the upstream instead"
                        .to_owned(),
                )
            }
        }
        (None, Some(provided)) => {
            // Non-derivable pool. Existing alias retained unless a differing
            // one is provided.
            if normalize(provided) == existing {
                Ok(existing.to_owned())
            } else {
                Err(
                    "alias is immutable: a differing user-provided alias is not accepted"
                        .to_owned(),
                )
            }
        }
        (None, None) => Ok(existing.to_owned()),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn ep(scheme: &str, host: &str, port: Option<u16>) -> Endpoint {
        Endpoint {
            scheme: scheme.to_owned(),
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn single_hostname_standard_port_derived_plain() {
        assert!(matches!(
            derive_alias(&[ep("https", "api.openai.com", None)]),
            DeriveOutcome::Derived(ref a) if a == "api.openai.com"
        ));
    }

    #[test]
    fn single_hostname_non_standard_port_kept() {
        assert!(matches!(
            derive_alias(&[ep("https", "api.openai.com", Some(8443))]),
            DeriveOutcome::Derived(ref a) if a == "api.openai.com:8443"
        ));
    }

    #[test]
    fn ip_requires_explicit_alias() {
        assert!(matches!(
            derive_alias(&[ep("http", "127.0.0.1", Some(9000))]),
            DeriveOutcome::NotDerivable
        ));
        assert!(matches!(
            derive_alias(&[ep("http", "10.0.1.1", None), ep("http", "10.0.1.2", None)]),
            DeriveOutcome::NotDerivable
        ));
    }

    #[test]
    fn common_suffix_derived() {
        // us.vendor.com + eu.vendor.com → vendor.com
        assert!(matches!(
            derive_alias(&[
                ep("https", "us.vendor.com", None),
                ep("https", "eu.vendor.com", None)
            ]),
            DeriveOutcome::Derived(ref a) if a == "vendor.com"
        ));
        // Non-standard uniform port preserved.
        assert!(matches!(
            derive_alias(&[
                ep("https", "us.vendor.com", Some(8443)),
                ep("https", "eu.vendor.com", Some(8443))
            ]),
            DeriveOutcome::Derived(ref a) if a == "vendor.com:8443"
        ));
    }

    #[test]
    fn bare_public_suffix_not_derivable() {
        // foo.co.uk + bar.co.uk → co.uk is a bare public suffix → reject.
        assert!(matches!(
            derive_alias(&[
                ep("https", "foo.co.uk", None),
                ep("https", "bar.co.uk", None)
            ]),
            DeriveOutcome::NotDerivable
        ));
        assert!(is_bare_public_suffix("co.uk"));
        assert!(!is_bare_public_suffix("vendor.com"));
    }

    #[test]
    fn heterogeneous_pools_not_derivable() {
        assert!(matches!(
            derive_alias(&[
                ep("https", "us.foo.com", None),
                ep("https", "eu.bar.com", None)
            ]),
            DeriveOutcome::NotDerivable
        ));
    }

    #[test]
    fn create_hostname_pool_rejects_diff_alias() {
        let endpoints = [
            ep("https", "us.vendor.com", None),
            ep("https", "eu.vendor.com", None),
        ];
        let res = resolve_create_alias(Some("custom"), &endpoints);
        assert!(res.is_err());
        let res = resolve_create_alias(Some("vendor.com"), &endpoints);
        assert_eq!(res.unwrap().alias, "vendor.com");
        let res = resolve_create_alias(None, &endpoints);
        assert_eq!(res.unwrap().alias, "vendor.com");
    }

    #[test]
    fn create_ip_requires_alias() {
        let endpoints = [ep("http", "127.0.0.1", Some(9000))];
        assert!(resolve_create_alias(None, &endpoints).is_err());
        let res = resolve_create_alias(Some("my-service"), &endpoints).unwrap();
        assert_eq!(res.alias, "my-service");
        assert!(!res.derived);
    }

    #[test]
    fn update_alias_immutable() {
        let endpoints = [ep("https", "api.openai.com", None)];
        // Same endpoints → same derived alias → OK.
        assert_eq!(
            resolve_update_alias("api.openai.com", true, None, &endpoints).unwrap(),
            "api.openai.com"
        );
        // Endpoint would change derived alias → rejected.
        let changed = [ep("https", "eu.openai.com", None)];
        assert!(resolve_update_alias("api.openai.com", true, None, &changed).is_err());
        // Total alias override → rejected even with same endpoints.
        assert!(resolve_update_alias("api.openai.com", true, Some("other"), &endpoints).is_err());
        // Exact-match alias tolerated.
        assert_eq!(
            resolve_update_alias("api.openai.com", true, Some("Api.OpenAI.Com"), &endpoints)
                .unwrap(),
            "api.openai.com"
        );
    }

    #[test]
    fn normalization_is_case_insensitive_trailing_dot_stripped() {
        assert_eq!(normalize("Api.OpenAI.COM."), "api.openai.com");
    }

    #[test]
    fn rfc1123_hostname_validation() {
        assert!(validate_hostname("api.openai.com").is_ok());
        assert!(validate_hostname("api.openai.com.").is_ok());
        assert!(validate_hostname("a-1.b-2.c").is_ok());
        assert!(validate_hostname("-bad.example.com").is_err());
        assert!(validate_hostname("bad-.example.com").is_err());
        assert!(validate_hostname("bad_label.example.com").is_err());
        let too_long = format!("{}.com", "a".repeat(64));
        assert!(validate_hostname(&too_long).is_err());
    }
}
