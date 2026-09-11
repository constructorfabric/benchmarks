//! Alias derivation, normalization and endpoint validation
//! (`cpt-cf-oagw-fr-alias-resolution`).
//!
//! Alias behaviour is decided entirely by endpoint type: hostname-based
//! endpoints always auto-derive, IP-based or otherwise non-derivable pools
//! require an explicit alias.

use std::net::IpAddr;

use crate::domain::error::{OagwError, OagwResult};
use crate::domain::model::{Endpoint, Scheme};

/// Normalize an alias: ASCII lowercase with trailing dots stripped.
#[must_use]
pub fn normalize_alias(raw: &str) -> String {
    raw.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Normalize a hostname: ASCII lowercase, trailing FQDN dot stripped.
#[must_use]
pub fn normalize_host(raw: &str) -> String {
    raw.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Validate an alias against `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
#[must_use]
pub fn is_valid_alias(alias: &str) -> bool {
    let bytes = alias.as_bytes();
    if bytes.is_empty() {
        return false;
    }
    let alnum = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    if !alnum(bytes[0]) {
        return false;
    }
    if bytes.len() == 1 {
        return true;
    }
    if !alnum(bytes[bytes.len() - 1]) {
        return false;
    }
    bytes[1..bytes.len() - 1]
        .iter()
        .all(|&b| alnum(b) || b == b'.' || b == b':' || b == b'-')
}

/// Whether a host literal is an IP address rather than a hostname.
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    let trimmed = host.trim_start_matches('[').trim_end_matches(']');
    trimmed.parse::<IpAddr>().is_ok()
}

/// RFC 1123 hostname validation, as specified under "Hostname Validation".
///
/// # Errors
/// Returns a validation error naming the first rule the host breaks.
pub fn validate_hostname(host: &str) -> OagwResult<()> {
    if host.is_empty() {
        return Err(OagwError::validation("endpoint host must not be empty"));
    }
    if is_ip_literal(host) {
        return Ok(());
    }
    if host.len() > 253 {
        return Err(OagwError::validation(format!(
            "endpoint host '{host}' exceeds the 253-character limit"
        )));
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > 63 {
            return Err(OagwError::validation(format!(
                "endpoint host '{host}' has a label that is empty or longer than 63 characters"
            )));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(OagwError::validation(format!(
                "endpoint host '{host}' contains a label with characters outside [A-Za-z0-9-]"
            )));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(OagwError::validation(format!(
                "endpoint host '{host}' has a label starting or ending with a hyphen"
            )));
        }
    }
    Ok(())
}

/// The longest common domain suffix of `hosts`, as a registrable domain.
///
/// Returns `None` when the shared suffix has fewer than two labels or is a
/// bare public suffix (`co.uk`), both of which make the pool non-derivable.
#[must_use]
pub fn common_domain_suffix(hosts: &[String]) -> Option<String> {
    if hosts.is_empty() {
        return None;
    }
    let split: Vec<Vec<&str>> = hosts
        .iter()
        .map(|h| h.split('.').collect::<Vec<_>>())
        .collect();
    let min_len = split.iter().map(Vec::len).min()?;

    let mut shared = 0usize;
    'outer: while shared < min_len {
        let idx_from_end = shared + 1;
        let reference = split[0][split[0].len() - idx_from_end];
        for labels in &split[1..] {
            if labels[labels.len() - idx_from_end] != reference {
                break 'outer;
            }
        }
        shared += 1;
    }

    if shared < 2 {
        return None;
    }
    let labels = &split[0][split[0].len() - shared..];
    let candidate = labels.join(".");

    // The shared suffix must be a registrable domain, not a bare public
    // suffix: `foo.co.uk` + `bar.co.uk` share `co.uk`, which is not.
    if psl::suffix_str(&candidate).is_some_and(|s| s.eq_ignore_ascii_case(&candidate)) {
        return None;
    }
    if psl::domain_str(&candidate).is_none() {
        return None;
    }
    Some(candidate)
}

/// Compute the alias derived from a pool of endpoints, if derivable.
///
/// * single hostname → `host`, or `host:port` on a non-standard port
/// * several hostnames sharing a registrable domain → `suffix[:port]`
/// * IP literals, or no registrable common suffix → `None`
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }
    if endpoints.iter().any(|e| is_ip_literal(&e.host)) {
        return None;
    }

    let scheme = endpoints[0].scheme;
    let port = endpoints[0].port;

    let mut hosts: Vec<String> = Vec::new();
    for ep in endpoints {
        let host = normalize_host(&ep.host);
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }

    let base = if hosts.len() == 1 {
        hosts[0].clone()
    } else {
        common_domain_suffix(&hosts)?
    };

    if port == scheme.standard_port() {
        Some(base)
    } else {
        Some(format!("{base}:{port}"))
    }
}

/// Whether the derived alias came from a *common suffix* over several hosts,
/// which is what makes `X-OAGW-Target-Host` mandatory at proxy time
/// (ADR-0001, behaviour matrix).
#[must_use]
pub fn alias_is_common_suffix(endpoints: &[Endpoint], alias: &str) -> bool {
    if endpoints.len() < 2 {
        return false;
    }
    let hosts: Vec<String> = {
        let mut out: Vec<String> = Vec::new();
        for ep in endpoints {
            let h = normalize_host(&ep.host);
            if !out.contains(&h) {
                out.push(h);
            }
        }
        out
    };
    if hosts.len() < 2 {
        return false;
    }
    compute_derived_alias(endpoints).is_some_and(|derived| derived == alias)
}

/// Endpoint-pool invariants: at least one endpoint, and identical scheme and
/// port across the pool (`cpt-cf-oagw-fr-alias-resolution`, "Multi-Endpoint
/// Pooling").
///
/// # Errors
/// Returns a validation error describing the first violated invariant.
pub fn validate_endpoints(endpoints: &[Endpoint]) -> OagwResult<()> {
    let Some(first) = endpoints.first() else {
        return Err(OagwError::validation(
            "server.endpoints must contain at least one endpoint",
        ));
    };
    for ep in endpoints {
        validate_hostname(&ep.host)?;
        if ep.port == 0 {
            return Err(OagwError::validation("endpoint port must be 1-65535"));
        }
        if ep.scheme != first.scheme {
            return Err(OagwError::validation(
                "all endpoints of an upstream must share the same scheme",
            ));
        }
        if ep.port != first.port {
            return Err(OagwError::validation(
                "all endpoints of an upstream must share the same port",
            ));
        }
    }
    Ok(())
}

/// Resolve the effective alias at create time.
///
/// * derivable pool: the user may omit the alias or repeat the derived value
///   verbatim; anything else is a `400`
/// * non-derivable pool: an explicit alias is mandatory
///
/// # Errors
/// Returns `400 ValidationError` when the rules above are broken.
pub fn resolve_create_alias(
    endpoints: &[Endpoint],
    provided: Option<&str>,
) -> OagwResult<String> {
    let provided = provided.map(normalize_alias).filter(|s| !s.is_empty());
    if let Some(alias) = provided.as_deref()
        && !is_valid_alias(alias)
    {
        return Err(OagwError::validation(format!(
            "alias '{alias}' does not match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$"
        )));
    }

    match compute_derived_alias(endpoints) {
        Some(derived) => match provided {
            None => Ok(derived),
            Some(alias) if alias == derived => Ok(derived),
            Some(alias) => Err(OagwError::validation(format!(
                "alias is auto-derived for hostname-based endpoints; expected '{derived}', got \
                 '{alias}'"
            ))),
        },
        None => provided.ok_or_else(|| {
            OagwError::validation(
                "alias is required: the endpoint pool is IP-based or has no registrable common \
                 domain suffix, so no alias can be derived",
            )
        }),
    }
}

/// Enforce alias immutability on replace (`PUT`).
///
/// The alias is the routing key in `/v1/proxy/{alias}/...`, so any change is
/// rejected; the operator must delete and re-create.
///
/// # Errors
/// Returns `400 ValidationError` when the update would change the alias.
pub fn enforce_alias_update(
    existing_alias: &str,
    endpoints: &[Endpoint],
    provided: Option<&str>,
) -> OagwResult<String> {
    let provided = provided.map(normalize_alias).filter(|s| !s.is_empty());
    if let Some(alias) = provided.as_deref()
        && !is_valid_alias(alias)
    {
        return Err(OagwError::validation(format!(
            "alias '{alias}' does not match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$"
        )));
    }
    if let Some(alias) = provided.as_deref()
        && alias != existing_alias
    {
        return Err(OagwError::validation(format!(
            "alias is immutable once set: '{existing_alias}' cannot become '{alias}'; delete and \
             re-create the upstream instead"
        )));
    }

    match compute_derived_alias(endpoints) {
        Some(derived) if derived == existing_alias => Ok(derived),
        Some(derived) => Err(OagwError::validation(format!(
            "endpoint change would move the derived alias from '{existing_alias}' to '{derived}'; \
             the alias is immutable — delete and re-create the upstream instead"
        ))),
        // Non-derivable pool: the existing (explicit) alias is retained.
        None => Ok(existing_alias.to_owned()),
    }
}

/// Standard-port helper re-exported for the DTO layer.
#[must_use]
pub fn default_port(scheme: Scheme) -> u16 {
    scheme.standard_port()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::Scheme;

    fn ep(scheme: Scheme, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn single_hostname_standard_port_derives_the_hostname() {
        let eps = vec![ep(Scheme::Https, "api.openai.com", 443)];
        assert_eq!(
            compute_derived_alias(&eps).as_deref(),
            Some("api.openai.com")
        );
    }

    #[test]
    fn single_hostname_non_standard_port_keeps_the_port() {
        let eps = vec![ep(Scheme::Https, "api.openai.com", 8443)];
        assert_eq!(
            compute_derived_alias(&eps).as_deref(),
            Some("api.openai.com:8443")
        );
    }

    #[test]
    fn http_on_port_80_is_standard() {
        let eps = vec![ep(Scheme::Http, "example.com", 80)];
        assert_eq!(compute_derived_alias(&eps).as_deref(), Some("example.com"));
        let eps = vec![ep(Scheme::Http, "example.com", 8080)];
        assert_eq!(
            compute_derived_alias(&eps).as_deref(),
            Some("example.com:8080")
        );
    }

    #[test]
    fn multi_host_uses_the_registrable_common_suffix() {
        let eps = vec![
            ep(Scheme::Https, "us.vendor.com", 443),
            ep(Scheme::Https, "eu.vendor.com", 443),
        ];
        assert_eq!(compute_derived_alias(&eps).as_deref(), Some("vendor.com"));
        assert!(alias_is_common_suffix(&eps, "vendor.com"));
    }

    #[test]
    fn multi_host_common_suffix_preserves_a_non_standard_port() {
        let eps = vec![
            ep(Scheme::Https, "us.vendor.com", 8443),
            ep(Scheme::Https, "eu.vendor.com", 8443),
        ];
        assert_eq!(
            compute_derived_alias(&eps).as_deref(),
            Some("vendor.com:8443")
        );
    }

    #[test]
    fn a_bare_public_suffix_is_not_derivable() {
        let eps = vec![
            ep(Scheme::Https, "foo.co.uk", 443),
            ep(Scheme::Https, "bar.co.uk", 443),
        ];
        assert_eq!(compute_derived_alias(&eps), None);
    }

    #[test]
    fn heterogeneous_hosts_are_not_derivable() {
        let eps = vec![
            ep(Scheme::Https, "us.foo.com", 443),
            ep(Scheme::Https, "eu.bar.com", 443),
        ];
        assert_eq!(compute_derived_alias(&eps), None);
    }

    #[test]
    fn ip_pools_are_not_derivable() {
        let eps = vec![
            ep(Scheme::Https, "10.0.1.1", 443),
            ep(Scheme::Https, "10.0.1.2", 443),
        ];
        assert_eq!(compute_derived_alias(&eps), None);
        assert!(!alias_is_common_suffix(&eps, "my-service"));
    }

    #[test]
    fn create_rejects_a_user_alias_on_a_derivable_pool() {
        let eps = vec![ep(Scheme::Https, "api.openai.com", 443)];
        let err = resolve_create_alias(&eps, Some("openai")).expect_err("must reject");
        assert_eq!(err.status(), 400);
        // The exact derived value is tolerated for idempotency.
        assert_eq!(
            resolve_create_alias(&eps, Some("API.OpenAI.COM")).expect("idempotent"),
            "api.openai.com"
        );
        assert_eq!(
            resolve_create_alias(&eps, None).expect("derived"),
            "api.openai.com"
        );
    }

    #[test]
    fn create_requires_an_alias_on_a_non_derivable_pool() {
        let eps = vec![
            ep(Scheme::Https, "10.0.1.1", 443),
            ep(Scheme::Https, "10.0.1.2", 443),
        ];
        assert!(resolve_create_alias(&eps, None).is_err());
        assert_eq!(
            resolve_create_alias(&eps, Some("my-internal-service")).expect("explicit"),
            "my-internal-service"
        );
    }

    #[test]
    fn update_rejects_an_endpoint_change_that_moves_the_alias() {
        let old = vec![ep(Scheme::Https, "api.openai.com", 443)];
        let new = vec![ep(Scheme::Https, "api.anthropic.com", 443)];
        assert!(enforce_alias_update("api.openai.com", &old, None).is_ok());
        assert!(enforce_alias_update("api.openai.com", &new, None).is_err());
        // Even with an explicit alias, hostname -> IP is rejected.
        let to_ip = vec![ep(Scheme::Https, "10.0.0.1", 443)];
        assert!(enforce_alias_update("api.openai.com", &to_ip, Some("other")).is_err());
    }

    #[test]
    fn update_retains_the_explicit_alias_for_ip_pools() {
        let eps = vec![ep(Scheme::Https, "10.0.1.9", 443)];
        assert_eq!(
            enforce_alias_update("my-service", &eps, Some("my-service")).expect("no-op"),
            "my-service"
        );
        assert_eq!(
            enforce_alias_update("my-service", &eps, None).expect("retained"),
            "my-service"
        );
    }

    #[test]
    fn endpoint_pools_must_be_homogeneous() {
        let mixed = vec![
            ep(Scheme::Https, "a.vendor.com", 443),
            ep(Scheme::Http, "b.vendor.com", 443),
        ];
        assert!(validate_endpoints(&mixed).is_err());

        let mixed_port = vec![
            ep(Scheme::Https, "a.vendor.com", 443),
            ep(Scheme::Https, "b.vendor.com", 8443),
        ];
        assert!(validate_endpoints(&mixed_port).is_err());

        assert!(validate_endpoints(&[]).is_err());
    }

    #[test]
    fn hostname_validation_follows_rfc_1123() {
        assert!(validate_hostname("api.openai.com").is_ok());
        assert!(validate_hostname("127.0.0.1").is_ok());
        assert!(validate_hostname("::1").is_ok());
        assert!(validate_hostname("-bad.example.com").is_err());
        assert!(validate_hostname("bad-.example.com").is_err());
        assert!(validate_hostname("a..b").is_err());
        assert!(validate_hostname(&"a".repeat(64)).is_err());
        assert!(validate_hostname("under_score.example.com").is_err());
    }

    #[test]
    fn alias_pattern_enforced() {
        assert!(is_valid_alias("api.openai.com"));
        assert!(is_valid_alias("vendor.com:8443"));
        assert!(is_valid_alias("my-service"));
        assert!(is_valid_alias("a"));
        assert!(!is_valid_alias(""));
        assert!(!is_valid_alias("-lead"));
        assert!(!is_valid_alias("trail-"));
        assert!(!is_valid_alias("Upper"));
        assert!(!is_valid_alias("has space"));
    }

    #[test]
    fn alias_normalization_lowercases_and_strips_trailing_dots() {
        assert_eq!(normalize_alias("Api.OpenAI.COM."), "api.openai.com");
        assert_eq!(normalize_host("Example.COM."), "example.com");
    }
}
