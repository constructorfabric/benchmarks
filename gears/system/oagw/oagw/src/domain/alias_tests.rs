//! Tests for alias derivation and validation (DESIGN §3.1, PRD §5.5).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::domain::models::EndpointScheme;

fn endpoint(scheme: EndpointScheme, host: &str, port: u16) -> Endpoint {
    Endpoint::new(scheme, host, port)
}

// --- normalisation ---------------------------------------------------------

#[test]
fn normalises_case_and_trailing_dots() {
    assert_eq!(normalize_alias("API.OpenAI.COM."), "api.openai.com");
    assert_eq!(normalize_alias("  Example.COM  "), "example.com");
    assert_eq!(normalize_alias("EXAMPLE"), "example");
    assert_eq!(normalize_alias("198.51.100.7"), "198.51.100.7");
}

#[test]
fn alias_pattern_is_enforced() {
    assert!(alias_is_valid("api.openai.com"));
    assert!(alias_is_valid("api.openai.com:8443"));
    assert!(alias_is_valid("a-b.c1"));
    assert!(!alias_is_valid(""));
    assert!(!alias_is_valid("-leading"));
    assert!(!alias_is_valid("trailing-"));
    assert!(!alias_is_valid("has space"));
    assert!(!alias_is_valid("slash/inside"));
    assert!(!alias_is_valid(".leading.dot"));
}

// --- hostname validation ---------------------------------------------------

#[test]
fn accepts_valid_hostnames() {
    for host in [
        "localhost",
        "api.openai.com",
        "a-b.example.co.uk",
        "123.example.com",
        "xn--80ak6aa92e.com",
    ] {
        assert!(validate_hostname(host).is_ok(), "expected ok: {host}");
    }
}

#[test]
fn rejects_invalid_hostnames() {
    for host in [
        "",
        "-lead.example.com",
        "trail-.example.com",
        "double..dot.com",
        "under_score.example.com",
        "sp ace.example.com",
        "colon:in:hostname",
        "brackets[::1]",
    ] {
        let err = validate_hostname(host);
        assert!(err.is_err(), "expected error: {host}");
        assert_eq!(err.unwrap_err().status(), 400);
    }
    assert!(validate_hostname(&format!("{}.example.com", "a".repeat(64))).is_err());
}

#[test]
fn accepts_ip_literals_as_hosts() {
    assert!(validate_hostname("198.51.100.7").is_ok());
    assert!(validate_hostname("2001:db8::1").is_ok());
    assert!(validate_hostname("::1").is_ok());
}

// --- endpoint host normalisation -------------------------------------------

#[test]
fn normalizes_endpoint_hosts() {
    assert_eq!(normalize_host("API.OpenAI.COM.").unwrap(), "api.openai.com");
    assert_eq!(normalize_host("2001:DB8::1").unwrap(), "2001:db8::1");
    assert!(normalize_host("[::1]").is_err());
    assert!(normalize_host("api.openai.com/path").is_err());
    assert!(normalize_host("").is_err());
}

// --- derivation ------------------------------------------------------------

#[test]
fn single_host_standard_port_uses_bare_hostname() {
    let derivation = derive_alias(&[endpoint(EndpointScheme::Https, "API.OpenAI.COM.", 443)]).unwrap();
    assert_eq!(derivation.alias, "api.openai.com");
    assert_eq!(derivation.source, AliasSource::SingleHost);
    assert_eq!(derivation.port, None);
}

#[test]
fn single_host_non_standard_port_appends_port() {
    let derivation = derive_alias(&[endpoint(EndpointScheme::Https, "api.openai.com", 8_443)]).unwrap();
    assert_eq!(derivation.alias, "api.openai.com:8443");
    assert_eq!(derivation.port, Some(8_443));
}

#[test]
fn http_standard_port_is_80() {
    let derivation = derive_alias(&[endpoint(EndpointScheme::Http, "api.example.com", 80)]).unwrap();
    assert_eq!(derivation.alias, "api.example.com");

    let other = derive_alias(&[endpoint(EndpointScheme::Http, "api.example.com", 8_080)]).unwrap();
    assert_eq!(other.alias, "api.example.com:8080");
}

#[test]
fn pooled_hosts_collapse_to_common_registrable_suffix() {
    let derivation = derive_alias(&[
        endpoint(EndpointScheme::Https, "api1.openai.com", 443),
        endpoint(EndpointScheme::Https, "api2.openai.com", 443),
        endpoint(EndpointScheme::Https, "api3.openai.com", 443),
    ])
    .unwrap();
    assert_eq!(derivation.alias, "openai.com");
    assert_eq!(derivation.source, AliasSource::CommonSuffix);
    assert_eq!(derivation.common_suffix.as_deref(), Some("openai.com"));
}

#[test]
fn pooled_non_standard_port_is_preserved() {
    let derivation = derive_alias(&[
        endpoint(EndpointScheme::Https, "api1.openai.com", 8_443),
        endpoint(EndpointScheme::Https, "api2.openai.com", 8_443),
    ])
    .unwrap();
    assert_eq!(derivation.alias, "openai.com:8443");
}

#[test]
fn pooled_hosts_without_common_suffix_are_not_derivable() {
    let err = derive_alias(&[
        endpoint(EndpointScheme::Https, "api.openai.com", 443),
        endpoint(EndpointScheme::Https, "api.anthropic.com", 443),
    ])
    .unwrap_err();
    assert!(matches!(err, AliasError::NotDerivable { .. }), "{err:?}");
}

#[test]
fn bare_public_suffix_is_not_a_valid_alias() {
    // `co.uk` is a public suffix, so the pool's common suffix is not a
    // registrable domain and no alias can be derived (DESIGN §3.1).
    let err = derive_alias(&[
        endpoint(EndpointScheme::Https, "a.co.uk", 443),
        endpoint(EndpointScheme::Https, "b.co.uk", 443),
    ])
    .unwrap_err();
    assert!(matches!(err, AliasError::NotDerivable { .. }), "{err:?}");
    assert!(err.to_string().contains("co.uk"), "{err:?}");

    // A registrable common suffix is still derivable.
    let derivation = derive_alias(&[
        endpoint(EndpointScheme::Https, "a.example.com", 443),
        endpoint(EndpointScheme::Https, "b.example.com", 443),
    ])
    .unwrap();
    assert_eq!(derivation.alias, "example.com");
}

#[test]
fn ip_based_pools_require_an_explicit_alias() {
    let err = derive_alias(&[endpoint(EndpointScheme::Https, "198.51.100.7", 443)]).unwrap_err();
    assert!(matches!(err, AliasError::IpBased { .. }), "{err:?}");
    let domain = err.into_domain();
    assert_eq!(domain.status(), 400);
    assert!(domain.detail().contains("explicit alias"));

    let err = derive_alias(&[
        endpoint(EndpointScheme::Https, "2001:db8::1", 443),
        endpoint(EndpointScheme::Https, "2001:db8::2", 443),
    ])
    .unwrap_err();
    assert!(matches!(err, AliasError::IpBased { .. }), "{err:?}");
}

#[test]
fn empty_pool_is_rejected() {
    let err = derive_alias(&[]).unwrap_err();
    assert!(matches!(err, AliasError::NotDerivable { .. }));
}

#[test]
fn invalid_endpoint_host_is_reported() {
    let err = derive_alias(&[endpoint(EndpointScheme::Https, "not a host", 443)]).unwrap_err();
    assert!(matches!(err, AliasError::InvalidEndpoint { .. }), "{err:?}");
}

// --- common suffix helper --------------------------------------------------

#[test]
fn common_suffix_is_label_wise() {
    let hosts = vec![
        "api1.openai.com".to_owned(),
        "api2.openai.com".to_owned(),
    ];
    assert_eq!(common_suffix(&hosts).as_deref(), Some("openai.com"));

    let none = vec!["a.com".to_owned(), "b.org".to_owned()];
    assert_eq!(common_suffix(&none), None);

    let case_insensitive = vec!["A.Example.com".to_owned(), "b.example.COM".to_owned()];
    assert_eq!(common_suffix(&case_insensitive).as_deref(), Some("example.com"));
}

// --- update enforcement ----------------------------------------------------

#[test]
fn unchanged_pool_keeps_alias() {
    let pool = [endpoint(EndpointScheme::Https, "api.openai.com", 443)];
    enforce_alias_update("api.openai.com", &pool, Some("api.openai.com"), &pool).unwrap();
    enforce_alias_update("api.openai.com", &pool, None, &pool).unwrap();
}

#[test]
fn alias_is_immutable_for_hostname_upstreams() {
    let old = [endpoint(EndpointScheme::Https, "api.openai.com", 443)];
    let new = [endpoint(EndpointScheme::Https, "api.anthropic.com", 443)];
    let err = enforce_alias_update("api.openai.com", &old, Some("api.openai.com"), &new).unwrap_err();
    assert_eq!(err.status(), 400);
    let extensions = err.extensions();
    // The rejected value is the one the caller supplied; the alias the new
    // pool would derive is reported in `detail` (and as `alias`).
    assert_eq!(extensions.invalid_value.as_deref(), Some("api.openai.com"));
    assert_eq!(extensions.alias.as_deref(), Some("api.anthropic.com"));
    assert!(err.detail().contains("api.anthropic.com"), "{err:?}");
}

#[test]
fn ip_based_upstream_may_change_its_alias() {
    let old = [endpoint(EndpointScheme::Https, "198.51.100.7", 443)];
    let new = [endpoint(EndpointScheme::Https, "198.51.100.8", 443)];
    enforce_alias_update("legacy-name", &old, Some("new-name"), &new).unwrap();
}

#[test]
fn hostname_pool_cannot_become_non_derivable() {
    let old = [endpoint(EndpointScheme::Https, "api.openai.com", 443)];
    let new = [endpoint(EndpointScheme::Https, "198.51.100.7", 443)];
    let err = enforce_alias_update("api.openai.com", &old, None, &new);
    assert!(err.is_err());
}

#[test]
fn injectable_derivation_is_used() {
    let old = [endpoint(EndpointScheme::Https, "api.openai.com", 443)];
    let new = [endpoint(EndpointScheme::Https, "api.anthropic.com", 443)];

    // A derivation function that always yields the same alias is accepted.
    let stable = enforce_alias_update_with("api.openai.com", &old, None, &new, |hosts| {
        Ok(AliasDerivation {
            alias: "api.openai.com".to_owned(),
            source: AliasSource::CommonSuffix,
            common_suffix: hosts.iter().map(|endpoint| endpoint.host.clone()).next(),
            port: None,
        })
    });
    assert!(stable.is_ok());

    // A derivation function that accepts the existing pool but rejects the new
    // one propagates the failure: the injected resolver is the one consulted.
    let failing = enforce_alias_update_with("api.openai.com", &old, None, &new, |hosts| {
        if hosts.len() == 1 && hosts[0].host == "api.openai.com" {
            Ok(AliasDerivation {
                alias: "api.openai.com".to_owned(),
                source: AliasSource::SingleHost,
                common_suffix: Some("api.openai.com".to_owned()),
                port: None,
            })
        } else {
            Err(DomainError::AliasNotDerivable {
                detail: "nope".to_owned(),
                valid_hosts: Vec::new(),
            })
        }
    });
    assert_eq!(failing.unwrap_err().status(), 400);
}

// --- endpoint equality -----------------------------------------------------

#[test]
fn endpoint_equality_is_order_insensitive() {
    let a = [
        endpoint(EndpointScheme::Https, "api.openai.com", 443),
        endpoint(EndpointScheme::Https, "api2.openai.com", 443),
    ];
    let b = [
        endpoint(EndpointScheme::Https, "api2.openai.com", 443),
        endpoint(EndpointScheme::Https, "api.openai.com.", 443),
    ];
    assert!(endpoints_equal(&a, &b));
    let c = [endpoint(EndpointScheme::Https, "api.openai.com", 8_443)];
    assert!(!endpoints_equal(&a, &c));
}
