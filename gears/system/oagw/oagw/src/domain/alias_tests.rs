#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use crate::domain::alias::{
    compute_derived_alias, enforce_alias_create, enforce_alias_update, is_ip, validate_alias_shape,
};
use crate::domain::model::{Endpoint, EndpointScheme};

fn https(host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme: EndpointScheme::Https,
        host: host.to_owned(),
        port,
    }
}

fn pool(hosts: &[(&str, u16)]) -> Vec<Endpoint> {
    hosts
        .iter()
        .map(|(host, port)| https(host, *port))
        .collect()
}

#[test]
fn single_hostname_standard_port_derives_the_hostname() {
    let derived = compute_derived_alias(&pool(&[("api.openai.com", 443)]));
    assert_eq!(derived.as_deref(), Some("api.openai.com"));
}

#[test]
fn single_hostname_non_standard_port_derives_host_and_port() {
    let derived = compute_derived_alias(&pool(&[("api.openai.com", 8443)]));
    assert_eq!(derived.as_deref(), Some("api.openai.com:8443"));
}

#[test]
fn single_ip_endpoint_does_not_derive() {
    assert_eq!(compute_derived_alias(&pool(&[("10.0.1.1", 443)])), None);
    assert_eq!(compute_derived_alias(&pool(&[("2001:db8::1", 443)])), None);
}

#[test]
fn common_registrable_suffix_derives_the_shared_domain() {
    let derived = compute_derived_alias(&pool(&[("us.vendor.com", 443), ("eu.vendor.com", 443)]));
    assert_eq!(derived.as_deref(), Some("vendor.com"));
}

#[test]
fn common_suffix_on_non_standard_port_keeps_the_port() {
    let derived = compute_derived_alias(&pool(&[("us.vendor.com", 8443), ("eu.vendor.com", 8443)]));
    assert_eq!(derived.as_deref(), Some("vendor.com:8443"));
}

#[test]
fn bare_public_suffix_is_not_derivable() {
    // `co.uk` is a public suffix, so `foo.co.uk` + `bar.co.uk` must not derive.
    let derived = compute_derived_alias(&pool(&[("foo.co.uk", 443), ("bar.co.uk", 443)]));
    assert_eq!(derived, None);
}

#[test]
fn single_label_suffix_is_not_derivable() {
    assert_eq!(
        compute_derived_alias(&pool(&[("foo", 443), ("bar", 443)])),
        None
    );
}

#[test]
fn heterogeneous_hostnames_are_not_derivable() {
    let derived = compute_derived_alias(&pool(&[("us.foo.com", 443), ("eu.bar.com", 443)]));
    assert_eq!(derived, None);
}

#[test]
fn mixed_ip_pool_is_not_derivable() {
    let endpoints = vec![https("us.vendor.com", 443), https("10.0.1.2", 443)];
    assert_eq!(compute_derived_alias(&endpoints), None);
}

#[test]
fn mismatched_ports_are_not_derivable() {
    let endpoints = vec![https("us.vendor.com", 443), https("eu.vendor.com", 8443)];
    assert_eq!(compute_derived_alias(&endpoints), None);
}

#[test]
fn deeper_hosts_share_the_inner_registrable_suffix() {
    let derived = compute_derived_alias(&pool(&[("a.b.vendor.com", 443), ("c.vendor.com", 443)]));
    assert_eq!(derived.as_deref(), Some("vendor.com"));
}

#[test]
fn hosts_equal_to_the_shared_suffix_still_derive() {
    let derived = compute_derived_alias(&pool(&[("vendor.com", 443), ("us.vendor.com", 443)]));
    assert_eq!(derived.as_deref(), Some("vendor.com"));
}

#[test]
fn hosts_are_normalized_before_derivation() {
    let endpoints = vec![https("Api.OpenAI.COM.", 443)];
    assert_eq!(
        compute_derived_alias(&endpoints).as_deref(),
        Some("api.openai.com")
    );
}

#[test]
fn ip_detection() {
    assert!(is_ip("10.0.1.1"));
    assert!(is_ip("2001:db8::1"));
    assert!(!is_ip("api.openai.com"));
    assert!(!is_ip("999.1.1.1"));
}

#[test]
fn alias_shape_accepts_the_documented_alphabet() {
    for ok in ["a", "api.openai.com", "vendor.com:8443", "my-service", "0"] {
        assert!(validate_alias_shape(ok).is_ok(), "{ok}");
    }
    for bad in [
        "",
        "-lead",
        "trail-",
        "Upper",
        "with space",
        "under_score",
        &"a".repeat(254),
    ] {
        assert!(validate_alias_shape(bad).is_err(), "{bad}");
    }
}

#[test]
fn create_tolerates_an_explicit_alias_matching_the_derivation() {
    let endpoints = pool(&[("api.openai.com", 443)]);
    let alias = enforce_alias_create(Some("api.openai.com"), &endpoints).expect("accepted");
    assert_eq!(alias, "api.openai.com");
}

#[test]
fn create_rejects_a_differing_explicit_alias_for_hostname_pools() {
    let endpoints = pool(&[("api.openai.com", 443)]);
    let err = enforce_alias_create(Some("my-openai"), &endpoints).expect_err("rejected");
    assert!(err.to_string().contains("api.openai.com"), "{err}");
}

#[test]
fn create_requires_an_explicit_alias_for_ip_pools() {
    let endpoints = pool(&[("10.0.1.1", 443)]);
    let err = enforce_alias_create(None, &endpoints).expect_err("rejected");
    assert!(err.to_string().contains("alias is required"), "{err}");

    let alias = enforce_alias_create(Some("my-service"), &endpoints).expect("accepted");
    assert_eq!(alias, "my-service");
}

#[test]
fn create_normalizes_and_strips_the_root_dot() {
    let endpoints = pool(&[("10.0.1.1", 443)]);
    let alias = enforce_alias_create(Some("My-Service."), &endpoints).expect("accepted");
    assert_eq!(alias, "my-service");
}

#[test]
fn update_accepts_an_unchanged_derivation() {
    let endpoints = pool(&[("api.openai.com", 443)]);
    enforce_alias_update("api.openai.com", None, &endpoints, &endpoints).expect("allowed");
}

#[test]
fn update_rejects_a_changed_derivation() {
    let previous = pool(&[("api.openai.com", 443)]);
    let next = pool(&[("api.anthropic.com", 443)]);
    let err = enforce_alias_update("api.openai.com", None, &previous, &next).expect_err("rejected");
    assert!(err.to_string().contains("immutable"), "{err}");
}

#[test]
fn update_rejects_hostname_to_ip_even_with_an_explicit_alias() {
    // Derivable -> non-derivable is always rejected: the routing key would
    // silently survive a change of endpoint family.
    let previous = pool(&[("api.openai.com", 443)]);
    let next = pool(&[("10.0.1.1", 443)]);
    for requested in [None, Some("api.openai.com")] {
        let err = enforce_alias_update("api.openai.com", requested, &previous, &next)
            .expect_err("rejected");
        assert!(err.to_string().contains("immutable"), "{err}");
    }
}

#[test]
fn update_keeps_the_existing_alias_for_ip_to_ip() {
    let previous = pool(&[("10.0.1.1", 443)]);
    let next = pool(&[("10.0.1.2", 443)]);
    let alias = enforce_alias_update("my-service", None, &previous, &next).expect("allowed");
    assert_eq!(alias, "my-service");

    // A differing explicit alias is refused.
    let err = enforce_alias_update("my-service", Some("renamed"), &previous, &next)
        .expect_err("rejected");
    assert!(err.to_string().contains("immutable"), "{err}");
}

#[test]
fn update_accepts_ip_to_hostname_when_the_derivation_matches() {
    let previous = pool(&[("10.0.1.1", 443)]);
    let next = pool(&[("my-service.example.com", 443)]);
    let alias =
        enforce_alias_update("my-service.example.com", None, &previous, &next).expect("allowed");
    assert_eq!(alias, "my-service.example.com");

    // And it is refused when the derivation does not match the existing alias.
    let err = enforce_alias_update("my-service", None, &previous, &next).expect_err("rejected");
    assert!(err.to_string().contains("immutable"), "{err}");
}

#[test]
fn update_rejects_an_explicit_alias_that_overrides_a_matching_derivation() {
    let endpoints = pool(&[("api.openai.com", 443)]);
    let err = enforce_alias_update("api.openai.com", Some("renamed"), &endpoints, &endpoints)
        .expect_err("rejected");
    assert!(err.to_string().contains("immutable"), "{err}");
}
