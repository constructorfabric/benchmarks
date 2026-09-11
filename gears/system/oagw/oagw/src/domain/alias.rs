//! Alias derivation and enforcement.
//!
//! Pure functions: derivation is table-driven over the endpoint pool and the
//! public suffix list, so the whole derivation matrix in `docs/DESIGN.md` is
//! one unit test with no runtime.

use crate::domain::dto::Endpoint;
use crate::domain::error::DomainError;

/// Longest alias a caller may supply, and the RFC 1123 limit.
const MAX_HOSTNAME_LEN: usize = 253;

/// Normalizes a host: ASCII lower-case, trailing dot stripped, brackets
/// removed from an IPv6 literal.
#[must_use]
pub fn normalize_host(host: &str) -> String {
    let trimmed = host.trim();
    let without_brackets = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(trimmed);
    without_brackets
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// Validates a host label per RFC 1123: 1–63 characters, ASCII alphanumeric or
/// hyphen, no leading or trailing hyphen.
fn is_valid_label(label: &str) -> bool {
    if label.is_empty() || label.len() > 63 {
        return false;
    }
    if label.starts_with('-') || label.ends_with('-') {
        return false;
    }
    label
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// RFC 1123 hostname validation. A trailing dot is tolerated and stripped.
#[must_use]
pub fn is_valid_hostname(host: &str) -> bool {
    let normalized = normalize_host(host);
    if normalized.is_empty() || normalized.len() > MAX_HOSTNAME_LEN {
        return false;
    }
    normalized.split('.').all(is_valid_label)
}

/// Whether the string is an IP literal (v4 or v6, brackets tolerated).
#[must_use]
pub fn is_ip_literal(host: &str) -> bool {
    normalize_host(host).parse::<std::net::IpAddr>().is_ok()
}

/// Normalizes an alias: ASCII lower-case, trailing dots stripped.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the normalized alias is empty or
/// does not match the routing-key character set.
pub fn normalize_alias(alias: &str) -> Result<String, DomainError> {
    let normalized = alias.trim().trim_end_matches('.').to_ascii_lowercase();
    if normalized.is_empty() {
        return Err(DomainError::Validation("alias must not be empty".into()));
    }
    if normalized.len() > MAX_HOSTNAME_LEN {
        return Err(DomainError::Validation(
            "alias exceeds the 253 character limit".into(),
        ));
    }
    let body = normalized.as_bytes();
    let first = *body.first().unwrap_or(&b'-');
    let last = *body.last().unwrap_or(&b'-');
    let label_ok = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b':' | b'-');
    if !(first.is_ascii_alphanumeric()
        && last.is_ascii_alphanumeric()
        && body.iter().all(|byte| label_ok(*byte)))
    {
        return Err(DomainError::Validation(format!(
            "alias `{normalized}` must match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$"
        )));
    }
    Ok(normalized)
}

/// The trailing sequence of complete labels shared by every hostname.
///
/// Returns `None` when the endpoints are not all hostnames, when any hostname
/// is invalid, or when there is no shared trailing label sequence.
fn longest_common_label_suffix(hosts: &[&str]) -> Option<String> {
    let mut splits = Vec::with_capacity(hosts.len());
    for host in hosts {
        let labels: Vec<&str> = host.split('.').collect();
        splits.push(labels);
    }
    let shortest = splits.iter().map(Vec::len).min()?;
    let mut shared: Vec<&str> = Vec::new();
    for index in 1..=shortest {
        let candidate = splits[0][splits[0].len() - index];
        if splits
            .iter()
            .all(|labels| labels[labels.len() - index] == candidate)
        {
            shared.insert(0, candidate);
        } else {
            break;
        }
    }
    if shared.is_empty() {
        None
    } else {
        Some(shared.join("."))
    }
}

/// Whether `candidate` is a registrable domain: at least two labels and not a
/// bare public suffix.
fn is_registrable(candidate: &str) -> bool {
    if candidate.split('.').count() < 2 {
        return false;
    }
    // A bare public suffix has no registrable domain above it.
    psl::domain_str(candidate) == Some(candidate)
}

/// Computes the alias an endpoint pool derives, or `None` when the pool is not
/// derivable and an explicit alias is required.
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }
    if endpoints.iter().any(Endpoint::is_ip) {
        return None;
    }
    let port = endpoints[0].port;
    if endpoints.iter().any(|endpoint| endpoint.port != port) {
        return None;
    }
    let scheme = endpoints[0].scheme;
    let non_standard = port != scheme.default_port();
    let suffix = if non_standard { format!(":{port}") } else { String::new() };

    if endpoints.len() == 1 {
        let host = normalize_host(&endpoints[0].host);
        if is_valid_hostname(&host) {
            return Some(format!("{host}{suffix}"));
        }
        return None;
    }

    let hosts: Vec<String> = endpoints
        .iter()
        .map(|endpoint| normalize_host(&endpoint.host))
        .collect();
    if hosts.iter().any(|host| !is_valid_hostname(host)) {
        return None;
    }
    let refs: Vec<&str> = hosts.iter().map(String::as_str).collect();
    let common = longest_common_label_suffix(&refs)?;
    if is_registrable(&common) {
        Some(format!("{common}{suffix}"))
    } else {
        None
    }
}

/// Whether the endpoint pool can derive an alias at all.
#[must_use]
pub fn is_derivable(endpoints: &[Endpoint]) -> bool {
    compute_derived_alias(endpoints).is_some()
}

/// Enforces alias behaviour at create time.
///
/// # Errors
/// Returns [`DomainError::Validation`] when a supplied alias conflicts with the
/// derived value, when derivation is impossible and no alias was supplied, or
/// when the supplied alias is not a legal routing key.
pub fn enforce_alias_create(
    endpoints: &[Endpoint],
    provided: Option<&str>,
) -> Result<String, DomainError> {
    let derived = compute_derived_alias(endpoints);
    match derived {
        Some(expected) => match provided {
            None => Ok(expected),
            Some(raw) => {
                let supplied = normalize_alias(raw)?;
                if supplied == expected {
                    Ok(expected)
                } else {
                    Err(DomainError::Validation(format!(
                        "alias `{supplied}` conflicts with the derived alias `{expected}`"
                    )))
                }
            }
        },
        None => match provided {
            None => Err(DomainError::Validation(
                "an explicit alias is required for this endpoint set".into(),
            )),
            Some(raw) => normalize_alias(raw),
        }
    }
}

/// Enforces alias immutability on update.
///
/// `old_endpoints` are the endpoints currently stored; `new_endpoints` are the
/// ones the caller proposes. The alias is the routing key, so any endpoint
/// change that would alter the derived alias is rejected.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the transition would change the
/// alias, or when a supplied alias differs from the retained one.
pub fn enforce_alias_update(
    old_endpoints: &[Endpoint],
    existing_alias: &str,
    new_endpoints: &[Endpoint],
    provided: Option<&str>,
) -> Result<String, DomainError> {
    let retained = match (
        compute_derived_alias(old_endpoints),
        compute_derived_alias(new_endpoints),
    ) {
        (Some(previous), Some(next)) => {
            if previous == next {
                next
            } else {
                return Err(DomainError::Validation(
                    "changing these endpoints would change the derived alias; delete and re-create the upstream"
                        .into(),
                ));
            }
        }
        (Some(_), None) => {
            return Err(DomainError::Validation(
                "changing these endpoints would make the alias non-derivable; delete and re-create the upstream"
                    .into(),
            ));
        }
        (None, Some(next)) => {
            if next == existing_alias {
                next
            } else {
                return Err(DomainError::Validation(
                    "changing these endpoints would change the derived alias; delete and re-create the upstream"
                        .into(),
                ));
            }
        }
        (None, None) => existing_alias.to_owned(),
    };

    match provided {
        None => Ok(retained),
        Some(raw) => {
            let supplied = normalize_alias(raw)?;
            if supplied == retained {
                Ok(retained)
            } else {
                Err(DomainError::Validation(format!(
                    "alias `{supplied}` differs from the retained alias `{retained}`; the alias is immutable"
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(scheme: crate::domain::dto::Scheme, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    fn https(host: &str) -> Endpoint {
        endpoint(crate::domain::dto::Scheme::Https, host, 443)
    }

    fn http(host: &str) -> Endpoint {
        endpoint(crate::domain::dto::Scheme::Http, host, 80)
    }

    // ---- derivation table -------------------------------------------------

    #[test]
    fn single_hostname_standard_port() {
        let derived = compute_derived_alias(&[https("api.openai.com")]);
        assert_eq!(derived.as_deref(), Some("api.openai.com"));
    }

    #[test]
    fn single_hostname_non_standard_port() {
        let endpoints = [endpoint(crate::domain::dto::Scheme::Https, "api.openai.com", 8443)];
        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("api.openai.com:8443")
        );
    }

    #[test]
    fn single_plaintext_hostname_standard_port() {
        assert_eq!(compute_derived_alias(&[http("localhost")]).as_deref(), Some("localhost"));
    }

    #[test]
    fn common_suffix_across_a_pool() {
        let endpoints = [https("us.vendor.com"), https("eu.vendor.com")];
        assert_eq!(compute_derived_alias(&endpoints).as_deref(), Some("vendor.com"));
    }

    #[test]
    fn common_suffix_with_non_standard_port() {
        let endpoints = [
            endpoint(crate::domain::dto::Scheme::Https, "us.vendor.com", 8443),
            endpoint(crate::domain::dto::Scheme::Https, "eu.vendor.com", 8443),
        ];
        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("vendor.com:8443")
        );
    }

    #[test]
    fn bare_public_suffix_is_not_derivable() {
        let endpoints = [https("foo.co.uk"), https("bar.co.uk")];
        assert!(compute_derived_alias(&endpoints).is_none());
    }

    #[test]
    fn no_common_suffix_is_not_derivable() {
        let endpoints = [https("api.openai.com"), https("api.another.com")];
        assert!(compute_derived_alias(&endpoints).is_none());
    }

    #[test]
    fn ip_endpoints_are_not_derivable() {
        let endpoints = [endpoint(crate::domain::dto::Scheme::Https, "10.0.1.1", 443)];
        assert!(compute_derived_alias(&endpoints).is_none());
    }

    #[test]
    fn heterogeneous_ports_are_not_derivable() {
        let endpoints = [
            endpoint(crate::domain::dto::Scheme::Https, "us.vendor.com", 443),
            endpoint(crate::domain::dto::Scheme::Https, "eu.vendor.com", 8443),
        ];
        assert!(compute_derived_alias(&endpoints).is_none());
    }

    #[test]
    fn single_host_with_public_suffix_is_still_derivable() {
        assert_eq!(compute_derived_alias(&[https("foo.co.uk")]).as_deref(), Some("foo.co.uk"));
    }

    #[test]
    fn mixed_ip_and_hostname_is_not_derivable() {
        let endpoints = [https("us.vendor.com"), endpoint(crate::domain::dto::Scheme::Https, "10.0.1.1", 443)];
        assert!(compute_derived_alias(&endpoints).is_none());
    }

    #[test]
    fn empty_pool_is_not_derivable() {
        assert!(compute_derived_alias(&[]).is_none());
    }

    // ---- create-time enforcement -----------------------------------------

    #[test]
    fn create_without_alias_uses_derivation() {
        let endpoints = [https("api.openai.com")];
        assert_eq!(
            enforce_alias_create(&endpoints, None).expect("derives"),
            "api.openai.com"
        );
    }

    #[test]
    fn create_with_conflicting_alias_is_rejected() {
        let endpoints = [https("api.openai.com")];
        let error = enforce_alias_create(&endpoints, Some("other"))
            .expect_err("conflicts");
        assert_eq!(error.status(), 400);
    }

    #[test]
    fn create_with_derived_alias_is_idempotent() {
        let endpoints = [https("api.openai.com")];
        assert_eq!(
            enforce_alias_create(&endpoints, Some("api.openai.com")).expect("matches"),
            "api.openai.com"
        );
    }

    #[test]
    fn create_with_ip_and_no_alias_is_rejected() {
        let endpoints = [endpoint(crate::domain::dto::Scheme::Https, "10.0.1.1", 443)];
        assert!(enforce_alias_create(&endpoints, None).is_err());
    }

    #[test]
    fn create_with_ip_and_explicit_alias_succeeds() {
        let endpoints = [endpoint(crate::domain::dto::Scheme::Https, "10.0.1.1", 443)];
        assert_eq!(
            enforce_alias_create(&endpoints, Some("My-Internal-Service"))
                .expect("explicit"),
            "my-internal-service"
        );
    }

    #[test]
    fn create_with_bare_public_suffix_pool_and_no_alias_is_rejected() {
        let endpoints = [https("foo.co.uk"), https("bar.co.uk")];
        assert!(enforce_alias_create(&endpoints, None).is_err());
    }

    #[test]
    fn create_normalizes_case_and_trailing_dot() {
        let endpoints = [https("api.openai.com")];
        assert_eq!(
            enforce_alias_create(&endpoints, None).expect("derives"),
            "api.openai.com"
        );
        let raw = Endpoint {
            scheme: crate::domain::dto::Scheme::Https,
            host: "API.OpenAI.COM.".into(),
            port: 443,
        };
        assert_eq!(
            compute_derived_alias(&[raw]).as_deref(),
            Some("api.openai.com")
        );
    }

    #[test]
    fn create_rejects_an_illegal_alias() {
        let endpoints = [endpoint(crate::domain::dto::Scheme::Https, "10.0.1.1", 443)];
        assert!(enforce_alias_create(&endpoints, Some("-bad-")).is_err());
        assert!(enforce_alias_create(&endpoints, Some("a b")).is_err());
    }

    // ---- update-time enforcement -----------------------------------------

    #[test]
    fn update_that_would_change_a_derived_alias_is_rejected() {
        let old = [https("api.openai.com")];
        let new = [https("api.another.com")];
        let error = enforce_alias_update(&old, "api.openai.com", &new, None).expect_err("rejected");
        assert_eq!(error.status(), 400);
    }

    #[test]
    fn update_that_preserves_the_derived_alias_is_allowed() {
        let old = [https("api.openai.com")];
        let new = [https("api.openai.com")];
        assert_eq!(
            enforce_alias_update(&old, "api.openai.com", &new, None).expect("kept"),
            "api.openai.com"
        );
    }

    #[test]
    fn update_from_derivable_to_non_derivable_is_always_rejected() {
        let old = [https("api.openai.com")];
        let new = [endpoint(crate::domain::dto::Scheme::Https, "10.0.1.1", 443)];
        assert!(enforce_alias_update(&old, "api.openai.com", &new, Some("api.openai.com")).is_err());
    }

    #[test]
    fn update_between_non_derivable_sets_retains_the_alias() {
        let old = [endpoint(crate::domain::dto::Scheme::Https, "10.0.1.1", 443)];
        let new = [endpoint(crate::domain::dto::Scheme::Https, "10.0.1.2", 443)];
        assert_eq!(
            enforce_alias_update(&old, "my-internal-service", &new, None).expect("retained"),
            "my-internal-service"
        );
        assert!(enforce_alias_update(&old, "my-internal-service", &new, Some("other")).is_err());
    }

    #[test]
    fn update_from_non_derivable_to_derivable_is_allowed_when_the_alias_matches() {
        let old = [endpoint(crate::domain::dto::Scheme::Https, "10.0.1.1", 443)];
        let new = [https("api.openai.com")];
        assert_eq!(
            enforce_alias_update(&old, "api.openai.com", &new, None).expect("allowed"),
            "api.openai.com"
        );
        assert!(enforce_alias_update(&old, "other", &new, None).is_err());
    }

    #[test]
    fn update_without_endpoint_change_tolerates_the_exact_alias() {
        let old = [https("api.openai.com")];
        assert_eq!(
            enforce_alias_update(&old, "api.openai.com", &old, Some("api.openai.com"))
                .expect("no-op"),
            "api.openai.com"
        );
    }

    // ---- hostname validation ---------------------------------------------

    #[test]
    fn rfc1123_rules() {
        assert!(is_valid_hostname("api.openai.com"));
        assert!(is_valid_hostname("api.openai.com."));
        assert!(is_valid_hostname("a-b.example.com"));
        assert!(!is_valid_hostname("-bad.example.com"));
        assert!(!is_valid_hostname("bad-.example.com"));
        assert!(!is_valid_hostname(""));
        assert!(!is_valid_hostname(&"a".repeat(254)));
        assert!(!is_valid_hostname(&format!("{}.example.com", "a".repeat(64))));
        assert!(is_valid_hostname(&format!("{}.example.com", "a".repeat(63))));
        assert!(!is_valid_hostname("exa mple.com"));
        assert!(is_valid_hostname("10.0.1.1"));
    }

    #[test]
    fn common_suffix_helper() {
        assert_eq!(
            longest_common_label_suffix(&["us.vendor.com", "eu.vendor.com"]).as_deref(),
            Some("vendor.com")
        );
        assert_eq!(
            longest_common_label_suffix(&["api.openai.com", "api.another.com"]).as_deref(),
            Some("com")
        );
        // `a.b` is the shared *prefix* here; c, d and e disagree, so no
        // trailing label sequence is shared at all.
        assert!(longest_common_label_suffix(&["a.b.c", "a.b.d", "a.b.e"]).is_none());
        assert_eq!(
            longest_common_label_suffix(&["a.b.c", "x.b.c"]).as_deref(),
            Some("b.c")
        );
        assert!(longest_common_label_suffix(&["alpha", "beta"]).is_none());
    }
}
