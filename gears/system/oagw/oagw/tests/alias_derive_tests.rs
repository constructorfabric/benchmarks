//! Alias-derivation tests.
//!
//! Covers `cpt-cf-oagw-dod-alias-derivation` and
//! `cpt-cf-oagw-algo-alias-derive`: the derived alias of a single-hostname
//! endpoint set on a standard and a non-standard port, the longest common
//! suffix of a pooled set, the public-suffix and IP-literal refusals, the
//! reconciliation with a caller-supplied alias, and the immutability
//! confirmed across a replacement.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use oagw::control_plane::alias_derive::{confirm_immutable, derive, resolve, standard_port};
use oagw::domain::alias::Alias;
use oagw::domain::error::ErrorKind;
use oagw::domain::scheme::Scheme;
use oagw::domain::upstream::Endpoint;
use oagw::{AliasError, DomainError};

/// An `https` endpoint.
fn https(host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme: Scheme::Https,
        host: oagw::EndpointHost::parse(host).expect("a valid endpoint host"),
        port: Some(port),
    }
}

/// An `http` endpoint.
fn http(host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme: Scheme::Http,
        host: oagw::EndpointHost::parse(host).expect("a valid endpoint host"),
        port: Some(port),
    }
}

/// An endpoint that leaves its port to the scheme default.
fn default_port(scheme: Scheme, host: &str) -> Endpoint {
    Endpoint {
        scheme,
        host: oagw::EndpointHost::parse(host).expect("a valid endpoint host"),
        port: None,
    }
}

/// Asserts the set derives `expected`.
fn derives(endpoints: &[Endpoint], expected: &str) {
    let alias = derive(endpoints).expect("the set is derivable");
    assert_eq!(alias.to_string(), expected, "the derived alias differs");
}

/// Asserts the set admits no alias.
fn not_derivable(endpoints: &[Endpoint]) {
    let error = derive(endpoints).expect_err("the set is not derivable");
    assert_eq!(error.detail(), "server.endpoints is not derivable");
}

#[test]
fn the_standard_ports_are_the_declared_ones() {
    assert_eq!(standard_port(Scheme::Http), 80);
    assert_eq!(standard_port(Scheme::Https), 443);
    assert_eq!(standard_port(Scheme::Wss), 443);
    assert_eq!(standard_port(Scheme::Wt), 443);
    assert_eq!(standard_port(Scheme::Grpc), 443);
}

#[test]
fn a_single_hostname_on_its_standard_port_derives_itself() {
    derives(&[https("api.openai.com", 443)], "api.openai.com");
    derives(&[default_port(Scheme::Https, "api.openai.com")], "api.openai.com");
    derives(&[http("api.openai.com", 80)], "api.openai.com");
}

#[test]
fn a_single_hostname_on_a_non_standard_port_appends_it() {
    derives(&[https("api.openai.com", 8443)], "api.openai.com:8443");
    derives(&[http("api.openai.com", 8080)], "api.openai.com:8080");
}

#[test]
fn the_two_forms_of_one_hostname_are_distinct_aliases() {
    let standard = derive(&[https("api.openai.com", 443)]).expect("standard");
    let lifted = derive(&[https("api.openai.com", 8443)]).expect("lifted");
    assert_ne!(standard, lifted, "the port is part of the alias");
}

#[test]
fn a_pooled_set_derives_the_longest_common_suffix() {
    derives(
        &[
            https("us.vendor.com", 443),
            https("eu.vendor.com", 443),
        ],
        "vendor.com",
    );
    derives(
        &[
            https("api.us.vendor.com", 443),
            https("api.eu.vendor.com", 443),
        ],
        "vendor.com",
    );
}

#[test]
fn a_pooled_set_on_a_non_standard_port_appends_the_port() {
    derives(
        &[https("us.vendor.com", 8443), https("eu.vendor.com", 8443)],
        "vendor.com:8443",
    );
}

#[test]
fn hostnames_sharing_no_two_label_suffix_are_not_derivable() {
    not_derivable(&[https("api.openai.com", 443), https("api.vendor.com", 443)]);
}

#[test]
fn a_bare_public_suffix_is_never_an_alias() {
    not_derivable(&[https("www.co.uk", 443), https("shop.co.uk", 443)]);
}

#[test]
fn an_ip_literal_makes_the_set_not_derivable() {
    not_derivable(&[https("10.0.0.1", 443)]);
    not_derivable(&[https("api.openai.com", 443), https("10.0.0.1", 443)]);
}

#[test]
fn an_empty_endpoint_set_is_not_derivable() {
    not_derivable(&[]);
}

#[test]
fn derived_aliases_are_normalized() {
    // `EndpointHost` normalizes case and a trailing dot, so the derived alias
    // is lowercase and dot-free whatever the body stated.
    let alias = derive(&[https("API.OpenAI.COM.", 443)]).expect("the host normalizes");
    assert_eq!(alias.to_string(), "api.openai.com");
    let pooled = derive(&[https("US.Vendor.com.", 443), https("eu.VENDOR.com", 443)])
        .expect("the pool normalizes");
    assert_eq!(pooled.to_string(), "vendor.com");
}

#[test]
fn a_non_derivable_set_requires_an_explicit_alias() {
    let endpoints = [https("10.0.0.1", 443)];
    let error = resolve(&endpoints, None, None).expect_err("no alias was supplied");
    assert_eq!(error.kind, ErrorKind::ValidationError);
    assert_eq!(error.detail, "server.endpoints is not derivable");
}

#[test]
fn a_non_derivable_set_accepts_an_explicit_alias() {
    let endpoints = [https("10.0.0.1", 443)];
    let alias = resolve(&endpoints, Some("gateway.internal:8443"), None)
        .expect("the explicit alias is accepted");
    assert_eq!(alias.to_string(), "gateway.internal:8443");
}

#[test]
fn a_non_derivable_set_refuses_an_alias_that_is_not_one() {
    let endpoints = [https("10.0.0.1", 443)];
    let error = resolve(&endpoints, Some("not an alias"), None)
        .expect_err("the supplied value is not an alias");
    assert_eq!(error.kind, ErrorKind::ValidationError);
    assert_eq!(error.detail, "server.endpoints is not derivable");
}

#[test]
fn a_derivable_set_supplies_the_alias_when_none_was_given() {
    let endpoints = [https("api.openai.com", 8443)];
    let alias = resolve(&endpoints, None, None).expect("the derived alias is stored");
    assert_eq!(alias.to_string(), "api.openai.com:8443");
}

#[test]
fn a_supplied_alias_equal_to_the_derived_one_is_accepted() {
    let endpoints = [https("api.openai.com", 443)];
    let alias = resolve(&endpoints, Some("api.openai.com"), None)
        .expect("the idempotent supplied alias is accepted");
    assert_eq!(alias.to_string(), "api.openai.com");
}

#[test]
fn a_supplied_alias_differing_from_the_derived_one_is_refused() {
    let endpoints = [https("api.openai.com", 443)];
    let error = resolve(&endpoints, Some("other.vendor.com"), None)
        .expect_err("the supplied alias differs");
    assert_eq!(error.kind, ErrorKind::ValidationError);
    assert_eq!(error.detail, "alias does not match the endpoint set");
}

#[test]
fn a_supplied_alias_that_is_not_an_alias_at_all_is_refused() {
    let endpoints = [https("api.openai.com", 443)];
    let error = resolve(&endpoints, Some("Not A Host"), None)
        .expect_err("the supplied value is not an alias");
    assert_eq!(error.kind, ErrorKind::ValidationError);
    assert!(
        !error.detail.contains("Not A Host"),
        "the detail echoed the supplied value: {error}"
    );
}

#[test]
fn a_replacement_confirming_the_stored_alias_is_accepted() {
    let endpoints = [https("api.openai.com", 443)];
    let stored = Alias::parse("api.openai.com").expect("a stored alias");
    let alias = resolve(&endpoints, None, Some(&stored)).expect("the alias is unchanged");
    assert_eq!(alias, stored);
}

#[test]
fn a_replacement_deriving_a_different_alias_conflicts() {
    let endpoints = [https("api.openai.com", 8443)];
    let stored = Alias::parse("api.openai.com").expect("a stored alias");
    let error = resolve(&endpoints, None, Some(&stored)).expect_err("the alias moved");
    assert_eq!(error.kind, ErrorKind::AliasConflict);
    assert_eq!(error.http_status(), 409);
    assert_eq!(error.detail, "alias is immutable across updates");
}

#[test]
fn a_replacement_adding_a_pooled_endpoint_that_keeps_the_alias_is_accepted() {
    let stored = Alias::parse("vendor.com").expect("a stored alias");
    let endpoints = [
        https("us.vendor.com", 443),
        https("eu.vendor.com", 443),
        https("ap.vendor.com", 443),
    ];
    let alias = resolve(&endpoints, None, Some(&stored))
        .expect("the pooled replacement keeps the alias");
    assert_eq!(alias, stored);
}

#[test]
fn confirm_immutable_answers_the_conflict_row() {
    let derived = Alias::parse("vendor.com").expect("derived");
    let stored = Alias::parse("other.com").expect("stored");
    assert!(confirm_immutable(&derived, &stored).is_err());
    assert!(confirm_immutable(&stored, &stored).is_ok());
}

#[test]
fn an_alias_error_is_the_domain_cause_of_a_refusal() {
    // The refusal path is driven by `AliasError`, which stays a domain error
    // and never carries the refused value.
    assert!(matches!(
        Alias::parse("Not A Host"),
        Err(AliasError::InvalidLabel)
    ));
    let refused: Result<Alias, DomainError> = Alias::parse("Not A Host")
        .map_err(|_| DomainError::gateway(ErrorKind::ValidationError, "alias is not valid"));
    assert!(refused.is_err());
}
