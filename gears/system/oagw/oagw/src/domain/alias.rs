//! Alias derivation, normalization, and enforcement rules (DESIGN.md §
//! Alias Derivation and Resolution).
//!
//! * Derivation: single hostname with standard port (HTTP:80,
//!   HTTPS/WSS/WT/gRPC:443) → hostname; non-standard port → hostname:port;
//!   multiple hostnames with a common registrable-domain suffix (≥2 labels,
//!   PSL-validated) and standard port → common suffix; non-standard port →
//!   common suffix:port. IPs are never derivable.
//! * Normalization: ASCII lowercase, trailing dots stripped.
//! * Enforcement: a user-supplied alias must either exactly match the derived
//!   value (idempotent no-op) or be explicitly required (IP / non-derivable,
//!   multi-host without common suffix).

use std::net::IpAddr;

use super::model::Endpoint;

/// Maximum total hostname length (RFC 1123).
const MAX_HOSTNAME_LEN: usize = 253;
/// Maximum label length (RFC 1123).
const MAX_LABEL_LEN: usize = 63;

/// Normalize an alias/hostname: ASCII lowercase, trailing dots stripped.
#[must_use]
pub fn normalize_alias(input: &str) -> String {
    input
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// Validate an RFC 1123 hostname (or IP literal). Returns the normalized
/// hostname on success.
pub fn validate_hostname(host: &str) -> Result<String, String> {
    let normalized = normalize_alias(host);
    if normalized.is_empty() {
        return Err("host must not be empty".to_owned());
    }
    // IP literals are valid hosts (IPv6 in brackets not expected here).
    if normalized.parse::<IpAddr>().is_ok() {
        return Ok(normalized);
    }
    if normalized.len() > MAX_HOSTNAME_LEN {
        return Err(format!(
            "hostname exceeds {MAX_HOSTNAME_LEN} characters: {normalized}"
        ));
    }
    for label in normalized.split('.') {
        if label.is_empty() {
            return Err(format!("hostname contains an empty label: {normalized}"));
        }
        if label.len() > MAX_LABEL_LEN {
            return Err(format!("hostname label exceeds {MAX_LABEL_LEN} characters: {label}"));
        }
        if !is_rfc1123_label(label) {
            return Err(format!(
                "hostname label `{label}` must be alphanumeric or hyphen, without leading/trailing hyphen"
            ));
        }
    }
    Ok(normalized)
}

fn is_rfc1123_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    if bytes.is_empty() || bytes[0] == b'-' || bytes[bytes.len() - 1] == b'-' {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
}

/// The standard port for a scheme (HTTP:80, HTTPS/WSS/WT/gRPC:443).
#[must_use]
pub fn standard_port(scheme: &str) -> Option<u16> {
    match scheme {
        "http" => Some(80),
        "https" | "wss" | "wt" | "grpc" => Some(443),
        _ => None,
    }
}

/// Whether `port` is the standard port for `scheme`.
#[must_use]
pub fn is_standard_port(scheme: &str, port: u16) -> bool {
    standard_port(scheme) == Some(port)
}

/// True when `host` is a hostname (not an IP literal).
#[must_use]
pub fn is_hostname(host: &str) -> bool {
    host.parse::<IpAddr>().is_err()
}

/// The IP-literal form for a single endpoint (None when the host is a
/// hostname).
#[must_use]
pub fn ip_of(host: &str) -> Option<IpAddr> {
    host.parse::<IpAddr>().ok()
}

/// Longest suffix of labels shared by every host, as a dotted string.
/// Returns `None` when there are no hosts.
fn common_suffix(hosts: &[String]) -> Option<String> {
    let mut suffix: Vec<&str> = Vec::new();
    let first: Vec<&str> = hosts[0].split('.').collect();
    'label: for idx in (0..first.len()).rev() {
        let label = first[idx];
        for host in &hosts[1..] {
            let labels: Vec<&str> = host.split('.').collect();
            let pos = labels.len() - (first.len() - idx);
            if labels.get(pos) != Some(&label) {
                break 'label;
            }
        }
        suffix.insert(0, label);
    }
    if suffix.is_empty() {
        None
    } else {
        Some(suffix.join("."))
    }
}

/// Whether `suffix` is a registrable domain acceptable as a derived alias:
/// at least two labels, not itself a public suffix (PSL-validated).
fn is_registrable_domain(suffix: &str) -> bool {
    if suffix.split('.').count() < 2 {
        return false;
    }
    // `psl::domain_str` returns the registrable (eTLD+1) domain for a valid
    // domain, and errors for bare public suffixes (e.g. `com`, `co.uk`).
    psl::domain_str(suffix).is_some() && psl::domain_str(suffix).unwrap() == suffix
}

/// Compute the derived alias for a set of endpoints (DESIGN.md alias
/// derivation). Returns `None` when no alias can be derived (explicit alias
/// required).
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }
    let scheme = &endpoints[0].scheme;
    let port = endpoints[0].port;
    // Pool endpoints must share scheme and port to be derivable.
    if endpoints
        .iter()
        .any(|e| e.scheme != *scheme || e.port != port)
    {
        return None;
    }
    let hosts: Option<Vec<String>> = endpoints
        .iter()
        .map(|e| {
            if is_hostname(&e.host) {
                validate_hostname(&e.host).ok()
            } else {
                None // IP endpoints require an explicit alias
            }
        })
        .collect();
    let hosts = hosts?;

    let base = if hosts.len() == 1 {
        hosts[0].clone()
    } else {
        let suffix = common_suffix(&hosts)?;
        if !is_registrable_domain(&suffix) {
            return None;
        }
        suffix
    };

    let with_port = if is_standard_port(scheme, port) {
        base
    } else {
        format!("{base}:{port}")
    };
    Some(normalize_alias(&with_port))
}

/// Validate the endpoint pool invariants for alias derivation and report
/// whether the user's alias choice is permitted.
///
/// Returns `Ok(())` when `provided` (when non-empty) is acceptable:
/// * equals the derived alias (idempotent no-op), or
/// * the derivation is impossible and any explicit alias is required.
pub fn validate_alias_choice(
    endpoints: &[Endpoint],
    provided: Option<&str>,
) -> Result<(), String> {
    resolve_alias_for_create(endpoints, provided).map(|_| ())
}

/// Whether an alias matches the schema pattern
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
#[must_use]
pub fn alias_pattern_ok(alias: &str) -> bool {
    let bytes = alias.as_bytes();
    if bytes.is_empty()
        || !bytes[0].is_ascii_alphanumeric()
        || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
    {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b':' | b'-'))
}

/// Validate that an alias is globally well-formed, returning the normalized
/// form.
pub fn normalize_and_check_alias(input: &str) -> Result<String, String> {
    let normalized = normalize_alias(input);
    if !alias_pattern_ok(&normalized) {
        return Err(format!(
            "alias `{input}` does not match the required pattern (lowercase alphanumeric, `.`, `:`, `-`)"
        ));
    }
    Ok(normalized)
}

/// Resolve the final alias for a new upstream (DESIGN.md alias enforcement
/// rules). Derivable endpoints yield the derived alias (a provided alias must
/// equal it, tolerating the exact value for idempotency); non-derivable
/// endpoints require an explicit, pattern-valid alias.
pub fn resolve_alias_for_create(
    endpoints: &[Endpoint],
    provided: Option<&str>,
) -> Result<String, String> {
    match compute_derived_alias(endpoints) {
        Some(derived) => match provided.map(normalize_alias) {
            Some(p) if p != derived => Err(format!(
                "alias `{p}` does not match the derived alias `{derived}` for these endpoints"
            )),
            _ => Ok(derived),
        },
        None => {
            let p = provided.ok_or_else(|| {
                "an explicit alias is required: the endpoints are not derivable (IP-based or no registrable common suffix)"
                    .to_owned()
            })?;
            let p = normalize_alias(p);
            if !alias_pattern_ok(&p) {
                return Err(format!("alias `{p}` does not match the alias pattern"));
            }
            Ok(p)
        }
    }
}

/// Enforce the alias update transitions table (DESIGN.md): the alias is
/// immutable once set, so the returned alias always equals the existing
/// alias. Endpoint changes that would alter the derived alias, or that move a
/// derivable upstream to non-derivable, are rejected — the operator must
/// delete and re-create the upstream.
pub fn enforce_alias_update(
    existing_alias: &str,
    old_endpoints: &[Endpoint],
    provided: Option<&str>,
    new_endpoints: &[Endpoint],
) -> Result<String, String> {
    let existing = normalize_alias(existing_alias);
    // No alias override is ever accepted; an equal value is a tolerated no-op.
    if let Some(p) = provided {
        if normalize_alias(p) != existing {
            return Err(format!(
                "alias cannot be changed: `{}` differs from the existing alias `{existing}`",
                normalize_alias(p)
            ));
        }
    }
    let old_derived = compute_derived_alias(old_endpoints);
    let new_derived = compute_derived_alias(new_endpoints);
    match (old_derived, new_derived) {
        (Some(_), Some(nd)) => {
            if nd != existing {
                return Err(format!(
                    "endpoint change would alter the derived alias to `{nd}`; the alias is immutable — delete and re-create the upstream"
                ));
            }
        }
        (Some(_), None) => {
            return Err(
                "endpoint change makes this upstream non-derivable; the alias is immutable — delete and re-create the upstream"
                    .to_owned(),
            );
        }
        (None, None) => {
            // Non-derivable -> Non-derivable: existing (explicit) alias retained.
        }
        (None, Some(nd)) => {
            if nd != existing {
                return Err(format!(
                    "endpoint change would derive alias `{nd}`; the alias is immutable — delete and re-create the upstream"
                ));
            }
        }
    }
    Ok(existing)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

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
    fn single_hostname_non_standard_port() {
        let eps = vec![ep("https", "api.openai.com", 8443)];
        assert_eq!(
            compute_derived_alias(&eps).as_deref(),
            Some("api.openai.com:8443")
        );
    }

    #[test]
    fn http_standard_port_is_80() {
        let eps = vec![ep("http", "example.test", 80)];
        assert_eq!(compute_derived_alias(&eps).as_deref(), Some("example.test"));
    }

    #[test]
    fn multi_hostname_common_registrable_suffix() {
        let eps = vec![ep("https", "us.vendor.com", 443), ep("https", "eu.vendor.com", 443)];
        assert_eq!(compute_derived_alias(&eps).as_deref(), Some("vendor.com"));
    }

    #[test]
    fn multi_hostname_common_suffix_nonstandard_port() {
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
    fn bare_public_suffix_not_derivable() {
        // us.co.uk + eu.co.uk share only `co.uk`, a public suffix.
        let eps = vec![ep("https", "us.co.uk", 443), ep("https", "eu.co.uk", 443)];
        assert_eq!(compute_derived_alias(&eps), None);
    }

    #[test]
    fn no_common_suffix_not_derivable() {
        let eps = vec![
            ep("https", "api.openai.com", 443),
            ep("https", "api.anthropic.com", 443),
        ];
        assert_eq!(compute_derived_alias(&eps), None);
    }

    #[test]
    fn ip_not_derivable() {
        let eps = vec![ep("https", "10.0.0.1", 443)];
        assert_eq!(compute_derived_alias(&eps), None);
        assert_eq!(compute_derived_alias(&[ep("https", "::1", 443)]), None);
    }

    #[test]
    fn mixed_scheme_not_derivable() {
        let eps = vec![ep("https", "a.com", 443), ep("wss", "a.com", 443)];
        assert_eq!(compute_derived_alias(&eps), None);
    }

    #[test]
    fn mixed_port_not_derivable() {
        let eps = vec![ep("https", "a.com", 443), ep("https", "a.com", 444)];
        assert_eq!(compute_derived_alias(&eps), None);
    }

    #[test]
    fn equivalent_alias_is_noop() {
        assert!(validate_alias_choice(
            &[ep("https", "api.openai.com", 443)],
            Some("API.OpenAI.COM.")
        )
        .is_ok());
        assert!(validate_alias_choice(
            &[ep("https", "10.0.0.1", 443)],
            Some("my-explicit-alias")
        )
        .is_ok());
    }

    #[test]
    fn mismatched_alias_rejected() {
        assert!(validate_alias_choice(
            &[ep("https", "api.openai.com", 443)],
            Some("openai")
        )
        .is_err());
        // Explicit alias required but none given.
        assert!(validate_alias_choice(&[ep("https", "10.0.0.1", 443)], None).is_err());
    }

    #[test]
    fn alias_pattern_enforced() {
        assert!(alias_pattern_ok("api.openai.com"));
        assert!(alias_pattern_ok("flat"));
        assert!(alias_pattern_ok("svc:8443"));
        // Underscore is not allowed by the schema pattern.
        assert!(!alias_pattern_ok("a-1.b_2"));
        assert!(!alias_pattern_ok("-leading"));
        assert!(!alias_pattern_ok("trailing-"));
    }

    #[test]
    fn hostname_validation() {
        assert!(validate_hostname("api.openai.com").is_ok());
        assert!(validate_hostname("Api.OpenAI.Com.").is_ok());
        assert!(validate_hostname("10.0.0.1").is_ok());
        assert!(validate_hostname("-bad.com").is_err());
        assert!(validate_hostname("bad-.com").is_err());
        assert!(validate_hostname("has space.com").is_err());
        assert!(validate_hostname("").is_err());
    }

    #[test]
    fn normalize_cases() {
        assert_eq!(normalize_alias("API.OpenAI.COM."), "api.openai.com");
    }

    #[test]
    fn create_derivable_without_alias() {
        assert_eq!(
            resolve_alias_for_create(&[ep("https", "api.openai.com", 443)], None)
                .as_deref(),
            Ok("api.openai.com")
        );
    }

    #[test]
    fn create_derivable_with_exact_alias_is_noop() {
        assert_eq!(
            resolve_alias_for_create(&[ep("https", "api.openai.com", 443)], Some("API.OpenAI.COM."))
                .as_deref(),
            Ok("api.openai.com")
        );
    }

    #[test]
    fn create_derivable_with_wrong_alias_rejected() {
        assert!(resolve_alias_for_create(&[ep("https", "api.openai.com", 443)], Some("openai"))
            .is_err());
    }

    #[test]
    fn create_ip_requires_explicit_alias() {
        assert!(resolve_alias_for_create(&[ep("https", "10.0.0.1", 443)], None).is_err());
        assert_eq!(
            resolve_alias_for_create(&[ep("https", "10.0.0.1", 443)], Some("my-service"))
                .as_deref(),
            Ok("my-service")
        );
    }

    #[test]
    fn update_keeps_derived_alias_across_equivalent_change() {
        // A single hostname derives itself, so swapping hostnames changes the
        // derived alias and must be rejected (DESIGN.md update table).
        let old = vec![ep("https", "us.vendor.com", 443)];
        let new = vec![ep("https", "eu.vendor.com", 443)];
        assert!(enforce_alias_update("us.vendor.com", &old, None, &new).is_err());
        // A pool whose common suffix stays `vendor.com` keeps the alias.
        let old = vec![
            ep("https", "us.vendor.com", 443),
            ep("https", "eu.vendor.com", 443),
        ];
        let new = vec![
            ep("https", "eu.vendor.com", 443),
            ep("https", "ap.vendor.com", 443),
        ];
        assert_eq!(
            enforce_alias_update("vendor.com", &old, None, &new).as_deref(),
            Ok("vendor.com")
        );
    }

    #[test]
    fn update_rejects_derived_alias_change() {
        let old = vec![ep("https", "api.openai.com", 443)];
        let new = vec![ep("https", "api.anthropic.com", 443)];
        assert!(enforce_alias_update("api.openai.com", &old, None, &new).is_err());
    }

    #[test]
    fn update_rejects_derivable_to_non_derivable() {
        let old = vec![ep("https", "api.openai.com", 443)];
        let new = vec![ep("https", "10.0.0.1", 443)];
        // Even with an explicit alias matching the existing, this is rejected.
        assert!(enforce_alias_update("api.openai.com", &old, Some("api.openai.com"), &new).is_err());
    }

    #[test]
    fn update_keeps_explicit_alias_for_ip_to_ip() {
        let old = vec![ep("https", "10.0.0.1", 443)];
        let new = vec![ep("https", "10.0.0.2", 443)];
        assert_eq!(
            enforce_alias_update("my-service", &old, None, &new).as_deref(),
            Ok("my-service")
        );
    }

    #[test]
    fn update_rejects_alias_override() {
        let old = vec![ep("https", "10.0.0.1", 443)];
        let new = vec![ep("https", "10.0.0.2", 443)];
        assert!(
            enforce_alias_update("my-service", &old, Some("other-service"), &new).is_err()
        );
    }

    #[test]
    fn update_no_change_is_noop() {
        let eps = vec![ep("https", "api.openai.com", 443)];
        assert_eq!(
            enforce_alias_update("api.openai.com", &eps, Some("api.openai.com"), &eps).as_deref(),
            Ok("api.openai.com")
        );
    }
}
