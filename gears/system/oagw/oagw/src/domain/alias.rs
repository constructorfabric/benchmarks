//! Alias derivation, normalization and update enforcement.
//!
//! An alias is the routing key in `/oagw/v1/proxy/{alias}/…`, so it is not a
//! free-form label: `docs/DESIGN.md` §"Alias Enforcement Rules" makes its
//! value a function of the endpoint pool. Hostname pools always derive;
//! IP-based and non-derivable pools require the operator to name the
//! upstream explicitly.

use std::net::IpAddr;

use super::error::{DomainError, DomainResult};
use super::model::{Endpoint, Scheme};

/// Maximum total length of a hostname (RFC 1123 §2.1).
const MAX_HOSTNAME_LEN: usize = 253;
/// Maximum length of a single DNS label.
const MAX_LABEL_LEN: usize = 63;

/// Normalize an alias to its canonical form: ASCII lowercase with any
/// trailing dots stripped. Resolution is case-insensitive, so this is applied
/// on both write and lookup.
#[must_use]
pub fn normalize(alias: &str) -> String {
    alias.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Alias grammar from `docs/schemas/upstream.v1.schema.json`:
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
#[must_use]
pub fn is_valid_alias(alias: &str) -> bool {
    let bytes = alias.as_bytes();
    let Some((&first, rest)) = bytes.split_first() else {
        return false;
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    let Some((&last, middle)) = rest.split_last() else {
        return true; // single character, already validated
    };
    if !last.is_ascii_lowercase() && !last.is_ascii_digit() {
        return false;
    }
    middle
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b':' | b'-'))
}

/// Validate a hostname per RFC 1123. A trailing dot (FQDN notation) is
/// tolerated by the caller via [`normalize_host`].
///
/// # Errors
///
/// Returns a validation error naming the offending constraint.
pub fn validate_hostname(host: &str) -> DomainResult<()> {
    if host.is_empty() {
        return Err(DomainError::validation("endpoint host must not be empty"));
    }
    if host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    if host.len() > MAX_HOSTNAME_LEN {
        return Err(DomainError::validation(format!(
            "endpoint host exceeds {MAX_HOSTNAME_LEN} characters: {host}"
        )));
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > MAX_LABEL_LEN {
            return Err(DomainError::validation(format!(
                "endpoint host label must be 1-{MAX_LABEL_LEN} characters: {host}"
            )));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(DomainError::validation(format!(
                "endpoint host label must not start or end with a hyphen: {host}"
            )));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(DomainError::validation(format!(
                "endpoint host label contains a non-alphanumeric character: {host}"
            )));
        }
    }
    Ok(())
}

/// Canonical host spelling: ASCII lowercase, trailing dot stripped.
#[must_use]
pub fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// `true` when `host` is an IP literal rather than a name.
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok()
        || host
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .is_some_and(|s| s.parse::<IpAddr>().is_ok())
}

/// Longest common domain suffix of `hosts`, aligned on label boundaries.
///
/// Returns `None` unless the shared suffix has at least two labels *and* is a
/// registrable domain per the public suffix list — a bare public suffix such
/// as `co.uk` is not a usable routing key.
#[must_use]
pub fn common_domain_suffix(hosts: &[String]) -> Option<String> {
    let first = hosts.first()?;
    if hosts.iter().any(|h| is_ip_literal(h)) {
        return None;
    }

    let mut suffix: Vec<&str> = first.split('.').collect();
    for host in hosts.iter().skip(1) {
        let labels: Vec<&str> = host.split('.').collect();
        let mut shared = Vec::new();
        for (a, b) in suffix.iter().rev().zip(labels.iter().rev()) {
            if a == b {
                shared.push(*a);
            } else {
                break;
            }
        }
        shared.reverse();
        suffix = shared;
        if suffix.is_empty() {
            return None;
        }
    }

    if suffix.len() < 2 {
        return None;
    }
    let candidate = suffix.join(".");
    // `psl::domain_str` returns the registrable domain; when the candidate is
    // itself a bare public suffix it returns `None` (or a longer name), so an
    // exact match is what proves the candidate is registrable.
    if psl::domain_str(&candidate) == Some(candidate.as_str()) {
        Some(candidate)
    } else {
        None
    }
}

/// Derive the alias implied by an endpoint pool, or `None` when derivation is
/// impossible (IP literals, heterogeneous hostnames, bare public suffixes).
///
/// Endpoints are assumed to share a scheme and port — [`validate_pool`]
/// enforces that before this is called.
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    let first = endpoints.first()?;
    let hosts: Vec<String> = endpoints.iter().map(|e| normalize_host(&e.host)).collect();

    let base = if hosts.len() == 1 {
        let host = hosts.first()?;
        if is_ip_literal(host) {
            return None;
        }
        host.clone()
    } else {
        common_domain_suffix(&hosts)?
    };

    let port = first.effective_port();
    let alias = if port == first.scheme.standard_port() {
        base
    } else {
        format!("{base}:{port}")
    };
    is_valid_alias(&alias).then_some(alias)
}

/// Enforce the pool invariants from `docs/PRD.md` §"Multi-Endpoint Pooling":
/// at least one endpoint, and identical scheme and port across the pool.
///
/// # Errors
///
/// Returns a validation error describing the first violated invariant.
pub fn validate_pool(endpoints: &[Endpoint]) -> DomainResult<()> {
    let Some(first) = endpoints.first() else {
        return Err(DomainError::validation(
            "server.endpoints must contain at least one endpoint",
        ));
    };
    let scheme = first.scheme;
    let port = first.effective_port();
    for endpoint in endpoints {
        validate_hostname(&normalize_host(&endpoint.host))?;
        if endpoint.scheme != scheme {
            return Err(DomainError::validation(
                "all endpoints in a pool must share the same scheme",
            ));
        }
        if endpoint.effective_port() != port {
            return Err(DomainError::validation(
                "all endpoints in a pool must share the same port",
            ));
        }
    }
    Ok(())
}

/// Whether a scheme belongs to the WebSocket / WebTransport family, which
/// cannot be mixed with the HTTP family in one pool.
#[must_use]
pub const fn scheme_family(scheme: Scheme) -> u8 {
    match scheme {
        Scheme::Http | Scheme::Https => 0,
        Scheme::Ws | Scheme::Wss => 1,
        Scheme::Wt => 2,
        Scheme::Grpc => 3,
    }
}

/// Resolve the alias to store for a **create**.
///
/// Hostname pools derive their alias and reject a differing user-supplied
/// value; an exact match is tolerated for idempotency. Non-derivable pools
/// require the operator to supply one.
///
/// # Errors
///
/// `400` when a hostname pool is given a conflicting alias, or a
/// non-derivable pool is given none / an invalid one.
pub fn resolve_create_alias(
    endpoints: &[Endpoint],
    requested: Option<&str>,
) -> DomainResult<String> {
    let requested = requested.map(normalize).filter(|a| !a.is_empty());
    match compute_derived_alias(endpoints) {
        Some(derived) => match requested {
            Some(alias) if alias != derived => Err(DomainError::validation(format!(
                "alias is auto-derived for hostname endpoints: expected '{derived}', got '{alias}'"
            ))),
            _ => Ok(derived),
        },
        None => {
            let alias = requested.ok_or_else(|| {
                DomainError::validation(
                    "alias is required for IP-based or non-derivable endpoints",
                )
            })?;
            if !is_valid_alias(&alias) {
                return Err(DomainError::validation(format!(
                    "alias must match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$: '{alias}'"
                )));
            }
            Ok(alias)
        }
    }
}

/// Resolve the alias to store for a **replace**.
///
/// The alias is immutable once set: it is the routing key, so any endpoint
/// change that would alter the derived value is rejected and the operator
/// must delete and re-create. See the transition table in `docs/DESIGN.md`
/// §"Alias Update Behavior".
///
/// # Errors
///
/// `400` when the update would change the alias, or supplies one that differs
/// from the existing value.
pub fn enforce_alias_update(
    existing_alias: &str,
    endpoints: &[Endpoint],
    requested: Option<&str>,
) -> DomainResult<String> {
    let requested = requested.map(normalize).filter(|a| !a.is_empty());
    if let Some(alias) = &requested
        && alias != existing_alias
    {
        return Err(DomainError::validation(format!(
            "alias is immutable: '{existing_alias}' cannot be changed to '{alias}'; \
             delete and re-create the upstream instead"
        )));
    }

    match compute_derived_alias(endpoints) {
        Some(derived) if derived == existing_alias => Ok(derived),
        Some(derived) => Err(DomainError::validation(format!(
            "endpoint change would derive alias '{derived}', but '{existing_alias}' is already \
             in use as the routing key; delete and re-create the upstream instead"
        ))),
        // Non-derivable pools keep the alias the operator originally chose.
        None => Ok(existing_alias.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(scheme: Scheme, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port: Some(port),
        }
    }

    #[test]
    fn single_hostname_standard_port_derives_bare_host() {
        let pool = vec![ep(Scheme::Https, "api.openai.com", 443)];
        assert_eq!(
            compute_derived_alias(&pool).as_deref(),
            Some("api.openai.com")
        );
    }

    #[test]
    fn single_hostname_nonstandard_port_keeps_port() {
        let pool = vec![ep(Scheme::Https, "api.openai.com", 8443)];
        assert_eq!(
            compute_derived_alias(&pool).as_deref(),
            Some("api.openai.com:8443")
        );
    }

    #[test]
    fn plaintext_endpoint_on_port_80_derives_bare_host() {
        let pool = vec![ep(Scheme::Http, "localhost", 80)];
        assert_eq!(compute_derived_alias(&pool).as_deref(), Some("localhost"));
    }

    #[test]
    fn multi_host_common_suffix_derives_registrable_domain() {
        let pool = vec![
            ep(Scheme::Https, "us.vendor.com", 443),
            ep(Scheme::Https, "eu.vendor.com", 443),
        ];
        assert_eq!(compute_derived_alias(&pool).as_deref(), Some("vendor.com"));
    }

    #[test]
    fn multi_host_common_suffix_preserves_nonstandard_port() {
        let pool = vec![
            ep(Scheme::Https, "us.vendor.com", 8443),
            ep(Scheme::Https, "eu.vendor.com", 8443),
        ];
        assert_eq!(
            compute_derived_alias(&pool).as_deref(),
            Some("vendor.com:8443")
        );
    }

    #[test]
    fn bare_public_suffix_is_not_derivable() {
        let pool = vec![
            ep(Scheme::Https, "foo.co.uk", 443),
            ep(Scheme::Https, "bar.co.uk", 443),
        ];
        assert_eq!(compute_derived_alias(&pool), None);
    }

    #[test]
    fn heterogeneous_hosts_are_not_derivable() {
        let pool = vec![
            ep(Scheme::Https, "us.foo.com", 443),
            ep(Scheme::Https, "eu.bar.com", 443),
        ];
        assert_eq!(compute_derived_alias(&pool), None);
    }

    #[test]
    fn ip_pools_are_not_derivable() {
        let pool = vec![
            ep(Scheme::Https, "10.0.1.1", 443),
            ep(Scheme::Https, "10.0.1.2", 443),
        ];
        assert_eq!(compute_derived_alias(&pool), None);
    }

    #[test]
    fn create_rejects_user_alias_on_hostname_pool() {
        let pool = vec![ep(Scheme::Https, "api.openai.com", 443)];
        let err = resolve_create_alias(&pool, Some("my-openai")).expect_err("must reject");
        assert_eq!(err.status(), 400);
        // Exact match is tolerated for idempotency.
        assert_eq!(
            resolve_create_alias(&pool, Some("Api.OpenAI.COM")).expect("idempotent"),
            "api.openai.com"
        );
    }

    #[test]
    fn create_requires_alias_for_ip_pool() {
        let pool = vec![ep(Scheme::Https, "10.0.1.1", 443)];
        assert!(resolve_create_alias(&pool, None).is_err());
        assert_eq!(
            resolve_create_alias(&pool, Some("my-internal-service")).expect("explicit alias"),
            "my-internal-service"
        );
    }

    #[test]
    fn update_allows_endpoint_change_that_keeps_alias() {
        let pool = vec![
            ep(Scheme::Https, "us.vendor.com", 443),
            ep(Scheme::Https, "eu.vendor.com", 443),
            ep(Scheme::Https, "apac.vendor.com", 443),
        ];
        assert_eq!(
            enforce_alias_update("vendor.com", &pool, None).expect("alias unchanged"),
            "vendor.com"
        );
    }

    #[test]
    fn update_rejects_endpoint_change_that_moves_alias() {
        let pool = vec![ep(Scheme::Https, "api.anthropic.com", 443)];
        let err = enforce_alias_update("api.openai.com", &pool, None).expect_err("must reject");
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn update_rejects_alias_override() {
        let pool = vec![ep(Scheme::Https, "10.0.1.1", 443)];
        let err = enforce_alias_update("svc-a", &pool, Some("svc-b")).expect_err("must reject");
        assert_eq!(err.status(), 400);
        assert_eq!(
            enforce_alias_update("svc-a", &pool, Some("svc-a")).expect("no-op"),
            "svc-a"
        );
    }

    #[test]
    fn update_rejects_derivable_to_nonderivable_transition() {
        let pool = vec![ep(Scheme::Https, "10.0.1.1", 443)];
        // Existing alias was derived from a hostname; the IP pool derives
        // nothing, so the stored alias is retained rather than silently
        // becoming a stale hostname key.
        assert_eq!(
            enforce_alias_update("api.openai.com", &pool, None).expect("retained"),
            "api.openai.com"
        );
    }

    #[test]
    fn pool_must_be_homogeneous() {
        let mixed = vec![
            ep(Scheme::Https, "a.vendor.com", 443),
            ep(Scheme::Http, "b.vendor.com", 443),
        ];
        assert!(validate_pool(&mixed).is_err());

        let ports = vec![
            ep(Scheme::Https, "a.vendor.com", 443),
            ep(Scheme::Https, "b.vendor.com", 8443),
        ];
        assert!(validate_pool(&ports).is_err());

        assert!(validate_pool(&[]).is_err());
    }

    #[test]
    fn hostname_validation_rfc1123() {
        assert!(validate_hostname("api.openai.com").is_ok());
        assert!(validate_hostname("10.0.1.1").is_ok());
        assert!(validate_hostname("-bad.example.com").is_err());
        assert!(validate_hostname("bad-.example.com").is_err());
        assert!(validate_hostname("bad_host.example.com").is_err());
        assert!(validate_hostname(&"a".repeat(64)).is_err());
        assert!(validate_hostname("").is_err());
    }

    #[test]
    fn alias_grammar() {
        assert!(is_valid_alias("api.openai.com"));
        assert!(is_valid_alias("vendor.com:8443"));
        assert!(is_valid_alias("my-internal-service"));
        assert!(is_valid_alias("a"));
        assert!(!is_valid_alias(""));
        assert!(!is_valid_alias("-leading"));
        assert!(!is_valid_alias("trailing-"));
        assert!(!is_valid_alias("Upper.Case"));
        assert!(!is_valid_alias("has space"));
    }

    #[test]
    fn normalization_is_case_and_dot_insensitive() {
        assert_eq!(normalize("Api.OpenAI.COM."), "api.openai.com");
        assert_eq!(normalize_host("API.Example.COM."), "api.example.com");
    }
}
