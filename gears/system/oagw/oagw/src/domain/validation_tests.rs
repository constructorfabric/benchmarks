#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Unit tests for the domain validation rules (DESIGN §Validation).

use super::*;
use crate::domain::model::Scheme;

fn ep(scheme: Scheme, host: &str, port: u16) -> Endpoint {
    Endpoint { scheme, host: host.to_string(), port: Some(port) }
}

#[test]
fn an_empty_pool_is_rejected() {
    let error = validate_endpoints(&[], true).unwrap_err();
    assert!(error.to_string().contains("at least one endpoint"), "{error}");
}

#[test]
fn an_invalid_host_is_rejected() {
    let error = validate_endpoints(&[ep(Scheme::Http, "bad host!", 80)], true).unwrap_err();
    assert!(error.to_string().contains("hostname"), "{error}");
}

#[test]
fn a_port_of_zero_is_rejected() {
    let error = validate_endpoints(&[ep(Scheme::Http, "a.example.com", 0)], true).unwrap_err();
    assert!(error.to_string().contains("between 1 and 65535"), "{error}");
}

#[test]
fn plaintext_http_is_rejected_when_the_policy_disallows_it() {
    let error = validate_endpoints(&[ep(Scheme::Http, "a.example.com", 80)], false).unwrap_err();
    assert!(error.to_string().contains("disabled by policy"), "{error}");
}

#[test]
fn a_pool_may_not_mix_schemes() {
    let pool = [ep(Scheme::Https, "a.example.com", 443), ep(Scheme::Http, "b.example.com", 443)];
    let error = validate_endpoints(&pool, true).unwrap_err();
    assert!(error.to_string().contains("same scheme"), "{error}");
}

#[test]
fn a_pool_may_not_mix_ports() {
    let pool = [ep(Scheme::Https, "a.example.com", 443), ep(Scheme::Https, "b.example.com", 8443)];
    let error = validate_endpoints(&pool, true).unwrap_err();
    assert!(error.to_string().contains("same port"), "{error}");
}

#[test]
fn a_consistent_pool_is_accepted() {
    let pool = [ep(Scheme::Https, "us.vendor.com", 8443), ep(Scheme::Https, "eu.vendor.com", 8443)];
    assert!(validate_endpoints(&pool, true).is_ok());
}

#[test]
fn an_implicit_port_is_compared_through_its_scheme_default() {
    let implicit = Endpoint { scheme: Scheme::Https, host: "b.example.com".to_string(), port: None };
    let pool = [ep(Scheme::Https, "a.example.com", 443), implicit.clone()];
    assert!(validate_endpoints(&pool, true).is_ok());
    let mismatched = [ep(Scheme::Https, "a.example.com", 8443), implicit];
    let error = validate_endpoints(&mismatched, true).unwrap_err();
    assert!(error.to_string().contains("same port"), "{error}");
}
