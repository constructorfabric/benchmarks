//! Alias derivation, validation and update enforcement
//! (`cpt-cf-oagw-fr-alias-resolution`).
//!
//! An alias is not an arbitrary label: hostname-based endpoints always derive
//! it, IP-based (or otherwise non-derivable) pools always require it from the
//! operator, and once set it is immutable because it is the routing key in
//! `/oagw/v1/proxy/{alias}/…`.

use crate::domain::error::OagwError;
use crate::domain::model::Endpoint;

/// Maximum total length of a hostname (RFC 1123).
const MAX_HOSTNAME_LEN: usize = 253;
/// Maximum length of a single hostname label (RFC 1123).
const MAX_LABEL_LEN: usize = 63;

/// Normalize an alias: trim, ASCII-lowercase, strip trailing dots.
#[must_use]
pub fn normalize_alias(raw: &str) -> String {
    raw.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Normalize a host: trim, ASCII-lowercase, strip the FQDN trailing dot.
#[must_use]
pub fn normalize_host(raw: &str) -> String {
    raw.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// `true` when `host` parses as an IPv4/IPv6 literal (bracketed or bare).
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    bare.parse::<std::net::IpAddr>().is_ok()
}

/// Validate a hostname per RFC 1123.
///
/// # Errors
///
/// Returns a validation error naming the offending constraint.
pub fn validate_hostname(host: &str) -> Result<(), OagwError> {
    if host.is_empty() {
        return Err(OagwError::validation("endpoint host must not be empty"));
    }
    if host.len() > MAX_HOSTNAME_LEN {
        return Err(OagwError::validation(format!(
            "endpoint host '{host}' exceeds {MAX_HOSTNAME_LEN} characters"
        )));
    }
    for label in host.split('.') {
        if label.is_empty() || label.len() > MAX_LABEL_LEN {
            return Err(OagwError::validation(format!(
                "endpoint host '{host}' has a label that is empty or longer than {MAX_LABEL_LEN} characters"
            )));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(OagwError::validation(format!(
                "endpoint host '{host}' has a label with characters outside [a-z0-9-]"
            )));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(OagwError::validation(format!(
                "endpoint host '{host}' has a label starting or ending with '-'"
            )));
        }
    }
    Ok(())
}

/// Validate an alias against the schema pattern
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
///
/// # Errors
///
/// Returns a validation error when the alias is empty or contains
/// disallowed characters.
pub fn validate_alias(alias: &str) -> Result<(), OagwError> {
    let bytes = alias.as_bytes();
    let invalid = || {
        OagwError::validation(format!(
            "alias '{alias}' must match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$"
        ))
    };
    match bytes {
        [] => Err(invalid()),
        [single] => {
            if single.is_ascii_lowercase() || single.is_ascii_digit() {
                Ok(())
            } else {
                Err(invalid())
            }
        }
        [first, middle @ .., last] => {
            let edge_ok = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
            if !edge_ok(first) || !edge_ok(last) {
                return Err(invalid());
            }
            if middle
                .iter()
                .all(|b| edge_ok(b) || *b == b'.' || *b == b':' || *b == b'-')
            {
                Ok(())
            } else {
                Err(invalid())
            }
        }
    }
}

/// Longest common domain suffix of `hosts`, on whole-label boundaries.
///
/// Returns `None` when the shared suffix has fewer than two labels or is a
/// bare public suffix (`co.uk`), both of which the PRD declares
/// non-derivable.
#[must_use]
pub fn common_domain_suffix(hosts: &[String]) -> Option<String> {
    let first = hosts.first()?;
    let mut candidate: Vec<&str> = first.split('.').collect();
    for host in hosts.iter().skip(1) {
        let labels: Vec<&str> = host.split('.').collect();
        let mut shared = 0usize;
        while shared < candidate.len()
            && shared < labels.len()
            && candidate[candidate.len() - 1 - shared] == labels[labels.len() - 1 - shared]
        {
            shared += 1;
        }
        candidate = candidate.split_off(candidate.len() - shared);
        if candidate.is_empty() {
            return None;
        }
    }
    if candidate.len() < 2 {
        return None;
    }
    let suffix = candidate.join(".");
    // Reject a shared suffix that is itself a public suffix — `foo.co.uk` and
    // `bar.co.uk` share `co.uk`, which is not a registrable domain.
    if psl::domain_str(suffix.as_str()) != Some(suffix.as_str()) {
        return None;
    }
    Some(suffix)
}

/// Compute the alias implied by an endpoint pool, or `None` when derivation
/// is not possible (IP literals, heterogeneous hostnames, bare public
/// suffixes).
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }
    let hosts: Vec<String> = endpoints.iter().map(|e| normalize_host(&e.host)).collect();
    if hosts.iter().any(|h| is_ip_literal(h)) {
        return None;
    }
    let base = if hosts.len() == 1 {
        hosts[0].clone()
    } else {
        common_domain_suffix(&hosts)?
    };

    // Every endpoint in a pool shares one port (validated separately), so the
    // first endpoint decides whether the alias carries `:port`.
    let endpoint = endpoints.first()?;
    if endpoint.port == endpoint.standard_port() {
        Some(base)
    } else {
        Some(format!("{base}:{}", endpoint.port))
    }
}

/// Resolve the alias for a **create**.
///
/// * hostname pools derive it; a user-provided alias is rejected unless it is
///   exactly the derived value (tolerated as an idempotent no-op);
/// * non-derivable pools require an explicit alias.
///
/// # Errors
///
/// Returns a validation error describing which of the two rules was broken.
pub fn resolve_alias_for_create(
    endpoints: &[Endpoint],
    provided: Option<&str>,
) -> Result<String, OagwError> {
    let provided = provided.map(normalize_alias).filter(|a| !a.is_empty());
    match compute_derived_alias(endpoints) {
        Some(derived) => match provided {
            Some(explicit) if explicit != derived => Err(OagwError::validation(format!(
                "alias is auto-derived for hostname endpoints: expected '{derived}', got \
                 '{explicit}'. Omit the field or send the derived value."
            ))),
            _ => {
                validate_alias(&derived)?;
                Ok(derived)
            }
        },
        None => {
            let explicit = provided.ok_or_else(|| {
                OagwError::validation(
                    "alias is required: the endpoint pool is IP-based or has no registrable \
                     common domain suffix, so no alias can be derived",
                )
            })?;
            validate_alias(&explicit)?;
            Ok(explicit)
        }
    }
}

/// Enforce alias immutability on a **replace**.
///
/// The alias is the routing key, so any endpoint change that would alter the
/// derived alias is rejected; the operator must delete and re-create. A
/// provided alias equal to the existing one is tolerated as a no-op.
///
/// # Errors
///
/// Returns a validation error when the alias would change.
pub fn enforce_alias_update(
    existing_alias: &str,
    endpoints: &[Endpoint],
    provided: Option<&str>,
) -> Result<String, OagwError> {
    let provided = provided.map(normalize_alias).filter(|a| !a.is_empty());
    let derived = compute_derived_alias(endpoints);

    if let Some(derived) = derived {
        if derived != existing_alias {
            return Err(OagwError::validation(format!(
                "alias is immutable: the new endpoints derive '{derived}' but this upstream is \
                 addressed as '{existing_alias}'. Delete and re-create the upstream instead."
            )));
        }
        if let Some(explicit) = provided
            && explicit != existing_alias
        {
            return Err(OagwError::validation(format!(
                "alias is immutable: '{explicit}' differs from '{existing_alias}'"
            )));
        }
        return Ok(existing_alias.to_owned());
    }

    // Non-derivable pool: the existing alias is retained, and a differing
    // user-provided alias is refused.
    match provided {
        Some(explicit) if explicit != existing_alias => Err(OagwError::validation(format!(
            "alias is immutable: '{explicit}' differs from '{existing_alias}'"
        ))),
        _ => Ok(existing_alias.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        common_domain_suffix, compute_derived_alias, enforce_alias_update, is_ip_literal,
        normalize_alias, resolve_alias_for_create, validate_alias, validate_hostname,
    };
    use crate::domain::model::Endpoint;

    fn ep(scheme: &str, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: scheme.to_owned(),
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn single_hostname_standard_port() {
        let eps = vec![ep("https", "api.openai.com", 443)];
        assert_eq!(
            compute_derived_alias(&eps).as_deref(),
            Some("api.openai.com")
        );
    }

    #[test]
    fn single_hostname_non_standard_port_keeps_port() {
        let eps = vec![ep("https", "api.openai.com", 8443)];
        assert_eq!(
            compute_derived_alias(&eps).as_deref(),
            Some("api.openai.com:8443")
        );
    }

    #[test]
    fn plaintext_standard_port_is_eighty() {
        let eps = vec![ep("http", "mock.local", 80)];
        assert_eq!(compute_derived_alias(&eps).as_deref(), Some("mock.local"));
        let eps = vec![ep("http", "mock.local", 8080)];
        assert_eq!(
            compute_derived_alias(&eps).as_deref(),
            Some("mock.local:8080")
        );
    }

    #[test]
    fn multi_hostname_uses_registrable_common_suffix() {
        let eps = vec![
            ep("https", "us.vendor.com", 443),
            ep("https", "eu.vendor.com", 443),
        ];
        assert_eq!(compute_derived_alias(&eps).as_deref(), Some("vendor.com"));

        let eps = vec![
            ep("https", "us.vendor.com", 8443),
            ep("https", "eu.vendor.com", 8443),
        ];
        assert_eq!(
            compute_derived_alias(&eps).as_deref(),
            Some("vendor.com:8443")
        );
    }

    #[test]
    fn bare_public_suffix_is_not_derivable() {
        let hosts = vec!["foo.co.uk".to_owned(), "bar.co.uk".to_owned()];
        assert_eq!(common_domain_suffix(&hosts), None);
    }

    #[test]
    fn no_common_suffix_is_not_derivable() {
        let eps = vec![
            ep("https", "us.foo.com", 443),
            ep("https", "eu.bar.com", 443),
        ];
        assert_eq!(compute_derived_alias(&eps), None);
    }

    #[test]
    fn ip_pools_are_not_derivable() {
        assert!(is_ip_literal("10.0.1.1"));
        assert!(is_ip_literal("::1"));
        assert!(is_ip_literal("[::1]"));
        assert!(!is_ip_literal("api.openai.com"));
        let eps = vec![ep("https", "10.0.1.1", 443), ep("https", "10.0.1.2", 443)];
        assert_eq!(compute_derived_alias(&eps), None);
    }

    #[test]
    fn create_rejects_user_alias_for_hostname_pool() {
        let eps = vec![ep("https", "api.openai.com", 443)];
        let err = resolve_alias_for_create(&eps, Some("my-openai")).expect_err("rejected");
        assert_eq!(err.status, 400);
        // The derived value itself is tolerated for idempotency.
        assert_eq!(
            resolve_alias_for_create(&eps, Some("API.OpenAI.COM")).expect("no-op"),
            "api.openai.com"
        );
        assert_eq!(
            resolve_alias_for_create(&eps, None).expect("derived"),
            "api.openai.com"
        );
    }

    #[test]
    fn create_requires_alias_for_ip_pool() {
        let eps = vec![ep("https", "10.0.1.1", 443)];
        let err = resolve_alias_for_create(&eps, None).expect_err("required");
        assert_eq!(err.status, 400);
        assert_eq!(
            resolve_alias_for_create(&eps, Some("my-internal-service")).expect("explicit"),
            "my-internal-service"
        );
    }

    #[test]
    fn update_rejects_endpoint_change_that_moves_the_alias() {
        let eps = vec![ep("https", "api.other.com", 443)];
        let err = enforce_alias_update("api.openai.com", &eps, None).expect_err("rejected");
        assert_eq!(err.status, 400);

        let same = vec![ep("https", "api.openai.com", 443)];
        assert_eq!(
            enforce_alias_update("api.openai.com", &same, None).expect("unchanged"),
            "api.openai.com"
        );
    }

    #[test]
    fn update_rejects_hostname_to_ip_transition() {
        let eps = vec![ep("https", "10.0.1.1", 443)];
        // Non-derivable pool: the previous derived alias is retained, and a
        // differing explicit alias is refused.
        assert_eq!(
            enforce_alias_update("api.openai.com", &eps, None).expect("retained"),
            "api.openai.com"
        );
        assert!(enforce_alias_update("api.openai.com", &eps, Some("something-else")).is_err());
    }

    #[test]
    fn hostname_validation_follows_rfc_1123() {
        validate_hostname("api.openai.com").expect("valid");
        validate_hostname("a").expect("single label");
        assert!(validate_hostname("").is_err());
        assert!(validate_hostname("-bad.example.com").is_err());
        assert!(validate_hostname("bad-.example.com").is_err());
        assert!(validate_hostname("bad_label.example.com").is_err());
        assert!(validate_hostname("a..b").is_err());
        assert!(validate_hostname(&"a".repeat(64)).is_err());
        assert!(validate_hostname(&format!("{}.com", "a".repeat(250))).is_err());
    }

    #[test]
    fn alias_pattern_is_enforced() {
        validate_alias("api.openai.com").expect("valid");
        validate_alias("vendor.com:8443").expect("valid with port");
        validate_alias("my-internal-service").expect("valid explicit");
        validate_alias("a").expect("single char");
        assert!(validate_alias("").is_err());
        assert!(validate_alias("-leading").is_err());
        assert!(validate_alias("trailing-").is_err());
        assert!(validate_alias("Upper.Case").is_err());
        assert!(validate_alias("has space").is_err());
        assert!(validate_alias("has/slash").is_err());
    }

    #[test]
    fn alias_normalization_lowercases_and_strips_dots() {
        assert_eq!(normalize_alias("  Api.OpenAI.COM.  "), "api.openai.com");
    }
}
