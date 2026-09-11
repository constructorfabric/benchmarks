//! Effective-configuration result types.
//!
//! Covers `cpt-cf-oagw-dod-effective-config-result` and the domain half of
//! `cpt-cf-oagw-feature-hierarchical-config`: the chain the platform resolver
//! supplies and the failure modes §1.4 names, the sharing modes a row declares,
//! and the two per-layer results. The alias-identity rule of
//! `cpt-cf-oagw-dod-alias-shadowing` is asserted here on the value object the
//! walk compares with, so a candidate set can never disagree with a stored
//! alias about case, a trailing dot, or a port.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use std::net::Ipv4Addr;

use oagw::Alias;
use oagw::domain::effective::{ChainError, Family, FamilyModes, TenantChain};
use oagw::domain::upstream::SharingMode;
use uuid::Uuid;

fn tenant() -> Uuid {
    Uuid::from_u128(0xa001)
}

fn ancestor() -> Uuid {
    Uuid::from_u128(0xa002)
}

fn root() -> Uuid {
    Uuid::from_u128(0xa003)
}

fn ipv4(value: u8) -> String {
    Ipv4Addr::new(10, 0, 0, value).to_string()
}

#[test]
fn chain_prepends_the_calling_tenant_the_resolver_omitted() {
    // The resolver answers the ancestors only when the tenant is not itself the
    // first element; the walk prepends the calling tenant either way, because
    // the calling tenant's own rows must be the closest candidates.
    let chain = TenantChain::from_resolver(tenant(), &[ancestor(), root()])
        .expect("an ordered chain is available");
    assert_eq!(chain.tenants(), &[tenant(), ancestor(), root()]);
    assert_eq!(chain.calling_tenant(), tenant());
}

#[test]
fn chain_keeps_the_resolver_order_when_the_calling_tenant_is_first() {
    let chain = TenantChain::from_resolver(tenant(), &[tenant(), ancestor()])
        .expect("a chain that already starts at the calling tenant is usable as given");
    assert_eq!(chain.tenants(), &[tenant(), ancestor()]);
}

#[test]
fn chain_answers_none_for_a_cyclic_answer() {
    // A repeated element is a cycle: the walk cannot order who shadows whom,
    // so the chain is unavailable and the caller fails closed.
    let chain = TenantChain::from_resolver(tenant(), &[ancestor(), tenant(), root()]);
    assert!(chain.is_none(), "a cycle is an unavailable chain");
}

#[test]
fn chain_answers_none_for_an_unordered_answer() {
    // A chain whose calling tenant is not its first element and cannot be
    // prepended without repeating an element is unordered, not merely rotated.
    let chain = TenantChain::from_resolver(tenant(), &[ancestor(), tenant()]);
    assert!(chain.is_none(), "an unordered chain is an unavailable chain");
}

#[test]
fn chain_answers_the_only_element_when_the_tenant_is_the_root() {
    let chain = TenantChain::from_resolver(root(), &[]).expect("the root is a chain of one");
    assert_eq!(chain.tenants(), &[root()]);
    assert_eq!(chain.depth_of(root()), Some(0));
    assert_eq!(chain.depth_of(tenant()), None);
}

#[test]
fn chain_depth_runs_from_the_calling_tenant_to_the_root() {
    let chain = TenantChain::from_resolver(tenant(), &[ancestor(), root()])
        .expect("an ordered chain is available");
    assert_eq!(chain.depth_of(tenant()), Some(0));
    assert_eq!(chain.depth_of(ancestor()), Some(1));
    assert_eq!(chain.depth_of(root()), Some(2));
    assert_eq!(chain.depth_of(Uuid::nil()), None, "no lookup is issued for a tenant outside the chain");
}

#[test]
fn chain_from_ordered_keeps_the_order_it_is_given_and_rejects_the_rest() {
    // The management flows build the chain from an ordered answer they already
    // hold; the constructor validates it rather than reordering it.
    let chain = TenantChain::from_ordered(vec![tenant(), ancestor(), root()])
        .expect("an ordered chain is available");
    assert_eq!(chain.tenants(), &[tenant(), ancestor(), root()]);
    assert_eq!(TenantChain::from_ordered(Vec::new()), Err(ChainError::Empty));
    assert_eq!(
        TenantChain::from_ordered(vec![tenant(), ancestor(), tenant()]),
        Err(ChainError::Cyclic)
    );
}

#[test]
fn family_modes_default_every_family_to_private() {
    // Every `sharing` member of the shipped schemas defaults to `private`, so
    // a row that declares none of them contributes nothing to any descendant.
    let modes = FamilyModes::new(None, None, None, None);
    assert_eq!(modes.mode_of(Family::Auth), SharingMode::Private);
    assert_eq!(modes.mode_of(Family::RateLimit), SharingMode::Private);
    assert_eq!(modes.mode_of(Family::Plugins), SharingMode::Private);
    assert_eq!(modes.mode_of(Family::Cors), SharingMode::Private);
}

#[test]
fn alias_identity_keeps_the_port_and_drops_the_case_and_the_trailing_dot() {
    // `cpt-cf-oagw-dod-alias-shadowing`: aliases compare on the normalized
    // form only, case-insensitively, with the port participating in identity.
    let bare = Alias::parse("api.openai.com").expect("a bare host is a valid alias");
    let dotted = Alias::parse("API.OpenAI.com.").expect("the trailing dot is normalized away");
    let ported = Alias::parse("api.openai.com:8443").expect("a port is a valid alias suffix");

    assert_eq!(bare, dotted, "case and a trailing dot are not part of identity");
    assert_ne!(bare, ported, "the port participates in identity");
    assert_eq!(bare.to_string(), "api.openai.com");
    assert_eq!(ported.to_string(), "api.openai.com:8443");
}

#[test]
fn endpoint_hosts_are_normalized_without_losing_the_ip_literal_form() {
    let host = oagw::EndpointHost::parse(&ipv4(7)).expect("an IPv4 literal is a valid endpoint host");
    assert_eq!(host.as_str(), ipv4(7));
    let named = oagw::EndpointHost::parse("Upstream.Example.COM.");
    assert_eq!(named.expect("an RFC 1123 name is a valid endpoint host").as_str(), "upstream.example.com");
}
