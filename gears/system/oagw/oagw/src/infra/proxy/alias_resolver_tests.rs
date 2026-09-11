//! Tests of the tenant-hierarchy alias walk
//! (`cpt-cf-oagw-flow-request-proxy-alias-resolution`,
//! `cpt-cf-oagw-algo-request-proxy-alias-resolve`).
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-alias-resolution:p1
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-disabled-upstream:p1
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-unit-tests:p1

use crate::infra::proxy::alias_resolver::{ancestor_layers, resolve, AliasWalk};
use crate::domain::alias::normalize_alias;
use crate::domain::error::DomainError;
use crate::domain::repo::{UpstreamRecord, UpstreamRepository};
use crate::domain::services::management::{Actor, AncestorResolver};
use std::sync::Arc;
use uuid::Uuid;

use crate::infra::authorization::TenantHierarchyAncestors;
use crate::infra::storage::Storage;
use crate::test_support::{upstream as an_upstream, FakeHierarchyTenantResolver};

fn leaf_upstream(tenant: Uuid, alias: &str) -> UpstreamRecord {
    UpstreamRecord { upstream: an_upstream(tenant, alias), plugin_bindings: Vec::new() }
}

fn root_upstream(tenant: Uuid, alias: &str, enabled: bool) -> UpstreamRecord {
    let mut record = an_upstream(tenant, alias);
    record.enabled = enabled;
    UpstreamRecord { upstream: record, plugin_bindings: Vec::new() }
}

/// A store holding upstreams per tenant, with the ancestor resolver over the
/// chain `leaf -> parent -> root`.
struct Fixture {
    upstreams: Arc<dyn UpstreamRepository>,
    ancestors: Arc<dyn AncestorResolver>,
    leaf: Uuid,
    parent: Uuid,
    root: Uuid,
}

impl Default for Fixture {
    fn default() -> Self {
        let storage = Storage::new();
        let (upstreams, _, _) = storage.repositories();
        let leaf = Uuid::new_v4();
        let parent = Uuid::new_v4();
        let root = Uuid::new_v4();
        Self {
            upstreams,
            ancestors: Arc::new(TenantHierarchyAncestors::new(FakeHierarchyTenantResolver::over(&[
                leaf, parent, root,
            ]))),
            leaf,
            parent,
            root,
        }
    }
}

async fn walk(fixture: &Fixture, requested: &str) -> Result<AliasWalk, DomainError> {
    let actor = Actor { tenant_id: fixture.leaf, principal_id: Uuid::new_v4() };
    resolve(&fixture.upstreams, &fixture.ancestors, &actor, requested).await
}

#[tokio::test]
async fn an_alias_of_the_calling_tenant_is_selected_at_distance_zero() {
    let fixture = Fixture::default();
    fixture
        .upstreams
        .create(fixture.leaf, leaf_upstream(fixture.leaf, "api.vendor.com"))
        .expect("created");
    let walk = walk(&fixture, "api.vendor.com").await.expect("resolved");
    assert_eq!(walk.selected, 0);
    assert_eq!(walk.levels.len(), 3);
    assert_eq!(walk.upstream().upstream.alias, "api.vendor.com");
}

#[test]
fn an_alias_is_normalized_case_insensitively_without_a_trailing_dot() {
    assert_eq!(normalize_alias("Api.OpenAI.COM."), "api.openai.com");
}

#[tokio::test]
async fn an_alias_is_resolved_case_insensitively_and_without_a_trailing_dot() {
    let fixture = Fixture::default();
    fixture
        .upstreams
        .create(fixture.leaf, leaf_upstream(fixture.leaf, "api.vendor.com"))
        .expect("created");
    let walk = walk(&fixture, "API.Vendor.COM.").await.expect("resolved");
    assert_eq!(walk.upstream().upstream.alias, "api.vendor.com");
}

#[tokio::test]
async fn a_descendant_shadows_the_ancestor_upstream_but_the_walk_continues() {
    let fixture = Fixture::default();
    fixture
        .upstreams
        .create(fixture.leaf, leaf_upstream(fixture.leaf, "api.vendor.com"))
        .expect("created");
    fixture
        .upstreams
        .create(fixture.root, root_upstream(fixture.root, "api.vendor.com", true))
        .expect("created");

    let walk = walk(&fixture, "api.vendor.com").await.expect("resolved");
    assert_eq!(walk.selected, 0);
    let shadowed: Vec<Uuid> = walk.shadowed().filter_map(|level| level.record.as_ref()).map(|record| record.upstream.tenant_id).collect();
    assert_eq!(shadowed, vec![fixture.root]);
    assert!(!walk.any_disabled());
}

#[tokio::test]
async fn a_disabled_ancestor_upstream_disables_the_alias_for_the_descendant() {
    let fixture = Fixture::default();
    fixture
        .upstreams
        .create(fixture.leaf, leaf_upstream(fixture.leaf, "api.vendor.com"))
        .expect("created");
    fixture
        .upstreams
        .create(fixture.root, root_upstream(fixture.root, "api.vendor.com", false))
        .expect("created");

    let walk = walk(&fixture, "api.vendor.com").await.expect("resolved");
    assert_eq!(walk.selected, 0);
    assert!(walk.any_disabled());
}

#[tokio::test]
async fn an_alias_only_an_ancestor_holds_resolves_to_the_ancestor() {
    let fixture = Fixture::default();
    fixture
        .upstreams
        .create(fixture.root, root_upstream(fixture.root, "api.vendor.com", true))
        .expect("created");

    let walk = walk(&fixture, "api.vendor.com").await.expect("resolved");
    assert_eq!(walk.selected, 2);
    assert_eq!(walk.levels[walk.selected].distance, 2);
    assert_eq!(walk.levels[walk.selected].tenant_id, fixture.root);
}

#[tokio::test]
async fn an_alias_no_level_holds_is_not_found() {
    let fixture = Fixture::default();
    let error = walk(&fixture, "api.vendor.com").await.expect_err("not found");
    assert!(matches!(error, DomainError::NotFound { .. }));
}

#[tokio::test]
async fn an_empty_alias_is_not_found() {
    let fixture = Fixture::default();
    let error = walk(&fixture, "").await.expect_err("not found");
    assert!(matches!(error, DomainError::NotFound { .. }));
}

#[tokio::test]
async fn the_ancestor_layers_are_ordered_root_first() {
    let fixture = Fixture::default();
    fixture
        .upstreams
        .create(fixture.leaf, leaf_upstream(fixture.leaf, "api.vendor.com"))
        .expect("created");
    fixture
        .upstreams
        .create(fixture.parent, leaf_upstream(fixture.parent, "api.vendor.com"))
        .expect("created");
    fixture
        .upstreams
        .create(fixture.root, leaf_upstream(fixture.root, "api.vendor.com"))
        .expect("created");

    let walk = walk(&fixture, "api.vendor.com").await.expect("resolved");
    let layers = ancestor_layers(&walk);
    assert_eq!(layers.len(), 2);
    assert_eq!(layers[0].upstream.tenant_id, fixture.root);
    assert_eq!(layers[1].upstream.tenant_id, fixture.parent);
}
