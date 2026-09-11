//! Alias derivation, validation and the update-immutability matrix (T013, T014).

use crate::domain::alias::{
    AliasUpdateVerdict, DerivationOutcome, compute_derived_alias, derive_alias,
    enforce_alias_create, enforce_alias_update_with, is_ip_literal, is_valid_alias,
    is_valid_hostname, normalise_alias, normalise_host, validate_endpoint_host,
};
use crate::domain::dto::{Endpoint, EndpointScheme};

fn endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint { scheme: EndpointScheme::Https, host: host.to_string(), port }
}

fn host(host: &str) -> Endpoint {
    endpoint(host, 443)
}

#[test]
fn a_single_hostname_with_the_standard_port_derives_the_host() {
    let endpoints = vec![host("api.openai.com")];
    assert_eq!(compute_derived_alias(endpoints.as_slice()), Some("api.openai.com".to_string()));
}

#[test]
fn a_single_hostname_on_a_custom_port_keeps_the_port() {
    let endpoints = vec![endpoint("api.openai.com", 8443)];
    assert_eq!(
        compute_derived_alias(endpoints.as_slice()),
        Some("api.openai.com:8443".to_string())
    );
}

#[test]
fn a_registrable_common_suffix_is_derived() {
    let endpoints = vec![host("us.vendor.com"), host("eu.vendor.com")];
    assert_eq!(compute_derived_alias(endpoints.as_slice()), Some("vendor.com".to_string()));
}

#[test]
fn a_common_suffix_carries_the_shared_port() {
    let endpoints = vec![endpoint("us.vendor.com", 8443), endpoint("eu.vendor.com", 8443)];
    assert_eq!(
        compute_derived_alias(endpoints.as_slice()),
        Some("vendor.com:8443".to_string())
    );
}

#[test]
fn a_bare_public_suffix_is_not_derivable() {
    // `co.uk` is a public suffix, not a registrable domain.
    let endpoints = vec![host("foo.co.uk"), host("bar.co.uk")];
    assert_eq!(compute_derived_alias(endpoints.as_slice()), None);
    assert_eq!(derive_alias(endpoints.as_slice()), DerivationOutcome::NotDerivable);
}

#[test]
fn hosts_sharing_nothing_are_not_derivable() {
    let endpoints = vec![host("us.foo.com"), host("eu.bar.com")];
    assert_eq!(derive_alias(endpoints.as_slice()), DerivationOutcome::NotDerivable);
}

#[test]
fn ip_endpoints_require_an_explicit_alias() {
    let endpoints = vec![host("10.0.0.5"), host("10.0.0.6")];
    assert_eq!(derive_alias(endpoints.as_slice()), DerivationOutcome::IpEndpoints);
    assert_eq!(derive_alias(&[host("10.0.0.5")]), DerivationOutcome::IpEndpoints);
}

#[test]
fn a_mixed_pool_of_ip_and_hostname_is_not_derivable() {
    let endpoints = vec![host("us.vendor.com"), host("10.0.0.6")];
    assert_eq!(derive_alias(endpoints.as_slice()), DerivationOutcome::NotDerivable);
}

#[test]
fn a_mixed_pool_of_ports_is_not_derivable() {
    let endpoints = vec![endpoint("us.vendor.com", 443), endpoint("eu.vendor.com", 8443)];
    assert_eq!(derive_alias(endpoints.as_slice()), DerivationOutcome::NotDerivable);
}

#[test]
fn no_endpoints_derives_nothing() {
    assert_eq!(derive_alias(&[]), DerivationOutcome::NoEndpoints);
    assert!(enforce_alias_create(&[], None).is_err());
}

#[test]
fn hostnames_normalise_case_and_trailing_dots() {
    assert_eq!(normalise_host("API.OpenAI.COM."), "api.openai.com");
    assert_eq!(normalise_alias("API.OpenAI.COM."), "api.openai.com");
    assert_eq!(normalise_host("api.openai.com"), "api.openai.com");
}

#[test]
fn an_ip_literal_is_recognised() {
    assert!(is_ip_literal("10.0.0.5"));
    assert!(is_ip_literal("::1"));
    assert!(!is_ip_literal("api.openai.com"));
}

#[test]
fn rfc_1123_host_validation() {
    assert!(is_valid_hostname("api.openai.com"));
    assert!(is_valid_hostname("a-b.c"));
    assert!(!is_valid_hostname(""));
    assert!(!is_valid_hostname("-leading"));
    assert!(!is_valid_hostname("trailing-"));
    assert!(!is_valid_hostname("under_score"));
    assert!(!is_valid_hostname("space host"));
    assert!(!is_valid_hostname(""));
}

#[test]
fn alias_validation_accepts_the_documented_pattern() {
    assert!(is_valid_alias("api.openai.com"));
    assert!(is_valid_alias("api.openai.com:8443"));
    assert!(is_valid_alias("tenant-a.svc"));
    assert!(is_valid_alias("a"));
    assert!(!is_valid_alias(""));
    assert!(!is_valid_alias("-leading"));
    assert!(!is_valid_alias("trailing-"));
    assert!(!is_valid_alias("under_score"));
    assert!(!is_valid_alias("with space"));
}

#[test]
fn endpoint_hosts_are_validated() {
    assert!(validate_endpoint_host(&host("api.openai.com")).is_ok());
    assert!(validate_endpoint_host(&host("10.0.0.5")).is_ok());
    assert!(validate_endpoint_host(&host("not a host")).is_err());
    assert!(validate_endpoint_host(&Endpoint {
        scheme: EndpointScheme::Https,
        host: String::new(),
        port: 443,
    })
    .is_err());
}

// ── Create-time alias enforcement ───────────────────────────────────────

#[test]
fn a_derived_alias_is_used_when_none_is_given() {
    let endpoints = vec![host("api.openai.com")];
    assert_eq!(enforce_alias_create(&endpoints, None).unwrap(), "api.openai.com");
}

#[test]
fn an_alias_matching_the_derivation_is_accepted() {
    let endpoints = vec![host("api.openai.com")];
    assert_eq!(
        enforce_alias_create(&endpoints, Some("api.openai.com")).unwrap(),
        "api.openai.com"
    );
}

#[test]
fn an_alias_overriding_the_derivation_is_rejected() {
    let endpoints = vec![host("api.openai.com")];
    let err = enforce_alias_create(&endpoints, Some("other.alias")).unwrap_err();
    assert!(err.contains("derived"), "{err}");
}

#[test]
fn ip_endpoints_demand_an_explicit_alias() {
    let endpoints = vec![host("10.0.0.5")];
    let err = enforce_alias_create(&endpoints, None).unwrap_err();
    assert!(err.contains("explicit alias"), "{err}");
    assert_eq!(enforce_alias_create(&endpoints, Some("shared-ip")).unwrap(), "shared-ip");
}

// ── Update-time alias immutability (the documented matrix) ──────────────

#[test]
fn derivable_to_derivable_with_the_same_alias_is_allowed() {
    let endpoints = vec![host("api.openai.com")];
    let verdict = enforce_alias_update_with("api.openai.com", &endpoints, &endpoints, None);
    assert!(verdict.is_allowed());
}

#[test]
fn derivable_to_derivable_with_a_different_alias_is_rejected() {
    let existing = vec![host("api.openai.com")];
    let requested = vec![host("vendor.com")];
    assert!(matches!(
        enforce_alias_update_with("api.openai.com", &existing, &requested, Some("vendor.com")),
        AliasUpdateVerdict::Reject(_)
    ));
}

#[test]
fn derivable_to_non_derivable_is_rejected_even_with_an_explicit_alias() {
    let existing = vec![host("api.openai.com")];
    let requested = vec![host("10.0.0.5")];
    for alias in [None, Some("api.openai.com"), Some("shared-ip")] {
        let verdict = enforce_alias_update_with("api.openai.com", &existing, &requested, alias);
        assert!(matches!(verdict, AliasUpdateVerdict::Reject(_)), "{verdict:?}");
    }
}

#[test]
fn non_derivable_to_non_derivable_keeps_the_alias() {
    let endpoints = vec![host("us.foo.com"), host("eu.bar.com")];
    let verdict = enforce_alias_update_with("explicit-alias", &endpoints, &endpoints, None);
    assert!(verdict.is_allowed(), "{verdict:?}");

    let differing = enforce_alias_update_with("explicit-alias", &endpoints, &endpoints, Some("other"));
    assert!(matches!(differing, AliasUpdateVerdict::Reject(_)), "{differing:?}");
}

#[test]
fn non_derivable_to_derivable_is_allowed_only_when_it_matches_the_existing_alias() {
    let existing = vec![host("10.0.0.5")];
    let requested = vec![host("api.openai.com")];
    let matching = enforce_alias_update_with("api.openai.com", &existing, &requested, None);
    assert!(matches!(matching, AliasUpdateVerdict::KeepDerived), "{matching:?}");

    let differing = enforce_alias_update_with("other-alias", &existing, &requested, None);
    assert!(matches!(differing, AliasUpdateVerdict::Reject(_)), "{differing:?}");
}

#[test]
fn an_endpoint_free_update_tolerates_the_exact_alias() {
    let endpoints = vec![host("us.foo.com"), host("eu.bar.com")];
    let verdict =
        enforce_alias_update_with("explicit-alias", &endpoints, &endpoints, Some("explicit-alias"));
    assert!(matches!(verdict, AliasUpdateVerdict::Keep), "{verdict:?}");
}
