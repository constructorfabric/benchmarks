//! Unit tests for alias derivation (DESIGN §5.5).

use super::*;
use crate::domain::model::Endpoint;

fn ep(host: &str, port: Option<u16>) -> Endpoint {
    Endpoint {
        scheme: crate::domain::model::Scheme::Http,
        host: host.to_string(),
        port,
    }
}

#[test]
fn single_endpoint_on_a_standard_port_yields_the_host() {
    assert_eq!(
        derive_alias(&[ep("api.example.com", Some(80))]).unwrap(),
        "api.example.com"
    );
}

#[test]
fn single_endpoint_on_a_nonstandard_port_yields_host_port() {
    assert_eq!(
        derive_alias(&[ep("api.example.com", Some(8081))]).unwrap(),
        "api.example.com:8081"
    );
}

#[test]
fn several_endpoints_share_their_common_suffix() {
    let alias = derive_alias(&[
        ep("a.example.com", None),
        ep("b.example.com", None),
        ep("c.example.com", None),
    ])
    .unwrap();
    assert_eq!(alias, "example.com");
}

#[test]
fn two_labels_are_a_valid_suffix_but_one_is_not() {
    assert_eq!(common_suffix(&["a.example.com", "b.example.com"]).as_deref(), Some("example.com"));
    assert_eq!(common_suffix(&["example.com", "example.com"]).as_deref(), Some("example.com"));
    assert_eq!(common_suffix(&["com", "com"]), None);
    assert_eq!(common_suffix(&["one", "two"]), None);
}

#[test]
fn a_bare_public_suffix_is_not_a_legal_alias() {
    let error = derive_alias(&[ep("com", None)]).unwrap_err();
    assert!(error.to_string().contains("public suffix"), "{error}");
}

#[test]
fn a_registrable_domain_under_a_public_suffix_is_legal() {
    // `co.uk` is the public suffix, so `a.co.uk` is a registrable domain — and a legal alias.
    assert_eq!(derive_alias(&[ep("a.co.uk", None)]).unwrap(), "a.co.uk");
}

#[test]
fn a_pool_whose_only_common_suffix_is_a_public_suffix_is_not_derivable() {
    let error = derive_alias(&[ep("foo.co.uk", None), ep("bar.co.uk", None)]).unwrap_err();
    assert!(error.to_string().contains("public suffix"), "{error}");
}

#[test]
fn ip_literals_do_not_derive_an_alias() {
    let error = derive_alias(&[ep("10.0.0.5", None)]).unwrap_err();
    assert!(error.to_string().contains("IP"), "{error}");
}

#[test]
fn differing_ports_refuse_the_derivation() {
    let error = derive_alias(&[ep("a.example.com", Some(80)), ep("b.example.com", Some(8080))])
        .unwrap_err();
    assert!(error.to_string().contains("differing ports"), "{error}");
}

#[test]
fn no_endpoints_refuses_the_derivation() {
    let error = derive_alias(&[]).unwrap_err();
    assert!(error.to_string().contains("no endpoints"), "{error}");
}

#[test]
fn hosts_are_normalized_before_the_derivation() {
    assert_eq!(derive_alias(&[ep("API.Example.COM.", None)]).unwrap(), "api.example.com");
}

#[test]
fn explicit_alias_may_repeat_the_derivation() {
    let endpoints = vec![ep("api.example.com", None)];
    assert!(alias_matches_derivation("api.example.com", &endpoints));
    assert!(alias_matches_derivation("API.example.com.", &endpoints));
    assert!(!alias_matches_derivation("other.example.com", &endpoints));
    assert!(!alias_matches_derivation("anything", &[]));
}

#[test]
fn aliases_are_compared_after_normalization() {
    assert!(aliases_equal("Api.Example.com.", "api.example.com"));
    assert!(!aliases_equal("api.example.com", "api2.example.com"));
}

#[test]
fn normalized_alias_lowercases_and_trims() {
    assert_eq!(normalize_alias("  Api.Example.COM. "), "api.example.com");
}
