//! Handler-level tests: every handler is driven directly through its
//! extractors, which keeps the assertions on status codes and bodies without
//! dragging the OpenAPI registry into the test.
use axum::{
    Extension, Json,
    extract::{Path, RawQuery},
    http::{StatusCode, Uri},
};
use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{
    CreatePluginRequest, EndpointPoolDto, PluginChainDto, RouteRequest, UpstreamRequest,
};
use crate::api::rest::error::{ApiError, ApiResult};
use crate::api::rest::handlers;
use crate::api::rest::handlers::SharedControlPlane;
use crate::config::OagwConfig;

fn context() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_type("user")
        .subject_tenant_id(Uuid::new_v4())
        .build()
        .expect("security context")
}

fn service() -> SharedControlPlane {
    handlers::control_plane(&OagwConfig::default())
}

fn route_request(upstream_id: Uuid, path: &str) -> RouteRequest {
    serde_json::from_value(json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": path, "path_suffix_mode": "append" } },
    }))
    .expect("route request")
}

fn endpoint_pool(host: &str) -> EndpointPoolDto {
    endpoint_pools(&[host])
}

/// An endpoint-pool payload with several hosts.
fn endpoint_pools(hosts: &[&str]) -> EndpointPoolDto {
    let endpoints: Vec<Value> = hosts
        .iter()
        .map(|host| json!({ "scheme": "https", "host": host, "port": 443 }))
        .collect();
    serde_json::from_value(json!({ "endpoints": endpoints })).expect("endpoint pool")
}

/// Turn a handler result into an axum response.
fn respond<T: axum::response::IntoResponse>(result: ApiResult<T>) -> axum::response::Response {
    match result {
        Ok(response) => axum::response::IntoResponse::into_response(response),
        Err(error) => axum::response::IntoResponse::into_response(error),
    }
}

/// Render a handler result, returning the status and the parsed body.
async fn render<T: axum::response::IntoResponse>(result: ApiResult<T>) -> (StatusCode, Value) {
    let response = respond(result);
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body")
        .to_vec();
    if bytes.is_empty() {
        (status, Value::Null)
    } else {
        (status, serde_json::from_slice(&bytes).expect("json body"))
    }
}

/// Render a handler result that carries no body, returning the status.
async fn render_empty<T: axum::response::IntoResponse>(result: ApiResult<T>) -> StatusCode {
    respond(result).status()
}

#[tokio::test]
async fn creating_an_upstream_derives_the_alias_and_advertises_the_location() {
    let ctx = context();
    let svc = service();
    let uri = Uri::from_static("/oagw/v1/upstreams");
    let result = handlers::create_upstream(
        uri,
        Extension(ctx.clone()),
        Extension(svc.clone()),
        Json(upstream_request("api.OpenAI.com")),
    )
    .await;

    let (status, body) = render(result).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["alias"], "api.openai.com");
    assert_eq!(body["server"]["endpoints"][0]["host"], "api.openai.com");
    assert_eq!(
        body["protocol"],
        "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    );
}

fn upstream_request(host: &str) -> UpstreamRequest {
    pool_request(&[host])
}

/// An upstream request whose endpoints all sit under one registrable domain,
/// so the alias derivation is stable across pool changes.
fn pool_request(hosts: &[&str]) -> UpstreamRequest {
    let endpoints: Vec<Value> = hosts
        .iter()
        .map(|host| json!({ "scheme": "https", "host": host, "port": 443 }))
        .collect();
    serde_json::from_value(json!({
        "server": { "endpoints": endpoints },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    }))
    .expect("upstream request")
}

#[tokio::test]
async fn an_unknown_protocol_is_a_400_validation_problem() {
    let ctx = context();
    let svc = service();
    let request: UpstreamRequest = serde_json::from_value(json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "api.example.com", "port": 443 }] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.mqtt.v1",
    }))
    .expect("request");
    let result = handlers::create_upstream(
        Uri::from_static("/oagw/v1/upstreams"),
        Extension(ctx),
        Extension(svc),
        Json(request),
    )
    .await;
    let (status, body) = render(result).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], crate::api::rest::error::VALIDATION_TYPE);
}

#[tokio::test]
async fn listing_applies_top_skip_and_select() {
    let ctx = context();
    let svc = service();
    for host in ["a.example.com", "b.example.com", "c.example.com"] {
        handlers::create_upstream(
            Uri::from_static("/oagw/v1/upstreams"),
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Json(upstream_request(host)),
        )
        .await
        .expect("create");
    }

    let result = handlers::list_upstreams(
        Extension(ctx.clone()),
        Extension(svc.clone()),
        RawQuery(Some("$orderby=alias%20desc&$top=2&$skip=1".to_owned())),
    )
    .await;
    let (status, body) = render(result).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 3);
    assert_eq!(body["top"], 2);
    assert_eq!(body["skip"], 1);
    let aliases: Vec<&str> = body["items"]
        .as_array()
        .expect("items")
        .iter()
        .map(|item| item["alias"].as_str().expect("alias"))
        .collect();
    assert_eq!(aliases, ["b.example.com", "a.example.com"]);

    // `$select` projects the response down to the requested fields.
    let result = handlers::list_upstreams(
        Extension(ctx),
        Extension(svc),
        RawQuery(Some("$select=id,alias".to_owned())),
    )
    .await;
    let (status, body) = render(result).await;
    assert_eq!(status, StatusCode::OK);
    let first = &body["items"][0];
    assert!(first.get("alias").is_some());
    assert!(
        first.get("server").is_none(),
        "unselected fields must disappear"
    );
}

#[tokio::test]
async fn an_unknown_list_parameter_is_rejected() {
    let ctx = context();
    let svc = service();
    let result = handlers::list_upstreams(
        Extension(ctx),
        Extension(svc),
        RawQuery(Some("$top=5&frobnicate=1".to_owned())),
    )
    .await;
    let (status, _) = render(result).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_put_delete_round_trip() {
    let ctx = context();
    let svc = service();
    let (_, created) = render(
        handlers::create_upstream(
            Uri::from_static("/oagw/v1/upstreams"),
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Json(upstream_request("api.example.com")),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();

    // GET by bare UUID and by the anonymous GTS id both resolve.
    let (status, fetched) = render(
        handlers::get_upstream(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path(id.clone()),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["id"].as_str().expect("id"), id);

    let gts_id = format!("gts.cf.core.oagw.upstream.v1~{id}");
    let (status, again) = render(
        handlers::get_upstream(Extension(ctx.clone()), Extension(svc.clone()), Path(gts_id)).await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again["id"], fetched["id"]);

    // PUT clears omitted optional fields and keeps the identity.
    let mut replacement = upstream_request("api.example.com");
    replacement.enabled = Some(false);
    replacement.tags = Some(vec!["edge".to_owned()]);
    let (status, replaced) = render(
        handlers::replace_upstream(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path(id.clone()),
            Json(replacement),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!replaced["enabled"].as_bool().expect("enabled"));
    assert_eq!(replaced["tags"].as_array().map(Vec::len), Some(1));
    assert_eq!(replaced["id"], fetched["id"]);

    // DELETE returns 204 and the resource is gone.
    let status = render_empty(
        handlers::delete_upstream(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path(id.clone()),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) =
        render(handlers::get_upstream(Extension(ctx), Extension(svc), Path(id)).await).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn echoing_a_different_tenant_is_an_immutable_field_violation() {
    let ctx = context();
    let svc = service();
    let (status, created) = render(
        handlers::create_upstream(
            Uri::from_static("/oagw/v1/upstreams"),
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Json(upstream_request("api.example.com")),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id: Uuid = created["id"].as_str().expect("id").parse().expect("uuid");

    let mut request = upstream_request("api.example.com");
    request.id = Some(id);
    request.tenant_id = Some(Uuid::new_v4());
    let (status, body) = render(
        handlers::replace_upstream(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path(id.to_string()),
            Json(request),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["context"]["field"], "tenant_id");
}

#[tokio::test]
async fn another_tenant_upstream_is_a_404() {
    let ctx = context();
    let svc = service();
    let (_, created) = render(
        handlers::create_upstream(
            Uri::from_static("/oagw/v1/upstreams"),
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Json(upstream_request("api.example.com")),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();
    let (status, body) =
        render(handlers::get_upstream(Extension(context()), Extension(svc), Path(id)).await).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["type"], crate::api::rest::error::NOT_FOUND_TYPE);
}

#[tokio::test]
async fn a_malformed_resource_id_is_a_400() {
    let (status, _) = render(
        handlers::get_upstream(
            Extension(context()),
            Extension(service()),
            Path("not-an-id".to_owned()),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn enable_and_disable_round_trip_over_the_wire() {
    let ctx = context();
    let svc = service();
    let (_, created) = render(
        handlers::create_upstream(
            Uri::from_static("/oagw/v1/upstreams"),
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Json(upstream_request("api.example.com")),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();
    let (status, disabled) = render(
        handlers::disable_upstream(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path(id.clone()),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!disabled["enabled"].as_bool().expect("enabled"));
    let (status, enabled) =
        render(handlers::enable_upstream(Extension(ctx), Extension(svc), Path(id)).await).await;
    assert_eq!(status, StatusCode::OK);
    assert!(enabled_is(&enabled));
}

fn enabled_is(body: &Value) -> bool {
    body["enabled"].as_bool().expect("enabled")
}

#[tokio::test]
async fn the_endpoint_pool_is_grown_replaced_and_shrunk() {
    let ctx = context();
    let svc = service();
    let (_, created) = render(
        handlers::create_upstream(
            Uri::from_static("/oagw/v1/upstreams"),
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Json(pool_request(&["api.eu.example.com", "api.us.example.com"])),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();
    assert_eq!(created["alias"], "example.com");

    // POST appends; the derivation stays `example.com`, so the alias holds.
    let (status, grown) = render(
        handlers::add_endpoints(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path(id.clone()),
            Json(endpoint_pool("api.ap.example.com")),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        grown["server"]["endpoints"].as_array().map(Vec::len),
        Some(3)
    );

    // GET returns the pool only.
    let (status, pool) = render(
        handlers::get_endpoints(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path(id.clone()),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pool["endpoints"].as_array().map(Vec::len), Some(3));

    // PUT replaces the whole pool.
    let (status, replaced) = render(
        handlers::replace_endpoints(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path(id.clone()),
            Json(endpoint_pools(&[
                "api.eu.example.com",
                "api.us.example.com",
            ])),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        replaced["server"]["endpoints"].as_array().map(Vec::len),
        Some(2)
    );

    // DELETE removes one position; the remaining pair still derives
    // `example.com`, so the alias survives.
    let (status, _) = render(
        handlers::add_endpoints(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path(id.clone()),
            Json(endpoint_pool("api.ap.example.com")),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let status = render_empty(
        handlers::delete_endpoint(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path((id.clone(), "2".to_owned())),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, pool) = render(
        handlers::get_endpoints(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path(id.clone()),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pool["endpoints"].as_array().map(Vec::len), Some(2));

    // An out-of-range position is a 404, not a silent no-op.
    let (status, _) = render(
        handlers::delete_endpoint(Extension(ctx), Extension(svc), Path((id, "9".to_owned()))).await,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "out of range");
}

#[tokio::test]
async fn an_endpoint_pool_must_stay_homogeneous() {
    let ctx = context();
    let svc = service();
    let (_, created) = render(
        handlers::create_upstream(
            Uri::from_static("/oagw/v1/upstreams"),
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Json(upstream_request("api.example.com")),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().expect("id").to_owned();
    let pool: EndpointPoolDto = serde_json::from_value(json!({
        "endpoints": [
            { "scheme": "https", "host": "api.example.com", "port": 443 },
            { "scheme": "https", "host": "api.example.com", "port": 8443 },
        ]
    }))
    .expect("pool");
    let (status, body) = render(
        handlers::replace_endpoints(Extension(ctx), Extension(svc), Path(id), Json(pool)).await,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["context"]["field"], "server.endpoints");
}

#[tokio::test]
async fn route_crud_and_match_key_conflicts() {
    let ctx = context();
    let svc = service();
    let (_, created) = render(
        handlers::create_upstream(
            Uri::from_static("/oagw/v1/upstreams"),
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Json(upstream_request("api.example.com")),
        )
        .await,
    )
    .await;
    let upstream_id: Uuid = created["id"].as_str().expect("id").parse().expect("uuid");

    let (status, route) = render(
        handlers::create_route(
            Uri::from_static("/oagw/v1/routes"),
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Json(route_request(upstream_id, "/v1")),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(route["match"]["http"]["path"], "/v1");

    // The same match key twice is a 409.
    let (status, body) = render(
        handlers::create_route(
            Uri::from_static("/oagw/v1/routes"),
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Json(route_request(upstream_id, "/v1")),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body["context"]["resource_type"],
        "gts.cf.core.oagw.route.v1~"
    );

    // Listing by `$filter` on `upstream_id` finds it.
    let filter = format!("$filter=upstream_id%20eq%20'{upstream_id}'");
    let (status, page) = render(
        handlers::list_routes(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            RawQuery(Some(filter)),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["total"], 1);

    // A route against a foreign upstream is a 404.
    let (status, _) = render(
        handlers::create_route(
            Uri::from_static("/oagw/v1/routes"),
            Extension(context()),
            Extension(svc.clone()),
            Json(route_request(upstream_id, "/v1")),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let route_id = route["id"].as_str().expect("id").to_owned();
    let status = render_empty(
        handlers::delete_route(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path(route_id),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn plugin_lifecycle_including_the_in_use_409() {
    let ctx = context();
    let svc = service();
    let request: CreatePluginRequest = serde_json::from_value(json!({
        "name": "signer",
        "type": "guard",
        "source": "def plugin(ctx): pass",
    }))
    .expect("plugin request");

    let (status, created) = render(
        handlers::create_plugin(
            Uri::from_static("/oagw/v1/plugins"),
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Json(request.clone()),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = created["id"].as_str().expect("id").to_owned();
    assert_eq!(created["type"], "guard");

    // The source endpoint returns the Starlark text.
    let (status, source) = render(
        handlers::get_plugin_source(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path(id.clone()),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(source["source"], "def plugin(ctx): pass");
    assert!(
        source["plugin_id"]
            .as_str()
            .expect("plugin id")
            .starts_with("gts.cf.core.oagw.guard_plugin.v1~")
    );

    // Deleting a row that does not exist is a 404.
    let (status, _) = render(
        handlers::get_plugin(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path(Uuid::new_v4().to_string()),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Binding it makes the delete a 409 that names the referencing upstream.
    let (_, upstream) = render(
        handlers::create_upstream(
            Uri::from_static("/oagw/v1/upstreams"),
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Json(upstream_request("api.example.com")),
        )
        .await,
    )
    .await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();
    let chain: PluginChainDto = serde_json::from_value(json!({ "items": [id] })).expect("chain");
    let (status, _) = render(
        handlers::put_upstream_plugins(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path(upstream_id.clone()),
            Json(chain),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = render(
        handlers::delete_plugin(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path(id.clone()),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["type"], crate::api::rest::error::PLUGIN_IN_USE_TYPE);
    assert_eq!(
        body["context"]["referenced_by"]["upstreams"][0],
        format!("gts.cf.core.oagw.upstream.v1~{upstream_id}")
    );

    // Unbinding frees the plugin again.
    let (status, _) = render(
        handlers::delete_upstream_plugin(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path((upstream_id, "0".to_owned())),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let status =
        render_empty(handlers::delete_plugin(Extension(ctx), Extension(svc), Path(id)).await).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn plugin_names_must_be_unique_per_tenant() {
    let ctx = context();
    let svc = service();
    let request: CreatePluginRequest = serde_json::from_value(json!({
        "name": "dup",
        "type": "transform",
        "source": "def plugin(ctx): pass",
    }))
    .expect("plugin request");
    let (status, _) = render(
        handlers::create_plugin(
            Uri::from_static("/oagw/v1/plugins"),
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Json(request.clone()),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = render(
        handlers::create_plugin(
            Uri::from_static("/oagw/v1/plugins"),
            Extension(ctx),
            Extension(svc),
            Json(request),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["type"], crate::api::rest::error::ALREADY_EXISTS_TYPE);
}

#[tokio::test]
async fn a_catalogued_only_plugin_cannot_be_bound() {
    let ctx = context();
    let svc = service();
    let (_, upstream) = render(
        handlers::create_upstream(
            Uri::from_static("/oagw/v1/upstreams"),
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Json(upstream_request("api.example.com")),
        )
        .await,
    )
    .await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();
    let chain: PluginChainDto = serde_json::from_value(json!({
        "items": ["gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1"]
    }))
    .expect("chain");
    let (status, body) = render(
        handlers::put_upstream_plugins(
            Extension(ctx),
            Extension(svc),
            Path(upstream_id),
            Json(chain),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["type"], crate::api::rest::error::VALIDATION_TYPE);
    assert_eq!(
        body["context"]["reason"],
        crate::domain::reason::PLUGIN_UNKNOWN
    );
}

#[tokio::test]
async fn plugin_chain_appends_and_positional_delete() {
    let ctx = context();
    let svc = service();
    let (_, upstream) = render(
        handlers::create_upstream(
            Uri::from_static("/oagw/v1/upstreams"),
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Json(upstream_request("api.example.com")),
        )
        .await,
    )
    .await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();

    let first: PluginChainDto = serde_json::from_value(json!({
        "items": ["gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"]
    }))
    .expect("chain");
    let (status, _) = render(
        handlers::put_upstream_plugins(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path(upstream_id.clone()),
            Json(first),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let second: PluginChainDto = serde_json::from_value(json!({
        "items": ["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"]
    }))
    .expect("chain");
    let (status, appended) = render(
        handlers::add_upstream_plugins(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path(upstream_id.clone()),
            Json(second),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        appended["plugins"]["items"].as_array().map(Vec::len),
        Some(2)
    );

    let (status, chain) = render(
        handlers::get_upstream_plugins(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path(upstream_id.clone()),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        chain["items"][0],
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
    );

    let status = render_empty(
        handlers::delete_upstream_plugin(
            Extension(ctx.clone()),
            Extension(svc),
            Path((upstream_id, "0".to_owned())),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn deleting_an_upstream_cascades_its_routes() {
    let ctx = context();
    let svc = service();
    let (_, upstream) = render(
        handlers::create_upstream(
            Uri::from_static("/oagw/v1/upstreams"),
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Json(upstream_request("api.example.com")),
        )
        .await,
    )
    .await;
    let upstream_id: Uuid = upstream["id"].as_str().expect("id").parse().expect("uuid");
    let (status, _) = render(
        handlers::create_route(
            Uri::from_static("/oagw/v1/routes"),
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Json(route_request(upstream_id, "/v1")),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let status = render_empty(
        handlers::delete_upstream(
            Extension(ctx.clone()),
            Extension(svc.clone()),
            Path(upstream_id.to_string()),
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, page) =
        render(handlers::list_routes(Extension(ctx), Extension(svc), RawQuery(None)).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["total"], 0);
}

#[test]
fn the_reference_projections_agree_with_the_domain() {
    let tenant = Uuid::new_v4();
    let plugin = crate::domain::model::Plugin {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        name: "p".to_owned(),
        kind: crate::domain::model::PluginKind::Guard,
        description: None,
        config: None,
        source: "def plugin(ctx): pass".to_owned(),
        created_at: 0,
        updated_at: 0,
    };
    let mut upstream = crate::domain::model::Upstream {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        alias: "api.example.com".to_owned(),
        enabled: true,
        tags: vec![],
        server: crate::domain::model::ServerConfig {
            endpoints: vec![
                crate::domain::model::Endpoint::new(
                    crate::domain::model::EndpointScheme::Https,
                    "api.example.com",
                    Some(443),
                )
                .expect("endpoint"),
            ],
        },
        protocol: crate::domain::model::Protocol::Http,
        auth: None,
        headers: None,
        plugins: Some(crate::domain::model::PluginsConfig {
            sharing: crate::domain::model::SharingMode::Private,
            items: vec![crate::domain::model::PluginBinding::bare(plugin.gts_id())],
        }),
        rate_limit: None,
        cors: None,
        created_at: 0,
        updated_at: 0,
    };
    assert!(handlers::references_upstream(&upstream, &plugin));
    upstream.plugins = None;
    assert!(!handlers::references_upstream(&upstream, &plugin));
    assert!(handlers::endpoint_pooled(
        &upstream,
        &upstream.server.endpoints[0]
    ));
    assert_eq!(handlers::endpoint_pool(&upstream).endpoints.len(), 1);

    let mut route = crate::domain::model::Route {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        upstream_id: upstream.id,
        name: None,
        tags: vec![],
        matcher: crate::domain::model::RouteMatcher::Http(crate::domain::model::HttpMatch {
            methods: vec!["GET".to_owned()],
            path: "/v1".to_owned(),
            path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
            query_allowlist: Vec::new(),
        }),
        priority: 0,
        enabled: true,
        plugins: None,
        rate_limit: None,
        cors: None,
        created_at: 0,
        updated_at: 0,
    };
    assert!(!handlers::references_route(&route, &plugin));
    route.plugins = Some(crate::domain::model::PluginsConfig {
        sharing: crate::domain::model::SharingMode::Private,
        items: vec![crate::domain::model::PluginBinding::bare(
            plugin.id.to_string(),
        )],
    });
    assert!(handlers::references_route(&route, &plugin));
}

#[test]
fn the_409_reference_projection_is_wire_shaped() {
    let references = crate::domain::error::PluginReferences {
        upstreams: vec!["gts.cf.core.oagw.upstream.v1~a".to_owned()],
        routes: vec!["gts.cf.core.oagw.route.v1~b".to_owned()],
    };
    let projected = handlers::referenced_by(&references);
    assert_eq!(projected.upstreams, vec!["gts.cf.core.oagw.upstream.v1~a"]);
    assert_eq!(projected.routes, vec!["gts.cf.core.oagw.route.v1~b"]);
}

#[test]
fn api_errors_render_as_problem_documents() {
    let response = axum::response::IntoResponse::into_response(ApiError::validation("bad"));
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json")
    );
}
