//! Tests for the store's tenancy rules.

use uuid::Uuid;

use crate::domain::upstream::{Endpoint, Server, Upstream};
use crate::store::{OagwStore, TenantChain};

fn endpoint(host: &str) -> Endpoint {
    Endpoint {
        scheme: "https".to_owned(),
        host: host.to_owned(),
        port: 443,
    }
}

fn upstream(host: &str) -> Upstream {
    Upstream {
        enabled: true,
        server: Server {
            endpoints: vec![endpoint(host)],
            ..Server::default()
        },
        ..Upstream::default()
    }
}

/// A root tenant with one descendant: the chain the descendant resolves with.
fn hierarchy() -> (Uuid, Uuid) {
    (Uuid::new_v4(), Uuid::new_v4())
}

/// The descendant's view of the world: itself first, then its ancestors.
fn descendant_chain(own: Uuid, root: Uuid) -> TenantChain {
    TenantChain::new(vec![own, root])
}

/// A descendant reads an ancestor's upstream — the sharing model the proxy relies on.
#[test]
fn a_descendant_reads_but_does_not_own_an_ancestors_upstream() {
    let (root, child) = hierarchy();
    let store = OagwStore::new();
    let created = store
        .insert_upstream(upstream("api.example.com"), root)
        .expect("the ancestor's upstream is created");

    let chain = descendant_chain(child, root);
    assert!(
        store.get_upstream(&created.id, &chain).is_some(),
        "an ancestor's upstream is visible to the descendant"
    );
    assert!(
        store.replace_upstream(&created.id, upstream("api.example.com"), &chain).is_err(),
        "a descendant may not replace an ancestor's upstream"
    );
    assert!(store.delete_upstream(&created.id, &chain).is_err());
}

/// The rule FR-010 names: a resource the ancestor disabled stays disabled, because the
/// descendant cannot flip the flag on a row it does not own.
#[test]
fn an_ancestor_disabled_upstream_cannot_be_re_enabled_by_a_descendant() {
    let (root, child) = hierarchy();
    let store = OagwStore::new();
    let created = store
        .insert_upstream(upstream("api.example.com"), root)
        .expect("the ancestor's upstream is created");

    let mut disabled = upstream("api.example.com");
    disabled.enabled = false;
    store
        .replace_upstream(&created.id, disabled, &TenantChain::single(root))
        .expect("the ancestor disables its own upstream");

    let chain = descendant_chain(child, root);
    let replacement = store.replace_upstream(&created.id, upstream("api.example.com"), &chain);
    assert!(
        replacement.is_err(),
        "a descendant must not re-enable an ancestor-disabled upstream"
    );
    assert!(
        !store.get_upstream(&created.id, &chain).expect("still visible").enabled,
        "the ancestor's disabling stands"
    );
}

/// The owner is unaffected: the same call the descendant was refused succeeds.
#[test]
fn the_owner_still_replaces_its_own_upstream() {
    let (root, child) = hierarchy();
    let store = OagwStore::new();
    let created = store
        .insert_upstream(upstream("api.example.com"), root)
        .expect("created");

    let chain = descendant_chain(child, root);
    assert!(
        store.get_upstream(&created.id, &chain).is_some(),
        "visible to the descendant"
    );
    let replaced = store.replace_upstream(
        &created.id,
        upstream("other.example.com"),
        &TenantChain::single(root),
    );
    assert!(replaced.is_ok(), "the owner writes its own row");
}

/// A resource outside the caller's chain stays indistinguishable from a missing one.
#[test]
fn a_stranger_tenants_row_is_not_found_rather_than_forbidden() {
    let (root, child) = hierarchy();
    let store = OagwStore::new();
    let created = store
        .insert_upstream(upstream("api.example.com"), root)
        .expect("created");

    let outsider = TenantChain::single(Uuid::new_v4());
    assert!(store.get_upstream(&created.id, &outsider).is_none());
    assert!(store.replace_upstream(&created.id, upstream("api.example.com"), &outsider).is_err());
    assert!(store.delete_upstream(&created.id, &outsider).is_err());
    assert!(
        store.get_upstream(&created.id, &descendant_chain(child, root)).is_some(),
        "the chain still resolves it"
    );
}
