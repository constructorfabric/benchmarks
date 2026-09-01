#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, RawQuery};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;
use toolkit_security::SecurityContext;

use super::{ListEnvelopeDto, PluginResponseDto, RouteResponseDto, UpstreamRequestDto};
use crate::domain::dto::{PluginCommand, RequestContext, RouteCommand, UpstreamCommand};
use crate::domain::model::{
    GUARD_PLUGIN_TYPE, HttpMatch, HttpMethod, MatchConfig, PathSuffixMode, PluginType, Protocol,
    ROUTE_TYPE, ServerConfig, UPSTREAM_TYPE, canonical_types, resource_gts_id,
};
use crate::domain::repo::{
    AllowAllAuthorizer, PluginRepository, RouteRepository, UpstreamRepository,
};
use crate::domain::services::ControlPlaneService;
use crate::infra::storage::{
    MemoryPluginRepository, MemoryRouteRepository, MemoryStore, MemoryUpstreamRepository,
};

const ALIAS: &str = "api.openai.com";
const UPSTREAMS: &str = "/oagw/v1/upstreams";
const ROUTES: &str = "/oagw/v1/routes";
const PLUGINS: &str = "/oagw/v1/plugins";
const VALIDATION_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";
const ROUTE_NOT_FOUND_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";

fn tenant() -> uuid::Uuid {
    uuid::Uuid::from_u128(0xA001)
}

fn stranger() -> uuid::Uuid {
    uuid::Uuid::from_u128(0xB002)
}

fn security(tenant: uuid::Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(uuid::Uuid::from_u128(0xC003))
        .subject_tenant_id(tenant)
        .build()
        .unwrap()
}

fn anonymous() -> SecurityContext {
    SecurityContext::anonymous()
}

fn endpoint(host: &str) -> crate::domain::model::Endpoint {
    crate::domain::model::Endpoint {
        scheme: crate::domain::model::EndpointScheme::Https,
        host: host.to_owned(),
        port: 443,
    }
}

fn service() -> Arc<ControlPlaneService> {
    let store = Arc::new(MemoryStore::new());
    let upstreams = Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store)));
    let routes = Arc::new(MemoryRouteRepository::new(Arc::clone(&store)));
    let plugins = Arc::new(MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::clone(&upstreams) as Arc<dyn UpstreamRepository>,
        Arc::clone(&routes) as Arc<dyn RouteRepository>,
    ));
    Arc::new(ControlPlaneService::new(
        upstreams as Arc<dyn UpstreamRepository>,
        routes as Arc<dyn RouteRepository>,
        plugins as Arc<dyn PluginRepository>,
        Arc::new(AllowAllAuthorizer),
        50,
        100,
    ))
}

fn uri(path: &str) -> Uri {
    Uri::builder().path_and_query(path).build().unwrap()
}

/// An empty header map: the shape every handler receives without tracing
/// headers.
fn headers() -> HeaderMap {
    HeaderMap::new()
}

fn upstream_command(alias: Option<&str>, host: &str) -> UpstreamCommand {
    UpstreamCommand {
        alias: alias.map(str::to_owned),
        protocol: Protocol::Http,
        enabled: true,
        server: ServerConfig {
            endpoints: vec![endpoint(host)],
        },
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec!["primary".to_owned()],
    }
}

fn route_command(upstream_id: uuid::Uuid, priority: u32) -> RouteCommand {
    RouteCommand {
        upstream_id,
        r#match: MatchConfig {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: "/v1/models".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        priority,
        enabled: true,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
    }
}

fn plugin_command(name: &str) -> PluginCommand {
    PluginCommand {
        plugin_type: PluginType::Guard,
        name: name.to_owned(),
        config_schema: None,
        source_code: "def apply(ctx):\n    return ctx\n".to_owned(),
        phases: Vec::new(),
    }
}

/// The GTS id a client writes in an upstream path segment.
fn upstream_id_of(row: &crate::domain::model::Upstream) -> String {
    resource_gts_id(UPSTREAM_TYPE, row.id)
}

/// The GTS id a client writes in a route path segment.
fn route_id_of(row: &crate::domain::model::Route) -> String {
    resource_gts_id(ROUTE_TYPE, row.id)
}

/// The GTS id a client writes in a plugin path segment.
fn plugin_id_of(row: &crate::domain::model::Plugin) -> String {
    resource_gts_id(row.plugin_type.gts_base_type(), row.id)
}

/// The gateway error-source header of a response, when it carries one.
fn status_of_header(response: &axum::response::Response) -> Option<&str> {
    response
        .headers()
        .get(super::super::error::ERROR_SOURCE_HEADER)
        .and_then(|value| value.to_str().ok())
}

/// Assert that a success response declares where it came from (`ADR`-0007).
fn assert_error_source(response: &axum::response::Response) {
    assert_eq!(status_of_header(response), Some("gateway"));
}

fn context(tenant: uuid::Uuid) -> RequestContext {
    RequestContext {
        tenant,
        subject: "svc.oagw.test".to_owned(),
    }
}

async fn seeded_upstream(svc: &ControlPlaneService) -> crate::domain::model::Upstream {
    svc.create_upstream(&context(tenant()), upstream_command(None, ALIAS))
        .await
        .unwrap()
}

async fn seeded_route(
    svc: &ControlPlaneService,
    upstream_id: uuid::Uuid,
) -> crate::domain::model::Route {
    svc.create_route(&context(tenant()), route_command(upstream_id, 1))
        .await
        .unwrap()
}

async fn seeded_plugin(svc: &ControlPlaneService) -> crate::domain::model::Plugin {
    svc.create_plugin(&context(tenant()), plugin_command("require-tenant"))
        .await
        .unwrap()
}

fn upstream_body(alias: Option<&str>, host: &str) -> UpstreamRequestDto {
    serde_json::from_value(serde_json::json!({
        "alias": alias,
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "host": host } ] },
        "tags": ["primary"]
    }))
    .unwrap()
}

fn route_body(upstream_id: &str) -> Json<super::RouteRequestDto> {
    Json(
        serde_json::from_value(serde_json::json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET"], "path": "/v1/models" } }
        }))
        .unwrap(),
    )
}

/// A route payload with an explicit `cors` override.
fn route_body_with_cors(
    upstream_id: &str,
    cors: &serde_json::Value,
) -> Json<super::RouteRequestDto> {
    Json(
        serde_json::from_value(serde_json::json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET"], "path": "/v1/models" } },
            "cors": cors
        }))
        .unwrap(),
    )
}

fn plugin_body(name: &str) -> Json<super::PluginRequestDto> {
    Json(
        serde_json::from_value(serde_json::json!({
            "plugin_type": "guard",
            "name": name,
            "source_code": "def apply(ctx):\n    return ctx\n",
            "phases": ["on_request"]
        }))
        .unwrap(),
    )
}

/// Pull the problem out of a failing handler result without needing the DTO to
/// be `Debug`.
fn problem_of<T>(
    result: Result<T, super::super::error::OagwProblem>,
) -> super::super::error::OagwProblem {
    result
        .err()
        .unwrap_or_else(|| panic!("expected the handler to fail"))
}

async fn body_of(response: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

// ── request context ────────────────────────────────────────────────────────

#[test]
fn a_caller_without_a_tenant_identity_is_rejected() {
    let error =
        super::request_context(&uri("/oagw/v1/upstreams"), &headers(), &anonymous()).unwrap_err();
    assert_eq!(error.status(), 403);
    assert_eq!(error.kind(), canonical_types::PERMISSION_DENIED);
    assert_eq!(
        error.extensions().instance.as_deref(),
        Some("/oagw/v1/upstreams")
    );

    let resolved =
        super::request_context(&uri("/oagw/v1/upstreams"), &headers(), &security(tenant()))
            .unwrap();
    assert_eq!(resolved.tenant, tenant());
    assert_eq!(resolved.subject, uuid::Uuid::from_u128(0xC003).to_string());
}

// ── upstreams ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_upstream_answers_201_with_a_location_header() {
    let svc = service();
    let response = super::create_upstream(
        uri(UPSTREAMS),
        Extension(Arc::clone(&svc)),
        Extension(security(tenant())),
        headers(),
        Json(upstream_body(None, ALIAS)),
    )
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let location = response
        .headers()
        .get(axum::http::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .unwrap()
        .to_owned();
    assert!(location.starts_with(UPSTREAMS));
    assert!(location.contains(UPSTREAM_TYPE));

    let document = body_of(response).await;
    assert_eq!(document["alias"], ALIAS);
    assert_eq!(document["tags"][0], "primary");
    assert!(document["created_at"].is_string());
}

#[tokio::test]
async fn create_upstream_maps_an_alias_conflict_onto_409() {
    let svc = service();
    seeded_upstream(&svc).await;

    let error = problem_of(
        super::create_upstream(
            uri(UPSTREAMS),
            Extension(Arc::clone(&svc)),
            Extension(security(tenant())),
            headers(),
            Json(upstream_body(None, ALIAS)),
        )
        .await,
    );
    assert_eq!(error.status(), 409);
    assert_eq!(error.extensions().instance.as_deref(), Some(UPSTREAMS));
}

#[tokio::test]
async fn a_foreign_tenant_cannot_read_an_ancestor_resource() {
    let svc = service();
    let created = seeded_upstream(&svc).await;

    let error = problem_of(
        super::get_upstream(
            uri(&format!("{UPSTREAMS}/{}", upstream_id_of(&created))),
            Extension(Arc::clone(&svc)),
            Extension(security(stranger())),
            headers(),
            Path(upstream_id_of(&created)),
        )
        .await,
    );
    assert_eq!(error.status(), 404);
    assert_eq!(error.kind(), canonical_types::NOT_FOUND);
    assert_eq!(
        error.extensions().instance.as_deref(),
        Some(format!("{UPSTREAMS}/{}", upstream_id_of(&created)).as_str())
    );
}

#[tokio::test]
async fn get_upstream_returns_the_projected_row() {
    let svc = service();
    let created = seeded_upstream(&svc).await;

    let read = super::get_upstream(
        uri(&format!("{UPSTREAMS}/{}", upstream_id_of(&created))),
        Extension(Arc::clone(&svc)),
        Extension(security(tenant())),
        headers(),
        Path(upstream_id_of(&created)),
    )
    .await
    .unwrap();
    assert_eq!(read.status(), StatusCode::OK);
    assert_eq!(status_of_header(&read), Some("gateway"));
    let document = body_of(read).await;
    assert_eq!(document["alias"], ALIAS);
    assert_eq!(
        document["protocol"],
        "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    );
    assert_eq!(document["server"]["endpoints"][0]["host"], ALIAS);
}

/// `F9` — a path id without the typed GTS prefix is a malformed request, not a
/// missing row: `400` with the OAGW validation identity.
#[tokio::test]
async fn a_malformed_resource_id_is_a_400_validation_problem() {
    let svc = service();
    let created = seeded_upstream(&svc).await;

    for id in [
        "not-an-id".to_owned(),
        created.id.to_string(),
        format!("{}{}", crate::domain::model::ROUTE_TYPE, created.id),
    ] {
        let error = problem_of(
            super::get_upstream(
                uri(&format!("{UPSTREAMS}/{id}")),
                Extension(Arc::clone(&svc)),
                Extension(security(tenant())),
                headers(),
                Path(id.clone()),
            )
            .await,
        );
        assert_eq!(error.status(), 400, "{id}");
        assert_eq!(error.kind(), VALIDATION_ERROR, "{id}");
        assert_eq!(
            error.extensions().instance.as_deref(),
            Some(format!("{UPSTREAMS}/{id}").as_str())
        );
    }
}

/// `F10` — a missing plugin is reported under the base type the caller used.
#[tokio::test]
async fn a_missing_plugin_is_reported_under_the_requested_base_type() {
    let svc = service();
    for base in [
        GUARD_PLUGIN_TYPE,
        crate::domain::model::TRANSFORM_PLUGIN_TYPE,
        crate::domain::model::AUTH_PLUGIN_TYPE,
    ] {
        let id = format!("{base}{}", uuid::Uuid::from_u128(0xF00F));
        let error = problem_of(
            super::get_plugin(
                uri(&format!("{PLUGINS}/{id}")),
                Extension(Arc::clone(&svc)),
                Extension(security(tenant())),
                headers(),
                Path(id.clone()),
            )
            .await,
        );
        assert_eq!(error.status(), 404, "{id}");
        assert_eq!(
            error.extensions().instance.as_deref(),
            Some(format!("{PLUGINS}/{id}").as_str())
        );
        let resource = body_of(error.clone().into_response()).await;
        assert_eq!(resource["type"], canonical_types::NOT_FOUND);
        assert!(
            resource["detail"].as_str().unwrap().starts_with(base),
            "{resource}"
        );
    }
}

#[tokio::test]
async fn list_upstreams_applies_the_documented_system_options() {
    let svc = service();
    let ctx = context(tenant());
    for host in ["a.openai.com", "b.openai.com", "c.openai.com"] {
        svc.create_upstream(&ctx, upstream_command(Some(host), host))
            .await
            .unwrap();
    }

    let query = "$top=2&$skip=1&$orderby=alias%20desc&$select=alias";
    let list = super::list_upstreams(
        uri(&format!("{UPSTREAMS}?{query}")),
        Extension(Arc::clone(&svc)),
        Extension(security(tenant())),
        headers(),
        RawQuery(Some(query.to_owned())),
    )
    .await
    .unwrap();
    assert_eq!(status_of_header(&list), Some("gateway"));
    let envelope = serde_json::from_value::<serde_json::Value>(body_of(list).await).unwrap();
    let envelope = ListEnvelopeDto {
        items: envelope["items"].as_array().cloned().unwrap_or_default(),
        page_info: super::PageMetaDto {
            limit: envelope["page_info"]["limit"].as_u64().unwrap_or_default() as u64,
            skip: envelope["page_info"]["skip"].as_u64().unwrap_or_default(),
        },
    };

    assert_eq!(envelope.page_info.limit, 2);
    assert_eq!(envelope.page_info.skip, 1);
    assert_eq!(envelope.items.len(), 2);
    assert_eq!(envelope.items[0]["alias"], "b.openai.com");
    assert!(
        envelope.items[0].get("id").is_none(),
        "$select must project"
    );
}

#[tokio::test]
async fn list_upstreams_rejects_an_unknown_system_option_with_400() {
    let svc = service();
    let error = problem_of(
        super::list_upstreams(
            uri(UPSTREAMS),
            Extension(Arc::clone(&svc)),
            Extension(security(tenant())),
            headers(),
            RawQuery(Some("$count=true".to_owned())),
        )
        .await,
    );
    assert_eq!(error.status(), 400);
    assert_eq!(error.kind(), canonical_types::INVALID_ARGUMENT);
}

#[tokio::test]
async fn list_upstreams_applies_the_default_page_size() {
    let svc = service();
    let ctx = context(tenant());
    for index in 0..3_u32 {
        let host = format!("h{index}.openai.com");
        svc.create_upstream(&ctx, upstream_command(Some(&host), &host))
            .await
            .unwrap();
    }

    let list = super::list_upstreams(
        uri(UPSTREAMS),
        Extension(Arc::clone(&svc)),
        Extension(security(tenant())),
        headers(),
        RawQuery(None),
    )
    .await
    .unwrap();
    let envelope = serde_json::from_value::<serde_json::Value>(body_of(list).await).unwrap();
    let envelope = ListEnvelopeDto {
        items: envelope["items"].as_array().cloned().unwrap_or_default(),
        page_info: super::PageMetaDto {
            limit: envelope["page_info"]["limit"].as_u64().unwrap_or_default() as u64,
            skip: envelope["page_info"]["skip"].as_u64().unwrap_or_default(),
        },
    };
    assert_eq!(envelope.page_info.limit, 50);
    assert_eq!(envelope.items.len(), 3);
}

/// A listed resource must spell the same anonymous GTS id every other view of
/// it spells, so a client can list and then address the row it found.
#[tokio::test]
async fn list_items_carry_the_anonymous_gts_id_their_paths_require() {
    let svc = service();
    let ctx = context(tenant());
    let upstream = svc
        .create_upstream(&ctx, upstream_command(None, "list.openai.com"))
        .await
        .unwrap();
    svc.create_route(&ctx, route_command(upstream.id, 1))
        .await
        .unwrap();
    svc.create_plugin(&ctx, plugin_command("listed-guard"))
        .await
        .unwrap();

    let upstreams = serde_json::from_value::<serde_json::Value>(
        body_of(
            super::list_upstreams(
                uri(UPSTREAMS),
                Extension(Arc::clone(&svc)),
                Extension(security(tenant())),
                headers(),
                RawQuery(None),
            )
            .await
            .unwrap(),
        )
        .await,
    )
    .unwrap();
    let routes = serde_json::from_value::<serde_json::Value>(
        body_of(
            super::list_routes(
                uri(ROUTES),
                Extension(Arc::clone(&svc)),
                Extension(security(tenant())),
                headers(),
                RawQuery(None),
            )
            .await
            .unwrap(),
        )
        .await,
    )
    .unwrap();
    let plugins = serde_json::from_value::<serde_json::Value>(
        body_of(
            super::list_plugins(
                uri(PLUGINS),
                Extension(Arc::clone(&svc)),
                Extension(security(tenant())),
                headers(),
                RawQuery(None),
            )
            .await
            .unwrap(),
        )
        .await,
    )
    .unwrap();

    let upstream_id = upstreams["items"][0]["id"].as_str().unwrap();
    let route_id = routes["items"][0]["id"].as_str().unwrap();
    let plugin_id = plugins["items"][0]["id"].as_str().unwrap();
    assert_eq!(upstream_id, upstream_id_of(&upstream), "{upstream_id}");
    assert_eq!(routes["items"][0]["upstream_id"], upstream_id);
    assert!(route_id.starts_with(ROUTE_TYPE), "{route_id}");
    assert!(
        plugin_id.starts_with("gts.cf.core.oagw.guard_plugin.v1~"),
        "{plugin_id}"
    );

    // Round trip: the listed id is the one the path parser accepts.
    let fetched = super::get_route(
        uri(&format!("{ROUTES}/{route_id}")),
        Extension(Arc::clone(&svc)),
        Extension(security(tenant())),
        headers(),
        Path(route_id.to_owned()),
    )
    .await
    .unwrap();
    assert_eq!(status_of_header(&fetched), Some("gateway"));
}

#[tokio::test]
async fn replace_upstream_returns_the_replacement() {
    let svc = service();
    let created = seeded_upstream(&svc).await;

    let replaced = super::replace_upstream(
        uri(&format!("{UPSTREAMS}/{}", upstream_id_of(&created))),
        Extension(Arc::clone(&svc)),
        Extension(security(tenant())),
        headers(),
        Path(upstream_id_of(&created)),
        Json(upstream_body(None, ALIAS)),
    )
    .await
    .unwrap();
    assert_eq!(status_of_header(&replaced), Some("gateway"));
    let document = body_of(replaced).await;
    assert_eq!(document["alias"], ALIAS);
    assert_eq!(document["created_at"], created.created_at);
}

#[tokio::test]
async fn delete_upstream_answers_204() {
    let svc = service();
    let created = seeded_upstream(&svc).await;

    let response = super::delete_upstream(
        uri(&format!("{UPSTREAMS}/{}", upstream_id_of(&created))),
        Extension(Arc::clone(&svc)),
        Extension(security(tenant())),
        headers(),
        Path(upstream_id_of(&created)),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_error_source(&response);
}

// ── routes ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_route_answers_201_and_projects_the_gts_upstream_id() {
    let svc = service();
    let upstream = seeded_upstream(&svc).await;

    let response = super::create_route(
        uri(ROUTES),
        Extension(Arc::clone(&svc)),
        Extension(security(tenant())),
        headers(),
        route_body(&format!("{UPSTREAM_TYPE}{}", upstream.id)),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let location = response
        .headers()
        .get(axum::http::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .unwrap()
        .to_owned();
    assert!(location.starts_with("/oagw/v1/routes"));

    let document = body_of(response).await;
    assert_eq!(
        document["upstream_id"],
        resource_gts_id(UPSTREAM_TYPE, upstream.id)
    );
}

#[tokio::test]
async fn create_route_rejects_a_non_gts_upstream_id_with_400() {
    let svc = service();
    let error = problem_of(
        super::create_route(
            uri(ROUTES),
            Extension(Arc::clone(&svc)),
            Extension(security(tenant())),
            headers(),
            route_body(&uuid::Uuid::new_v4().to_string()),
        )
        .await,
    );
    assert_eq!(error.status(), 400);
    assert_eq!(error.kind(), VALIDATION_ERROR);
}

#[tokio::test]
async fn a_duplicate_match_rule_answers_409() {
    let svc = service();
    let upstream = seeded_upstream(&svc).await;
    let body = route_body(&format!("{UPSTREAM_TYPE}{}", upstream.id));

    super::create_route(
        uri(ROUTES),
        Extension(Arc::clone(&svc)),
        Extension(security(tenant())),
        headers(),
        body,
    )
    .await
    .unwrap();

    let again = route_body(&format!("{UPSTREAM_TYPE}{}", upstream.id));
    let error = problem_of(
        super::create_route(
            uri(ROUTES),
            Extension(Arc::clone(&svc)),
            Extension(security(tenant())),
            headers(),
            again,
        )
        .await,
    );
    assert_eq!(error.status(), 409);
}

#[tokio::test]
async fn get_route_round_trips_the_match_rule() {
    let svc = service();
    let upstream = seeded_upstream(&svc).await;
    let route = seeded_route(&svc, upstream.id).await;

    let read = super::get_route(
        uri(&format!("{ROUTES}/{}", route_id_of(&route))),
        Extension(Arc::clone(&svc)),
        Extension(security(tenant())),
        headers(),
        Path(route_id_of(&route)),
    )
    .await
    .unwrap();
    assert_eq!(status_of_header(&read), Some("gateway"));
    let document = body_of(read).await;
    assert_eq!(document["priority"], 1);
    assert_eq!(
        document["upstream_id"],
        format!("{UPSTREAM_TYPE}{}", upstream.id)
    );
    assert!(document["match"]["http"].is_object());
}

#[tokio::test]
async fn delete_route_answers_204() {
    let svc = service();
    let upstream = seeded_upstream(&svc).await;
    let route = seeded_route(&svc, upstream.id).await;

    let response = super::delete_route(
        uri(&format!("{ROUTES}/{}", route_id_of(&route))),
        Extension(Arc::clone(&svc)),
        Extension(security(tenant())),
        headers(),
        Path(route_id_of(&route)),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_error_source(&response);
}

// ── plugins ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_plugin_answers_201_with_a_kind_specific_location() {
    let svc = service();
    let response = super::create_plugin(
        uri(PLUGINS),
        Extension(Arc::clone(&svc)),
        Extension(security(tenant())),
        headers(),
        plugin_body("require-tenant"),
    )
    .await
    .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let location = response
        .headers()
        .get(axum::http::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .unwrap()
        .to_owned();
    assert!(location.contains(GUARD_PLUGIN_TYPE));

    let document = body_of(response).await;
    assert_eq!(document["name"], "require-tenant");
    assert!(
        document.get("source_code").is_none(),
        "the source is served separately"
    );
}

#[tokio::test]
async fn list_plugins_exposes_the_type_alias_for_filtering() {
    let svc = service();
    seeded_plugin(&svc).await;

    let query = "$filter=type%20eq%20'guard'";
    let list = super::list_plugins(
        uri(&format!("{PLUGINS}?{query}")),
        Extension(Arc::clone(&svc)),
        Extension(security(tenant())),
        headers(),
        RawQuery(Some(query.to_owned())),
    )
    .await
    .unwrap();
    let envelope = serde_json::from_value::<serde_json::Value>(body_of(list).await).unwrap();
    let envelope = ListEnvelopeDto {
        items: envelope["items"].as_array().cloned().unwrap_or_default(),
        page_info: super::PageMetaDto {
            limit: envelope["page_info"]["limit"].as_u64().unwrap_or_default() as u64,
            skip: envelope["page_info"]["skip"].as_u64().unwrap_or_default(),
        },
    };
    assert_eq!(envelope.items.len(), 1);
    assert_eq!(envelope.items[0]["type"], "guard");
}

#[tokio::test]
async fn plugin_source_is_served_with_the_starlark_media_type() {
    let svc = service();
    let plugin = seeded_plugin(&svc).await;

    let response = super::get_plugin_source(
        uri(&format!("{PLUGINS}/{}/source", plugin_id_of(&plugin))),
        Extension(Arc::clone(&svc)),
        Extension(security(tenant())),
        headers(),
        Path(plugin_id_of(&plugin)),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap()
            .starts_with("text/x-starlark")
    );

    let document = body_of(response).await;
    assert!(
        document["source_code"]
            .as_str()
            .unwrap()
            .contains("def apply")
    );
    assert!(
        document["id"]
            .as_str()
            .unwrap()
            .starts_with(GUARD_PLUGIN_TYPE)
    );
}

#[tokio::test]
async fn deleting_an_in_use_plugin_answers_409_with_referenced_by() {
    let svc = service();
    let plugin = seeded_plugin(&svc).await;
    let ctx = context(tenant());

    // A guard plugin is referenced by the route chain that binds it.
    let upstream = seeded_upstream(&svc).await;
    let mut command = route_command(upstream.id, 1);
    command.plugins = Some(crate::domain::model::PluginsConfig {
        sharing: crate::domain::model::SharingMode::Private,
        items: vec![crate::domain::model::PluginRef::Id(format!(
            "{GUARD_PLUGIN_TYPE}{}",
            plugin.id
        ))],
    });
    let route = svc.create_route(&ctx, command).await.unwrap();

    let error = problem_of(
        super::delete_plugin(
            uri(&format!("{PLUGINS}/{}", plugin_id_of(&plugin))),
            Extension(Arc::clone(&svc)),
            Extension(security(tenant())),
            headers(),
            Path(plugin_id_of(&plugin)),
        )
        .await,
    );
    assert_eq!(error.status(), 409);
    assert_eq!(
        error.kind(),
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"
    );
    let referenced_by = error.extensions().referenced_by.clone().unwrap();
    assert!(referenced_by.upstreams.is_empty());
    assert_eq!(
        referenced_by.routes,
        vec![resource_gts_id(ROUTE_TYPE, route.id)]
    );
    assert_eq!(
        error.extensions().plugin_id.as_deref(),
        Some(format!("{GUARD_PLUGIN_TYPE}{}", plugin.id).as_str())
    );
}

#[tokio::test]
async fn deleting_an_unbound_plugin_answers_204() {
    let svc = service();
    let plugin = seeded_plugin(&svc).await;

    let response = super::delete_plugin(
        uri(&format!("{PLUGINS}/{}", plugin_id_of(&plugin))),
        Extension(Arc::clone(&svc)),
        Extension(security(tenant())),
        headers(),
        Path(plugin_id_of(&plugin)),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_error_source(&response);
}

// ── authorization ──────────────────────────────────────────────────────────

#[tokio::test]
async fn a_caller_without_a_tenant_is_denied_before_any_lookup() {
    let svc = service();
    let error = problem_of(
        super::create_upstream(
            uri(UPSTREAMS),
            Extension(Arc::clone(&svc)),
            Extension(anonymous()),
            headers(),
            Json(upstream_body(None, ALIAS)),
        )
        .await,
    );
    assert_eq!(error.status(), 403);
    assert_eq!(error.kind(), canonical_types::PERMISSION_DENIED);
    assert_eq!(error.extensions().instance.as_deref(), Some(UPSTREAMS));
}

// ── proxy: the data plane through the transport layer ──────────────────────

/// A `GET` request with no body, as the transport layer receives it.
fn proxy_request(path: &str, header: Option<(&str, &str)>) -> axum::extract::Request {
    request_with(
        axum::http::Method::GET,
        path,
        &[header.unwrap_or(("x-oagw-probe", "probe"))],
    )
}

fn request_with(
    method: axum::http::Method,
    path: &str,
    headers: &[(&str, &str)],
) -> axum::extract::Request {
    let mut builder = axum::http::Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(axum::body::Body::empty()).unwrap()
}

async fn proxied(
    engine: &Arc<super::ProxyEngine>,
    path: &str,
    header: Option<(&str, &str)>,
) -> Result<axum::response::Response, super::OagwProblem> {
    // The `{*path_suffix}` axum captures does not carry the leading slash.
    let suffix = path
        .split_once("/proxy/")
        .and_then(|(_, rest)| rest.split_once('/'))
        .map(|(_, suffix)| suffix.to_owned());
    let alias = path
        .split_once("/proxy/")
        .map(|(_, rest)| rest.split('/').next().unwrap_or(rest))
        .unwrap_or_default()
        .to_owned();
    super::proxy_with_suffix(
        uri(path),
        Path((alias, suffix.unwrap_or_default())),
        Extension(Arc::clone(engine)),
        Extension(security(tenant())),
        proxy_request(path, header),
    )
    .await
}

/// The `OPTIONS` form of [`proxied`], for the CORS preflight tests.
async fn proxied_options(
    engine: &Arc<super::ProxyEngine>,
    path: &str,
    headers: &[(&str, &str)],
) -> Result<axum::response::Response, super::OagwProblem> {
    let suffix = path
        .split_once("/proxy/")
        .and_then(|(_, rest)| rest.split_once('/'))
        .map(|(_, suffix)| suffix.to_owned());
    let alias = path
        .split_once("/proxy/")
        .map(|(_, rest)| rest.split('/').next().unwrap_or(rest))
        .unwrap_or_default()
        .to_owned();
    super::proxy_with_suffix(
        uri(path),
        Path((alias, suffix.unwrap_or_default())),
        Extension(Arc::clone(engine)),
        Extension(security(tenant())),
        request_with(axum::http::Method::OPTIONS, path, headers),
    )
    .await
}

#[tokio::test]
async fn a_proxied_call_returns_the_upstream_status_body_and_source() {
    let server = httpmock::MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/models");
        then.status(200)
            .header("x-vendor", "vendor-1")
            .body("{\"models\":[]}");
    });
    let engine = engine_for_alias(
        crate::domain::model::Protocol::Http,
        "127.0.0.1",
        server.port(),
        "127.0.0.1",
        "/v1",
    )
    .await;

    let response = proxied(&engine, "/oagw/v1/proxy/127.0.0.1/v1/models", None)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(status_of_header(&response), Some("upstream"));
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    assert_eq!(bytes, &b"{\"models\":[]}"[..]);
}

#[tokio::test]
async fn an_unroutable_alias_is_a_404_problem_from_the_gateway() {
    let server = httpmock::MockServer::start();
    let engine = engine_for_alias(
        crate::domain::model::Protocol::Http,
        "127.0.0.1",
        server.port(),
        "127.0.0.1",
        "/ws",
    )
    .await;

    let error = problem_of(proxied(&engine, "/oagw/v1/proxy/127.0.0.1/v1/other", None).await);
    assert_eq!(error.status(), 404);
    assert_eq!(
        error.kind(),
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
    assert_eq!(
        error.extensions().instance.as_deref(),
        Some("/oagw/v1/proxy/127.0.0.1/v1/other")
    );
    assert_eq!(status_of_header(&error.into_response()), Some("gateway"));
}

#[tokio::test]
async fn an_alias_no_upstream_serves_is_a_404_route_not_found() {
    let server = httpmock::MockServer::start();
    let engine = engine_for_alias(
        crate::domain::model::Protocol::Http,
        "127.0.0.1",
        server.port(),
        "127.0.0.1",
        "/v1",
    )
    .await;

    let error = problem_of(proxied(&engine, "/oagw/v1/proxy/absent.vendor/v1", None).await);
    assert_eq!(error.status(), 404);
    assert_eq!(error.kind(), ROUTE_NOT_FOUND_ERROR);
    assert_eq!(
        status_of_header(&error.clone().into_response()),
        Some("gateway")
    );
    assert_eq!(
        error.extensions().instance.as_deref(),
        Some("/oagw/v1/proxy/absent.vendor/v1")
    );
}

#[tokio::test]
async fn the_proxy_requires_a_tenant_identity() {
    let server = httpmock::MockServer::start();
    let engine = engine_for_alias(
        crate::domain::model::Protocol::Http,
        "127.0.0.1",
        server.port(),
        "127.0.0.1",
        "/ws",
    )
    .await;

    let error = super::proxy(
        uri("/oagw/v1/proxy/127.0.0.1"),
        Path("127.0.0.1".to_owned()),
        Extension(engine),
        Extension(anonymous()),
        proxy_request("/oagw/v1/proxy/127.0.0.1", None),
    )
    .await
    .unwrap_err();
    assert_eq!(error.status(), 403);
}

#[tokio::test]
async fn a_declared_body_past_the_ceiling_is_a_413() {
    let server = httpmock::MockServer::start();
    let engine = engine_for_alias(
        crate::domain::model::Protocol::Http,
        "127.0.0.1",
        server.port(),
        "127.0.0.1",
        "/ws",
    )
    .await;
    let path = "/oagw/v1/proxy/127.0.0.1/v1/upload";
    let request = axum::http::Request::builder()
        .method(axum::http::Method::POST)
        .uri(path)
        .header(
            "content-length",
            (crate::infra::proxy::body::MAX_BODY_BYTES + 1).to_string(),
        )
        .body(axum::body::Body::empty())
        .unwrap();

    let error = problem_of(
        super::proxy_with_suffix(
            uri(path),
            Path(("127.0.0.1".to_owned(), "/v1/upload".to_owned())),
            Extension(engine),
            Extension(security(tenant())),
            request,
        )
        .await,
    );
    assert_eq!(error.status(), 413);
    assert_eq!(status_of_header(&error.into_response()), Some("gateway"));
}

#[tokio::test]
async fn a_websocket_upgrade_is_tunnelled_to_the_upstream() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // The upstream answers the handshake and echoes raw bytes.
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let engine = engine_for_alias(
        crate::domain::model::Protocol::Http,
        "127.0.0.1",
        upstream_port,
        "127.0.0.1",
        "/ws",
    )
    .await;
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (socket, _) = upstream.accept().await.unwrap();
        let (mut reader, mut writer) = socket.into_split();
        let mut head = Vec::new();
        let mut buffer = [0u8; 4096];
        while !head.ends_with(b"\r\n\r\n") {
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(read) => head.extend_from_slice(&buffer[..read]),
            }
        }
        let request = String::from_utf8_lossy(&head).to_string();
        let key = request
            .lines()
            .find_map(|line| line.strip_prefix("sec-websocket-key: "))
            .unwrap_or("")
            .trim()
            .to_owned();
        let reply = format!(
            "HTTP/1.1 101 Switching Protocols\r\nconnection: Upgrade\r\nupgrade: \
             websocket\r\nsec-websocket-accept: {key}\r\n\r\n"
        );
        writer.write_all(reply.as_bytes()).await.unwrap();
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(read) => {
                    if writer.write_all(&buffer[..read]).await.is_err() {
                        return;
                    }
                }
            }
        }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = listener.local_addr().unwrap();
    tokio::spawn(serve_proxy(engine, listener));

    let mut caller = tokio::net::TcpStream::connect(gateway).await.unwrap();
    let (mut reader, mut writer) = caller.split();
    let handshake = concat!(
        "GET /oagw/v1/proxy/127.0.0.1/ws HTTP/1.1\r\nhost: gateway.example\r\n",
        "connection: Upgrade\r\nupgrade: websocket\r\n",
        "sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
        "sec-websocket-version: 13\r\n\r\n"
    );
    writer.write_all(handshake.as_bytes()).await.unwrap();

    let mut head = Vec::new();
    let mut buffer = [0u8; 1024];
    while !head.ends_with(b"\r\n\r\n") {
        let read = reader.read(&mut buffer).await.unwrap();
        assert!(read > 0, "the gateway closed the handshake");
        head.extend_from_slice(&buffer[..read]);
    }
    let answer = String::from_utf8_lossy(&head).to_string();
    assert!(answer.starts_with("HTTP/1.1 101"), "{answer}");

    // A masked WebSocket text frame carrying `hello`, echoed back by the
    // upstream: the tunnel is bidirectional.
    let frame = masked_frame(b"hello");
    writer.write_all(&frame).await.unwrap();
    let mut echoed = Vec::new();
    let read = tokio::time::timeout(std::time::Duration::from_secs(5), reader.read(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    echoed.extend_from_slice(&buffer[..read]);
    assert_eq!(echoed, frame, "the tunnel must echo the caller's frames");
}

/// The platform mounts the proxy behind `RequestBodyLimitLayer`, which replaces
/// the request body with a bounded one. The tunnel must survive it.
#[tokio::test]
async fn the_tunnel_survives_a_bounded_request_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn bound(
        request: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        let (parts, body) = request.into_parts();
        let bounded = http_body_util::Limited::new(body, 64 * 1024 * 1024);
        next.run(axum::http::Request::from_parts(
            parts,
            axum::body::Body::new(bounded),
        ))
        .await
    }

    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let engine = engine_for_alias(
        crate::domain::model::Protocol::Http,
        "127.0.0.1",
        upstream_port,
        "127.0.0.1",
        "/ws",
    )
    .await;
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (socket, _) = upstream.accept().await.unwrap();
        let (mut reader, mut writer) = socket.into_split();
        let mut head = Vec::new();
        let mut buffer = [0u8; 4096];
        while !head.ends_with(b"\r\n\r\n") {
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(read) => head.extend_from_slice(&buffer[..read]),
            }
        }
        let request = String::from_utf8_lossy(&head).to_string();
        let key = request
            .lines()
            .find_map(|line| line.strip_prefix("sec-websocket-key: "))
            .unwrap_or("")
            .trim()
            .to_owned();
        let reply = format!(
            "HTTP/1.1 101 Switching Protocols\r\nconnection: Upgrade\r\nupgrade: \
             websocket\r\nsec-websocket-accept: {key}\r\n\r\n"
        );
        writer.write_all(reply.as_bytes()).await.unwrap();
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(read) => {
                    if writer.write_all(&buffer[..read]).await.is_err() {
                        return;
                    }
                }
            }
        }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let app = axum::Router::new()
            .route("/oagw/v1/proxy/{alias}", axum::routing::any(super::proxy))
            .route(
                "/oagw/v1/proxy/{alias}/{*path_suffix}",
                axum::routing::any(super::proxy_with_suffix),
            )
            .layer(axum::middleware::from_fn(bound))
            .layer(axum::Extension(std::sync::Arc::clone(&engine)))
            .layer(axum::Extension(security(tenant())));
        drop(axum::serve(listener, app).await);
    });

    let mut caller = tokio::net::TcpStream::connect(gateway).await.unwrap();
    let (mut reader, mut writer) = caller.split();
    let handshake = concat!(
        "GET /oagw/v1/proxy/127.0.0.1/ws HTTP/1.1\r\nhost: gateway.example\r\n",
        "connection: Upgrade\r\nupgrade: websocket\r\n",
        "sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
        "sec-websocket-version: 13\r\n\r\n"
    );
    writer.write_all(handshake.as_bytes()).await.unwrap();

    let mut head = Vec::new();
    let mut buffer = [0u8; 1024];
    while !head.ends_with(b"\r\n\r\n") {
        let read = reader.read(&mut buffer).await.unwrap();
        assert!(read > 0, "the gateway closed the handshake");
        head.extend_from_slice(&buffer[..read]);
    }
    assert!(
        String::from_utf8_lossy(&head).starts_with("HTTP/1.1 101"),
        "{}",
        String::from_utf8_lossy(&head)
    );

    let frame = masked_frame(b"hello");
    writer.write_all(&frame).await.unwrap();
    let read = tokio::time::timeout(std::time::Duration::from_secs(5), reader.read(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buffer[..read], &frame, "the tunnel must survive the limit");
}

#[tokio::test]
async fn a_streamed_answer_reaches_the_caller_before_the_upstream_finishes() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // The upstream writes one event, pauses, then the second one: the caller
    // must observe the first while the upstream is still holding the second.
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let engine = engine_for_alias(
        crate::domain::model::Protocol::Http,
        "127.0.0.1",
        upstream_port,
        "127.0.0.1",
        "/v1",
    )
    .await;
    tokio::spawn(async move {
        let (socket, _) = upstream.accept().await.unwrap();
        let (mut reader, mut writer) = socket.into_split();
        let mut head = Vec::new();
        let mut buffer = [0u8; 4096];
        while !head.ends_with(b"\r\n\r\n") {
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(read) => head.extend_from_slice(&buffer[..read]),
            }
        }
        drop(
            writer
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\ndata: \
                             first\n\n",
                )
                .await,
        );
        drop(writer.flush().await);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        drop(writer.write_all(b"data: second\n\n").await);
        drop(writer.shutdown().await);
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway = listener.local_addr().unwrap();
    tokio::spawn(serve_proxy(engine, listener));

    let mut caller = tokio::net::TcpStream::connect(gateway).await.unwrap();
    caller
        .write_all(
            b"GET /oagw/v1/proxy/127.0.0.1/v1/models HTTP/1.1\r\nhost: \
                     gateway.example\r\n\r\n",
        )
        .await
        .unwrap();

    let mut seen = Vec::new();
    let mut buffer = [0u8; 1024];
    read_until(&mut caller, &mut seen, &mut buffer, b"data: first").await;
    assert!(
        seen.starts_with(b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&seen)
    );
    assert!(
        !contains(&seen, b"data: second"),
        "the second event must not arrive before the upstream writes it: {}",
        String::from_utf8_lossy(&seen)
    );

    // The caller was mid-stream, not the reader of one buffered body.
    let observed_at = std::time::Instant::now();
    read_until(&mut caller, &mut seen, &mut buffer, b"data: second").await;
    assert!(
        observed_at.elapsed() >= std::time::Duration::from_millis(100),
        "the second event arrived {:?} later, so the answer was buffered",
        observed_at.elapsed()
    );
}

/// Read from `caller` until `needle` appears in `seen`, bounded by a budget.
async fn read_until(
    caller: &mut tokio::net::TcpStream,
    seen: &mut Vec<u8>,
    buffer: &mut [u8],
    needle: &[u8],
) {
    use tokio::io::AsyncReadExt;

    while !contains(seen, needle) {
        let read = tokio::time::timeout(std::time::Duration::from_secs(5), caller.read(buffer))
            .await
            .expect("the gateway did not answer in time")
            .expect("the gateway closed the stream");
        assert!(
            read > 0,
            "the stream ended before {needle:?}: {}",
            String::from_utf8_lossy(seen)
        );
        seen.extend_from_slice(&buffer[..read]);
    }
}

/// `true` when `haystack` contains `needle`.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Serve the two proxy routes the way the platform mounts them.
async fn serve_proxy(engine: Arc<super::ProxyEngine>, listener: tokio::net::TcpListener) {
    let app = axum::Router::new()
        .route("/oagw/v1/proxy/{alias}", axum::routing::any(super::proxy))
        .route(
            "/oagw/v1/proxy/{alias}/{*path_suffix}",
            axum::routing::any(super::proxy_with_suffix),
        )
        .layer(axum::Extension(engine))
        .layer(axum::Extension(security(tenant())));
    drop(axum::serve(listener, app).await);
}

/// One masked text frame, as a WebSocket client writes it.
fn masked_frame(payload: &[u8]) -> Vec<u8> {
    let length = u8::try_from(payload.len()).expect("the test frames stay short");
    let mut frame = vec![0x81u8];
    frame.push(0x80u8 | length);
    frame.extend_from_slice(&[0x2au8, 0x2bu8, 0x2cu8, 0x2du8]);
    for (index, byte) in payload.iter().enumerate() {
        frame.push(byte ^ [0x2au8, 0x2bu8, 0x2cu8, 0x2du8][index % 4]);
    }
    frame
}

/// An engine over one loopback upstream, addressed by its alias, with one route
/// on `path`.
async fn engine_for_alias(
    protocol: crate::domain::model::Protocol,
    host: &str,
    port: u16,
    alias: &str,
    path: &str,
) -> Arc<super::ProxyEngine> {
    engine_for_methods(
        protocol,
        host,
        port,
        alias,
        path,
        &[crate::domain::model::HttpMethod::Get],
    )
    .await
}

/// The same engine, with the route's accepted methods spelled out.
async fn engine_for_methods(
    protocol: crate::domain::model::Protocol,
    host: &str,
    port: u16,
    alias: &str,
    path: &str,
    methods: &[crate::domain::model::HttpMethod],
) -> Arc<super::ProxyEngine> {
    use crate::infra::plugin::oauth2_client_cred_auth::TokenCacheConfig;
    use crate::infra::plugin::registry::AuthPluginRegistry;
    use crate::infra::plugin::secrets::StaticSecretResolver;
    use crate::infra::proxy::policy::ProxyPolicy;
    use crate::infra::proxy::resolver::StaticHierarchy;
    use crate::infra::proxy::ssrf::SsrfGuard;
    use crate::infra::proxy::transport::UpstreamTransport;

    let store = Arc::new(MemoryStore::new());
    let upstreams = Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store)));
    let routes = Arc::new(MemoryRouteRepository::new(Arc::clone(&store)));
    let row = crate::domain::model::Upstream {
        id: uuid::Uuid::from_u128(0x11),
        tenant_id: tenant(),
        alias: alias.to_owned(),
        protocol,
        enabled: true,
        server: crate::domain::model::ServerConfig {
            endpoints: vec![crate::domain::model::Endpoint {
                scheme: crate::domain::model::EndpointScheme::Http,
                host: host.to_owned(),
                port,
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
    };
    // Blocking-free: the store is in-process.
    async {
        upstreams.insert(&row).await.unwrap();
        routes
            .insert(&crate::domain::model::Route {
                id: uuid::Uuid::from_u128(0x21),
                tenant_id: tenant(),
                upstream_id: row.id,
                r#match: crate::domain::model::MatchConfig {
                    http: Some(crate::domain::model::HttpMatch {
                        methods: methods.to_vec(),
                        path: path.to_owned(),
                        query_allowlist: Vec::new(),
                        path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
                    }),
                    grpc: None,
                },
                priority: 0,
                enabled: true,
                rate_limit: None,
                cors: None,
                plugins: None,
                tags: Vec::new(),
                created_at: "2026-01-01T00:00:00Z".to_owned(),
                updated_at: "2026-01-01T00:00:00Z".to_owned(),
            })
            .await
            .unwrap();
    }
    .await;
    let policy = ProxyPolicy::new(5, true);
    let cache_config = TokenCacheConfig::default();
    Arc::new(super::ProxyEngine::new(
        upstreams as Arc<dyn UpstreamRepository>,
        routes as Arc<dyn RouteRepository>,
        Arc::new(StaticHierarchy::new(Vec::new())),
        AuthPluginRegistry::with_builtins(
            Arc::new(StaticSecretResolver::default()),
            None,
            cache_config,
        ),
        SsrfGuard::disabled(),
        policy,
        UpstreamTransport::new(&policy).unwrap(),
    ))
}

// ── route cors (F8) ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_route_cors_override_round_trips() {
    let svc = service();
    let upstream = seeded_upstream(&svc).await;
    let upstream_id = format!("{UPSTREAM_TYPE}{}", upstream.id);

    let response = super::create_route(
        uri(ROUTES),
        Extension(Arc::clone(&svc)),
        Extension(security(tenant())),
        headers(),
        route_body_with_cors(
            &upstream_id,
            &serde_json::json!({
                "enabled": true,
                "allowed_origins": ["https://studio.example"],
                "allowed_methods": ["GET", "POST"],
                "allow_credentials": true
            }),
        ),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    assert_eq!(status_of_header(&response), Some("gateway"));

    let document = body_of(response).await;
    assert_eq!(document["cors"]["enabled"], true);
    assert_eq!(
        document["cors"]["allowed_origins"][0],
        "https://studio.example"
    );
    assert_eq!(document["cors"]["allowed_methods"][0], "GET");

    // The stored row carries it, and the read path projects it again.
    let route = svc
        .list_routes(
            &context(tenant()),
            &crate::domain::dto::ListQuery::default(),
        )
        .await
        .unwrap()
        .remove(0);
    let cors = route.cors.expect("the cors override is stored");
    assert!(cors.enabled);
    assert!(cors.allow_credentials);
}

#[tokio::test]
async fn a_route_cors_override_is_validated() {
    let svc = service();
    let upstream = seeded_upstream(&svc).await;
    let upstream_id = format!("{UPSTREAM_TYPE}{}", upstream.id);

    let error = problem_of(
        super::create_route(
            uri(ROUTES),
            Extension(Arc::clone(&svc)),
            Extension(security(tenant())),
            headers(),
            route_body_with_cors(
                &upstream_id,
                &serde_json::json!({
                    "enabled": true,
                    "allowed_origins": ["*"],
                    "allow_credentials": true
                }),
            ),
        )
        .await,
    );
    assert_eq!(error.status(), 400);
    assert_eq!(error.kind(), VALIDATION_ERROR);
    assert!(error.detail().contains("must not contain '*'"), "{error:?}");
}

// ── error-source header on success (F12) ───────────────────────────────────

/// `F12` — 200, 201, 204 and the Starlark source all carry the header.
#[tokio::test]
async fn every_success_response_declares_the_gateway_source() {
    let svc = service();
    let upstream = seeded_upstream(&svc).await;
    let upstream_id = upstream_id_of(&upstream);

    // 201 on create.
    let created = super::create_upstream(
        uri(UPSTREAMS),
        Extension(Arc::clone(&svc)),
        Extension(security(tenant())),
        headers(),
        Json(upstream_body(None, "created.openai.com")),
    )
    .await
    .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    assert_error_source(&created);

    // 200 on read.
    let read = super::get_upstream(
        uri(&format!("{UPSTREAMS}/{upstream_id}")),
        Extension(Arc::clone(&svc)),
        Extension(security(tenant())),
        headers(),
        Path(upstream_id),
    )
    .await
    .unwrap();
    assert_eq!(read.status(), StatusCode::OK);
    assert_eq!(status_of_header(&read), Some("gateway"));

    // 204 on delete.
    let deleted = super::delete_upstream(
        uri(&format!("{UPSTREAMS}/{}", upstream_id_of(&upstream))),
        Extension(Arc::clone(&svc)),
        Extension(security(tenant())),
        headers(),
        Path(upstream_id_of(&upstream)),
    )
    .await
    .unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert_error_source(&deleted);

    // 200 with the Starlark media type.
    let plugin = seeded_plugin(&svc).await;
    let source = super::get_plugin_source(
        uri(&format!("{PLUGINS}/{}/source", plugin_id_of(&plugin))),
        Extension(Arc::clone(&svc)),
        Extension(security(tenant())),
        headers(),
        Path(plugin_id_of(&plugin)),
    )
    .await
    .unwrap();
    assert_eq!(source.status(), StatusCode::OK);
    assert_eq!(status_of_header(&source), Some("gateway"));
}

// ── trace correlation (F13) ────────────────────────────────────────────────

#[tokio::test]
async fn a_traced_request_stamps_the_correlation_id_on_its_problems() {
    let svc = service();
    let mut headers = headers();
    headers.insert("x-trace-id", "abc".parse().unwrap());

    let error = problem_of(
        super::get_upstream(
            uri(&format!("{UPSTREAMS}/not-an-id")),
            Extension(Arc::clone(&svc)),
            Extension(security(tenant())),
            headers,
            Path("not-an-id".to_owned()),
        )
        .await,
    );
    assert_eq!(error.extensions().trace_id.as_deref(), Some("abc"));

    let document = body_of(error.into_response()).await;
    assert_eq!(document["trace_id"], "abc");
}

#[tokio::test]
async fn a_request_without_correlation_headers_has_no_trace_id() {
    let svc = service();
    let error = problem_of(
        super::get_upstream(
            uri(&format!("{UPSTREAMS}/not-an-id")),
            Extension(Arc::clone(&svc)),
            Extension(security(tenant())),
            headers(),
            Path("not-an-id".to_owned()),
        )
        .await,
    );
    assert!(error.extensions().trace_id.is_none(), "{error:?}");
    let document = body_of(error.into_response()).await;
    assert!(document.get("trace_id").is_none(), "{document}");
}

/// A `traceparent` correlation id wins over the convenience headers.
#[tokio::test]
async fn the_traceparent_header_is_preferred() {
    let svc = service();
    let mut headers = headers();
    headers.insert("traceparent", "invalid-value".parse().unwrap());
    headers.insert("x-trace-id", "from-x-trace-id".parse().unwrap());

    let error = problem_of(
        super::get_upstream(
            uri(&format!("{UPSTREAMS}/not-an-id")),
            Extension(Arc::clone(&svc)),
            Extension(security(tenant())),
            headers,
            Path("not-an-id".to_owned()),
        )
        .await,
    );
    assert_eq!(
        error.extensions().trace_id.as_deref(),
        Some("from-x-trace-id")
    );
}

// ── envelope shape ─────────────────────────────────────────────────────────

#[test]
fn the_page_meta_reports_the_effective_window() {
    let envelope = ListEnvelopeDto {
        items: Vec::new(),
        page_info: super::PageMetaDto {
            limit: 50,
            skip: 10,
        },
    };
    assert_eq!(envelope.page_info.limit, 50);
    assert_eq!(envelope.page_info.skip, 10);
}

#[test]
fn response_dtos_are_named_by_their_gts_id() {
    let row = PluginResponseDto {
        id: format!("{GUARD_PLUGIN_TYPE}3f2c"),
        plugin_type: PluginType::Guard,
        name: "require-tenant".to_owned(),
        config_schema: None,
        phases: Vec::new(),
        created_at: "2026-01-01T00:00:00Z".to_owned(),
        updated_at: "2026-01-01T00:00:00Z".to_owned(),
    };
    assert_eq!(row.id, format!("{GUARD_PLUGIN_TYPE}3f2c"));

    let route = crate::domain::model::Route {
        id: uuid::Uuid::nil(),
        tenant_id: tenant(),
        upstream_id: uuid::Uuid::from_u128(0xD004),
        r#match: MatchConfig {
            http: None,
            grpc: None,
        },
        priority: 0,
        enabled: true,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
        created_at: String::new(),
        updated_at: String::new(),
    };
    let response = RouteResponseDto::from_row(&route);
    assert_eq!(response.id, format!("{ROUTE_TYPE}{}", route.id));
    assert_eq!(
        response.upstream_id,
        format!("{UPSTREAM_TYPE}{}", route.upstream_id)
    );
}

#[tokio::test]
async fn a_cors_preflight_is_answered_here_and_never_proxied() {
    // The upstream is never dialled: the mock matches nothing, so a proxied
    // call would fail with a connection error instead of the permissive `204`
    // (`ADR`-0004, preflight request handling).
    let server = httpmock::MockServer::start();
    let engine = engine_for_alias(
        crate::domain::model::Protocol::Http,
        "127.0.0.1",
        server.port(),
        "127.0.0.1",
        "/v1",
    )
    .await;

    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::OPTIONS).path("/v1/users");
        then.status(204);
    });
    let response = proxied_options(
        &engine,
        "/oagw/v1/proxy/127.0.0.1/v1/users",
        &[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "POST"),
        ],
    )
    .await
    .expect("a preflight is never an error");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        mock.calls(),
        0,
        "the upstream is never dialled for a preflight"
    );
    assert_eq!(
        response.headers().get("access-control-allow-origin"),
        Some(&"https://app.example.com".parse().unwrap())
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-methods")
            .and_then(|value| value.to_str().ok()),
        Some("POST"),
        "the requested method must be echoed back"
    );
    assert_eq!(
        response.headers().get("access-control-max-age"),
        Some(&"86400".parse().unwrap())
    );
    let vary = response
        .headers()
        .get("vary")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        vary.contains("Access-Control-Request-Method"),
        "the preflight answer must vary on the request method, got '{vary}'"
    );
}

#[tokio::test]
async fn an_options_with_an_origin_but_no_request_method_is_not_a_preflight() {
    // `OPTIONS` carrying an `Origin` alone is a plain request the upstream
    // would answer, not a preflight: the gateway must not stamp its permissive
    // CORS answer on it. The contract's method allowlist admits no `OPTIONS`
    // route, so the call falls through to the routing miss.
    let server = httpmock::MockServer::start();
    let engine = engine_for_alias(
        crate::domain::model::Protocol::Http,
        "127.0.0.1",
        server.port(),
        "127.0.0.1",
        "/v1",
    )
    .await;

    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::OPTIONS).path("/v1/users");
        then.status(204);
    });
    let error = proxied_options(
        &engine,
        "/oagw/v1/proxy/127.0.0.1/v1/users",
        &[("origin", "https://app.example.com")],
    )
    .await
    .expect_err("no route of the contract admits OPTIONS");
    assert_eq!(error.status(), 404);
    assert_eq!(error.kind(), ROUTE_NOT_FOUND_ERROR);
    assert_eq!(mock.calls(), 0);
}

#[tokio::test]
async fn a_preflight_never_reaches_the_upstream() {
    // The mock matches nothing: a dialled call would fail with a connection
    // error instead of the permissive `204` (`ADR`-0004, preflight request
    // handling).
    let server = httpmock::MockServer::start();
    let engine = engine_for_alias(
        crate::domain::model::Protocol::Http,
        "127.0.0.1",
        server.port(),
        "127.0.0.1",
        "/v1",
    )
    .await;

    let mock = server.mock(|when, then| {
        when.method(httpmock::Method::OPTIONS).path("/v1/users");
        then.status(204);
    });
    let response = proxied_options(
        &engine,
        "/oagw/v1/proxy/127.0.0.1/v1/users",
        &[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "POST"),
        ],
    )
    .await
    .expect("a preflight is never an error");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(mock.calls(), 0, "the preflight must be answered locally");
}

#[tokio::test]
async fn a_content_length_body_is_not_reframed_as_chunked() {
    let server = httpmock::MockServer::start();
    server.mock(|when, then| {
        when.method(httpmock::Method::GET).path("/v1/models");
        then.status(200)
            .header("content-length", "15")
            .body("seventeen bytes");
    });
    let engine = engine_for_alias(
        crate::domain::model::Protocol::Http,
        "127.0.0.1",
        server.port(),
        "127.0.0.1",
        "/v1",
    )
    .await;

    let response = proxied(&engine, "/oagw/v1/proxy/127.0.0.1/v1/models", None)
        .await
        .expect("the proxied call must succeed");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-length")
            .map(axum::http::HeaderValue::as_bytes),
        Some(b"15".as_slice()),
        "the upstream's declared size must survive the proxy"
    );
}
