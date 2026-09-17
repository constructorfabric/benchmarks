//! Alias derivation, validation and update enforcement.
//!
//! Implements the alias rules from `docs/DESIGN.md` §3.1 ("Alias Resolution"):
//!
//! * single hostname on a standard port → the hostname;
//! * single hostname on a non-standard port → `hostname:port`;
//! * several hostnames sharing a PSL-registrable common suffix (≥ 2 labels,
//!   not a bare public suffix) → the common suffix, plus `:port` when the pool
//!   is not on the scheme's standard port (`us.vendor.com:8443` +
//!   `eu.vendor.com:8443` → `vendor.com:8443`);
//! * a common suffix that is only a bare public suffix (`foo.co.uk`,
//!   `bar.co.uk`), heterogeneous hostnames with no common suffix, and any
//!   IP-literal pool are **non-derivable** — an explicit alias is required.
//!
//! Aliases are normalised to ASCII lowercase with trailing dots stripped and
//! are unique per `(tenant_id, alias)`, never globally.

use crate::config::MAX_ALIAS_LEN;
use crate::domain::error::DomainError;
use crate::domain::models::{AliasResolution, Endpoint, EndpointScheme};

/// Alias pattern from `docs/schemas/upstream.v1.schema.json`:
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
#[must_use]
pub fn is_valid_alias(alias: &str) -> bool {
    let bytes = alias.as_bytes();
    if bytes.is_empty() || bytes.len() > MAX_ALIAS_LEN {
        return false;
    }
    let first = bytes[0];
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    if !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return false;
    }
    bytes.iter().all(|b| {
        b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'.' || *b == b':' || *b == b'-'
    })
}

/// Normalises an alias: trimmed, ASCII lowercase, trailing dots stripped.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Validates a caller-supplied explicit alias.
///
/// # Errors
///
/// [`DomainError::Validation`] when the alias is empty, too long, or does not
/// match the schema alias pattern.
pub fn validate_alias(alias: &str) -> Result<(), DomainError> {
    if is_valid_alias(alias) {
        Ok(())
    } else {
        Err(DomainError::Validation(format!(
            "invalid alias '{alias}': must match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$ and be at most \
             {MAX_ALIAS_LEN} characters"
        )))
    }
}

/// Longest suffix (in labels) shared by every host in `hosts`.
///
/// Returns `None` when the hosts share nothing beyond the last label, or when a
/// host is missing entirely.
fn shared_label_suffix(hosts: &[&str]) -> Option<String> {
    let first = hosts.first()?;
    let labels: Vec<&str> = first.split('.').collect();
    let mut best: Option<usize> = None;
    'outer: for take in (2..=labels.len()).rev() {
        let candidate = labels[labels.len() - take..].join(".");
        for host in hosts.iter().skip(1) {
            if !host.split('.').rev().take(take).eq(candidate.split('.').rev()) {
                continue 'outer;
            }
        }
        best = Some(take);
        break;
    }
    best.map(|take| labels[labels.len() - take..].join("."))
}

/// Longest registrable common suffix of a hostname pool, per DESIGN.md.
///
/// A suffix is registrable when it has at least two labels and is not itself a
/// bare public suffix (`co.uk` is a public suffix, `vendor.com` is not).
/// Returns `None` when no such suffix exists.
#[must_use]
pub fn common_domain_suffix(hosts: &[&str]) -> Option<String> {
    let mut suffix = shared_label_suffix(hosts)?;
    while !suffix.is_empty() {
        if suffix.contains('.') && psl::domain_str(&suffix).is_some() {
            return Some(suffix.to_owned());
        }
        // Drop the left-most label and retry: the candidate may have been the
        // public suffix itself (`a.b.co.uk` / `x.b.co.uk` → `b.co.uk`).
        suffix = suffix.split_once('.').map(|(_, rest)| rest.to_owned())?;
    }
    None
}

/// Computes the alias derived from an endpoint pool, or `None` when the pool is
/// non-derivable and an explicit alias is required.
///
/// A single hostname yields `hostname` (or `hostname:port` on a non-standard
/// port). Multiple hostnames yield the registrable common suffix. IP endpoints
/// are never derivable.
#[must_use]
pub fn compute_derived_alias(endpoints: &[Endpoint]) -> Option<String> {
    if endpoints.is_empty() {
        return None;
    }

    // Every endpoint must share the same port for a pool; a single endpoint
    // contributes its own port.
    let ports: std::collections::BTreeSet<u16> =
        endpoints.iter().map(|e| e.port).collect();
    let port = if ports.len() == 1 { ports.into_iter().next() } else { None };

    let hosts: Vec<String> = endpoints
        .iter()
        .map(|e| e.normalized_host())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    // Any IP literal in the pool makes the alias non-derivable.
    if endpoints.iter().any(Endpoint::is_ip) {
        return None;
    }

    let base = if hosts.len() == 1 {
        hosts.into_iter().next()?
    } else {
        let refs: Vec<&str> = hosts.iter().map(String::as_str).collect();
        common_domain_suffix(&refs)?
    };

    // Strip the standard port from the derived alias.
    let standard = endpoints
        .iter()
        .all(|e| e.port == e.scheme.standard_port());
    if standard {
        Some(base)
    } else {
        port.map(|p| format!("{base}:{p}"))
    }
}

/// Classifies how the API must handle the alias of a create/replace body.
///
/// Returns [`AliasResolution::ExplicitMatchesDerived`] when the caller supplied
/// exactly the derived value, [`AliasResolution::Derived`] when the alias is
/// omitted and derivable, and [`AliasResolution::Explicit`] for a non-derivable
/// pool with an explicit alias.
///
/// # Errors
///
/// * [`DomainError::Validation`] when the explicit alias is malformed, when it
///   differs from the derived value, or when a non-derivable pool omits it.
pub fn resolve_alias_for_spec(
    endpoints: &[Endpoint],
    explicit: Option<&str>,
) -> Result<(AliasResolution, String), DomainError> {
    let derived = compute_derived_alias(endpoints);
    match (explicit, derived) {
        (None, Some(value)) => Ok((AliasResolution::Derived, value)),
        (None, None) => Err(DomainError::Validation(
            "alias is required: this endpoint pool is not derivable (IP endpoints, or hostnames \
             with no registrable common suffix)"
                .to_owned(),
        )),
        (Some(raw), derived) => {
            let alias = normalize_alias(raw);
            validate_alias(&alias)?;
            match derived {
                Some(value) if value == alias => {
                    Ok((AliasResolution::ExplicitMatchesDerived, value))
                }
                Some(value) => Err(DomainError::Validation(format!(
                    "alias '{alias}' does not match the value derived from the endpoints \
                     ('{value}'); endpoint-derived aliases are immutable"
                ))),
                None => Ok((AliasResolution::Explicit, alias)),
            }
        }
    }
}

/// Enforces the alias transition table for an endpoint change on an existing
/// upstream (DESIGN.md §3.1, "Alias Update Behavior").
///
/// * endpoints unchanged → the existing alias is kept; a differing
///   caller-supplied alias is rejected;
/// * derivable → derivable and the recomputed alias equals the existing one →
///   allowed;
/// * anything that would change the alias → rejected (400); the operator must
///   delete and re-create the upstream.
///
/// # Errors
///
/// [`DomainError::Validation`] for every rejected transition.
pub fn enforce_alias_update_with(
    current_alias: &str,
    current_endpoints: &[Endpoint],
    new_endpoints: &[Endpoint],
    new_alias: Option<&str>,
) -> Result<String, DomainError> {
    let requested = new_alias.map(normalize_alias);

    // No endpoint change: the exact alias is tolerated (no-op), an override is
    // rejected.
    if current_endpoints == new_endpoints {
        return match requested {
            None => Ok(current_alias.to_owned()),
            Some(alias) if alias == current_alias => Ok(current_alias.to_owned()),
            Some(alias) => Err(DomainError::Validation(format!(
                "alias is immutable: '{alias}' overrides '{current_alias}'"
            ))),
        };
    }

    let old_derived = compute_derived_alias(current_endpoints);
    let new_derived = compute_derived_alias(new_endpoints);
    match (old_derived, new_derived) {
        (Some(_), Some(value)) if value == current_alias => Ok(current_alias.to_owned()),
        (Some(_), Some(value)) => Err(DomainError::Validation(format!(
            "endpoints change would alter the derived alias from '{current_alias}' to '{value}'; \
             the alias is the routing key and cannot be edited — delete and re-create this \
             upstream"
        ))),
        (Some(_), None) => Err(DomainError::Validation(format!(
            "endpoints change would make upstream '{current_alias}' non-derivable; the alias \
             cannot be edited — delete and re-create this upstream"
        ))),
        (None, Some(value)) if value == current_alias => Ok(current_alias.to_owned()),
        (None, Some(_)) => Err(DomainError::Validation(format!(
            "endpoints change would alter the alias of upstream '{current_alias}'; delete and \
             re-create this upstream"
        ))),
        (None, None) => match requested {
            None => Ok(current_alias.to_owned()),
            Some(alias) if alias == current_alias => Ok(current_alias.to_owned()),
            Some(alias) => Err(DomainError::Validation(format!(
                "alias is immutable: '{alias}' does not match '{current_alias}'"
            ))),
        },
    }
}

/// Same as [`enforce_alias_update_with`] for a pool that is already known to be
/// hostname-derived: the recomputed alias must equal `current_alias`.
///
/// # Errors
///
/// [`DomainError::Validation`] when derivation fails or yields a different
/// alias.
pub fn enforce_alias_update_derived(
    current_alias: &str,
    new_endpoints: &[Endpoint],
) -> Result<String, DomainError> {
    match compute_derived_alias(new_endpoints) {
        Some(value) if value == current_alias => Ok(current_alias.to_owned()),
        Some(value) => Err(DomainError::Validation(format!(
            "derived alias would change from '{current_alias}' to '{value}'; delete and re-create \
             this upstream"
        ))),
        None => Err(DomainError::Validation(format!(
            "upstream '{current_alias}' would become non-derivable; delete and re-create it"
        ))),
    }
}

/// `true` when `candidate` shadows (equals) `existing` after normalisation.
#[must_use]
pub fn aliases_equal(a: &str, b: &str) -> bool {
    normalize_alias(a) == normalize_alias(b)
}

/// `true` when `scheme` uses a port that is omitted from a derived alias.
#[must_use]
pub const fn is_standard_port(scheme: EndpointScheme, port: u16) -> bool {
    port == scheme.standard_port()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn ep(host: &str, port: u16) -> Endpoint {
        Endpoint::new(EndpointScheme::Https, host, port)
    }

    #[test]
    fn single_hostname_on_standard_port_yields_hostname() {
        let derived = compute_derived_alias(&[ep("api.openai.com", 443)]);
        assert_eq!(derived.as_deref(), Some("api.openai.com"));
    }

    #[test]
    fn single_hostname_on_non_standard_port_yields_host_port() {
        let derived = compute_derived_alias(&[ep("api.openai.com", 8443)]);
        assert_eq!(derived.as_deref(), Some("api.openai.com:8443"));
    }

    #[test]
    fn http_uses_port_80_as_the_standard_port() {
        let endpoints = vec![Endpoint::new(EndpointScheme::Http, "local.service", 80)];
        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("local.service")
        );
        let endpoints = vec![Endpoint::new(EndpointScheme::Http, "local.service", 8080)];
        assert_eq!(
            compute_derived_alias(&endpoints).as_deref(),
            Some("local.service:8080")
        );
    }

    #[test]
    fn multi_host_pool_derives_the_registrable_common_suffix() {
        let derived = compute_derived_alias(&[ep("us.vendor.com", 443), ep("eu.vendor.com", 443)]);
        assert_eq!(derived.as_deref(), Some("vendor.com"));
    }

    #[test]
    fn multi_host_pool_on_non_standard_port_keeps_the_port() {
        let derived = compute_derived_alias(&[ep("us.vendor.com", 8443), ep("eu.vendor.com", 8443)]);
        assert_eq!(derived.as_deref(), Some("vendor.com:8443"));
    }

    #[test]
    fn bare_public_suffix_is_not_derivable() {
        assert!(common_domain_suffix(&["foo.co.uk", "bar.co.uk"]).is_none());
        let derived = compute_derived_alias(&[ep("foo.co.uk", 443), ep("bar.co.uk", 443)]);
        assert_eq!(derived, None);
    }

    #[test]
    fn heterogeneous_hostnames_are_not_derivable() {
        let derived = compute_derived_alias(&[ep("us.foo.com", 443), ep("eu.bar.com", 443)]);
        assert_eq!(derived, None);
    }

    #[test]
    fn ip_endpoints_are_never_derivable() {
        let endpoints = vec![Endpoint::new(EndpointScheme::Https, "10.0.1.1", 443)];
        assert_eq!(compute_derived_alias(&endpoints), None);
        let endpoints = vec![
            Endpoint::new(EndpointScheme::Https, "10.0.1.1", 443),
            Endpoint::new(EndpointScheme::Https, "10.0.1.2", 443),
        ];
        assert_eq!(compute_derived_alias(&endpoints), None);
    }

    #[test]
    fn three_level_suffix_is_the_shared_registrable_labels() {
        // The shared suffix `b.vendor.com` is a registrable domain (not a bare
        // public suffix), so it is the alias — not the bare `vendor.com`.
        let derived = compute_derived_alias(&[ep("a.b.vendor.com", 443), ep("c.b.vendor.com", 443)]);
        assert_eq!(derived.as_deref(), Some("b.vendor.com"));
    }

    #[test]
    fn normalizes_case_and_trailing_dot() {
        assert_eq!(normalize_alias("Api.OpenAI.COM."), "api.openai.com");
        assert!(is_valid_alias("api.openai.com"));
        assert!(is_valid_alias("vendor.com:8443"));
        assert!(is_valid_alias("my-service"));
        assert!(!is_valid_alias("-bad"));
        assert!(!is_valid_alias("bad-"));
        assert!(!is_valid_alias(""));
        assert!(!is_valid_alias("UPPER.case"));
        assert!(!is_valid_alias("has space"));
    }

    #[test]
    fn alias_validation_reports_the_offending_value() {
        let err = validate_alias("Not_Valid").unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
        assert!(validate_alias("ok-alias.example").is_ok());
    }

    #[test]
    fn create_with_an_alias_matching_the_derived_value_is_tolerated() {
        let endpoints = vec![ep("api.openai.com", 443)];
        let (resolution, alias) =
            resolve_alias_for_spec(&endpoints, Some("api.openai.com".to_owned()).as_deref())
                .expect("idempotent explicit alias");
        assert_eq!(resolution, AliasResolution::ExplicitMatchesDerived);
        assert_eq!(alias, "api.openai.com");
    }

    #[test]
    fn create_with_a_differing_alias_is_rejected() {
        let endpoints = vec![ep("api.openai.com", 443)];
        let err =
            resolve_alias_for_spec(&endpoints, Some("other.example".to_owned()).as_deref())
                .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn non_derivable_pool_requires_an_explicit_alias() {
        let endpoints = vec![Endpoint::new(EndpointScheme::Https, "10.0.1.1", 443)];
        let err = resolve_alias_for_spec(&endpoints, None).unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
        let (resolution, alias) =
            resolve_alias_for_spec(&endpoints, Some("my-service".to_owned()).as_deref())
                .expect("explicit alias accepted for IP pool");
        assert_eq!(resolution, AliasResolution::Explicit);
        assert_eq!(alias, "my-service");
    }

    #[test]
    fn unchanged_endpoints_keep_the_alias() {
        let current = vec![ep("api.openai.com", 443)];
        let alias = enforce_alias_update_with("api.openai.com", &current, &current, None)
            .expect("no-op update");
        assert_eq!(alias, "api.openai.com");
    }

    #[test]
    fn unchanged_endpoints_tolerate_the_exact_alias_only() {
        let current = vec![Endpoint::new(EndpointScheme::Https, "10.0.1.1", 443)];
        assert_eq!(
            enforce_alias_update_with("my-service", &current, &current, Some("my-service"))
                .expect("exact match is a no-op"),
            "my-service"
        );
        let err =
            enforce_alias_update_with("my-service", &current, &current, Some("other")).unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn endpoint_change_that_keeps_the_alias_is_allowed() {
        let current = vec![ep("us.vendor.com", 443), ep("eu.vendor.com", 443)];
        let updated = vec![ep("apac.vendor.com", 443), ep("eu.vendor.com", 443)];
        assert_eq!(
            enforce_alias_update_with("vendor.com", &current, &updated, None).expect("same alias"),
            "vendor.com"
        );
    }

    #[test]
    fn endpoint_change_that_alters_the_alias_is_rejected() {
        let current = vec![ep("us.vendor.com", 443), ep("eu.vendor.com", 443)];
        let updated = vec![ep("api.other.com", 443)];
        let err = enforce_alias_update_with("vendor.com", &current, &updated, None).unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn hostname_to_ip_transition_is_rejected_even_with_an_alias() {
        let current = vec![ep("api.openai.com", 443)];
        let updated = vec![Endpoint::new(EndpointScheme::Https, "10.0.1.1", 443)];
        let err =
            enforce_alias_update_with("api.openai.com", &current, &updated, Some("my-service"))
                .unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn ip_to_hostname_transition_is_allowed_only_when_the_alias_matches() {
        let current = vec![Endpoint::new(EndpointScheme::Https, "10.0.1.1", 443)];
        let updated = vec![ep("api.openai.com", 443)];
        assert_eq!(
            enforce_alias_update_with("api.openai.com", &current, &updated, None)
                .expect("derived alias equals the existing one"),
            "api.openai.com"
        );
        let err = enforce_alias_update_with("other.example", &current, &updated, None).unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn ip_to_ip_transition_retains_the_existing_alias() {
        let current = vec![Endpoint::new(EndpointScheme::Https, "10.0.1.1", 443)];
        let updated = vec![Endpoint::new(EndpointScheme::Https, "10.0.1.2", 443)];
        assert_eq!(
            enforce_alias_update_with("my-service", &current, &updated, None)
                .expect("alias retained"),
            "my-service"
        );
        assert_eq!(
            enforce_alias_update_with("my-service", &current, &updated, Some("my-service"))
                .expect("re-supplying the alias is a no-op"),
            "my-service"
        );
        let err =
            enforce_alias_update_with("my-service", &current, &updated, Some("renamed")).unwrap_err();
        assert!(matches!(err, DomainError::Validation(_)));
    }

    #[test]
    fn enforce_alias_update_derived_mirrors_the_general_rule() {
        let updated = vec![ep("api.openai.com", 443)];
        assert!(enforce_alias_update_derived("api.openai.com", &updated).is_ok());
        assert!(enforce_alias_update_derived("other.example", &updated).is_err());
        let ips = vec![Endpoint::new(EndpointScheme::Https, "10.0.1.1", 443)];
        assert!(enforce_alias_update_derived("api.openai.com", &ips).is_err());
    }

    #[test]
    fn helpers_are_stable() {
        assert!(aliases_equal("Api.Example.COM.", "api.example.com"));
        assert!(!aliases_equal("a.example", "b.example"));
        assert!(is_standard_port(EndpointScheme::Https, 443));
        assert!(!is_standard_port(EndpointScheme::Http, 443));
        assert!(compute_derived_alias(&[]).is_none());
    }
}
