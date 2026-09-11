//! Unit tests for alias derivation and immutability.
// @cpt-dod:cpt-cf-oagw-dod-upstream-management-alias-derivation:p1
// @cpt-dod:cpt-cf-oagw-dod-upstream-management-unit-tests:p1

use super::*;
use crate::domain::dto::{Endpoint, EndpointScheme, ServerConfig, Upstream};
use crate::domain::gts_helpers::PROTOCOL_HTTP;

fn ep(scheme: EndpointScheme, host: &str, port: u16) -> Endpoint {
    Endpoint { scheme, host: host.to_owned(), port }
}

fn https(host: &str) -> Endpoint {
    ep(EndpointScheme::Https, host, 443)
}

fn server(endpoints: Vec<Endpoint>) -> ServerConfig {
    ServerConfig { endpoints }
}

/// The `DomainError::detail` text of a rejection naming `alias`.
fn rejected(reason: &str) -> String {
    format!("field `alias` rejected: {reason}")
}

/// An upstream record over `endpoints`, with `alias` (empty = not supplied).
fn upstream(alias: &str, endpoints: Vec<Endpoint>) -> Upstream {
    Upstream {
        id: uuid::Uuid::new_v4(),
        tenant_id: uuid::Uuid::nil(),
        alias: alias.to_owned(),
        protocol: PROTOCOL_HTTP.to_owned(),
        enabled: true,
        server: server(endpoints),
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec![],
    }
}

#[test]
fn standard_port_follows_the_scheme() {
    assert_eq!(standard_port(EndpointScheme::Http), 80);
    assert_eq!(standard_port(EndpointScheme::Https), 443);
    assert_eq!(standard_port(EndpointScheme::Wss), 443);
    assert_eq!(standard_port(EndpointScheme::Wt), 443);
    assert_eq!(standard_port(EndpointScheme::Grpc), 443);
}

#[test]
fn single_https_host_on_the_standard_port_yields_the_hostname() {
    // Acceptance criterion: single `https` host on 443 -> hostname.
    let alias = derive_alias(&server(vec![https("api.vendor.com")])).expect("derivable");
    assert_eq!(alias, "api.vendor.com");
}

#[test]
fn single_host_on_a_nonstandard_port_yields_hostname_and_port() {
    // Acceptance criterion: single host on 8443 -> `hostname:8443`.
    let alias = derive_alias(&server(vec![https("api.vendor.com:8443")])).expect("derivable");
    assert_eq!(alias, "api.vendor.com:8443");
    let alias = derive_alias(&server(vec![ep(EndpointScheme::Http, "api.vendor.com", 8080)])).expect("derivable");
    assert_eq!(alias, "api.vendor.com:8080");
    // The standard `http` port derives the bare hostname.
    let alias = derive_alias(&server(vec![ep(EndpointScheme::Http, "API.vendor.com.", 80)])).expect("derivable");
    assert_eq!(alias, "api.vendor.com");
}

#[test]
fn two_hostnames_with_a_common_registrable_suffix_yield_the_suffix() {
    // Acceptance criterion: `us.vendor.com` + `eu.vendor.com` -> `vendor.com`.
    let alias =
        derive_alias(&server(vec![https("us.vendor.com"), https("eu.vendor.com")])).expect("derivable");
    assert_eq!(alias, "vendor.com");
    // With a non-standard pool port -> `suffix:port`.
    let alias = derive_alias(&server(vec![
        https("us.vendor.com:8443"),
        https("eu.vendor.com:8443"),
    ]))
    .expect("derivable");
    assert_eq!(alias, "vendor.com:8443");
}

#[test]
fn a_bare_public_suffix_is_not_a_derivable_alias() {
    // Acceptance criterion: `foo.co.uk` + `bar.co.uk` -> nothing, because the
    // only common suffix is the bare public suffix `co.uk`.
    let err = derive_alias(&server(vec![https("foo.co.uk"), https("bar.co.uk")]))
        .expect_err("bare public suffix");
    assert_eq!(
        err.detail(),
        Some(rejected(AliasDerivationError::BarePublicSuffix.reason()).as_str())
    );
    assert!(err.to_string().contains("bare public suffix"));
    // And the host itself is a bare public suffix.
    assert_eq!(registrable_domain("co.uk"), None);
    assert_eq!(registrable_domain("foo.co.uk"), Some("foo.co.uk".to_owned()));
    assert_eq!(registrable_domain("us.vendor.com"), Some("vendor.com".to_owned()));
}

#[test]
fn hostnames_without_a_common_registrable_suffix_are_not_derivable() {
    let err = derive_alias(&server(vec![https("us.vendor.com"), https("api.other.org")]))
        .expect_err("no common suffix");
    assert_eq!(
        err.detail(),
        Some(rejected(AliasDerivationError::NoCommonSuffix.reason()).as_str())
    );
}

#[test]
fn an_ip_endpoint_pool_is_never_derivable() {
    let err = derive_alias(&server(vec![https("10.0.0.1")])).expect_err("IP host");
    assert_eq!(
        err.detail(),
        Some(rejected(AliasDerivationError::NoHostnameEndpoint.reason()).as_str())
    );
    let err = derive_alias(&server(vec![https("10.0.0.1"), https("10.0.0.2")]))
        .expect_err("IP hosts");
    assert_eq!(
        err.detail(),
        Some(rejected(AliasDerivationError::NoHostnameEndpoint.reason()).as_str())
    );
}

#[test]
fn an_empty_pool_is_rejected() {
    let err = derive_alias(&server(vec![])).expect_err("empty pool");
    assert_eq!(
        err.detail(),
        Some(rejected(AliasDerivationError::NoEndpoints.reason()).as_str())
    );
}

#[test]
fn a_single_trailing_dot_is_tolerated_fqdn_notation() {
    let alias = derive_alias(&server(vec![https("us.vendor.com."), https("eu.vendor.com.")]))
        .expect("derivable");
    assert_eq!(alias, "vendor.com");
}

#[test]
fn derived_alias_is_normalized_and_pattern_checked() {
    // Step 1 normalization: ASCII lowercase, trailing FQDN dot stripped.
    let alias = derive_alias(&server(vec![https("US.Vendor.Com.")])).expect("derivable");
    assert_eq!(alias, "us.vendor.com");
    assert!(alias_is_valid(&alias));
    // Step 6 normalization of the derived common suffix.
    let alias = derive_alias(&server(vec![https("US.Vendor.Com."), https("EU.Vendor.Com.")]))
        .expect("derivable");
    assert_eq!(alias, "vendor.com");
    assert!(alias_is_valid(&alias));
}

#[test]
fn alias_class_distinguishes_derivable_pools() {
    assert_eq!(alias_class(&server(vec![https("api.vendor.com")])), AliasClass::Derivable);
    assert_eq!(alias_class(&server(vec![https("10.0.0.1")])), AliasClass::NonDerivable);
    assert_eq!(
        alias_class(&server(vec![https("foo.co.uk"), https("bar.co.uk")])),
        AliasClass::NonDerivable
    );
}

#[test]
fn an_unchanged_pool_retains_the_stored_alias() {
    // Unchanged pool, no supplied alias -> the stored alias stands.
    let stored = upstream("api.vendor.com", vec![https("api.vendor.com")]);
    let mut replacement = upstream("", vec![https("api.vendor.com")]);
    replacement.id = stored.id;
    assert_eq!(validate_alias_replacement(&stored, &replacement).expect("ok"), "api.vendor.com");

    // Unchanged pool, equal supplied alias -> idempotent no-op.
    replacement.alias = "api.vendor.com".to_owned();
    assert_eq!(validate_alias_replacement(&stored, &replacement).expect("ok"), "api.vendor.com");

    // Unchanged pool, differing supplied alias -> rejected.
    replacement.alias = "other.vendor.com".to_owned();
    let err = validate_alias_replacement(&stored, &replacement).expect_err("differing alias");
    assert!(err.to_string().contains("immutable"), "`{err}` explains immutability");
    assert!(err.to_string().contains("override"), "`{err}` names the override");
}

#[test]
fn a_deriviable_pool_is_rejected_when_it_stops_being_deriviable() {
    // Derivable -> non-derivable: rejected always, even when the supplied
    // alias equals the stored alias.
    let stored = upstream("api.vendor.com", vec![https("api.vendor.com")]);
    let replacement = upstream("api.vendor.com", vec![https("10.0.0.1")]);
    let err = validate_alias_replacement(&stored, &replacement).expect_err("rejected always");
    assert!(err.to_string().contains("delete and re-create"), "`{err}`");
    assert!(!err.to_string().contains("10.0.0.1"), "no endpoint host is echoed");
}

#[test]
fn a_non_deriviable_pool_retains_the_stored_alias() {
    // Non-derivable -> non-derivable: the stored alias stands (an IP-based
    // upstream keeps its explicit alias; DESIGN §3.2 supersedes §3.3).
    let stored = upstream("10-0-0-1", vec![https("10.0.0.1")]);
    let replacement = upstream("", vec![https("10.0.0.2")]);
    assert_eq!(validate_alias_replacement(&stored, &replacement).expect("ok"), "10-0-0-1");
    // A differing supplied alias is rejected.
    let replacement = upstream("other", vec![https("10.0.0.2")]);
    let err = validate_alias_replacement(&stored, &replacement).expect_err("override");
    assert!(err.to_string().contains("immutable"));
}

#[test]
fn a_non_deriviable_pool_becoming_deriviable_must_match_the_stored_alias() {
    let stored = upstream("us.vendor.com", vec![https("10.0.0.1")]);
    let replacement = upstream("", vec![https("us.vendor.com")]);
    assert_eq!(validate_alias_replacement(&stored, &replacement).expect("ok"), "us.vendor.com");
    let replacement = upstream("", vec![https("eu.vendor.com")]);
    let err = validate_alias_replacement(&stored, &replacement).expect_err("derived differs");
    assert!(err.to_string().contains("delete and re-create"));
}

#[test]
fn a_deriviable_pool_must_recompute_the_same_alias() {
    let stored = upstream("vendor.com", vec![https("us.vendor.com"), https("eu.vendor.com")]);
    let replacement = upstream("", vec![https("eu.vendor.com"), https("us.vendor.com")]);
    assert_eq!(validate_alias_replacement(&stored, &replacement).expect("ok"), "vendor.com");
    let replacement = upstream("", vec![https("api.vendor.com")]);
    let err = validate_alias_replacement(&stored, &replacement).expect_err("recomputation differs");
    assert!(err.to_string().contains("delete and re-create"));
}
