//! Table-driven tests for alias derivation and the update-transition matrix.

use uuid::Uuid;

use super::*;
use crate::domain::model::EndpointScheme;

fn ep(scheme: EndpointScheme, host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme,
        host: host.to_owned(),
        port: Some(port),
    }
}

fn https(host: &str, port: u16) -> Endpoint {
    ep(EndpointScheme::Https, host, port)
}

// ---------------------------------------------------------------------------
// Host validation
// ---------------------------------------------------------------------------

#[test]
fn hostname_validation_accepts_and_rejects() {
    let long_label = "x".repeat(64);
    let long_name = "a.".repeat(200);
    let long_name = long_name.trim_end_matches('.');
    let cases: Vec<(&str, bool)> = vec![
        ("api.openai.com", true),
        ("API.OPENAI.COM", true),
        ("a-b.c", true),
        ("123.example.com", true),
        ("127.0.0.1", true),
        ("::1", true),
        ("", false),
        ("-leading.example.com", false),
        ("trailing-.example.com", false),
        ("under_score.example.com", false),
        ("double..dot.com", false),
        ("dot.at.end.", true), // trailing FQDN dot tolerated
        (long_label.as_str(), false),
        (long_name, false),
    ];
    for (host, ok) in cases {
        let result = validate_host(host);
        assert_eq!(result.is_ok(), ok, "host `{host}`");
    }
}

#[test]
fn hostname_length_limits() {
    let long_label = format!("{}.com", "a".repeat(64));
    assert!(validate_host(&long_label).is_err());
    let ok_label = format!("{}.com", "a".repeat(63));
    assert!(validate_host(&ok_label).is_ok());

    let long_host = format!("{}.{}", "b".repeat(250), "com");
    assert!(validate_host(&long_host).is_err());
}

// ---------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------

#[test]
fn alias_normalization() {
    assert_eq!(normalize_alias("  Api.OpenAI.COM. "), "api.openai.com");
    assert_eq!(normalize_alias("MY-SERVICE"), "my-service");
    assert_eq!(normalize_host("Example.COM."), "example.com");
}

// ---------------------------------------------------------------------------
// Derivation
// ---------------------------------------------------------------------------

#[test]
fn single_host_standard_port_omits_port() {
    let d = compute_derived_alias(&[https("api.openai.com", 443)]).unwrap();
    assert_eq!(d, AliasDerivation::Derived("api.openai.com".to_owned()));
}

#[test]
fn single_host_non_standard_port_keeps_port() {
    let d = compute_derived_alias(&[https("api.openai.com", 8443)]).unwrap();
    assert_eq!(
        d,
        AliasDerivation::Derived("api.openai.com:8443".to_owned())
    );
}

#[test]
fn http_standard_port_is_80() {
    let d = compute_derived_alias(&[ep(EndpointScheme::Http, "svc.local", 80)]).unwrap();
    assert_eq!(d, AliasDerivation::Derived("svc.local".to_owned()));

    let d = compute_derived_alias(&[ep(EndpointScheme::Http, "svc.local", 8080)]).unwrap();
    assert_eq!(d, AliasDerivation::Derived("svc.local:8080".to_owned()));
}

#[test]
fn common_registrable_suffix_derives_suffix_alias() {
    let endpoints = vec![https("us.vendor.com", 443), https("eu.vendor.com", 443)];
    let d = compute_derived_alias(&endpoints).unwrap();
    assert_eq!(d, AliasDerivation::Derived("vendor.com".to_owned()));
}

#[test]
fn common_suffix_preserves_non_standard_port() {
    let endpoints = vec![https("us.vendor.com", 8443), https("eu.vendor.com", 8443)];
    let d = compute_derived_alias(&endpoints).unwrap();
    assert_eq!(d, AliasDerivation::Derived("vendor.com:8443".to_owned()));
}

#[test]
fn bare_public_suffix_is_not_derivable() {
    let endpoints = vec![https("foo.co.uk", 443), https("bar.co.uk", 443)];
    let d = compute_derived_alias(&endpoints).unwrap();
    assert!(matches!(d, AliasDerivation::NotDerivable(_)), "{d:?}");
}

#[test]
fn unrelated_hostnames_are_not_derivable() {
    let endpoints = vec![https("us.foo.com", 443), https("eu.bar.com", 443)];
    assert!(matches!(
        compute_derived_alias(&endpoints).unwrap(),
        AliasDerivation::NotDerivable(_)
    ));
}

#[test]
fn ip_endpoints_require_explicit_alias() {
    let endpoints = vec![https("10.0.1.1", 443), https("10.0.1.2", 443)];
    assert!(matches!(
        compute_derived_alias(&endpoints).unwrap(),
        AliasDerivation::NotDerivable(_)
    ));
}

#[test]
fn mixed_ip_and_hostname_is_not_derivable() {
    let endpoints = vec![https("10.0.1.1", 443), https("api.vendor.com", 443)];
    assert!(matches!(
        compute_derived_alias(&endpoints).unwrap(),
        AliasDerivation::NotDerivable(_)
    ));
}

#[test]
fn inconsistent_pool_is_rejected() {
    let endpoints = vec![https("us.vendor.com", 443), https("eu.vendor.com", 8443)];
    assert!(compute_derived_alias(&endpoints).is_err());

    let endpoints = vec![
        https("us.vendor.com", 443),
        ep(EndpointScheme::Http, "eu.vendor.com", 443),
    ];
    assert!(compute_derived_alias(&endpoints).is_err());
}

#[test]
fn empty_endpoint_list_is_rejected() {
    assert!(compute_derived_alias(&[]).is_err());
}

#[test]
fn invalid_host_is_rejected() {
    let endpoints = vec![https("-bad.example.com", 443)];
    assert!(compute_derived_alias(&endpoints).is_err());
}

#[test]
fn trailing_fqdn_dot_is_stripped_from_derived_alias() {
    let d = compute_derived_alias(&[https("api.openai.com.", 443)]).unwrap();
    assert_eq!(d, AliasDerivation::Derived("api.openai.com".to_owned()));
}

// ---------------------------------------------------------------------------
// Create enforcement
// ---------------------------------------------------------------------------

#[test]
fn create_derives_when_no_alias_supplied() {
    let endpoints = vec![https("api.openai.com", 443)];
    assert_eq!(
        enforce_alias_create(&endpoints, None).unwrap(),
        "api.openai.com"
    );
}

#[test]
fn create_tolerates_exact_derived_alias() {
    let endpoints = vec![https("api.openai.com", 443)];
    assert_eq!(
        enforce_alias_create(&endpoints, Some("api.openai.com")).unwrap(),
        "api.openai.com"
    );
    // Idempotent even when the caller used different casing.
    assert_eq!(
        enforce_alias_create(&endpoints, Some("API.OpenAI.COM")).unwrap(),
        "api.openai.com"
    );
}

#[test]
fn create_rejects_mismatched_alias_on_hostname_endpoints() {
    let endpoints = vec![https("api.openai.com", 443)];
    let err = enforce_alias_create(&endpoints, Some("my-openai")).unwrap_err();
    assert_eq!(err.status(), http::StatusCode::BAD_REQUEST);
}

#[test]
fn create_requires_alias_for_ip_endpoints() {
    let endpoints = vec![https("10.0.1.1", 443), https("10.0.1.2", 443)];
    assert!(enforce_alias_create(&endpoints, None).is_err());
    assert_eq!(
        enforce_alias_create(&endpoints, Some("my-internal-service")).unwrap(),
        "my-internal-service"
    );
}

#[test]
fn create_alias_shape_is_validated() {
    let endpoints = vec![https("10.0.1.1", 443)];
    assert!(enforce_alias_create(&endpoints, Some("-bad alias!")).is_err());
}

// ---------------------------------------------------------------------------
// Update transition matrix
// ---------------------------------------------------------------------------

fn upstream(endpoints: &[Endpoint], alias: &str) -> (String, bool) {
    let _ = Uuid::new_v4();
    let derivable = matches!(
        compute_derived_alias(endpoints).unwrap(),
        AliasDerivation::Derived(_)
    );
    (alias.to_owned(), derivable)
}

#[test]
fn derivable_to_derivable_same_alias_is_allowed() {
    let old = vec![https("api.openai.com", 443)];
    let (alias, derivable) = upstream(&old, "api.openai.com");
    let new = vec![https("api.openai.com", 443), https("alt.openai.com", 443)];
    // Derives openai.com... but the existing alias is api.openai.com, so this must reject.
    assert!(enforce_alias_update(&alias, derivable, &new, None).is_err());
}

#[test]
fn no_endpoint_change_is_a_noop() {
    let old = vec![https("api.openai.com", 443)];
    let (alias, derivable) = upstream(&old, "api.openai.com");
    assert_eq!(
        enforce_alias_update(&alias, derivable, &old, None).unwrap(),
        "api.openai.com"
    );
    // Exact-match alias tolerated.
    assert_eq!(
        enforce_alias_update(&alias, derivable, &old, Some("api.openai.com")).unwrap(),
        "api.openai.com"
    );
}

#[test]
fn alias_override_is_rejected() {
    let old = vec![https("api.openai.com", 443)];
    let (alias, derivable) = upstream(&old, "api.openai.com");
    let err = enforce_alias_update(&alias, derivable, &old, Some("other-name")).unwrap_err();
    assert_eq!(err.status(), http::StatusCode::BAD_REQUEST);
}

#[test]
fn ip_to_ip_retains_alias() {
    let old = vec![https("10.0.1.1", 443), https("10.0.1.2", 443)];
    let (alias, derivable) = upstream(&old, "my-internal-service");
    let new = vec![https("10.0.2.1", 443), https("10.0.2.2", 443)];
    assert_eq!(
        enforce_alias_update(&alias, derivable, &new, None).unwrap(),
        "my-internal-service"
    );
    // Same alias supplied explicitly is accepted.
    assert_eq!(
        enforce_alias_update(&alias, derivable, &new, Some("my-internal-service")).unwrap(),
        "my-internal-service"
    );
    // A differing alias is rejected.
    assert!(enforce_alias_update(&alias, derivable, &new, Some("renamed")).is_err());
}

#[test]
fn hostname_to_ip_is_always_rejected() {
    let old = vec![https("api.openai.com", 443)];
    let (alias, derivable) = upstream(&old, "api.openai.com");
    let new = vec![https("10.0.1.1", 443)];
    assert!(enforce_alias_update(&alias, derivable, &new, None).is_err());
    assert!(enforce_alias_update(&alias, derivable, &new, Some("api.openai.com")).is_err());
}

#[test]
fn ip_to_hostname_with_matching_alias_is_allowed() {
    let old = vec![https("10.0.1.1", 443)];
    let (alias, derivable) = upstream(&old, "10.0.1.1");
    let new = vec![https("10.0.1.1", 443)];
    assert_eq!(
        enforce_alias_update(&alias, derivable, &new, None).unwrap(),
        "10.0.1.1"
    );
}

#[test]
fn ip_to_hostname_with_different_alias_is_rejected() {
    let old = vec![https("10.0.1.1", 443)];
    let (alias, derivable) = upstream(&old, "10.0.1.1");
    let new = vec![https("api.openai.com", 443)];
    assert!(enforce_alias_update(&alias, derivable, &new, None).is_err());
}
