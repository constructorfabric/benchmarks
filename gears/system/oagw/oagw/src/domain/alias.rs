//! Alias derivation and enforcement rules.
//!
//! Normative source: `DESIGN.md §3.2 Alias Resolution` and
//! `PRD §5.5 Alias Resolution and Shadowing`.

use std::net::IpAddr;

use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, Protocol, ServerConfig};

/// Standard port for a URL scheme (the port omitted from derived aliases).
#[must_use]
pub fn standard_port_for_scheme(scheme: &str) -> u16 {
    if scheme.eq_ignore_ascii_case("http") {
        80
    } else {
        443
    }
}

/// Validate a hostname per RFC 1123: total ≤ 253 chars, each label 1–63
/// chars, ASCII alphanumeric + hyphen only, no leading/trailing hyphen.
/// A single trailing dot (FQDN notation) is tolerated and stripped.
///
/// IP addresses pass through validation but are not treated as hostnames for
/// derivation.
#[must_use]
pub fn is_valid_hostname(host: &str) -> bool {
    if host.parse::<IpAddr>().is_ok() {
        return true;
    }
    let h = host.strip_suffix('.').unwrap_or(host);
    if h.is_empty() || h.len() > 253 {
        return false;
    }
    h.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

/// Whether a host is an IP address literal.
#[must_use]
pub fn is_ip(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok()
}

/// Normalize an alias: ASCII lowercase, trailing dots stripped.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// Whether a value is a valid *registrable domain* (≥2 labels and not a bare
/// public suffix per the Public Suffix List).
#[must_use]
pub fn is_registrable_domain(candidate: &str) -> bool {
    let labels: Vec<&str> = candidate.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    // `psl::suffix_str` returns the public suffix when it covers the whole
    // input — meaning `candidate` itself is a bare public suffix (e.g.
    // `co.uk`). `psl::domain_str` returns the registrable domain; for a bare
    // public suffix there is none.
    match psl::suffix_str(candidate) {
        Some(suffix) if suffix == candidate => false,
        _ => psl::domain_str(candidate).is_some(),
    }
}

/// Longest common dot-separated suffix of the given hostnames.
///
/// Only suffixes with at least two labels are considered (a single-label
/// result is never a registrable domain).
#[must_use]
fn common_suffix(hosts: &[&str]) -> Option<String> {
    let split: Vec<Vec<&str>> = hosts
        .iter()
        .map(|h| h.trim_end_matches('.').split('.').collect::<Vec<_>>())
        .collect();
    let first = split.first()?;
    let mut suffix: Vec<&str> = Vec::new();
    for i in 0..first.len() {
        let label = first[first.len() - 1 - i];
        if split.iter().all(|labels| {
            labels
                .get(labels.len().saturating_sub(i + 1))
                .is_some_and(|l| *l == label)
        }) {
            suffix.insert(0, label);
        } else {
            break;
        }
    }
    if suffix.len() < 2 {
        return None;
    }
    Some(suffix.join("."))
}

/// Compute the auto-derived alias for an upstream's server configuration, if
/// derivable.
///
/// Returns `None` when an explicit alias is required (IP endpoints, no
/// registrable common suffix, bare public suffix).
#[must_use]
pub fn compute_derived_alias(protocol: Protocol, server: &ServerConfig) -> Option<String> {
    if server.endpoints.is_empty() {
        return None;
    }
    let hosts: Vec<&str> = server
        .endpoints
        .iter()
        .map(|e| e.host.trim_end_matches('.'))
        .collect();

    if hosts.len() == 1 {
        let ep = &server.endpoints[0];
        if is_ip(&ep.host) {
            return None;
        }
        if !is_valid_hostname(&ep.host) {
            return None;
        }
        return Some(alias_for_host(ep));
    }

    // Multi-endpoint pool: require all hostnames (reject mixed IP+hostname).
    if hosts.iter().any(|h| is_ip(h)) {
        return None;
    }
    if hosts.iter().any(|h| !is_valid_hostname(h)) {
        return None;
    }
    let suffix = common_suffix(&hosts)?;
    if !is_registrable_domain(&suffix) {
        return None;
    }

    // Non-standard ports are preserved as `suffix:port` (all endpoints share
    // a single port — enforced by ServerConfig::validate).
    let port = server.endpoints[0].port;
    let scheme = &server.endpoints[0].scheme;
    let std = standard_port_for_scheme(scheme) == port;
    let _ = protocol;
    if std {
        Some(suffix.to_ascii_lowercase())
    } else {
        Some(format!("{suffix}:{port}"))
    }
}

/// Alias for a single hostname endpoint.
#[must_use]
fn alias_for_host(ep: &Endpoint) -> String {
    let host = ep.host.trim_end_matches('.').to_ascii_lowercase();
    let std = standard_port_for_scheme(&ep.scheme) == ep.port;
    if std {
        host
    } else {
        format!("{host}:{}", ep.port)
    }
}

/// Whether an endpoint pool is hostname-derivable.
#[must_use]
pub fn is_derivable(protocol: Protocol, server: &ServerConfig) -> bool {
    compute_derived_alias(protocol, server).is_some()
}

/// Enforce the alias update rules ("Alias Update Behavior" in DESIGN §3.2).
///
/// `existing` is the alias currently stored (`None` for a fresh create);
/// `existing_derivable` reports whether the previous endpoint configuration
/// was derivable (only meaningful when `existing` is `Some`) so the
/// `Derivable → Non-derivable` transition can be rejected unconditionally.
///
/// # Errors
///
/// `DomainError::AliasViolation` when the requested alias would change the
/// routing key or breaks endpoint-type rules.
#[allow(clippy::too_many_lines)]
pub fn enforce_alias_update(
    tenant_alias_taken: impl Fn(&str) -> bool,
    existing: Option<&str>,
    existing_derivable: bool,
    explicit_alias: Option<&str>,
    protocol: Protocol,
    server: &ServerConfig,
) -> Result<String, DomainError> {
    let derived = compute_derived_alias(protocol, server);
    let explicit = explicit_alias.as_ref().map(|a| normalize_alias(a));

    let effective = match (derived, existing_derivable, existing) {
        // Now derivable: the alias is always auto-derived and immutable.
        (Some(d), _, _) => {
            if let Some(existing_alias) = existing
                && existing_alias != d
            {
                return Err(alias_immutable_error(existing_alias, &d));
            }
            if let Some(provided) = &explicit
                && provided != &d
            {
                return Err(DomainError::AliasViolation(format!(
                    "alias '{provided}' differs from the auto-derived alias '{d}'"
                )));
            }
            d
        }
        // Derivable → Non-derivable (e.g. hostname → IP): rejected always,
        // even when an explicit alias is provided (DESIGN §3.2 table).
        (None, true, Some(existing_alias)) => {
            return Err(DomainError::AliasViolation(format!(
                "endpoint change would make the alias non-derivable; delete and re-create the upstream instead (existing alias '{existing_alias}')"
            )));
        }
        // Non-derivable endpoints (IP-based, heterogeneous, etc.): the alias
        // is retained or must exactly match on update.
        (None, _, Some(existing_alias)) => match explicit {
            Some(provided) if provided != existing_alias => {
                return Err(alias_immutable_error(existing_alias, &provided));
            }
            Some(_) => existing_alias.to_owned(),
            // Alias field omitted on update: keep the existing alias.
            None => existing_alias.to_owned(),
        },
        // Fresh create with non-derivable endpoints: explicit alias required.
        (None, _, None) => match explicit {
            Some(provided) => provided,
            None => {
                return Err(DomainError::AliasViolation(
                    "explicit alias is required for IP-based or non-derivable endpoints".to_owned(),
                ));
            }
        },
    };

    if tenant_alias_taken(&effective) {
        return Err(DomainError::AliasViolation(format!(
            "alias '{effective}' already exists for this tenant"
        )));
    }
    Ok(effective)
}

fn alias_immutable_error(existing: &str, would_be: &str) -> DomainError {
    DomainError::AliasViolation(format!(
        "alias is immutable once set (existing '{existing}' vs derived '{would_be}'); \
         delete and re-create the upstream instead"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, ServerConfig};

    fn ep(scheme: &str, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: scheme.to_owned(),
            host: host.to_owned(),
            port,
        }
    }

    fn srv(eps: Vec<Endpoint>) -> ServerConfig {
        ServerConfig { endpoints: eps }
    }

    fn none(_: &str) -> bool {
        false
    }

    #[test]
    fn single_hostname_standard_port() {
        let s = srv(vec![ep("https", "api.openai.com", 443)]);
        assert_eq!(
            compute_derived_alias(Protocol::Http, &s),
            Some("api.openai.com".to_owned())
        );
        // http on 80
        let s2 = srv(vec![ep("http", "api.example.com", 80)]);
        assert_eq!(
            compute_derived_alias(Protocol::Http, &s2),
            Some("api.example.com".to_owned())
        );
    }

    #[test]
    fn single_hostname_nonstandard_port() {
        let s = srv(vec![ep("https", "api.openai.com", 8443)]);
        assert_eq!(
            compute_derived_alias(Protocol::Http, &s),
            Some("api.openai.com:8443".to_owned())
        );
    }

    #[test]
    fn single_hostname_standard_type_mismatch() {
        // https on port 80 is NOT standard for the scheme.
        let s = srv(vec![ep("https", "api.example.com", 80)]);
        assert_eq!(
            compute_derived_alias(Protocol::Http, &s),
            Some("api.example.com:80".to_owned())
        );
    }

    #[test]
    fn common_suffix_registrable() {
        let s = srv(vec![
            ep("https", "us.vendor.com", 443),
            ep("https", "eu.vendor.com", 443),
        ]);
        assert_eq!(
            compute_derived_alias(Protocol::Http, &s),
            Some("vendor.com".to_owned())
        );
    }

    #[test]
    fn common_suffix_bare_public_suffix_rejected() {
        let s = srv(vec![
            ep("https", "foo.co.uk", 443),
            ep("https", "bar.co.uk", 443),
        ]);
        assert_eq!(compute_derived_alias(Protocol::Http, &s), None);
    }

    #[test]
    fn registrable_domain_checks() {
        assert!(is_registrable_domain("vendor.com"));
        assert!(is_registrable_domain("foo.co.uk"));
        assert!(!is_registrable_domain("co.uk"));
        assert!(!is_registrable_domain("com"));
        assert!(!is_registrable_domain("uk"));
    }

    #[test]
    fn no_common_suffix_rejected() {
        let s = srv(vec![
            ep("https", "us.foo.com", 443),
            ep("https", "eu.bar.com", 443),
        ]);
        assert_eq!(compute_derived_alias(Protocol::Http, &s), None);
    }

    #[test]
    fn common_suffix_non_standard_port() {
        let s = srv(vec![
            ep("https", "us.vendor.com", 8443),
            ep("https", "eu.vendor.com", 8443),
        ]);
        assert_eq!(
            compute_derived_alias(Protocol::Http, &s),
            Some("vendor.com:8443".to_owned())
        );
    }

    #[test]
    fn ip_endpoints_require_explicit_alias() {
        let s = srv(vec![ep("http", "10.0.1.1", 80)]);
        assert_eq!(compute_derived_alias(Protocol::Http, &s), None);
        assert!(is_ip("10.0.1.1"));
        assert!(!is_ip("api.openai.com"));
    }

    #[test]
    fn hostname_validation() {
        assert!(is_valid_hostname("api.openai.com"));
        assert!(is_valid_hostname("api.openai.com."));
        assert!(is_valid_hostname("a-b.c"));
        assert!(!is_valid_hostname("-bad.example.com"));
        assert!(!is_valid_hostname("bad-.example.com"));
        assert!(!is_valid_hostname("bad_host"));
        assert!(!is_valid_hostname(""));
        assert!(!is_valid_hostname("a..b"));
        assert!(is_valid_hostname("10.0.1.1")); // IPs pass
    }

    #[test]
    fn normalization() {
        assert_eq!(normalize_alias("Api.OpenAI.COM"), "api.openai.com");
        assert_eq!(normalize_alias("api.openai.com."), "api.openai.com");
    }

    #[test]
    fn enforce_alias_create_derivable_rejects_user_alias() {
        let s = srv(vec![ep("https", "api.openai.com", 443)]);
        let err = enforce_alias_update(none, None, false, Some("my-alias"), Protocol::Http, &s)
            .err()
            .unwrap();
        assert!(err.to_string().contains("differs from the auto-derived alias"));
        // Exact derived alias tolerated.
        let ok = enforce_alias_update(
            none,
            None,
            false,
            Some("api.openai.com"),
            Protocol::Http,
            &s,
        )
        .unwrap();
        assert_eq!(ok, "api.openai.com");
    }

    #[test]
    fn enforce_alias_create_ip_requires_explicit() {
        let s = srv(vec![ep("http", "10.0.1.1", 80)]);
        assert!(enforce_alias_update(none, None, false, None, Protocol::Http, &s).is_err());
        let ok =
            enforce_alias_update(none, None, false, Some("my-service"), Protocol::Http, &s)
                .unwrap();
        assert_eq!(ok, "my-service");
    }

    #[test]
    fn enforce_alias_update_immutable() {
        let s = srv(vec![ep("https", "api.old.com", 443)]);
        // Endpoints changed so derived changes: rejected.
        let s2 = srv(vec![ep("https", "api.new.com", 443)]);
        let err = enforce_alias_update(none, Some("api.old.com"), true, None, Protocol::Http, &s2)
            .err()
            .unwrap();
        assert!(err.to_string().contains("immutable"));

        // Same endpoints, derived unchanged: tolerated.
        let ok = enforce_alias_update(none, Some("api.old.com"), true, None, Protocol::Http, &s)
            .unwrap();
        assert_eq!(ok, "api.old.com");
    }

    #[test]
    fn enforce_alias_ip_to_ip_user_provides_same() {
        let s = srv(vec![ep("http", "10.0.1.2", 80)]);
        let ok = enforce_alias_update(
            none,
            Some("my-service"),
            false,
            Some("my-service"),
            Protocol::Http,
            &s,
        )
        .unwrap();
        assert_eq!(ok, "my-service");
        // Different explicit alias rejected.
        assert!(enforce_alias_update(
            none,
            Some("my-service"),
            false,
            Some("other"),
            Protocol::Http,
            &s,
        )
        .is_err());
    }

    #[test]
    fn enforce_alias_hostname_to_ip_rejected() {
        let s_ip = srv(vec![ep("http", "10.0.1.2", 80)]);
        assert!(
            enforce_alias_update(none, Some("api.old.com"), true, None, Protocol::Http, &s_ip)
                .is_err()
        );
    }

    #[test]
    fn alias_uniqueness_enforced() {
        let s = srv(vec![ep("https", "api.openai.com", 443)]);
        let taken = |alias: &str| alias == "api.openai.com";
        assert!(enforce_alias_update(taken, None, false, None, Protocol::Http, &s).is_err());
    }
}
