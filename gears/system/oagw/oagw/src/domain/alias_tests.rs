//! Unit tests for alias derivation, normalisation and update enforcement.
//!
//! Each derivation row mirrors a row of the alias behaviour table in
//! DESIGN.md §3.3.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::domain::model::{Endpoint, EndpointScheme, ServerConfig};

fn ep(host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme: if port == 80 {
            EndpointScheme::Http
        } else {
            EndpointScheme::Https
        },
        host: host.to_owned(),
        port,
    }
}

fn ep_scheme(scheme: EndpointScheme, host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme,
        host: host.to_owned(),
        port,
    }
}

fn server(endpoints: Vec<Endpoint>) -> ServerConfig {
    ServerConfig { endpoints }
}

#[test]
fn single_hostname_on_standard_port_derives_the_hostname() {
    let derived = derive(&[ep("api.openai.com", 443)]);
    assert_eq!(derived, DerivedAlias::Derived("api.openai.com".to_owned()));
}

#[test]
fn single_hostname_on_non_standard_port_appends_the_port() {
    let derived = derive(&[ep("api.openai.com", 8443)]);
    assert_eq!(
        derived,
        DerivedAlias::Derived("api.openai.com:8443".to_owned())
    );
}

#[test]
fn http_standard_port_is_80() {
    let derived = derive(&[ep_scheme(EndpointScheme::Http, "api.example.com", 80)]);
    assert_eq!(derived, DerivedAlias::Derived("api.example.com".to_owned()));

    let derived = derive(&[ep_scheme(EndpointScheme::Http, "api.example.com", 8080)]);
    assert_eq!(
        derived,
        DerivedAlias::Derived("api.example.com:8080".to_owned())
    );
}

#[test]
fn multi_host_pool_with_registrable_suffix_derives_the_suffix() {
    let derived = derive(&[ep("us.vendor.com", 443), ep("eu.vendor.com", 443)]);
    assert_eq!(derived, DerivedAlias::Derived("vendor.com".to_owned()));
}

#[test]
fn multi_host_pool_with_non_standard_port_keeps_the_port_in_the_alias() {
    let derived = derive(&[ep("us.vendor.com", 8443), ep("eu.vendor.com", 8443)]);
    assert_eq!(derived, DerivedAlias::Derived("vendor.com:8443".to_owned()));
}

#[test]
fn bare_public_suffix_is_not_derivable() {
    let derived = derive(&[ep("foo.co.uk", 443), ep("bar.co.uk", 443)]);
    assert!(matches!(derived, DerivedAlias::NotDerivable(_)));
}

#[test]
fn no_common_suffix_is_not_derivable() {
    let derived = derive(&[ep("us.foo.com", 443), ep("eu.bar.com", 443)]);
    assert!(matches!(derived, DerivedAlias::NotDerivable(_)));
}

#[test]
fn ip_endpoints_require_an_explicit_alias() {
    let single = derive(&[ep("10.0.1.1", 443)]);
    assert!(matches!(single, DerivedAlias::NotDerivable(_)));

    let pool = derive(&[ep("10.0.1.1", 443), ep("10.0.1.2", 443)]);
    assert!(matches!(pool, DerivedAlias::NotDerivable(_)));
}

#[test]
fn ipv6_endpoints_require_an_explicit_alias() {
    let derived = derive(&[ep("2001:db8::1", 443)]);
    assert!(matches!(derived, DerivedAlias::NotDerivable(_)));
}

#[test]
fn normalise_lowercases_and_strips_trailing_dots() {
    assert_eq!(normalise("API.OpenAI.COM."), "api.openai.com");
    assert_eq!(normalise("  Vendor.COM  "), "vendor.com");
    assert_eq!(normalise("."), "");
}

#[test]
fn alias_pattern_is_enforced() {
    assert!(is_valid("api.openai.com"));
    assert!(is_valid("a"));
    assert!(is_valid("my-service:8443"));
    assert!(is_valid("10.0.1.1"));
    assert!(!is_valid(""));
    assert!(!is_valid("-leading"));
    assert!(!is_valid("trailing-"));
    assert!(!is_valid("has space"));
    assert!(!is_valid("\u{dc}nicode"));
    assert!(!is_valid("/path"));
}

#[test]
fn common_suffix_requires_two_labels() {
    assert_eq!(
        common_domain_suffix(&["api.example.com", "alt.example.com"]),
        Some("example.com".to_owned())
    );
    assert_eq!(common_domain_suffix(&[]), None);
    // A bare public suffix is never a usable alias, however many hosts share it.
    assert_eq!(common_domain_suffix(&["foo.co.uk", "bar.co.uk"]), None);
    // A single host is its own registrable suffix; `derive` handles that case
    // in its single-endpoint branch, so the helper only sees it directly here.
    assert_eq!(
        common_domain_suffix(&["example.com"]),
        Some("example.com".to_owned())
    );
}

#[test]
fn common_suffix_returns_the_longest_shared_labels() {
    assert_eq!(
        common_domain_suffix(&["a.b.example.com", "c.d.example.com"]),
        Some("example.com".to_owned())
    );
}

#[test]
fn common_suffix_may_sit_deeper_than_the_registrable_domain() {
    // `api.example.com` is a perfectly usable alias for a pool that shares it.
    assert_eq!(
        common_domain_suffix(&["eu.api.example.com", "us.api.example.com"]),
        Some("api.example.com".to_owned())
    );
}

#[test]
fn create_accepts_the_exact_derived_alias() {
    let cfg = server(vec![ep("api.openai.com", 443)]);
    assert_eq!(
        resolve_for_create(&cfg, Some("api.openai.com")).unwrap(),
        "api.openai.com"
    );
}

#[test]
fn create_accepts_the_derived_alias_in_mixed_case() {
    let cfg = server(vec![ep("API.OpenAI.COM.", 443)]);
    assert_eq!(
        resolve_for_create(&cfg, Some("API.OpenAI.COM.")).unwrap(),
        "api.openai.com"
    );
}

#[test]
fn create_rejects_a_differing_alias_for_hostname_endpoints() {
    let cfg = server(vec![ep("api.openai.com", 443)]);
    let err = resolve_for_create(&cfg, Some("other")).unwrap_err();
    assert!(matches!(err, DomainError::Invalid(_)));
    assert!(err.to_string().contains("api.openai.com"));
}

#[test]
fn create_requires_an_alias_for_ip_endpoints() {
    let cfg = server(vec![ep("10.0.1.1", 443)]);
    let err = resolve_for_create(&cfg, None).unwrap_err();
    assert!(matches!(err, DomainError::Invalid(_)));

    let alias = resolve_for_create(&cfg, Some("my-service")).unwrap();
    assert_eq!(alias, "my-service");
}

#[test]
fn create_requires_an_alias_for_a_bare_public_suffix_pool() {
    let cfg = server(vec![ep("foo.co.uk", 443), ep("bar.co.uk", 443)]);
    assert!(resolve_for_create(&cfg, None).is_err());
    assert_eq!(
        resolve_for_create(&cfg, Some("uk-pool")).unwrap(),
        "uk-pool"
    );
}

#[test]
fn create_rejects_an_invalid_explicit_alias() {
    let cfg = server(vec![ep("10.0.1.1", 443)]);
    assert!(resolve_for_create(&cfg, Some("-bad-")).is_err());
}

#[test]
fn update_tolerates_the_unchanged_alias() {
    let cfg = server(vec![ep("api.openai.com", 443)]);
    assert_eq!(
        enforce_alias_update("api.openai.com", &cfg, Some("api.openai.com")).unwrap(),
        "api.openai.com"
    );
    assert_eq!(
        enforce_alias_update("api.openai.com", &cfg, None).unwrap(),
        "api.openai.com"
    );
}

#[test]
fn update_rejects_an_alias_change() {
    let cfg = server(vec![ep("api.openai.com", 443)]);
    let err = enforce_alias_update("api.openai.com", &cfg, Some("other")).unwrap_err();
    assert!(matches!(err, DomainError::Invalid(_)));
}

#[test]
fn update_rejects_an_endpoint_change_that_alters_the_derived_alias() {
    let changed = server(vec![ep("api.other.com", 443)]);
    let err = enforce_alias_update("api.openai.com", &changed, None).unwrap_err();
    assert!(matches!(err, DomainError::Invalid(_)));
    assert!(err.to_string().contains("delete and re-create"));
}

#[test]
fn update_rejects_transition_to_non_derivable() {
    let to_ip = server(vec![ep("10.0.1.1", 443)]);
    let err = enforce_alias_update("api.openai.com", &to_ip, Some("api.openai.com")).unwrap_err();
    assert!(matches!(err, DomainError::Invalid(_)));
}

#[test]
fn update_rejects_transition_from_non_derivable_with_a_differing_alias() {
    let still_ip = server(vec![ep("10.0.1.9", 443)]);
    let err = enforce_alias_update("my-service", &still_ip, Some("renamed")).unwrap_err();
    assert!(matches!(err, DomainError::Invalid(_)));
}

#[test]
fn update_allows_a_non_derivable_pool_that_keeps_its_alias() {
    let still_ip = server(vec![ep("10.0.1.9", 443)]);
    assert_eq!(
        enforce_alias_update("my-service", &still_ip, None).unwrap(),
        "my-service"
    );
}

#[test]
fn ip_based_detection() {
    assert!(is_ip_based(&[ep("10.0.1.1", 443), ep("10.0.1.2", 443)]));
    assert!(!is_ip_based(&[ep("api.openai.com", 443)]));
    assert!(!is_ip_based(&[]));
}

#[test]
fn empty_endpoint_pool_is_not_derivable() {
    assert!(matches!(derive(&[]), DerivedAlias::NotDerivable(_)));
}
