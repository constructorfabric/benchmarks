//! Tests of alias derivation, validation, uniqueness and shadowing.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(coverage_nightly, coverage(off))]

use uuid::uuid;

use super::*;
use crate::domain::upstream::Endpoint;
use crate::error::OagwError;

const TENANT: uuid::Uuid = uuid!("00000000-0000-0000-0000-0000000000a1");
const CHILD: uuid::Uuid = uuid!("00000000-0000-0000-0000-0000000000a2");

fn https(host: &str) -> Endpoint {
    Endpoint::new(EndpointScheme::Https, host, None).unwrap()
}

fn https_port(host: &str, port: u16) -> Endpoint {
    Endpoint::new(EndpointScheme::Https, host, Some(port)).unwrap()
}

#[test]
fn single_hostname_derives_the_alias() {
    let pool = [https("api.openai.com")];
    let derived = derive_alias(&pool).unwrap();
    let AliasDerivation::Derived(alias) = derived else {
        panic!("hostname-based pool must derive its alias");
    };
    assert_eq!(alias.as_str(), "api.openai.com");
    assert_eq!(alias.host(), "api.openai.com");
    assert_eq!(alias.port(), None);
}

#[test]
fn non_standard_port_is_part_of_the_derived_alias() {
    let pool = [https_port("api.openai.com", 8443)];
    let AliasDerivation::Derived(alias) = derive_alias(&pool).unwrap() else {
        panic!("derivation must succeed");
    };
    assert_eq!(alias.as_str(), "api.openai.com:8443");
    assert_eq!(alias.port(), Some(8443));
    assert_eq!(alias.host(), "api.openai.com");
}

#[test]
fn standard_ports_are_omitted_from_the_derived_alias() {
    let pool = [Endpoint::new(EndpointScheme::Http, "api.example.com", Some(80)).unwrap()];
    let AliasDerivation::Derived(alias) = derive_alias(&pool).unwrap() else {
        panic!("derivation must succeed");
    };
    assert_eq!(alias.as_str(), "api.example.com");
}

#[test]
fn multi_endpoint_pool_derives_the_common_suffix() {
    let pool = [https("us.vendor.com"), https("eu.vendor.com")];
    let AliasDerivation::Derived(alias) = derive_alias(&pool).unwrap() else {
        panic!("derivation must succeed");
    };
    assert_eq!(alias.as_str(), "vendor.com");
}

#[test]
fn multi_endpoint_pool_with_non_standard_port_keeps_the_port() {
    let pool = [
        https_port("us.vendor.com", 8443),
        https_port("eu.vendor.com", 8443),
    ];
    let AliasDerivation::Derived(alias) = derive_alias(&pool).unwrap() else {
        panic!("derivation must succeed");
    };
    assert_eq!(alias.as_str(), "vendor.com:8443");
}

#[test]
fn a_bare_public_suffix_is_never_derived() {
    // `co.uk` is a public suffix: the pool is non-derivable and an explicit
    // alias is required (`docs/PRD.md` §5.5).
    let pool = [https("foo.co.uk"), https("bar.co.uk")];
    let derived = derive_alias(&pool).unwrap();
    assert_eq!(derived, AliasDerivation::ExplicitRequired);
}

#[test]
fn unrelated_hostnames_require_an_explicit_alias() {
    let pool = [https("us.foo.com"), https("eu.bar.com")];
    assert_eq!(
        derive_alias(&pool).unwrap(),
        AliasDerivation::ExplicitRequired
    );
}

#[test]
fn ip_endpoints_require_an_explicit_alias() {
    let pool = [
        Endpoint::new(EndpointScheme::Https, "10.0.1.1", None).unwrap(),
        Endpoint::new(EndpointScheme::Https, "10.0.1.2", None).unwrap(),
    ];
    assert_eq!(
        derive_alias(&pool).unwrap(),
        AliasDerivation::ExplicitRequired
    );
}

#[test]
fn mixed_ports_require_an_explicit_alias() {
    let pool = [
        https_port("a.vendor.com", 8443),
        https_port("b.vendor.com", 9000),
    ];
    assert_eq!(
        derive_alias(&pool).unwrap(),
        AliasDerivation::ExplicitRequired
    );
}

#[test]
fn an_empty_pool_cannot_derive_an_alias() {
    assert!(derive_alias(&[]).is_err());
}

#[test]
fn aliases_are_normalized_to_lowercase_without_trailing_dots() {
    let alias = Alias::parse("Api.OpenAI.COM.").unwrap();
    assert_eq!(alias.as_str(), "api.openai.com");
    assert_eq!(
        Alias::parse("my-internal-service").unwrap().as_str(),
        "my-internal-service"
    );
}

#[test]
fn resolution_is_case_insensitive() {
    let left = Alias::parse("Api.OpenAI.COM").unwrap();
    let right = Alias::parse("api.openai.com").unwrap();
    assert_eq!(left, right);
    assert_eq!(left.as_ref(), "api.openai.com");
}

#[test]
fn invalid_aliases_are_rejected() {
    let invalid = [
        "",
        "   ",
        "-leading.example.com",
        "under_score.example.com",
        "api.example.com:0",
        "api.example.com:99999",
        "api.example.com:8443:1",
        "api example.com",
    ];
    for value in invalid {
        assert!(Alias::parse(value).is_err(), "'{value}' must be rejected");
    }
}

#[test]
fn an_alias_may_not_exceed_the_rfc_1035_bound() {
    let long = format!("{}{}", "a".repeat(250), ".example.com");
    assert!(Alias::parse(&long).is_err());
    let ok = format!("{}{}", "a".repeat(200), ".example.com");
    assert!(Alias::parse(&ok).is_ok());
}

#[test]
fn a_bare_public_suffix_is_rejected_as_an_explicit_alias() {
    let err = Alias::parse("co.uk").unwrap_err();
    assert!(
        matches!(err, OagwError::AliasShadowsPublicSuffix { .. }),
        "{err}"
    );
    // A registrable domain in front of the suffix is fine.
    assert!(Alias::parse("vendor.co.uk").is_ok());
    // Single-label aliases are not hostnames and cannot shadow a suffix.
    assert!(Alias::parse("my-internal-service").is_ok());
}

#[test]
fn hostname_pools_accept_the_derived_alias_only() {
    let pool = [https("api.openai.com")];
    let derived = derive_alias(&pool).unwrap();
    let AliasDerivation::Derived(alias) = derived else {
        panic!("derivation must succeed");
    };
    // Idempotent no-op: the exact derived value is tolerated.
    assert!(resolve_alias(&pool, Some(&alias)).is_ok());
    // Any other value is rejected.
    let other = Alias::parse("other.openai.com").unwrap();
    let err = resolve_alias(&pool, Some(&other)).unwrap_err();
    assert_eq!(err.http_status(), 400);
    // Omitting the alias auto-derives it.
    assert_eq!(
        resolve_alias(&pool, None).unwrap().as_str(),
        "api.openai.com"
    );
}

#[test]
fn ip_pools_require_the_explicit_alias() {
    let pool = [Endpoint::new(EndpointScheme::Https, "10.0.1.1", None).unwrap()];
    assert!(resolve_alias(&pool, None).is_err());
    let explicit = Alias::parse("my-service").unwrap();
    assert_eq!(resolve_alias(&pool, Some(&explicit)).unwrap(), explicit);
}

#[test]
fn aliases_are_unique_per_tenant() {
    let upstream_a = uuid!("00000000-0000-0000-0000-0000000000b1");
    let alias = Alias::parse("api.example.com").unwrap();
    let registrations = vec![AliasRegistration::new(TENANT, upstream_a, alias.clone())];
    // Same tenant: conflict.
    let err = ensure_alias_unique(&alias.key_for(TENANT), &registrations).unwrap_err();
    assert_eq!(err.http_status(), 409);
    assert!(
        err.to_string()
            .contains("00000000-0000-0000-0000-0000000000b1")
    );
    // Descendant tenant: no conflict, it shadows instead.
    assert!(ensure_alias_unique(&alias.key_for(CHILD), &registrations).is_ok());
}

#[test]
fn descendants_shadow_ancestor_aliases() {
    let ancestor = Alias::parse("api.example.com").unwrap();
    let shadow = Alias::parse("api.example.com:8443").unwrap();
    let scopes = [
        TenantAliasScope {
            tenant_id: CHILD,
            aliases: vec![shadow.clone()],
        },
        TenantAliasScope {
            tenant_id: TENANT,
            aliases: vec![ancestor.clone()],
        },
    ];
    let (owner, matched) = resolve_shadowing(&Alias::parse("api.example.com").unwrap(), &scopes)
        .expect("the ancestor alias must resolve");
    assert_eq!(owner, TENANT);
    assert_eq!(*matched, ancestor);
    let (owner, matched) = resolve_shadowing(&shadow, &scopes).expect("the shadow must resolve");
    assert_eq!(owner, CHILD);
    assert_eq!(*matched, shadow);
}

#[test]
fn an_unknown_alias_resolves_to_nothing() {
    let scopes = [TenantAliasScope {
        tenant_id: TENANT,
        aliases: vec![Alias::parse("api.example.com").unwrap()],
    }];
    let unknown = Alias::parse("unknown.example.com").unwrap();
    assert!(resolve_shadowing(&unknown, &scopes).is_none());
}

#[test]
fn alias_keys_bind_a_tenant_to_an_alias() {
    let alias = Alias::parse("api.example.com").unwrap();
    let key = alias.key_for(TENANT);
    assert_eq!(key.tenant_id(), TENANT);
    assert_eq!(key.alias(), &alias);
    let registration =
        AliasRegistration::new(TENANT, uuid!("00000000-0000-0000-0000-0000000000c1"), alias);
    assert_eq!(registration.key(), key);
}
