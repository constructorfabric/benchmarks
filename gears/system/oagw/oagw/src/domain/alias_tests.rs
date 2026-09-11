//! Alias derivation and enforcement — the full matrix from
//! `docs/DESIGN.md` §"Alias Enforcement Rules".

use super::*;
use crate::domain::model::Scheme;

fn endpoint(scheme: Scheme, host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme,
        host: host.to_owned(),
        port,
    }
}

fn https(host: &str) -> Endpoint {
    endpoint(Scheme::Https, host, 443)
}

#[test]
fn normalize_lowercases_and_strips_trailing_dots() {
    assert_eq!(normalize("  Api.OpenAI.COM.  "), "api.openai.com");
    assert_eq!(normalize("VENDOR.com:8443"), "vendor.com:8443");
}

#[test]
fn ip_literals_are_recognized_in_every_spelling() {
    assert!(is_ip_literal("10.0.1.1"));
    assert!(is_ip_literal("::1"));
    assert!(is_ip_literal("[2001:db8::1]"));
    assert!(!is_ip_literal("api.openai.com"));
}

#[test]
fn hostname_validation_follows_rfc_1123() {
    assert_eq!(validate_host("API.Example.COM.").unwrap(), "api.example.com");
    assert!(validate_host("").is_err());
    assert!(validate_host("-leading.example.com").is_err());
    assert!(validate_host("trailing-.example.com").is_err());
    assert!(validate_host("under_score.example.com").is_err());
    assert!(validate_host("a..b").is_err());
    let too_long = format!("{}.example.com", "a".repeat(64));
    assert!(validate_host(&too_long).is_err());
    // IP literals bypass label rules.
    assert_eq!(validate_host("10.0.1.1").unwrap(), "10.0.1.1");
}

#[test]
fn single_hostname_on_a_standard_port_derives_the_bare_host() {
    let derived = compute_derived_alias(&[https("api.openai.com")]);
    assert_eq!(derived.as_deref(), Some("api.openai.com"));
}

#[test]
fn single_hostname_on_a_nonstandard_port_keeps_the_port() {
    let derived = compute_derived_alias(&[endpoint(Scheme::Https, "api.openai.com", 8443)]);
    assert_eq!(derived.as_deref(), Some("api.openai.com:8443"));
}

#[test]
fn plaintext_standard_port_is_eighty() {
    let derived = compute_derived_alias(&[endpoint(Scheme::Http, "api.example.com", 80)]);
    assert_eq!(derived.as_deref(), Some("api.example.com"));
    let derived = compute_derived_alias(&[endpoint(Scheme::Http, "api.example.com", 443)]);
    assert_eq!(derived.as_deref(), Some("api.example.com:443"));
}

#[test]
fn multiple_hostnames_derive_the_registrable_common_suffix() {
    let derived = compute_derived_alias(&[https("us.vendor.com"), https("eu.vendor.com")]);
    assert_eq!(derived.as_deref(), Some("vendor.com"));
}

#[test]
fn common_suffix_keeps_a_nonstandard_port() {
    let derived = compute_derived_alias(&[
        endpoint(Scheme::Https, "us.vendor.com", 8443),
        endpoint(Scheme::Https, "eu.vendor.com", 8443),
    ]);
    assert_eq!(derived.as_deref(), Some("vendor.com:8443"));
}

#[test]
fn a_bare_public_suffix_is_not_derivable() {
    // `co.uk` is a public suffix, so it is nobody's routing target.
    assert_eq!(compute_derived_alias(&[https("foo.co.uk"), https("bar.co.uk")]), None);
}

#[test]
fn heterogeneous_hostnames_without_a_common_suffix_are_not_derivable() {
    assert_eq!(compute_derived_alias(&[https("us.foo.com"), https("eu.bar.com")]), None);
}

#[test]
fn ip_pools_are_never_derivable() {
    assert_eq!(compute_derived_alias(&[https("10.0.1.1"), https("10.0.1.2")]), None);
}

#[test]
fn create_derives_when_no_alias_is_supplied() {
    let alias = resolve_alias_for_create(&[https("api.openai.com")], None).unwrap();
    assert_eq!(alias, "api.openai.com");
}

#[test]
fn create_tolerates_an_alias_equal_to_the_derived_value() {
    let alias =
        resolve_alias_for_create(&[https("api.openai.com")], Some("API.OpenAI.com")).unwrap();
    assert_eq!(alias, "api.openai.com");
}

#[test]
fn create_rejects_an_alias_that_contradicts_derivation() {
    let err = resolve_alias_for_create(&[https("api.openai.com")], Some("openai")).unwrap_err();
    assert_eq!(err.status(), 400);
    assert!(err.detail.contains("auto-derived"), "{}", err.detail);
}

#[test]
fn create_requires_an_alias_for_a_nonderivable_pool() {
    let err = resolve_alias_for_create(&[https("10.0.1.1")], None).unwrap_err();
    assert_eq!(err.status(), 400);
    assert!(err.detail.contains("required"), "{}", err.detail);
}

#[test]
fn create_accepts_an_explicit_alias_for_an_ip_pool() {
    let alias = resolve_alias_for_create(&[https("10.0.1.1")], Some("My-Service")).unwrap();
    assert_eq!(alias, "my-service");
}

#[test]
fn explicit_alias_must_match_the_schema_pattern() {
    assert!(validate_alias("my-service").is_ok());
    assert!(validate_alias("vendor.com:8443").is_ok());
    assert!(validate_alias("-leading").is_err());
    assert!(validate_alias("trailing-").is_err());
    assert!(validate_alias("has space").is_err());
    assert!(validate_alias("").is_err());
}

#[test]
fn update_allows_an_endpoint_change_that_keeps_the_alias() {
    let alias =
        enforce_alias_update("vendor.com", &[https("us.vendor.com"), https("ap.vendor.com")], None)
            .unwrap();
    assert_eq!(alias, "vendor.com");
}

#[test]
fn update_rejects_an_endpoint_change_that_would_move_the_alias() {
    let err = enforce_alias_update("api.openai.com", &[https("api.anthropic.com")], None)
        .unwrap_err();
    assert_eq!(err.status(), 400);
    assert!(err.detail.contains("immutable"), "{}", err.detail);
}

#[test]
fn update_rejects_a_differing_user_alias_even_for_an_ip_pool() {
    let err = enforce_alias_update("my-service", &[https("10.0.1.1")], Some("other")).unwrap_err();
    assert_eq!(err.status(), 400);
    assert!(err.detail.contains("immutable"), "{}", err.detail);
}

#[test]
fn update_keeps_the_alias_of_a_nonderivable_pool() {
    let alias = enforce_alias_update("my-service", &[https("10.0.1.2")], None).unwrap();
    assert_eq!(alias, "my-service");
}

#[test]
fn update_rejects_derivable_to_nonderivable_transitions() {
    // hostname → IP: derivation now fails, so the old alias survives only if
    // the operator restates it; a *different* one is refused.
    let err =
        enforce_alias_update("api.openai.com", &[https("10.0.1.1")], Some("ip-pool")).unwrap_err();
    assert_eq!(err.status(), 400);
}
