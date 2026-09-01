//! Alias derivation matrix tests (`DESIGN.md` §3.1).

use crate::domain::alias::{
    compute_derived_alias, enforce_alias_on_create, enforce_alias_update_with, is_ip_address,
    normalize, validate_alias, validate_hostname,
};
use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, EndpointScheme};

fn https(host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme: EndpointScheme::Https,
        host: host.to_owned(),
        port,
    }
}

#[test]
fn single_hostname_standard_port() {
    let endpoints = vec![https("api.openai.com", 443)];
    assert_eq!(
        compute_derived_alias(&endpoints).as_deref(),
        Some("api.openai.com")
    );
}

#[test]
fn single_hostname_non_standard_port() {
    let endpoints = vec![https("api.openai.com", 8443)];
    assert_eq!(
        compute_derived_alias(&endpoints).as_deref(),
        Some("api.openai.com:8443")
    );
}

#[test]
fn multiple_hostnames_registrable_common_suffix() {
    let endpoints = vec![https("us.vendor.com", 443), https("eu.vendor.com", 443)];
    assert_eq!(
        compute_derived_alias(&endpoints).as_deref(),
        Some("vendor.com")
    );
}

#[test]
fn multiple_hostnames_with_non_standard_port_keep_port() {
    let endpoints = vec![https("us.vendor.com", 8443), https("eu.vendor.com", 8443)];
    assert_eq!(
        compute_derived_alias(&endpoints).as_deref(),
        Some("vendor.com:8443")
    );
}

#[test]
fn bare_public_suffix_is_not_derivable() {
    let endpoints = vec![https("foo.co.uk", 443), https("bar.co.uk", 443)];
    assert_eq!(compute_derived_alias(&endpoints), None);
}

#[test]
fn heterogeneous_hostnames_are_not_derivable() {
    let endpoints = vec![https("us.foo.com", 443), https("eu.bar.com", 443)];
    assert_eq!(compute_derived_alias(&endpoints), None);
}

#[test]
fn ip_addresses_are_not_derivable() {
    assert_eq!(compute_derived_alias(&[https("10.0.1.1", 443)]), None);
    assert_eq!(
        compute_derived_alias(&[https("10.0.1.1", 443), https("10.0.1.2", 443)]),
        None
    );
    assert_eq!(
        compute_derived_alias(&[https("2001:db8::1", 443), https("2001:db8::2", 443)]),
        None
    );
}

#[test]
fn mixed_schemes_or_ports_are_not_derivable() {
    let endpoints = vec![
        Endpoint {
            scheme: EndpointScheme::Https,
            host: "us.vendor.com".to_owned(),
            port: 443,
        },
        Endpoint {
            scheme: EndpointScheme::Wss,
            host: "eu.vendor.com".to_owned(),
            port: 443,
        },
    ];
    assert_eq!(compute_derived_alias(&endpoints), None);

    let mixed_ports = vec![https("us.vendor.com", 443), https("eu.vendor.com", 8443)];
    assert_eq!(compute_derived_alias(&mixed_ports), None);
}

#[test]
fn normalization_is_lowercase_and_strips_trailing_dot() {
    assert_eq!(normalize("Api.OpenAI.COM."), "api.openai.com");
    assert_eq!(
        compute_derived_alias(&[https("API.OpenAI.COM.", 443)]).as_deref(),
        Some("api.openai.com")
    );
}

#[test]
fn create_rejects_alias_differing_from_derived() {
    let endpoints = vec![https("api.openai.com", 443)];
    assert!(enforce_alias_on_create(&endpoints, Some("openai")).is_err());
    assert_eq!(
        enforce_alias_on_create(&endpoints, Some("api.openai.com"))
            .expect("idempotent alias is tolerated"),
        "api.openai.com"
    );
    assert!(enforce_alias_on_create(&endpoints, None).is_ok());
}

#[test]
fn create_requires_alias_for_ip_endpoints() {
    let endpoints = vec![https("10.0.1.1", 443)];
    let err = enforce_alias_on_create(&endpoints, None).unwrap_err();
    assert!(matches!(err, DomainError::Validation(_)));
    assert_eq!(
        enforce_alias_on_create(&endpoints, Some("my-service")).expect("explicit alias"),
        "my-service"
    );
}

#[test]
fn create_rejects_invalid_alias() {
    let endpoints = vec![https("10.0.1.1", 443)];
    assert!(enforce_alias_on_create(&endpoints, Some("my service")).is_err());
    assert!(enforce_alias_on_create(&endpoints, Some("-my-service")).is_err());
    assert!(enforce_alias_on_create(&endpoints, Some("my_service")).is_err());
    assert!(enforce_alias_on_create(&endpoints, Some("")).is_err());
    // Case and a trailing dot are normalized away, not rejected.
    assert_eq!(
        enforce_alias_on_create(&endpoints, Some("My-Service.")).expect("normalized alias"),
        "my-service"
    );
}

#[test]
fn update_allows_alias_preserving_endpoint_change() {
    let old = vec![https("api.openai.com", 443)];
    let new = vec![https("api.openai.com", 443), https("eu.openai.com", 443)];
    // Both derive `openai.com` — wait: single endpoint derives the full
    // hostname, so this transition changes the derived alias and is rejected.
    assert!(enforce_alias_update_with("api.openai.com", &new, None).is_err());
    // Same single endpoint: derived alias unchanged.
    assert_eq!(
        enforce_alias_update_with("api.openai.com", &old, None).unwrap(),
        "api.openai.com"
    );
}

#[test]
fn update_rejects_derivable_to_non_derivable_even_with_explicit_alias() {
    let ip = vec![https("10.0.1.1", 443)];
    assert!(enforce_alias_update_with("api.openai.com", &ip, Some("my-service")).is_err());
}

#[test]
fn update_rejects_non_derivable_alias_override() {
    let ips = vec![https("10.0.1.1", 443), https("10.0.1.2", 443)];
    assert!(enforce_alias_update_with("my-service", &ips, Some("other")).is_err());
    assert_eq!(
        enforce_alias_update_with("my-service", &ips, Some("My-Service."))
            .expect("normalized idempotent alias"),
        "my-service"
    );
    // No alias supplied: existing alias retained.
    assert_eq!(
        enforce_alias_update_with("my-service", &ips, None).unwrap(),
        "my-service"
    );
}

#[test]
fn update_rejects_non_derivable_to_derivable() {
    let host = vec![https("api.openai.com", 443)];
    assert!(enforce_alias_update_with("my-service", &host, None).is_err());
    assert!(enforce_alias_update_with("my-service", &host, Some("api.openai.com")).is_err());
}

#[test]
fn hostname_validation_rules() {
    assert!(validate_hostname("api.openai.com").is_ok());
    assert!(validate_hostname("api.openai.com.").is_ok());
    assert!(validate_hostname("xn--bcher-kva.example").is_ok());
    assert!(validate_hostname("").is_err());
    assert!(validate_hostname("-bad.example.com").is_err());
    assert!(validate_hostname("bad-.example.com").is_err());
    assert!(validate_hostname("ba d.example.com").is_err());
    assert!(validate_hostname("a..b").is_err());
    assert!(validate_hostname(&"a".repeat(64)).is_err());
    let long_host = format!("{}.example.com", "a".repeat(260));
    assert!(validate_hostname(&long_host).is_err());
    assert!(validate_hostname("10.0.1.1").is_ok());
    assert!(validate_hostname("2001:db8::1").is_ok());
}

#[test]
fn ip_detection() {
    assert!(is_ip_address("10.0.1.1"));
    assert!(is_ip_address("2001:db8::1"));
    assert!(!is_ip_address("api.openai.com"));
    assert!(!is_ip_address("999.1.1.1"));
}

#[test]
fn alias_validation_rules() {
    assert!(validate_alias("api.openai.com").is_ok());
    assert!(validate_alias("vendor.com:8443").is_ok());
    assert!(validate_alias("my-service").is_ok());
    assert!(validate_alias("1service").is_ok());
    assert!(validate_alias("").is_err());
    assert!(validate_alias("-service").is_err());
    assert!(validate_alias("service-").is_err());
    assert!(validate_alias("My-Service").is_err());
    assert!(validate_alias("my service").is_err());
}
