#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::Arc;

use super::ControlPlaneService;
use crate::domain::dto::{ListQuery, PluginCommand, RequestContext, RouteCommand, UpstreamCommand};
use crate::domain::error::DomainError;
use crate::domain::model::{
    Endpoint, EndpointScheme, GUARD_PLUGIN_TYPE, HttpMatch, HttpMethod, MatchConfig,
    PathSuffixMode, PluginType, Protocol, ROUTE_TYPE, ServerConfig, Upstream,
};
use crate::domain::repo::{
    AllowAllAuthorizer, ManagementAuthorizer, PluginRepository, RouteRepository, UpstreamRepository,
};
use crate::infra::storage::{
    MemoryPluginRepository, MemoryRouteRepository, MemoryStore, MemoryUpstreamRepository,
};

const ALIAS: &str = "api.openai.com";

fn tenant_a() -> uuid::Uuid {
    uuid::Uuid::from_u128(0xA001)
}

fn tenant_b() -> uuid::Uuid {
    uuid::Uuid::from_u128(0xB002)
}

fn ctx(tenant: uuid::Uuid) -> RequestContext {
    RequestContext {
        tenant,
        subject: "svc.oagw.test".to_owned(),
    }
}

fn endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme: EndpointScheme::Https,
        host: host.to_owned(),
        port,
    }
}

fn pool(host: &str) -> Vec<Endpoint> {
    vec![endpoint(host, 443)]
}

fn http_match(path: &str, methods: &[HttpMethod]) -> MatchConfig {
    MatchConfig {
        http: Some(HttpMatch {
            methods: methods.to_vec(),
            path: path.to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        }),
        grpc: None,
    }
}

fn upstream_command(alias: Option<&str>, endpoints: Vec<Endpoint>) -> UpstreamCommand {
    UpstreamCommand {
        alias: alias.map(str::to_owned),
        protocol: Protocol::Http,
        enabled: true,
        server: ServerConfig { endpoints },
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
    }
}

fn route_command(upstream_id: uuid::Uuid, path: &str, priority: u32) -> RouteCommand {
    RouteCommand {
        upstream_id,
        r#match: http_match(path, &[HttpMethod::Get]),
        priority,
        enabled: true,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
    }
}

fn plugin_command(kind: PluginType, name: &str) -> PluginCommand {
    PluginCommand {
        plugin_type: kind,
        name: name.to_owned(),
        config_schema: None,
        source_code: "def apply(ctx):\n    return ctx\n".to_owned(),
        phases: Vec::new(),
    }
}

fn chain(plugin_id: uuid::Uuid) -> crate::domain::model::PluginsConfig {
    crate::domain::model::PluginsConfig {
        sharing: crate::domain::model::SharingMode::Private,
        items: vec![crate::domain::model::PluginRef::Id(format!(
            "{GUARD_PLUGIN_TYPE}{plugin_id}"
        ))],
    }
}

/// The GTS id a client would put in the path for `plugin`.
fn requested_id(plugin: &crate::domain::model::Plugin) -> String {
    crate::domain::model::resource_gts_id(plugin.plugin_type.gts_base_type(), plugin.id)
}

fn aliases(rows: &[Upstream]) -> Vec<&str> {
    rows.iter().map(|row| row.alias.as_str()).collect()
}

/// The control plane over in-memory repositories.
struct Fixture {
    service: ControlPlaneService,
    upstreams: Arc<MemoryUpstreamRepository>,
    routes: Arc<MemoryRouteRepository>,
    plugins: Arc<MemoryPluginRepository>,
}

impl Fixture {
    fn with_page_sizes(default_page_size: u64, max_page_size: u64) -> Self {
        let store = Arc::new(MemoryStore::new());
        let upstreams = Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store)));
        let routes = Arc::new(MemoryRouteRepository::new(Arc::clone(&store)));
        let plugins = Arc::new(MemoryPluginRepository::new(
            Arc::clone(&store),
            Arc::clone(&upstreams) as Arc<dyn UpstreamRepository>,
            Arc::clone(&routes) as Arc<dyn RouteRepository>,
        ));
        let service = ControlPlaneService::new(
            Arc::clone(&upstreams) as Arc<dyn UpstreamRepository>,
            Arc::clone(&routes) as Arc<dyn RouteRepository>,
            Arc::clone(&plugins) as Arc<dyn PluginRepository>,
            Arc::new(AllowAllAuthorizer),
            default_page_size,
            max_page_size,
        );
        Self {
            service,
            upstreams,
            routes,
            plugins,
        }
    }

    fn new() -> Self {
        Self::with_page_sizes(
            crate::config::DEFAULT_PAGE_SIZE,
            crate::config::MAX_PAGE_SIZE,
        )
    }

    async fn seeded_upstream(&self, tenant: uuid::Uuid) -> Upstream {
        self.service
            .create_upstream(&ctx(tenant), upstream_command(None, pool(ALIAS)))
            .await
            .unwrap()
    }

    async fn seeded_plugin(&self, tenant: uuid::Uuid, name: &str) -> crate::domain::model::Plugin {
        self.service
            .create_plugin(&ctx(tenant), plugin_command(PluginType::Guard, name))
            .await
            .unwrap()
    }
}

// ── upstream lifecycle ─────────────────────────────────────────────────────

#[tokio::test]
async fn create_upstream_generates_a_server_side_id_and_stamps_instants() {
    let fixture = Fixture::new();
    let created = fixture.seeded_upstream(tenant_a()).await;

    let read = fixture
        .service
        .get_upstream(&ctx(tenant_a()), created.id)
        .await
        .unwrap();
    assert_eq!(read.alias, ALIAS);
    assert_eq!(read.created_at, read.updated_at);
    assert!(read.created_at.ends_with('Z'));
    assert_eq!(read.created_at.len(), 20);
    assert_eq!(read.protocol, Protocol::Http);
    assert_eq!(read.server.endpoints[0].host, ALIAS);
}

#[tokio::test]
async fn alias_uniqueness_conflicts_within_a_tenant() {
    let fixture = Fixture::new();
    fixture.seeded_upstream(tenant_a()).await;

    let error = fixture
        .service
        .create_upstream(&ctx(tenant_a()), upstream_command(None, pool(ALIAS)))
        .await
        .unwrap_err();
    match error {
        DomainError::Conflict { detail } => assert!(detail.contains(ALIAS), "{detail}"),
        other => panic!("expected Conflict, got {other:?}"),
    }
}

#[tokio::test]
async fn two_tenants_may_route_the_same_alias() {
    let fixture = Fixture::new();
    fixture.seeded_upstream(tenant_a()).await;
    assert!(
        fixture
            .service
            .create_upstream(&ctx(tenant_b()), upstream_command(None, pool(ALIAS)))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_non_derivable_pool_requires_an_explicit_alias() {
    let fixture = Fixture::new();
    let error = fixture
        .service
        .create_upstream(&ctx(tenant_a()), upstream_command(None, pool("10.0.0.1")))
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }));

    let created = fixture
        .service
        .create_upstream(
            &ctx(tenant_a()),
            upstream_command(Some("ip-pool"), pool("10.0.0.1")),
        )
        .await
        .unwrap();
    assert_eq!(created.alias, "ip-pool");
}

#[tokio::test]
async fn replace_upstream_keeps_the_identity_and_creation_instant() {
    let fixture = Fixture::new();
    let created = fixture.seeded_upstream(tenant_a()).await;

    let replaced = fixture
        .service
        .replace_upstream(
            &ctx(tenant_a()),
            created.id,
            upstream_command(None, pool(ALIAS)),
        )
        .await
        .unwrap();

    assert_eq!(replaced.id, created.id);
    assert_eq!(replaced.alias, created.alias);
    assert_eq!(replaced.created_at, created.created_at);
}

#[tokio::test]
async fn replace_upstream_refuses_to_rename_the_routing_key() {
    let fixture = Fixture::new();
    let created = fixture.seeded_upstream(tenant_a()).await;

    let renamed = upstream_command(Some("other.openai.com"), pool("other.openai.com"));
    let error = fixture
        .service
        .replace_upstream(&ctx(tenant_a()), created.id, renamed)
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }));
}

#[tokio::test]
async fn replace_upstream_cannot_take_an_alias_owned_by_another_row() {
    let fixture = Fixture::new();
    let tenant = tenant_a();
    let first = fixture.seeded_upstream(tenant).await;
    let second = fixture
        .service
        .create_upstream(
            &ctx(tenant),
            upstream_command(Some("other.openai.com"), pool("other.openai.com")),
        )
        .await
        .unwrap();

    // The alias is the routing key, so a replacement whose endpoints derive a
    // different alias is refused before the uniqueness check can even run.
    let stolen = upstream_command(None, pool("other.openai.com"));
    let error = fixture
        .service
        .replace_upstream(&ctx(tenant), first.id, stolen)
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }));
    assert_eq!(second.alias, "other.openai.com");
}

#[tokio::test]
async fn ancestor_resources_are_invisible_across_tenants() {
    let fixture = Fixture::new();
    let created = fixture.seeded_upstream(tenant_a()).await;

    assert!(matches!(
        fixture
            .service
            .get_upstream(&ctx(tenant_b()), created.id)
            .await,
        Err(DomainError::NotFound { .. })
    ));
    assert!(
        fixture
            .service
            .list_upstreams(&ctx(tenant_b()), &ListQuery::default())
            .await
            .unwrap()
            .is_empty()
    );

    let error = fixture
        .service
        .replace_upstream(
            &ctx(tenant_b()),
            created.id,
            upstream_command(None, pool(ALIAS)),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::NotFound { .. }));

    let error = fixture
        .service
        .delete_upstream(&ctx(tenant_b()), created.id)
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::NotFound { .. }));
}

#[tokio::test]
async fn deleting_an_upstream_cascades_its_routes() {
    let fixture = Fixture::new();
    let tenant = tenant_a();
    let upstream = fixture.seeded_upstream(tenant).await;
    let route = fixture
        .service
        .create_route(&ctx(tenant), route_command(upstream.id, "/v1/models", 1))
        .await
        .unwrap();

    fixture
        .service
        .delete_upstream(&ctx(tenant), upstream.id)
        .await
        .unwrap();

    assert!(matches!(
        fixture.service.get_route(&ctx(tenant), route.id).await,
        Err(DomainError::NotFound { .. })
    ));
    assert!(
        fixture
            .service
            .list_upstreams(&ctx(tenant), &ListQuery::default())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        fixture
            .upstreams
            .find(tenant, upstream.id)
            .await
            .unwrap()
            .is_none()
    );
}

// ── route lifecycle ────────────────────────────────────────────────────────

#[tokio::test]
async fn create_route_requires_a_tenant_owned_upstream() {
    let fixture = Fixture::new();
    let owned = fixture.seeded_upstream(tenant_a()).await;

    assert!(matches!(
        fixture
            .service
            .create_route(&ctx(tenant_b()), route_command(owned.id, "/v1", 1))
            .await,
        Err(DomainError::NotFound { .. })
    ));
}

#[tokio::test]
async fn duplicate_match_rules_conflict() {
    let fixture = Fixture::new();
    let tenant = tenant_a();
    let upstream = fixture.seeded_upstream(tenant).await;

    fixture
        .service
        .create_route(&ctx(tenant), route_command(upstream.id, "/v1/models", 10))
        .await
        .unwrap();

    let error = fixture
        .service
        .create_route(&ctx(tenant), route_command(upstream.id, "/v1/models", 10))
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::Conflict { .. }));

    // A different priority is a different match key.
    assert!(
        fixture
            .service
            .create_route(&ctx(tenant), route_command(upstream.id, "/v1/models", 11))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn replace_route_keeps_upstream_id_immutable() {
    let fixture = Fixture::new();
    let tenant = tenant_a();
    let upstream = fixture.seeded_upstream(tenant).await;
    let route = fixture
        .service
        .create_route(&ctx(tenant), route_command(upstream.id, "/v1/models", 1))
        .await
        .unwrap();

    let other = fixture
        .service
        .create_upstream(
            &ctx(tenant),
            upstream_command(Some("api.anthropic.com"), pool("api.anthropic.com")),
        )
        .await
        .unwrap();

    let error = fixture
        .service
        .replace_route(
            &ctx(tenant),
            route.id,
            route_command(other.id, "/v1/models", 1),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }));
    assert_eq!(
        fixture
            .service
            .get_route(&ctx(tenant), route.id)
            .await
            .unwrap()
            .upstream_id,
        upstream.id
    );
}

#[tokio::test]
async fn replace_route_tolerates_its_own_match_rule() {
    let fixture = Fixture::new();
    let tenant = tenant_a();
    let upstream = fixture.seeded_upstream(tenant).await;
    let route = fixture
        .service
        .create_route(&ctx(tenant), route_command(upstream.id, "/v1/models", 1))
        .await
        .unwrap();

    let replaced = fixture
        .service
        .replace_route(
            &ctx(tenant),
            route.id,
            route_command(upstream.id, "/v1/models", 1),
        )
        .await
        .unwrap();
    assert_eq!(replaced.id, route.id);
    assert_eq!(replaced.created_at, route.created_at);
}

#[tokio::test]
async fn route_delete_is_scoped() {
    let fixture = Fixture::new();
    let tenant = tenant_a();
    let upstream = fixture.seeded_upstream(tenant).await;
    let route = fixture
        .service
        .create_route(&ctx(tenant), route_command(upstream.id, "/v1", 1))
        .await
        .unwrap();

    assert!(matches!(
        fixture
            .service
            .delete_route(&ctx(tenant_b()), route.id)
            .await,
        Err(DomainError::NotFound { .. })
    ));
    fixture
        .service
        .delete_route(&ctx(tenant), route.id)
        .await
        .unwrap();
    assert!(matches!(
        fixture.service.get_route(&ctx(tenant), route.id).await,
        Err(DomainError::NotFound { .. })
    ));
}

// ── plugin lifecycle ───────────────────────────────────────────────────────

#[tokio::test]
async fn plugin_names_are_unique_per_tenant_and_kind() {
    let fixture = Fixture::new();
    let tenant = tenant_a();
    fixture
        .service
        .create_plugin(
            &ctx(tenant),
            plugin_command(PluginType::Guard, "require-tenant"),
        )
        .await
        .unwrap();

    // Whitespace is trimmed before the collision check.
    let error = fixture
        .service
        .create_plugin(
            &ctx(tenant),
            plugin_command(PluginType::Guard, "  require-tenant  "),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::Conflict { .. }));

    // A different kind may reuse the name.
    assert!(
        fixture
            .service
            .create_plugin(
                &ctx(tenant),
                plugin_command(PluginType::Transform, "require-tenant")
            )
            .await
            .is_ok()
    );
    // Another tenant may reuse the name.
    assert!(
        fixture
            .service
            .create_plugin(
                &ctx(tenant_b()),
                plugin_command(PluginType::Guard, "require-tenant")
            )
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn plugin_name_and_source_are_validated() {
    let fixture = Fixture::new();
    let tenant = tenant_a();

    let blank = fixture
        .service
        .create_plugin(&ctx(tenant), plugin_command(PluginType::Guard, "   "))
        .await
        .unwrap_err();
    assert!(matches!(blank, DomainError::Validation { .. }));

    let oversized = fixture
        .service
        .create_plugin(
            &ctx(tenant),
            plugin_command(PluginType::Guard, &"x".repeat(129)),
        )
        .await
        .unwrap_err();
    assert!(matches!(oversized, DomainError::Validation { .. }));

    let mut empty_source = plugin_command(PluginType::Guard, "blank-source");
    empty_source.source_code = "   \n".to_owned();
    let error = fixture
        .service
        .create_plugin(&ctx(tenant), empty_source)
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }));
}

#[tokio::test]
async fn plugin_source_is_served_separately_from_the_row() {
    let fixture = Fixture::new();
    let tenant = tenant_a();
    let plugin = fixture.seeded_plugin(tenant, "require-tenant").await;
    let requested = requested_id(&plugin);

    let source = fixture
        .service
        .get_plugin_source(&ctx(tenant), plugin.id, &requested)
        .await
        .unwrap();
    assert!(source.contains("def apply"));

    let row = fixture
        .service
        .get_plugin(&ctx(tenant), plugin.id, &requested)
        .await
        .unwrap();
    assert_eq!(row.id, plugin.id);
    assert_eq!(row.plugin_type, PluginType::Guard);
    assert!(row.source_code.contains("def apply"));
    assert!(matches!(
        fixture
            .service
            .get_plugin(&ctx(tenant_b()), plugin.id, &requested)
            .await,
        Err(DomainError::NotFound { .. })
    ));
}

#[tokio::test]
async fn deleting_an_unbound_plugin_succeeds() {
    let fixture = Fixture::new();
    let tenant = tenant_a();
    let plugin = fixture.seeded_plugin(tenant, "require-tenant").await;
    let requested = requested_id(&plugin);

    fixture
        .service
        .delete_plugin(&ctx(tenant), plugin.id, &requested)
        .await
        .unwrap();
    assert!(matches!(
        fixture
            .service
            .get_plugin(&ctx(tenant), plugin.id, &requested)
            .await,
        Err(DomainError::NotFound { .. })
    ));
}

#[tokio::test]
async fn deleting_an_in_use_plugin_conflicts_with_referenced_by() {
    let fixture = Fixture::new();
    let tenant = tenant_a();
    let plugin = fixture.seeded_plugin(tenant, "require-tenant").await;
    let requested = requested_id(&plugin);

    // Guards bind to route chains, so the referencing row is a route.
    let upstream = fixture.seeded_upstream(tenant).await;
    let mut command = route_command(upstream.id, "/v1/models", 1);
    command.plugins = Some(chain(plugin.id));
    let route = fixture
        .service
        .create_route(&ctx(tenant), command)
        .await
        .unwrap();

    let error = fixture
        .service
        .delete_plugin(&ctx(tenant), plugin.id, &requested)
        .await
        .unwrap_err();
    match error {
        DomainError::PluginInUse {
            plugin_id,
            upstreams,
            routes,
        } => {
            assert!(plugin_id.starts_with(GUARD_PLUGIN_TYPE));
            assert!(plugin_id.ends_with(&plugin.id.to_string()));
            assert!(upstreams.is_empty());
            assert_eq!(routes.len(), 1);
            assert!(routes[0].starts_with(ROUTE_TYPE));
        }
        other => panic!("expected PluginInUse, got {other:?}"),
    }
    assert_eq!(route.priority, 1);

    // Once the referencing route is gone the plugin is deletable again.
    fixture
        .service
        .delete_route(&ctx(tenant), route.id)
        .await
        .unwrap();
    fixture
        .service
        .delete_plugin(&ctx(tenant), plugin.id, &requested)
        .await
        .unwrap();
}

#[tokio::test]
async fn plugin_references_span_upstreams_and_routes() {
    let fixture = Fixture::new();
    let tenant = tenant_a();
    let plugin = fixture.seeded_plugin(tenant, "require-tenant").await;
    let requested = requested_id(&plugin);

    let upstream = fixture.seeded_upstream(tenant).await;
    let mut route = route_command(upstream.id, "/v1/models", 1);
    route.plugins = Some(chain(plugin.id));
    let route_row = fixture
        .service
        .create_route(&ctx(tenant), route)
        .await
        .unwrap();

    let error = fixture
        .service
        .delete_plugin(&ctx(tenant), plugin.id, &requested)
        .await
        .unwrap_err();
    match error {
        DomainError::PluginInUse { routes, .. } => {
            assert_eq!(routes.len(), 1);
            assert!(routes[0].starts_with(ROUTE_TYPE));
            assert!(routes[0].ends_with(&route_row.id.to_string()));
        }
        other => panic!("expected PluginInUse, got {other:?}"),
    }
}

// ── list query params ──────────────────────────────────────────────────────

#[tokio::test]
async fn list_pages_are_offset_and_truncated() {
    let fixture = Fixture::with_page_sizes(2, 3);
    let tenant = tenant_a();
    for index in 0..4_u32 {
        let host = format!("h{index}.openai.com");
        fixture
            .service
            .create_upstream(&ctx(tenant), upstream_command(Some(&host), pool(&host)))
            .await
            .unwrap();
    }

    let query = ListQuery {
        top: Some(2),
        skip: 1,
        ..ListQuery::default()
    };
    let page = fixture
        .service
        .list_upstreams(&ctx(tenant), &query)
        .await
        .unwrap();
    assert_eq!(aliases(&page), vec!["h1.openai.com", "h2.openai.com"]);
}

#[tokio::test]
async fn list_truncates_to_the_requested_page_size() {
    let fixture = Fixture::with_page_sizes(2, 100);
    let tenant = tenant_a();
    for index in 0..5_u32 {
        let host = format!("h{index}.openai.com");
        fixture
            .service
            .create_upstream(&ctx(tenant), upstream_command(Some(&host), pool(&host)))
            .await
            .unwrap();
    }
    // The configured default page size is applied by the transport's
    // `build_list_query`, so the service sees an explicit `$top`.
    let page = fixture
        .service
        .list_upstreams(
            &ctx(tenant),
            &ListQuery {
                top: Some(2),
                ..ListQuery::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(page.len(), 2);
}

#[tokio::test]
async fn list_orders_by_alias_ascending() {
    let fixture = Fixture::new();
    let tenant = tenant_a();
    fixture.seeded_upstream(tenant).await;
    fixture
        .service
        .create_upstream(
            &ctx(tenant),
            upstream_command(Some("api.anthropic.com"), pool("api.anthropic.com")),
        )
        .await
        .unwrap();

    let rows = fixture
        .service
        .list_upstreams(&ctx(tenant), &ListQuery::default())
        .await
        .unwrap();
    assert_eq!(aliases(&rows), ["api.anthropic.com", "api.openai.com"]);
}

#[tokio::test]
async fn plugin_list_can_be_narrowed_to_a_kind() {
    let fixture = Fixture::new();
    let tenant = tenant_a();
    fixture
        .service
        .create_plugin(&ctx(tenant), plugin_command(PluginType::Guard, "b-guard"))
        .await
        .unwrap();
    fixture
        .service
        .create_plugin(
            &ctx(tenant),
            plugin_command(PluginType::Transform, "a-transform"),
        )
        .await
        .unwrap();

    let all = fixture
        .service
        .list_plugins(&ctx(tenant), &ListQuery::default(), None)
        .await
        .unwrap();
    assert_eq!(all.len(), 2);

    let guards = fixture
        .service
        .list_plugins(&ctx(tenant), &ListQuery::default(), Some(PluginType::Guard))
        .await
        .unwrap();
    assert_eq!(guards.len(), 1);
    assert_eq!(guards[0].name, "b-guard");
}

// ── authorization ──────────────────────────────────────────────────────────

struct DenyAll;

#[async_trait::async_trait]
impl ManagementAuthorizer for DenyAll {
    async fn authorize(
        &self,
        _tenant: uuid::Uuid,
        _subject: &str,
        _resource: &str,
        _action: &str,
    ) -> Result<(), DomainError> {
        Err(DomainError::AccessDenied {
            detail: "denied".to_owned(),
        })
    }
}

#[tokio::test]
async fn management_operations_are_authorized_before_anything_else() {
    let store = Arc::new(MemoryStore::new());
    let service = ControlPlaneService::new(
        Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store))),
        Arc::new(MemoryRouteRepository::new(Arc::clone(&store))),
        Arc::new(MemoryPluginRepository::new(
            Arc::clone(&store),
            Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store))),
            Arc::new(MemoryRouteRepository::new(Arc::clone(&store))),
        )),
        Arc::new(DenyAll),
        50,
        100,
    );

    let error = service
        .create_upstream(&ctx(tenant_a()), upstream_command(None, pool(ALIAS)))
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::AccessDenied { .. }));

    let error = service
        .list_upstreams(&ctx(tenant_a()), &ListQuery::default())
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::AccessDenied { .. }));

    let error = service
        .get_route(&ctx(tenant_a()), uuid::Uuid::new_v4())
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::AccessDenied { .. }));

    let error = service
        .delete_plugin(
            &ctx(tenant_a()),
            uuid::Uuid::new_v4(),
            &format!("{GUARD_PLUGIN_TYPE}{}", uuid::Uuid::new_v4()),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::AccessDenied { .. }));
}

#[tokio::test]
async fn page_size_accessors_expose_the_configuration() {
    let fixture = Fixture::with_page_sizes(7, 21);
    assert_eq!(fixture.service.default_page_size(), 7);
    assert_eq!(fixture.service.max_page_size(), 21);
}

#[tokio::test]
async fn list_is_scoped_even_when_the_store_holds_other_tenants() {
    let fixture = Fixture::new();
    fixture.seeded_upstream(tenant_a()).await;
    fixture.seeded_upstream(tenant_b()).await;

    let own = fixture
        .service
        .list_upstreams(&ctx(tenant_a()), &ListQuery::default())
        .await
        .unwrap();
    assert_eq!(aliases(&own), [ALIAS]);
    assert_eq!(own[0].tenant_id, tenant_a());
    let _ = &fixture.routes;
    let _ = &fixture.plugins;
    let _ = <MemoryUpstreamRepository as UpstreamRepository>::find;
}
