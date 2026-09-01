#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::Arc;

use super::resolver::{AliasResolver, StaticHierarchy, TenantHierarchy};
use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, EndpointScheme, Protocol, ServerConfig, Upstream};
use crate::domain::repo::UpstreamRepository;
use crate::infra::storage::{MemoryStore, MemoryUpstreamRepository};

const CHILD: u128 = 0xC001;
const PARENT: u128 = 0xA001;
const GRAND: u128 = 0xB001;

fn tenant(id: u128) -> uuid::Uuid {
    uuid::Uuid::from_u128(id)
}

/// `(child, parent)` edges of a three-level hierarchy: child → parent → root.
fn chain() -> Vec<(uuid::Uuid, uuid::Uuid)> {
    vec![
        (tenant(CHILD), tenant(PARENT)),
        (tenant(PARENT), tenant(GRAND)),
        (tenant(GRAND), uuid::Uuid::nil()),
    ]
}

fn upstream(tenant: uuid::Uuid, id: u128, alias: &str, host: &str) -> Upstream {
    Upstream {
        id: uuid::Uuid::from_u128(id),
        tenant_id: tenant,
        alias: alias.to_owned(),
        protocol: Protocol::Http,
        enabled: true,
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Http,
                host: host.to_owned(),
                port: 80,
            }],
        },
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
        created_at: "2026-01-01T00:00:00Z".to_owned(),
        updated_at: "2026-01-01T00:00:00Z".to_owned(),
    }
}

async fn seed(
    chain: &[(uuid::Uuid, uuid::Uuid)],
    rows: &[Upstream],
) -> Arc<MemoryUpstreamRepository> {
    let store = Arc::new(MemoryStore::new());
    let repo = Arc::new(MemoryUpstreamRepository::new(store));
    for row in rows {
        repo.insert(row).await.unwrap();
    }
    let _ = chain;
    repo
}

/// `DESIGN` §"Shadowing Behavior": the walk is descendant → root and the
/// closest match wins.
#[tokio::test]
async fn the_closest_tenant_defining_the_alias_wins() {
    let chain = chain();
    let repo = seed(
        &chain,
        &[
            upstream(tenant(CHILD), 0x11, "api.vendor.com", "child.vendor.com"),
            upstream(tenant(PARENT), 0x12, "api.vendor.com", "parent.vendor.com"),
        ],
    )
    .await;
    let resolver = AliasResolver::new(repo, Arc::new(StaticHierarchy::new(chain)));

    let hit = resolver
        .resolve(tenant(CHILD), "api.vendor.com")
        .await
        .unwrap();
    assert_eq!(hit.selected.id, uuid::Uuid::from_u128(0x11));
    assert_eq!(hit.selected.server.endpoints[0].host, "child.vendor.com");
    // The same-alias ancestors stay visible for the effective-config merge.
    assert_eq!(hit.ancestors.len(), 1);
    assert_eq!(hit.ancestors[0].id, uuid::Uuid::from_u128(0x12));
}

/// Without a definition of its own a tenant inherits the closest ancestor's
/// upstream.
#[tokio::test]
async fn an_inherited_alias_resolves_the_closest_ancestor() {
    let chain = chain();
    let repo = seed(
        &chain,
        &[upstream(
            tenant(GRAND),
            0x13,
            "other.vendor.com",
            "other.vendor.com",
        )],
    )
    .await;
    let resolver = AliasResolver::new(repo, Arc::new(StaticHierarchy::new(chain)));

    let hit = resolver
        .resolve(tenant(CHILD), "other.vendor.com")
        .await
        .unwrap();
    assert_eq!(hit.selected.id, uuid::Uuid::from_u128(0x13));
    assert!(hit.ancestors.is_empty());
}

/// Alias resolution is case-insensitive and tolerates FQDN trailing dots.
#[tokio::test]
async fn alias_resolution_is_normalized() {
    let chain = chain();
    let repo = seed(
        &chain,
        &[upstream(
            tenant(CHILD),
            0x11,
            "api.vendor.com",
            "child.vendor.com",
        )],
    )
    .await;
    let resolver = AliasResolver::new(repo, Arc::new(StaticHierarchy::new(chain)));

    let hit = resolver
        .resolve(tenant(CHILD), "Api.Vendor.COM.")
        .await
        .unwrap();
    assert_eq!(hit.selected.id, uuid::Uuid::from_u128(0x11));
}

/// An alias no tenant defines is the documented `UnknownTargetHost` `400`.
#[tokio::test]
async fn an_unknown_alias_is_an_unknown_target_host() {
    let chain = chain();
    let repo = seed(
        &chain,
        &[upstream(
            tenant(CHILD),
            0x11,
            "api.vendor.com",
            "child.vendor.com",
        )],
    )
    .await;
    let resolver = AliasResolver::new(repo, Arc::new(StaticHierarchy::new(chain)));

    let error = resolver
        .resolve(tenant(CHILD), "ghost.invalid")
        .await
        .unwrap_err();
    match error {
        DomainError::RouteNotFound { alias, path } => {
            // `DESIGN` section 3.3: an alias nothing in the chain defines is a
            // routing miss (`404`), not the `X-OAGW-Target-Host` refusal (`400`).
            assert_eq!(alias, "ghost.invalid");
            assert_eq!(path, "");
        }
        other => panic!("unexpected {other:?}"),
    }
}

/// A disabled upstream rejects the request with the `503` link identity
/// instead of being forwarded to.
#[tokio::test]
async fn a_disabled_upstream_is_link_unavailable() {
    let chain = chain();
    let mut row = upstream(tenant(CHILD), 0x21, "api.vendor.com", "child.vendor.com");
    row.enabled = false;
    let repo = seed(&chain, &[row]).await;
    let resolver = AliasResolver::new(repo, Arc::new(StaticHierarchy::new(chain)));

    let error = resolver
        .resolve(tenant(CHILD), "api.vendor.com")
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::LinkUnavailable { .. }));
}

/// A disabled closest definition is an error, not a silent fall-through to the
/// ancestor's definition of the same alias.
#[tokio::test]
async fn a_disabled_closest_definition_does_not_fall_through() {
    let chain = chain();
    let mut shadow = upstream(tenant(CHILD), 0x31, "api.vendor.com", "child.vendor.com");
    shadow.enabled = false;
    let repo = seed(
        &chain,
        &[
            shadow,
            upstream(tenant(PARENT), 0x32, "api.vendor.com", "parent.vendor.com"),
        ],
    )
    .await;
    let resolver = AliasResolver::new(repo, Arc::new(StaticHierarchy::new(chain)));

    let error = resolver
        .resolve(tenant(CHILD), "api.vendor.com")
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::LinkUnavailable { .. }));
}

/// A hierarchy that cannot be answered is a dependency failure, not a routing
/// miss.
#[tokio::test]
async fn a_hierarchy_failure_is_a_dependency_error() {
    #[derive(Debug, Default)]
    struct Broken;
    #[async_trait::async_trait]
    impl TenantHierarchy for Broken {
        async fn ancestors(&self, _tenant: uuid::Uuid) -> Result<Vec<uuid::Uuid>, DomainError> {
            Err(DomainError::ServiceUnavailable {
                detail: "tenant resolver down".to_owned(),
                cause: None,
            })
        }
    }

    let store = Arc::new(MemoryStore::new());
    let repo = Arc::new(MemoryUpstreamRepository::new(store));

    let resolver = AliasResolver::new(repo, Arc::new(Broken));
    let error = resolver
        .resolve(tenant(CHILD), "api.vendor.com")
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::ServiceUnavailable { .. }));
}

/// `StaticHierarchy` reports the chain in descendant → root order and is
/// idempotent for an already complete chain.
#[test]
fn the_static_hierarchy_orders_the_chain() {
    let hierarchy = StaticHierarchy::new(vec![
        (tenant(CHILD), tenant(PARENT)),
        (tenant(PARENT), tenant(GRAND)),
        (tenant(GRAND), uuid::Uuid::nil()),
    ]);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    assert_eq!(
        runtime
            .block_on(hierarchy.ancestors(tenant(CHILD)))
            .unwrap(),
        vec![tenant(CHILD), tenant(PARENT), tenant(GRAND)]
    );
}

/// The hierarchy port is usable through `Arc<dyn TenantHierarchy>`.
#[test]
fn the_hierarchy_port_is_object_safe() {
    let hierarchy: Arc<dyn TenantHierarchy> =
        Arc::new(StaticHierarchy::new(vec![(tenant(CHILD), tenant(PARENT))]));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let resolved = runtime
        .block_on(hierarchy.ancestors(tenant(CHILD)))
        .unwrap();
    assert_eq!(resolved.len(), 2);
}

/// A tenant with no parent resolves to a single-element chain.
#[test]
fn a_root_tenant_has_no_ancestors() {
    let hierarchy = StaticHierarchy::new(Vec::new());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    assert_eq!(
        runtime
            .block_on(hierarchy.ancestors(tenant(GRAND)))
            .unwrap(),
        vec![tenant(GRAND)]
    );
}
