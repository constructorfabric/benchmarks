#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::Arc;

use super::{MemoryPluginRepository, MemoryRouteRepository, MemoryStore, MemoryUpstreamRepository};
use crate::domain::error::DomainError;
use crate::domain::model::{
    Endpoint, EndpointScheme, HttpMatch, HttpMethod, MatchConfig, PathSuffixMode, Plugin,
    PluginType, Protocol, Route, ServerConfig, Upstream,
};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};

const ROW: &str = "3f2c1b2a-1b1c-2d3e-4f50-61728394a5b6";
const TRANSFORM_ROW: &str = "4d3d2c3b-2c2d-3e4f-5f61-728394a5b6c7";

fn tenant_a() -> uuid::Uuid {
    uuid::Uuid::from_u128(0xA001)
}

fn tenant_b() -> uuid::Uuid {
    uuid::Uuid::from_u128(0xB002)
}

fn upstream_row(tenant: uuid::Uuid, id: uuid::Uuid, alias: &str) -> Upstream {
    Upstream {
        id,
        tenant_id: tenant,
        alias: alias.to_owned(),
        protocol: Protocol::Http,
        enabled: true,
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Https,
                host: "api.openai.com".to_owned(),
                port: 443,
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

fn http_match(path: &str) -> MatchConfig {
    MatchConfig {
        http: Some(HttpMatch {
            methods: vec![HttpMethod::Get],
            path: path.to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        }),
        grpc: None,
    }
}

fn route_row(
    tenant: uuid::Uuid,
    upstream: uuid::Uuid,
    id: uuid::Uuid,
    path: &str,
    priority: u32,
) -> Route {
    Route {
        id,
        tenant_id: tenant,
        upstream_id: upstream,
        r#match: http_match(path),
        priority,
        enabled: true,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
        created_at: "2026-01-01T00:00:00Z".to_owned(),
        updated_at: "2026-01-01T00:00:00Z".to_owned(),
    }
}

fn plugin_row(tenant: uuid::Uuid, kind: PluginType) -> Plugin {
    Plugin {
        // Two rows of different kinds are two different GTS ids, so the uuid
        // tail has to differ as well or the second insert is a duplicate key.
        id: match kind {
            PluginType::Guard => ROW.parse().unwrap(),
            _ => TRANSFORM_ROW.parse().unwrap(),
        },
        tenant_id: tenant,
        plugin_type: kind,
        name: "guarded".to_owned(),
        config_schema: None,
        source_code: "def apply(ctx):\n    return ctx\n".to_owned(),
        phases: Vec::new(),
        created_at: "2026-01-01T00:00:00Z".to_owned(),
        updated_at: "2026-01-01T00:00:00Z".to_owned(),
        last_used_at: None,
        gc_eligible_at: None,
    }
}

/// A fully wired set of repositories over one shared store.
///
/// The `store` handle is retained by the caller: the repositories only hold
/// `Weak`-free `Arc`s, so keeping it out of the fixture would not change
/// ownership, but naming it documents the shared-state shape.
struct Fixture {
    #[allow(dead_code)]
    store: Arc<MemoryStore>,
    upstreams: Arc<MemoryUpstreamRepository>,
    routes: Arc<MemoryRouteRepository>,
    plugins: Arc<MemoryPluginRepository>,
}

impl Fixture {
    fn new() -> Self {
        let store = Arc::new(MemoryStore::new());
        let upstreams = Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store)));
        let routes = Arc::new(MemoryRouteRepository::new(Arc::clone(&store)));
        let plugins = Arc::new(MemoryPluginRepository::new(
            Arc::clone(&store),
            Arc::clone(&upstreams) as Arc<dyn UpstreamRepository>,
            Arc::clone(&routes) as Arc<dyn RouteRepository>,
        ));
        Self {
            store,
            upstreams,
            routes,
            plugins,
        }
    }
}

// ── upstreams ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn upstream_insert_then_find_round_trips() {
    let fixture = Fixture::new();
    let tenant = tenant_a();
    let id = uuid::Uuid::from_u128(0x1);
    fixture
        .upstreams
        .insert(&upstream_row(tenant, id, "api.openai.com"))
        .await
        .unwrap();
    let found = fixture.upstreams.find(tenant, id).await.unwrap();
    assert_eq!(
        found.map(|row| row.alias),
        Some("api.openai.com".to_owned())
    );
}

#[tokio::test]
async fn upstream_alias_is_unique_within_a_tenant_only() {
    let fixture = Fixture::new();
    let first = upstream_row(tenant_a(), uuid::Uuid::from_u128(0x1), "api.openai.com");
    fixture.upstreams.insert(&first).await.unwrap();

    // Same tenant, same alias → conflict.
    let duplicate = upstream_row(tenant_a(), uuid::Uuid::from_u128(0x2), "api.openai.com");
    assert!(matches!(
        fixture.upstreams.insert(&duplicate).await,
        Err(DomainError::Conflict { .. })
    ));

    // Another tenant may route the same alias.
    let other = upstream_row(tenant_b(), uuid::Uuid::from_u128(0x3), "api.openai.com");
    assert!(fixture.upstreams.insert(&other).await.is_ok());
}

#[tokio::test]
async fn upstream_find_is_tenant_scoped() {
    let fixture = Fixture::new();
    let id = uuid::Uuid::from_u128(0x10);
    fixture
        .upstreams
        .insert(&upstream_row(tenant_a(), id, "api.openai.com"))
        .await
        .unwrap();
    // Another tenant cannot see the row at all: it is unreachable, not filtered.
    assert_eq!(fixture.upstreams.find(tenant_b(), id).await.unwrap(), None);
    assert_eq!(
        fixture
            .upstreams
            .find_by_alias(tenant_b(), "api.openai.com")
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn upstream_list_orders_by_alias() {
    let fixture = Fixture::new();
    let tenant = tenant_a();
    for (id, alias) in [(0x20, "zeta.openai.com"), (0x21, "alpha.openai.com")] {
        fixture
            .upstreams
            .insert(&upstream_row(tenant, uuid::Uuid::from_u128(id), alias))
            .await
            .unwrap();
    }
    fixture
        .upstreams
        .insert(&upstream_row(
            tenant_b(),
            uuid::Uuid::from_u128(0x22),
            "aaa.openai.com",
        ))
        .await
        .unwrap();

    let aliases: Vec<String> = fixture
        .upstreams
        .list(tenant)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.alias)
        .collect();
    assert_eq!(aliases, ["alpha.openai.com", "zeta.openai.com"]);
}

#[tokio::test]
async fn upstream_update_requires_an_existing_row() {
    let fixture = Fixture::new();
    let id = uuid::Uuid::from_u128(0x30);
    let mut row = upstream_row(tenant_a(), id, "api.openai.com");
    assert!(matches!(
        fixture.upstreams.update(&row).await,
        Err(DomainError::NotFound { .. })
    ));
    fixture.upstreams.insert(&row).await.unwrap();
    row.enabled = false;
    fixture.upstreams.update(&row).await.unwrap();
    assert!(
        !fixture
            .upstreams
            .find(tenant_a(), id)
            .await
            .unwrap()
            .unwrap()
            .enabled
    );
}

#[tokio::test]
async fn upstream_delete_is_scoped_and_idempotent() {
    let fixture = Fixture::new();
    let id = uuid::Uuid::from_u128(0x40);
    fixture
        .upstreams
        .insert(&upstream_row(tenant_a(), id, "api.openai.com"))
        .await
        .unwrap();
    assert!(!fixture.upstreams.delete(tenant_b(), id).await.unwrap());
    assert!(fixture.upstreams.delete(tenant_a(), id).await.unwrap());
    assert!(!fixture.upstreams.delete(tenant_a(), id).await.unwrap());
}

// ── routes ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn route_list_by_upstream_orders_by_priority_descending() {
    let fixture = Fixture::new();
    let tenant = tenant_a();
    let upstream = uuid::Uuid::from_u128(0x50);
    for (id, path, priority) in [(0x51, "/low", 1), (0x52, "/high", 100), (0x53, "/mid", 50)] {
        fixture
            .routes
            .insert(&route_row(
                tenant,
                upstream,
                uuid::Uuid::from_u128(id),
                path,
                priority,
            ))
            .await
            .unwrap();
    }

    let priorities: Vec<u32> = fixture
        .routes
        .list_by_upstream(tenant, upstream)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.priority)
        .collect();
    assert_eq!(priorities, [100, 50, 1]);
}

#[tokio::test]
async fn route_find_is_tenant_scoped() {
    let fixture = Fixture::new();
    let id = uuid::Uuid::from_u128(0x60);
    fixture
        .routes
        .insert(&route_row(
            tenant_a(),
            uuid::Uuid::from_u128(0x50),
            id,
            "/v1",
            1,
        ))
        .await
        .unwrap();
    assert_eq!(fixture.routes.find(tenant_b(), id).await.unwrap(), None);
    assert!(fixture.routes.find(tenant_a(), id).await.unwrap().is_some());
}

#[tokio::test]
async fn route_update_reports_a_missing_row() {
    let fixture = Fixture::new();
    let row = route_row(
        tenant_a(),
        uuid::Uuid::from_u128(0x51),
        uuid::Uuid::from_u128(0x60),
        "/v1",
        1,
    );
    assert!(matches!(
        fixture.routes.update(&row).await,
        Err(DomainError::NotFound { .. })
    ));
}

#[tokio::test]
async fn route_delete_returns_false_when_absent_or_foreign() {
    let fixture = Fixture::new();
    let id = uuid::Uuid::from_u128(0x70);
    fixture
        .routes
        .insert(&route_row(
            tenant_a(),
            uuid::Uuid::from_u128(0x50),
            id,
            "/v1",
            1,
        ))
        .await
        .unwrap();
    assert!(!fixture.routes.delete(tenant_b(), id).await.unwrap());
    assert!(fixture.routes.delete(tenant_a(), id).await.unwrap());
}

// ── plugins ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn plugin_references_reports_both_tables() {
    let fixture = Fixture::new();
    let tenant = tenant_a();
    let plugin = plugin_row(tenant, PluginType::Guard);
    fixture.plugins.insert(&plugin).await.unwrap();

    let upstream_id = uuid::Uuid::from_u128(0x80);
    let mut upstream = upstream_row(tenant, upstream_id, "api.openai.com");
    upstream.plugins = Some(crate::domain::model::PluginsConfig {
        sharing: crate::domain::model::SharingMode::Private,
        items: vec![crate::domain::model::PluginRef::Id(format!(
            "{}{}",
            crate::domain::model::GUARD_PLUGIN_TYPE,
            plugin.id
        ))],
    });
    fixture.upstreams.insert(&upstream).await.unwrap();

    let route_id = uuid::Uuid::from_u128(0x81);
    let mut route = route_row(tenant, upstream_id, route_id, "/v1", 1);
    route.plugins = Some(crate::domain::model::PluginsConfig {
        sharing: crate::domain::model::SharingMode::Private,
        items: vec![crate::domain::model::PluginRef::Id(format!(
            "{}{}",
            crate::domain::model::GUARD_PLUGIN_TYPE,
            plugin.id
        ))],
    });
    fixture.routes.insert(&route).await.unwrap();

    let (upstreams, routes) = fixture.plugins.references(tenant, plugin.id).await.unwrap();
    assert_eq!(
        upstreams,
        vec![crate::domain::model::resource_gts_id(
            crate::domain::model::UPSTREAM_TYPE,
            upstream_id
        )]
    );
    assert_eq!(
        routes,
        vec![crate::domain::model::resource_gts_id(
            crate::domain::model::ROUTE_TYPE,
            route_id
        )]
    );
}

#[tokio::test]
async fn plugin_references_are_empty_for_an_unbound_plugin() {
    let fixture = Fixture::new();
    let plugin = plugin_row(tenant_a(), PluginType::Transform);
    fixture.plugins.insert(&plugin).await.unwrap();
    let (upstreams, routes) = fixture
        .plugins
        .references(tenant_a(), plugin.id)
        .await
        .unwrap();
    assert!(upstreams.is_empty());
    assert!(routes.is_empty());
}

#[tokio::test]
async fn plugin_list_can_narrow_to_a_kind() {
    let fixture = Fixture::new();
    let tenant = tenant_a();
    let mut guard = plugin_row(tenant, PluginType::Guard);
    guard.name = "b-guard".to_owned();
    let mut transform = plugin_row(tenant, PluginType::Transform);
    transform.name = "a-transform".to_owned();
    fixture.plugins.insert(&guard).await.unwrap();
    fixture.plugins.insert(&transform).await.unwrap();

    let names: Vec<String> = fixture
        .plugins
        .list(tenant, None)
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.name)
        .collect();
    assert_eq!(names, ["a-transform", "b-guard"]);

    let only_guards: Vec<String> = fixture
        .plugins
        .list(tenant, Some(PluginType::Guard))
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.name)
        .collect();
    assert_eq!(only_guards, ["b-guard"]);
}

#[tokio::test]
async fn plugin_find_is_tenant_scoped() {
    let fixture = Fixture::new();
    let plugin = plugin_row(tenant_a(), PluginType::Auth);
    fixture.plugins.insert(&plugin).await.unwrap();
    assert_eq!(
        fixture.plugins.find(tenant_b(), plugin.id).await.unwrap(),
        None
    );
}
