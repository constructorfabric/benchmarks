//! Alias derivation, validation, uniqueness and immutability.
//!
//! Aliases are the routing keys of `/api/oagw/v1/proxy/{alias}/{path}`, so they
//! are not free-form labels (PRD.md §5.5 "Alias Resolution", DESIGN.md §3.1
//! "Alias Resolution"):
//!
//! - hostname pools with a standard port derive the hostname
//! - hostname pools with a non-standard port derive `host:port`
//! - multi-hostname pools derive the longest common *registrable* domain
//!   suffix, validated against the public suffix list, optionally carrying
//!   `:port`
//! - a pool whose only common suffix is a bare public suffix (`co.uk`), whose
//!   hosts have no registrable domain in common, or that contains an IP
//!   literal is not derivable and requires an explicit alias
//! - a user-provided alias that differs from the derived value is rejected
//!   with 400; providing the exact derived value is an idempotent no-op
//! - the alias is immutable once set: an update that would change it is
//!   rejected, and the operator must delete and re-create the upstream
//! - aliases are normalized to ASCII lowercase with trailing dots stripped, and
//!   resolution is case-insensitive

use psl;

use crate::domain::model::{ALIAS_PATTERN, Endpoint, Host, Scheme};
use crate::error::GatewayError;

/// The field an alias validation failure is reported against.
pub const ALIAS_FIELD: &str = "alias";

/// Validates and normalizes a user-provided alias.
///
/// Normalization is ASCII lowercase with trailing dots stripped (DESIGN.md
/// §3.1 "Alias Normalization"); the result must match [`ALIAS_PATTERN`].
///
/// # Errors
///
/// Returns a 400 [`GatewayError`] when the normalized alias does not match
/// [`ALIAS_PATTERN`].
pub fn normalize_alias(raw: &str) -> Result<String, GatewayError> {
    let normalized = raw.trim().to_ascii_lowercase();
    let normalized = normalized.trim_end_matches('.').to_owned();

    if !matches_alias_pattern(&normalized) {
        return Err(GatewayError::validation(
            format!("alias `{raw}` must match `{ALIAS_PATTERN}`"),
            ALIAS_FIELD,
        ));
    }

    Ok(normalized)
}

/// Derives the alias of an endpoint pool, or `None` when the pool is not
/// derivable and an explicit alias is required.
///
/// Derivation succeeds for a single hostname (standard port -> hostname,
/// non-standard port -> `host:port`) and for a pool of hostnames that share a
/// registrable domain (PSL-validated). It fails for IP literals, for mixed
/// hostname/IP pools, for hostnames with no common registrable domain, and for
/// pools whose only common suffix is a bare public suffix.
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    let first = endpoints.first()?;
    let scheme = first.scheme;
    let port = first.port;
    let hosts = distinct_hostname_hosts(endpoints)?;

    if hosts.len() == 1 {
        return Some(alias_for_host(&hosts[0], scheme, port));
    }

    common_registrable_domain(&hosts).map(|domain| alias_for_host(&domain, scheme, port))
}

/// Resolves the alias of a *new* upstream: the derived value when derivable,
/// an explicit value otherwise.
///
/// # Errors
///
/// Returns a 400 [`GatewayError`] when the pool is not derivable and no alias
/// was submitted, or when the submitted alias differs from the derived value
/// (an exact match is tolerated as an idempotent no-op).
pub fn resolve_new_alias(
    submitted: Option<&str>,
    endpoints: &[Endpoint],
) -> Result<String, GatewayError> {
    if let Some(derived) = compute_derived_alias(endpoints) {
        let submitted = submitted.map(normalize_alias).transpose()?;
        match submitted {
            None => Ok(derived),
            Some(alias) if alias == derived => Ok(alias),
            Some(alias) => Err(GatewayError::validation(
                format!(
                    "alias `{alias}` differs from the auto-derived alias `{derived}`; hostname \
                     based endpoints always derive their alias"
                ),
                ALIAS_FIELD,
            )),
        }
    } else {
        let Some(raw) = submitted else {
            return Err(GatewayError::validation(
                "an explicit alias is required: the endpoints are IP-based or have no \
                 registrable domain in common",
                ALIAS_FIELD,
            ));
        };

        normalize_alias(raw)
    }
}

/// Enforces alias immutability on update (DESIGN.md §3.1 "Alias Update
/// Behavior").
///
/// The alias never changes: the existing value is returned whenever the
/// transition is legal. A transition that would change the derived alias is
/// rejected, even when an explicit alias is supplied, because the alias is the
/// routing key.
///
/// # Errors
///
/// Returns a 400 [`GatewayError`] when the new endpoints would change the
/// derived alias, when a derivable pool becomes non-derivable, or when the
/// submitted alias differs from the existing one.
pub fn enforce_alias_update(
    existing_alias: &str,
    existing_endpoints: &[Endpoint],
    new_endpoints: &[Endpoint],
    submitted: Option<&str>,
) -> Result<String, GatewayError> {
    let existing_derived = compute_derived_alias(existing_endpoints);
    let new_derived = compute_derived_alias(new_endpoints);
    let submitted = submitted.map(normalize_alias).transpose()?;

    match (existing_derived, new_derived) {
        (Some(old), Some(new)) if new == old => no_op_alias(existing_alias, submitted),
        (None, Some(new)) if new == existing_alias => no_op_alias(existing_alias, submitted),
        (Some(_), _) | (None, Some(_)) => Err(immutable_alias_error(existing_alias)),
        (None, None) => no_op_alias(existing_alias, submitted),
    }
}

/// The 400 returned when an update would change the alias of a stored upstream.
fn immutable_alias_error(existing_alias: &str) -> GatewayError {
    GatewayError::validation(
        format!(
            "the alias `{existing_alias}` is immutable and these endpoints would change it; delete \
             and re-create the upstream instead"
        ),
        ALIAS_FIELD,
    )
}

/// The alias is unchanged; a submitted alias must match it exactly.
fn no_op_alias(existing: &str, submitted: Option<String>) -> Result<String, GatewayError> {
    match submitted {
        Some(alias) if alias == existing => Ok(existing.to_owned()),
        Some(alias) => Err(GatewayError::validation(
            format!(
                "the alias `{existing}` is immutable and cannot be replaced by `{alias}`; delete \
                 and re-create the upstream instead"
            ),
            ALIAS_FIELD,
        )),
        None => Ok(existing.to_owned()),
    }
}

/// Whether `port` is the standard port for `scheme` and therefore omitted from
/// a derived alias: HTTP 80, HTTPS/WSS/WebTransport/gRPC 443.
#[must_use]
pub fn is_standard_port(scheme: Scheme, port: u16) -> bool {
    scheme.standard_port() == port
}

/// The alias form of a single host: the hostname itself on the standard port,
/// `host:port` otherwise.
#[must_use]
pub fn alias_for_host(host: &Host, scheme: Scheme, port: u16) -> String {
    if is_standard_port(scheme, port) {
        host.as_str().to_owned()
    } else {
        host.with_port(port)
    }
}

/// The distinct, already-normalized hosts of a pool, or `None` when any
/// endpoint is an IP literal (which makes the whole pool non-derivable).
fn distinct_hostname_hosts(endpoints: &[Endpoint]) -> Option<Vec<Host>> {
    let mut hosts: Vec<Host> = Vec::with_capacity(endpoints.len());
    for endpoint in endpoints {
        if endpoint.host.is_ip_literal() {
            return None;
        }
        if !hosts.contains(&endpoint.host) {
            hosts.push(endpoint.host.clone());
        }
    }

    Some(hosts)
}

/// The registrable domain shared by every host, validated against the public
/// suffix list.
///
/// `foo.co.uk` and `bar.co.uk` share only the public suffix `co.uk`, so their
/// registrable domains differ and the pool is not derivable; a host that *is*
/// a bare public suffix has no registrable domain at all.
fn common_registrable_domain(hosts: &[Host]) -> Option<Host> {
    let mut registrable: Option<Host> = None;
    for host in hosts {
        let domain = psl::domain_str(host.as_str())?;
        let domain = Host::parse(domain).ok()?;
        match &registrable {
            Some(previous) if *previous == domain => {}
            Some(_) => return None,
            None => registrable = Some(domain),
        }
    }

    registrable
}

/// Whether `candidate` matches [`ALIAS_PATTERN`]:
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
fn matches_alias_pattern(candidate: &str) -> bool {
    let bytes = candidate.as_bytes();
    if bytes.is_empty() {
        return false;
    }

    let is_edge = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let is_inner = |byte: u8| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b':' | b'-')
    };

    if bytes.len() == 1 {
        return is_edge(bytes[0]);
    }

    let last = bytes.len() - 1;
    is_edge(bytes[0]) && is_edge(bytes[last]) && bytes[1..last].iter().all(|byte| is_inner(*byte))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::error::GatewayErrorKind;

    /// Endpoint with the given scheme, host and port.
    fn endpoint(scheme: Scheme, host: &str, port: u16) -> Endpoint {
        Endpoint::new(scheme, Host::parse(host).unwrap(), port)
    }

    /// `https://host:port`
    fn https(host: &str, port: u16) -> Endpoint {
        endpoint(Scheme::Https, host, port)
    }

    fn pool(endpoints: &[Endpoint]) -> Vec<Endpoint> {
        endpoints.to_vec()
    }

    fn assert_validation(error: &GatewayError, field: &str) {
        assert_eq!(error.kind(), GatewayErrorKind::Validation, "{error}");
        assert_eq!(error.status(), 400, "{error}");
        assert_eq!(error.extensions().extra["field"], field, "{error}");
    }

    // -- pattern ----------------------------------------------------------

    #[test]
    fn test_normalize_alias_accepts_pattern_matches() {
        for (raw, expected) in [
            ("api", "api"),
            ("a", "a"),
            ("0", "0"),
            ("api.openai.com", "api.openai.com"),
            ("my-service", "my-service"),
            ("a:8443", "a:8443"),
            ("a-b.c", "a-b.c"),
            ("API", "api"),
        ] {
            assert_eq!(normalize_alias(raw).unwrap(), expected, "{raw}");
        }
    }

    #[test]
    fn test_normalize_alias_normalizes_case_and_trailing_dot() {
        assert_eq!(
            normalize_alias("Api.OpenAI.COM.").unwrap(),
            "api.openai.com"
        );
        assert_eq!(normalize_alias("  MY-SERVICE  ").unwrap(), "my-service");
    }

    #[test]
    fn test_normalize_alias_rejects_pattern_violations() {
        for raw in [
            "",
            "-api",
            "api-",
            ".api",
            "api openai",
            "api/openai",
            "api+openai",
            "h\u{e9}llo",
        ] {
            let error = normalize_alias(raw).unwrap_err();
            assert_validation(&error, ALIAS_FIELD);
            assert!(error.detail().contains(ALIAS_PATTERN), "{raw}: {error}");
        }
    }

    // -- single host derivation -------------------------------------------

    #[test]
    fn test_derive_alias_single_hostname_standard_port() {
        for (scheme, port) in [
            (Scheme::Https, 443),
            (Scheme::Http, 80),
            (Scheme::Wss, 443),
            (Scheme::Wt, 443),
            (Scheme::Grpc, 443),
        ] {
            let endpoints = pool(&[endpoint(scheme, "api.openai.com", port)]);
            assert_eq!(
                compute_derived_alias(&endpoints).as_deref(),
                Some("api.openai.com"),
                "{scheme} on {port}",
            );
        }
    }

    #[test]
    fn test_derive_alias_single_hostname_non_standard_port() {
        let endpoints = pool(&[https("api.openai.com", 8443)]);

        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("api.openai.com:8443")
        );
        assert_eq!(
            compute_derived_alias(&pool(&[endpoint(Scheme::Http, "api.openai.com", 8080)]))
                .as_deref(),
            Some("api.openai.com:8080"),
        );
    }

    #[test]
    fn test_is_standard_port_follows_the_scheme() {
        assert!(is_standard_port(Scheme::Https, 443));
        assert!(is_standard_port(Scheme::Wss, 443));
        assert!(is_standard_port(Scheme::Wt, 443));
        assert!(is_standard_port(Scheme::Grpc, 443));
        assert!(is_standard_port(Scheme::Http, 80));
        assert!(!is_standard_port(Scheme::Http, 443));
        assert!(!is_standard_port(Scheme::Https, 80));
        assert!(!is_standard_port(Scheme::Https, 8443));
    }

    #[test]
    fn test_alias_for_host_matches_design_examples() {
        let host = Host::parse("api.openai.com").unwrap();

        assert_eq!(alias_for_host(&host, Scheme::Https, 443), "api.openai.com");
        assert_eq!(
            alias_for_host(&host, Scheme::Https, 8443),
            "api.openai.com:8443"
        );
        assert_eq!(alias_for_host(&host, Scheme::Http, 80), "api.openai.com");
    }

    // -- multi host derivation --------------------------------------------

    #[test]
    fn test_derive_alias_multi_host_common_registrable_suffix() {
        let endpoints = pool(&[https("us.vendor.com", 443), https("eu.vendor.com", 443)]);

        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("vendor.com")
        );
    }

    #[test]
    fn test_derive_alias_multi_host_keeps_non_standard_port() {
        let endpoints = pool(&[https("us.vendor.com", 8443), https("eu.vendor.com", 8443)]);

        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("vendor.com:8443")
        );
    }

    #[test]
    fn test_derive_alias_dedupes_identical_hosts() {
        let endpoints = pool(&[https("api.openai.com", 443), https("api.openai.com", 443)]);

        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("api.openai.com")
        );
    }

    #[test]
    fn test_derive_alias_rejects_bare_public_suffix_pool() {
        // `co.uk` is a public suffix: the registrable domains differ, so the
        // pool is not derivable and an explicit alias is required.
        let endpoints = pool(&[https("foo.co.uk", 443), https("bar.co.uk", 443)]);

        assert_eq!(compute_derived_alias(&endpoints), None);
    }

    #[test]
    fn test_derive_alias_rejects_hosts_without_common_registrable_domain() {
        let endpoints = pool(&[https("us.foo.com", 443), https("eu.bar.com", 443)]);

        assert_eq!(compute_derived_alias(&endpoints), None);
    }

    #[test]
    fn test_derive_alias_rejects_ip_and_mixed_pools() {
        assert_eq!(
            compute_derived_alias(&pool(&[https("10.0.1.1", 443)])),
            None
        );
        assert_eq!(
            compute_derived_alias(&pool(&[https("10.0.1.1", 443), https("10.0.1.2", 443)])),
            None
        );
        assert_eq!(
            compute_derived_alias(&pool(&[
                https("10.0.1.1", 443),
                https("api.vendor.com", 443)
            ])),
            None
        );
    }

    #[test]
    fn test_derive_alias_of_empty_pool_is_none() {
        assert_eq!(compute_derived_alias(&[]), None);
    }

    // -- creation ---------------------------------------------------------

    #[test]
    fn test_resolve_new_alias_uses_the_derived_value() {
        let endpoints = pool(&[https("api.openai.com", 443)]);

        assert_eq!(
            resolve_new_alias(None, &endpoints).unwrap(),
            "api.openai.com"
        );
    }

    #[test]
    fn test_resolve_new_alias_tolerates_the_exact_derived_value() {
        let endpoints = pool(&[https("api.openai.com", 443)]);

        assert_eq!(
            resolve_new_alias(Some("api.openai.com"), &endpoints).unwrap(),
            "api.openai.com"
        );
    }

    #[test]
    fn test_resolve_new_alias_normalizes_the_submitted_value() {
        let endpoints = pool(&[https("api.openai.com", 443)]);

        assert_eq!(
            resolve_new_alias(Some("API.OpenAI.COM."), &endpoints).unwrap(),
            "api.openai.com"
        );
    }

    #[test]
    fn test_resolve_new_alias_rejects_a_differing_alias() {
        let endpoints = pool(&[https("api.openai.com", 443)]);

        let error = resolve_new_alias(Some("openai"), &endpoints).unwrap_err();

        assert_validation(&error, ALIAS_FIELD);
        assert!(error.detail().contains("auto-derived"), "{error}");
    }

    #[test]
    fn test_resolve_new_alias_requires_an_explicit_alias_for_ips() {
        let endpoints = pool(&[https("10.0.1.1", 443), https("10.0.1.2", 443)]);

        let error = resolve_new_alias(None, &endpoints).unwrap_err();

        assert_validation(&error, ALIAS_FIELD);
        assert!(error.detail().contains("explicit alias"), "{error}");
    }

    #[test]
    fn test_resolve_new_alias_accepts_an_explicit_alias_for_ips() {
        let endpoints = pool(&[https("10.0.1.1", 443)]);

        assert_eq!(
            resolve_new_alias(Some("my-service"), &endpoints).unwrap(),
            "my-service"
        );
    }

    #[test]
    fn test_resolve_new_alias_requires_an_alias_for_a_bare_public_suffix_pool() {
        let endpoints = pool(&[https("foo.co.uk", 443), https("bar.co.uk", 443)]);

        assert!(resolve_new_alias(None, &endpoints).is_err());
        assert_eq!(
            resolve_new_alias(Some("uk-vendor"), &endpoints).unwrap(),
            "uk-vendor"
        );
    }

    #[test]
    fn test_resolve_new_alias_rejects_an_invalid_alias() {
        let endpoints = pool(&[https("10.0.1.1", 443)]);

        let error = resolve_new_alias(Some("-bad-alias-"), &endpoints).unwrap_err();

        assert_validation(&error, ALIAS_FIELD);
    }

    // -- immutability -----------------------------------------------------

    #[test]
    fn test_update_with_equivalent_derivation_is_allowed() {
        let existing = pool(&[https("us.vendor.com", 443), https("eu.vendor.com", 443)]);
        let updated = pool(&[https("ca.vendor.com", 443), https("ny.vendor.com", 443)]);

        assert_eq!(
            enforce_alias_update("vendor.com", &existing, &updated, None).unwrap(),
            "vendor.com"
        );
    }

    #[test]
    fn test_update_that_would_change_a_derived_alias_is_rejected() {
        let existing = pool(&[https("api.openai.com", 443)]);
        let updated = pool(&[https("api.anthropic.com", 443)]);

        let error = enforce_alias_update("api.openai.com", &existing, &updated, None).unwrap_err();

        assert_validation(&error, ALIAS_FIELD);
        assert!(error.detail().contains("delete and re-create"), "{error}");
    }

    #[test]
    fn test_update_with_an_explicit_alias_cannot_overcome_a_derivation_change() {
        let existing = pool(&[https("api.openai.com", 443)]);
        let updated = pool(&[https("api.anthropic.com", 443)]);

        let error = enforce_alias_update(
            "api.openai.com",
            &existing,
            &updated,
            Some("api.openai.com"),
        )
        .unwrap_err();

        assert_validation(&error, ALIAS_FIELD);
    }

    #[test]
    fn test_update_from_derivable_to_non_derivable_is_always_rejected() {
        let existing = pool(&[https("api.openai.com", 443)]);
        let updated = pool(&[https("10.0.1.1", 443)]);

        assert!(enforce_alias_update("api.openai.com", &existing, &updated, None).is_err());
        assert!(
            enforce_alias_update(
                "api.openai.com",
                &existing,
                &updated,
                Some("api.openai.com")
            )
            .is_err()
        );
    }

    #[test]
    fn test_update_from_non_derivable_to_non_derivable_retains_the_alias() {
        let existing = pool(&[https("10.0.1.1", 443)]);
        let updated = pool(&[https("10.0.1.2", 443)]);

        assert_eq!(
            enforce_alias_update("my-service", &existing, &updated, None).unwrap(),
            "my-service"
        );
    }

    #[test]
    fn test_update_from_non_derivable_to_non_derivable_rejects_a_new_alias() {
        let existing = pool(&[https("10.0.1.1", 443)]);
        let updated = pool(&[https("10.0.1.2", 443)]);

        let error =
            enforce_alias_update("my-service", &existing, &updated, Some("renamed")).unwrap_err();

        assert_validation(&error, ALIAS_FIELD);
    }

    #[test]
    fn test_update_from_non_derivable_to_equivalent_derivation_is_allowed() {
        let existing = pool(&[https("10.0.1.1", 443)]);
        let updated = pool(&[https("api.openai.com", 443)]);

        assert_eq!(
            enforce_alias_update("api.openai.com", &existing, &updated, None).unwrap(),
            "api.openai.com"
        );
    }

    #[test]
    fn test_update_without_endpoint_change_tolerates_an_exact_alias() {
        let existing = pool(&[https("10.0.1.1", 443)]);
        let updated = pool(&[https("10.0.1.1", 443)]);

        assert_eq!(
            enforce_alias_update("my-service", &existing, &updated, Some("my-service")).unwrap(),
            "my-service"
        );
    }

    #[test]
    fn test_update_rejects_a_differing_submitted_alias() {
        let existing = pool(&[https("api.openai.com", 443)]);

        let error = enforce_alias_update("api.openai.com", &existing, &existing, Some("other"))
            .unwrap_err();

        assert_validation(&error, ALIAS_FIELD);
        assert!(error.detail().contains("immutable"), "{error}");
    }

    #[test]
    fn test_update_normalizes_the_submitted_alias_before_comparing() {
        let existing = pool(&[https("api.openai.com", 443)]);

        assert_eq!(
            enforce_alias_update(
                "api.openai.com",
                &existing,
                &existing,
                Some("API.OPENAI.COM")
            )
            .unwrap(),
            "api.openai.com"
        );
    }
}
