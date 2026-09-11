//! Alias derivation, normalisation and immutability rules.
//!
//! An alias is the routing key in `/oagw/v1/proxy/{alias}/...`, so it is not
//! a free-form label: `cpt-cf-oagw-fr-alias-resolution` makes it a function
//! of the endpoint pool. Hostname pools always auto-derive; IP-based or
//! otherwise non-derivable pools require the operator to supply one. Once
//! set, the alias is immutable.

use std::net::IpAddr;

use super::error::{OagwError, OagwResult};
use super::model::{Endpoint, Scheme};

/// Maximum total hostname length (RFC 1123).
const MAX_HOSTNAME_LEN: usize = 253;
/// Maximum length of one hostname label (RFC 1123).
const MAX_LABEL_LEN: usize = 63;

/// Whether `host` is an IP literal (v4, or v6 with or without brackets).
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    let trimmed = host.trim_start_matches('[').trim_end_matches(']');
    trimmed.parse::<IpAddr>().is_ok()
}

/// Normalise a host: ASCII lowercase, trailing FQDN dot stripped.
#[must_use]
pub fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Normalise an alias: ASCII lowercase, trailing dots stripped.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Validate an endpoint hostname per RFC 1123 (or accept an IP literal).
///
/// # Errors
///
/// `400` when the host is empty, too long, or has a malformed label.
pub fn validate_host(host: &str) -> OagwResult<()> {
    let host = normalize_host(host);
    if host.is_empty() {
        return Err(OagwError::field(
            "server.endpoints[].host",
            "host must not be empty",
        ));
    }
    if is_ip_literal(&host) {
        return Ok(());
    }
    if host.len() > MAX_HOSTNAME_LEN {
        return Err(OagwError::field(
            "server.endpoints[].host",
            format!("host exceeds {MAX_HOSTNAME_LEN} characters"),
        ));
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > MAX_LABEL_LEN {
            return Err(OagwError::field(
                "server.endpoints[].host",
                format!("host label must be 1-{MAX_LABEL_LEN} characters: {host:?}"),
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(OagwError::field(
                "server.endpoints[].host",
                format!("host label must not start or end with '-': {host:?}"),
            ));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(OagwError::field(
                "server.endpoints[].host",
                format!("host label contains characters outside [a-z0-9-]: {host:?}"),
            ));
        }
    }
    Ok(())
}

/// Validate an operator-supplied alias against the schema pattern
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
///
/// # Errors
///
/// `400` when the alias does not match.
pub fn validate_alias(alias: &str) -> OagwResult<()> {
    let invalid = || {
        OagwError::field(
            "alias",
            format!(
                "alias must match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$ after normalization: \
                 {alias:?}"
            ),
        )
    };
    if alias.is_empty() || alias.len() > MAX_HOSTNAME_LEN {
        return Err(invalid());
    }
    let bytes = alias.as_bytes();
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if !alnum(bytes[0]) || !alnum(bytes[bytes.len() - 1]) {
        return Err(invalid());
    }
    if !bytes
        .iter()
        .all(|&b| alnum(b) || b == b'.' || b == b':' || b == b'-')
    {
        return Err(invalid());
    }
    Ok(())
}

/// The longest common domain suffix of `hosts`, in labels, or `None` when
/// fewer than two labels are shared.
#[must_use]
pub fn common_domain_suffix(hosts: &[String]) -> Option<String> {
    let mut label_lists: Vec<Vec<&str>> = hosts
        .iter()
        .map(|h| h.split('.').rev().collect::<Vec<_>>())
        .collect();
    let first = label_lists.pop()?;
    let mut shared: Vec<&str> = first;
    for other in &label_lists {
        let len = shared
            .iter()
            .zip(other.iter())
            .take_while(|(a, b)| a == b)
            .count();
        shared.truncate(len);
    }
    if shared.len() < 2 {
        return None;
    }
    shared.reverse();
    Some(shared.join("."))
}

/// Whether `candidate` is itself a bare public suffix (`co.uk`), which the
/// design refuses to use as an alias.
#[must_use]
pub fn is_bare_public_suffix(candidate: &str) -> bool {
    psl::suffix_str(candidate).is_some_and(|suffix| suffix == candidate)
}

/// Derive the alias for an endpoint pool, or `None` when derivation fails.
///
/// Derivation fails for IP-based pools, for heterogeneous hostnames with no
/// shared registrable suffix, and for pools whose only shared suffix is a
/// bare public suffix.
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }
    if endpoints.iter().any(Endpoint::host_is_ip) {
        return None;
    }
    let hosts: Vec<String> = endpoints.iter().map(Endpoint::normalized_host).collect();
    let scheme: Scheme = endpoints[0].scheme;
    let port = endpoints[0].port();

    // A single host — or a pool that repeats one host — names itself; only a
    // heterogeneous pool needs a shared suffix.
    let base = if hosts.iter().all(|host| *host == hosts[0]) {
        hosts[0].clone()
    } else {
        let candidate = common_domain_suffix(&hosts)?;
        if is_bare_public_suffix(&candidate) {
            return None;
        }
        candidate
    };

    if port == scheme.default_port() {
        Some(base)
    } else {
        Some(format!("{base}:{port}"))
    }
}

/// Resolve the alias for a **create**.
///
/// Hostname pools auto-derive; an operator-supplied alias is tolerated only
/// when it is exactly the derived value (idempotency). Non-derivable pools
/// require an explicit alias.
///
/// # Errors
///
/// `400` when an alias is supplied for a derivable pool and differs from the
/// derived value, or when a non-derivable pool omits it.
pub fn resolve_alias_for_create(
    endpoints: &[Endpoint],
    provided: Option<&str>,
) -> OagwResult<String> {
    let provided = provided.map(normalize_alias).filter(|a| !a.is_empty());
    match compute_derived_alias(endpoints) {
        Some(derived) => match provided {
            Some(alias) if alias != derived => Err(OagwError::field(
                "alias",
                format!(
                    "alias is auto-derived for hostname-based endpoints (derived {derived:?}); \
                     a user-provided alias is rejected"
                ),
            )),
            _ => Ok(derived),
        },
        None => {
            let alias = provided.ok_or_else(|| {
                OagwError::field(
                    "alias",
                    "an explicit alias is required for IP-based or non-derivable endpoints",
                )
            })?;
            validate_alias(&alias)?;
            Ok(alias)
        }
    }
}

/// Enforce the alias-immutability matrix on a **replace**.
///
/// The alias is the routing key, so any endpoint change that would alter the
/// derived alias is refused — the operator deletes and re-creates instead.
///
/// # Errors
///
/// `400` when the alias would change, or when a differing alias is supplied.
pub fn enforce_alias_update(
    existing: &str,
    endpoints: &[Endpoint],
    provided: Option<&str>,
) -> OagwResult<String> {
    let provided = provided.map(normalize_alias).filter(|a| !a.is_empty());
    match compute_derived_alias(endpoints) {
        Some(derived) => {
            if let Some(alias) = &provided
                && *alias != derived
            {
                return Err(OagwError::field(
                    "alias",
                    format!(
                        "alias is auto-derived for hostname-based endpoints (derived \
                         {derived:?}); an alias override is not allowed"
                    ),
                ));
            }
            if derived != existing {
                return Err(OagwError::field(
                    "alias",
                    format!(
                        "alias is immutable once set: endpoints would derive {derived:?} but \
                         the upstream is routed as {existing:?}; delete and re-create instead"
                    ),
                ));
            }
            Ok(derived)
        }
        None => {
            if let Some(alias) = &provided
                && alias != existing
            {
                return Err(OagwError::field(
                    "alias",
                    format!(
                        "alias is immutable once set ({existing:?}); delete and re-create \
                         instead"
                    ),
                ));
            }
            Ok(existing.to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(scheme: Scheme, host: &str, port: Option<u16>) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn single_hostname_standard_port_derives_the_hostname() {
        let endpoints = vec![endpoint(Scheme::Https, "api.openai.com", Some(443))];
        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("api.openai.com")
        );
    }

    #[test]
    fn single_hostname_nonstandard_port_keeps_the_port() {
        let endpoints = vec![endpoint(Scheme::Https, "api.openai.com", Some(8443))];
        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("api.openai.com:8443")
        );
    }

    #[test]
    fn plaintext_default_port_is_omitted() {
        let endpoints = vec![endpoint(Scheme::Http, "example.com", Some(80))];
        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("example.com")
        );
        let endpoints = vec![endpoint(Scheme::Http, "example.com", Some(8080))];
        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("example.com:8080")
        );
    }

    #[test]
    fn multi_host_common_suffix_derives_the_suffix() {
        let endpoints = vec![
            endpoint(Scheme::Https, "us.vendor.com", Some(443)),
            endpoint(Scheme::Https, "eu.vendor.com", Some(443)),
        ];
        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("vendor.com")
        );
    }

    #[test]
    fn multi_host_common_suffix_keeps_a_nonstandard_port() {
        let endpoints = vec![
            endpoint(Scheme::Https, "us.vendor.com", Some(8443)),
            endpoint(Scheme::Https, "eu.vendor.com", Some(8443)),
        ];
        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("vendor.com:8443")
        );
    }

    #[test]
    fn bare_public_suffix_is_not_derivable() {
        let endpoints = vec![
            endpoint(Scheme::Https, "foo.co.uk", Some(443)),
            endpoint(Scheme::Https, "bar.co.uk", Some(443)),
        ];
        assert!(compute_derived_alias(&endpoints).is_none());
    }

    #[test]
    fn no_common_suffix_is_not_derivable() {
        let endpoints = vec![
            endpoint(Scheme::Https, "us.foo.com", Some(443)),
            endpoint(Scheme::Https, "eu.bar.com", Some(443)),
        ];
        assert!(compute_derived_alias(&endpoints).is_none());
    }

    #[test]
    fn ip_pools_are_not_derivable() {
        let endpoints = vec![
            endpoint(Scheme::Https, "10.0.1.1", Some(443)),
            endpoint(Scheme::Https, "10.0.1.2", Some(443)),
        ];
        assert!(compute_derived_alias(&endpoints).is_none());
    }

    #[test]
    fn create_rejects_an_alias_for_a_derivable_pool() {
        let endpoints = vec![endpoint(Scheme::Https, "api.openai.com", Some(443))];
        assert!(resolve_alias_for_create(&endpoints, Some("my-openai")).is_err());
        // The exact derived value is tolerated for idempotency.
        assert_eq!(
            resolve_alias_for_create(&endpoints, Some("API.OpenAI.COM."))
                .expect("exact derived value tolerated"),
            "api.openai.com"
        );
    }

    #[test]
    fn create_requires_an_alias_for_an_ip_pool() {
        let endpoints = vec![endpoint(Scheme::Https, "10.0.1.1", Some(443))];
        assert!(resolve_alias_for_create(&endpoints, None).is_err());
        // Aliases normalize to ASCII lowercase before the pattern check.
        assert_eq!(
            resolve_alias_for_create(&endpoints, Some("My-Internal-Service")).unwrap(),
            "my-internal-service"
        );
        assert!(
            resolve_alias_for_create(&endpoints, Some("bad_alias")).is_err(),
            "underscores are outside the alias pattern"
        );
    }

    #[test]
    fn update_rejects_an_alias_changing_endpoint_swap() {
        let old = vec![endpoint(Scheme::Https, "api.openai.com", Some(443))];
        let new = vec![endpoint(Scheme::Https, "api.anthropic.com", Some(443))];
        let existing = compute_derived_alias(&old).unwrap();
        assert!(enforce_alias_update(&existing, &new, None).is_err());
        // Same endpoints: the recomputed alias equals the existing one.
        assert_eq!(
            enforce_alias_update(&existing, &old, None).unwrap(),
            "api.openai.com"
        );
    }

    #[test]
    fn update_rejects_derivable_to_nonderivable_even_with_an_alias() {
        let new = vec![endpoint(Scheme::Https, "10.0.0.1", Some(443))];
        assert!(enforce_alias_update("api.openai.com", &new, Some("something")).is_err());
        // Keeping the existing alias is a no-op and is tolerated.
        assert_eq!(
            enforce_alias_update("api.openai.com", &new, Some("api.openai.com")).unwrap(),
            "api.openai.com"
        );
    }

    #[test]
    fn hostname_validation_follows_rfc1123() {
        assert!(validate_host("api.openai.com").is_ok());
        assert!(
            validate_host("api.openai.com.").is_ok(),
            "trailing dot tolerated"
        );
        assert!(validate_host("10.0.0.1").is_ok());
        assert!(validate_host("::1").is_ok());
        assert!(validate_host("-bad.example.com").is_err());
        assert!(validate_host("bad-.example.com").is_err());
        assert!(validate_host("bad_host.example.com").is_err());
        assert!(validate_host("").is_err());
        assert!(validate_host(&format!("{}.com", "a".repeat(64))).is_err());
    }

    #[test]
    fn alias_pattern_is_enforced() {
        assert!(validate_alias("api.openai.com").is_ok());
        assert!(validate_alias("api.openai.com:8443").is_ok());
        assert!(validate_alias("my-service").is_ok());
        assert!(validate_alias("-nope").is_err());
        assert!(validate_alias("nope-").is_err());
        assert!(validate_alias("Nope").is_err());
        assert!(validate_alias("no_underscores").is_err());
    }

    #[test]
    fn common_suffix_needs_two_labels() {
        assert_eq!(
            common_domain_suffix(&["a.example.com".to_owned(), "b.example.com".to_owned()])
                .as_deref(),
            Some("example.com")
        );
        assert_eq!(
            common_domain_suffix(&["a.com".to_owned(), "b.com".to_owned()]),
            None,
            "only one shared label"
        );
    }
}
