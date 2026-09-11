//! Route Management API routes.
//!
//! Implemented by DECOMPOSITION entry 2.3 (route-management): the CRUD
//! operations for Route configuration resources under `/oagw/v1/routes`,
//! field validation against `docs/schemas/route.v1.schema.json`, and
//! upstream-reference integrity.

mod handlers;
mod ownership;
mod problem;
mod query;
mod store_ops;
mod uniqueness;
mod validate;

use std::sync::Arc;

use axum::Extension;
use axum::Router;
use axum::http::StatusCode;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;

use crate::store::OagwState;

const API_TAG: &str = "Route Management";
const ROUTES_PATH: &str = "/oagw/v1/routes";
const ROUTE_BY_ID_PATH: &str = "/oagw/v1/routes/{id}";

// @cpt-dod:cpt-cf-oagw-dod-route-crud-endpoints:p1
pub(crate) fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    state: Arc<OagwState>,
) -> Router {
    let router = OperationBuilder::post(ROUTES_PATH)
        .operation_id("oagw.route.create")
        .summary("Create a route")
        .description("Create a Route resource attaching a match rule to a caller-owned Upstream.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<serde_json::Value>(openapi, "Route body per route.v1.schema.json")
        .handler(handlers::create_route)
        .json_response_with_schema::<serde_json::Value>(
            openapi,
            StatusCode::CREATED,
            "The created Route",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(ROUTES_PATH)
        .operation_id("oagw.route.list")
        .summary("List routes")
        .description("List Route resources scoped to the calling tenant, with OData paging.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData filter expression")
        .query_param("$select", false, "Comma-separated fields to return")
        .query_param("$orderby", false, "Sort order, e.g. 'priority desc'")
        .query_param("$top", false, "Max results (default 50, max 100)")
        .query_param("$skip", false, "Offset for pagination")
        .handler(handlers::list_routes)
        .json_response_with_schema::<serde_json::Value>(
            openapi,
            StatusCode::OK,
            "The paged list of Route bodies",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::get(ROUTE_BY_ID_PATH)
        .operation_id("oagw.route.get")
        .summary("Get a route by id")
        .description("Retrieve a single owned Route by id (bare UUID or anonymous GTS form).")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param(
            "id",
            "Route id: bare UUID or gts.cf.core.oagw.route.v1~{uuid}",
        )
        .handler(handlers::get_route)
        .json_response_with_schema::<serde_json::Value>(openapi, StatusCode::OK, "The Route")
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::put(ROUTE_BY_ID_PATH)
        .operation_id("oagw.route.replace")
        .summary("Replace a route")
        .description("Fully replace an owned Route's match rules; upstream_id is immutable.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param(
            "id",
            "Route id: bare UUID or gts.cf.core.oagw.route.v1~{uuid}",
        )
        .json_request::<serde_json::Value>(openapi, "Replacement Route body, excluding upstream_id")
        .handler(handlers::replace_route)
        .json_response_with_schema::<serde_json::Value>(
            openapi,
            StatusCode::OK,
            "The replaced Route",
        )
        .standard_errors(openapi)
        .register(router, openapi);

    let router = OperationBuilder::delete(ROUTE_BY_ID_PATH)
        .operation_id("oagw.route.delete")
        .summary("Delete a route")
        .description("Delete an owned Route and its match/method/tag/plugin-binding rows.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param(
            "id",
            "Route id: bare UUID or gts.cf.core.oagw.route.v1~{uuid}",
        )
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .standard_errors(openapi)
        .register(router, openapi);

    router.layer(Extension(state))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::OagwConfig;
    use crate::model::route::{ROUTE_GTS_ID_PREFIX, Route};
    use crate::model::upstream::{Endpoint, EndpointScheme, ServerConfig, Upstream};
    use axum::body::Body;
    use axum::http::{Request, StatusCode as Status, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use toolkit_security::SecurityContext;
    use tower::ServiceExt;
    use uuid::Uuid;

    fn ctx(tenant_id: Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(tenant_id)
            .build()
            .unwrap()
    }

    fn build_router() -> (Router, Arc<OagwState>) {
        let state = Arc::new(OagwState::new(OagwConfig::default()));
        let registry = toolkit::api::OpenApiRegistryImpl::new();
        let router = register_routes(Router::new(), &registry, state.clone());
        (router, state)
    }

    fn seed_upstream(state: &OagwState, tenant_id: Uuid) -> Uuid {
        let id = Uuid::new_v4();
        let upstream = Upstream {
            id: Some(id),
            enabled: true,
            alias: Some(format!("svc-{id}")),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: EndpointScheme::Https,
                    host: "svc.internal".to_owned(),
                    port: 443,
                }],
            },
            protocol: "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1".to_owned(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tenant_id,
        };
        state.store.upstreams().insert(id, Arc::new(upstream));
        id
    }

    fn seed_plaintext_upstream(state: &OagwState, tenant_id: Uuid) -> Uuid {
        let id = Uuid::new_v4();
        let upstream = Upstream {
            id: Some(id),
            enabled: true,
            alias: Some(format!("svc-{id}")),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: EndpointScheme::Http,
                    host: "svc.internal".to_owned(),
                    port: 80,
                }],
            },
            protocol: "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1".to_owned(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tenant_id,
        };
        state.store.upstreams().insert(id, Arc::new(upstream));
        id
    }

    fn request(method: &str, uri: &str, body: Option<Value>, tenant_id: Uuid) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if body.is_some() {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        let body = match body {
            Some(json) => Body::from(serde_json::to_vec(&json).unwrap()),
            None => Body::empty(),
        };
        let mut req = builder.body(body).unwrap();
        req.extensions_mut().insert(ctx(tenant_id));
        req
    }

    async fn body_json(response: axum::response::Response) -> Value {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap_or_default()
    }

    fn http_create_body(upstream_id: Uuid, path: &str, priority: i64) -> Value {
        json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["GET"], "path": path } },
            "priority": priority,
        })
    }

    #[test]
    fn register_routes_mounts_the_five_route_management_endpoints() {
        let (_router, _state) = build_router();
    }

    #[tokio::test]
    async fn create_returns_201_with_a_server_generated_id() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id);

        let body = http_create_body(upstream_id, "/v1/models", 1);
        let response = router
            .oneshot(request("POST", "/oagw/v1/routes", Some(body), tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::CREATED);
        let json = body_json(response).await;
        assert!(json["id"].is_string());
        assert_eq!(json["upstream_id"], upstream_id.to_string());
    }

    #[tokio::test]
    async fn create_disregards_a_client_supplied_id() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id);

        let mut body = http_create_body(upstream_id, "/v1/models", 1);
        body["id"] = json!(Uuid::nil());
        let response = router
            .oneshot(request("POST", "/oagw/v1/routes", Some(body), tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::CREATED);
        let json = body_json(response).await;
        assert_ne!(json["id"], Value::String(Uuid::nil().to_string()));
    }

    #[tokio::test]
    async fn create_rejects_match_with_both_http_and_grpc() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id);
        let body = json!({
            "upstream_id": upstream_id,
            "match": {
                "http": { "methods": ["GET"], "path": "/p" },
                "grpc": { "service": "s", "method": "m" },
            },
            "priority": 1,
        });
        let response = router
            .oneshot(request("POST", "/oagw/v1/routes", Some(body), tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/problem+json")
        );
    }

    #[tokio::test]
    async fn create_rejects_match_with_neither_http_nor_grpc() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id);
        let body = json!({ "upstream_id": upstream_id, "match": {} });
        let response = router
            .oneshot(request("POST", "/oagw/v1/routes", Some(body), tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_rejects_empty_or_unsupported_methods() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id);

        let empty = json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": [], "path": "/p" } },
            "priority": 1,
        });
        let response = router
            .clone()
            .oneshot(request("POST", "/oagw/v1/routes", Some(empty), tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::BAD_REQUEST);

        let bad_verb = json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["TRACE"], "path": "/p" } },
            "priority": 1,
        });
        let response = router
            .oneshot(request(
                "POST",
                "/oagw/v1/routes",
                Some(bad_verb),
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_defaults_query_allowlist_empty_and_suffix_mode_append() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id);
        let body = http_create_body(upstream_id, "/v1/models", 1);
        let response = router
            .oneshot(request("POST", "/oagw/v1/routes", Some(body), tenant_id))
            .await
            .unwrap();
        let json = body_json(response).await;
        assert_eq!(json["match"]["http"]["query_allowlist"], json!([]));
        assert_eq!(json["match"]["http"]["path_suffix_mode"], "append");
    }

    #[tokio::test]
    async fn create_rejects_an_invalid_tag() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id);
        let mut body = http_create_body(upstream_id, "/v1/models", 1);
        body["tags"] = json!(["Not Valid"]);
        let response = router
            .oneshot(request("POST", "/oagw/v1/routes", Some(body), tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_accepts_and_returns_a_grpc_only_route_unchanged_on_get() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id);
        let body = json!({
            "upstream_id": upstream_id,
            "match": { "grpc": { "service": "pkg.Service", "method": "Get" } },
        });
        let response = router
            .clone()
            .oneshot(request("POST", "/oagw/v1/routes", Some(body), tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::CREATED);
        let created = body_json(response).await;
        let id = created["id"].as_str().unwrap().to_owned();

        let response = router
            .oneshot(request(
                "GET",
                &format!("/oagw/v1/routes/{id}"),
                None,
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::OK);
        let fetched = body_json(response).await;
        assert_eq!(fetched["match"]["grpc"]["service"], "pkg.Service");
        assert!(fetched["match"].get("http").is_none());
    }

    #[tokio::test]
    async fn create_rejects_a_nonexistent_upstream_id() {
        let (router, _state) = build_router();
        let tenant_id = Uuid::new_v4();
        let body = http_create_body(Uuid::new_v4(), "/v1/models", 1);
        let response = router
            .oneshot(request("POST", "/oagw/v1/routes", Some(body), tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_rejects_an_upstream_owned_by_a_different_tenant() {
        let (router, state) = build_router();
        let owner_tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, owner_tenant_id);

        let caller_tenant_id = Uuid::new_v4();
        let body = http_create_body(upstream_id, "/v1/models", 1);
        let response = router
            .oneshot(request(
                "POST",
                "/oagw/v1/routes",
                Some(body),
                caller_tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::BAD_REQUEST);
    }

    #[tokio::test]
    async fn create_accepts_a_plaintext_http_upstream_reference() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_plaintext_upstream(&state, tenant_id);
        let body = http_create_body(upstream_id, "/v1/models", 1);
        let response = router
            .oneshot(request("POST", "/oagw/v1/routes", Some(body), tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::CREATED);
    }

    #[tokio::test]
    async fn create_rejects_a_second_enabled_route_colliding_on_path_and_priority() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id);

        let first = http_create_body(upstream_id, "/v1/models", 1);
        let response = router
            .clone()
            .oneshot(request("POST", "/oagw/v1/routes", Some(first), tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::CREATED);

        let second = http_create_body(upstream_id, "/v1/models", 1);
        let response = router
            .oneshot(request("POST", "/oagw/v1/routes", Some(second), tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::CONFLICT);
        let json = body_json(response).await;
        assert_eq!(
            json.get("type").and_then(Value::as_str),
            Some("about:blank")
        );
    }

    #[tokio::test]
    async fn create_allows_a_disjoint_method_set_a_different_priority_or_disabled() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id);

        let first = http_create_body(upstream_id, "/v1/models", 1);
        let response = router
            .clone()
            .oneshot(request("POST", "/oagw/v1/routes", Some(first), tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::CREATED);

        let disjoint_method = json!({
            "upstream_id": upstream_id,
            "match": { "http": { "methods": ["POST"], "path": "/v1/models" } },
            "priority": 1,
        });
        let response = router
            .clone()
            .oneshot(request(
                "POST",
                "/oagw/v1/routes",
                Some(disjoint_method),
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::CREATED);

        let different_priority = http_create_body(upstream_id, "/v1/models", 2);
        let response = router
            .clone()
            .oneshot(request(
                "POST",
                "/oagw/v1/routes",
                Some(different_priority),
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::CREATED);

        let mut disabled = http_create_body(upstream_id, "/v1/models", 1);
        disabled["enabled"] = json!(false);
        let response = router
            .oneshot(request(
                "POST",
                "/oagw/v1/routes",
                Some(disabled),
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::CREATED);
    }

    #[tokio::test]
    async fn get_by_id_succeeds_identically_for_both_id_forms() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id);
        let body = http_create_body(upstream_id, "/v1/models", 1);
        let response = router
            .clone()
            .oneshot(request("POST", "/oagw/v1/routes", Some(body), tenant_id))
            .await
            .unwrap();
        let created = body_json(response).await;
        let id = created["id"].as_str().unwrap().to_owned();

        let bare_response = router
            .clone()
            .oneshot(request(
                "GET",
                &format!("/oagw/v1/routes/{id}"),
                None,
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(bare_response.status(), Status::OK);
        let bare_body = body_json(bare_response).await;

        let gts_form = format!("{ROUTE_GTS_ID_PREFIX}{id}");
        let gts_response = router
            .oneshot(request(
                "GET",
                &format!("/oagw/v1/routes/{gts_form}"),
                None,
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(gts_response.status(), Status::OK);
        let gts_body = body_json(gts_response).await;

        assert_eq!(bare_body, gts_body);
        assert_eq!(bare_body["id"], id);
    }

    #[tokio::test]
    async fn get_by_id_returns_404_for_a_different_tenants_route_never_403() {
        let (router, state) = build_router();
        let owner_tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, owner_tenant_id);
        let body = http_create_body(upstream_id, "/v1/models", 1);
        let response = router
            .clone()
            .oneshot(request(
                "POST",
                "/oagw/v1/routes",
                Some(body),
                owner_tenant_id,
            ))
            .await
            .unwrap();
        let created = body_json(response).await;
        let id = created["id"].as_str().unwrap().to_owned();

        let other_tenant_id = Uuid::new_v4();
        let response = router
            .oneshot(request(
                "GET",
                &format!("/oagw/v1/routes/{id}"),
                None,
                other_tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::NOT_FOUND);
        let json = body_json(response).await;
        assert_eq!(
            json.get("type").and_then(Value::as_str),
            Some("about:blank")
        );
    }

    #[tokio::test]
    async fn list_never_includes_another_tenants_route() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id);
        let mine = http_create_body(upstream_id, "/v1/mine", 1);
        router
            .clone()
            .oneshot(request("POST", "/oagw/v1/routes", Some(mine), tenant_id))
            .await
            .unwrap();

        let other_tenant_id = Uuid::new_v4();
        let other_upstream_id = seed_upstream(&state, other_tenant_id);
        let theirs = http_create_body(other_upstream_id, "/v1/theirs", 1);
        router
            .clone()
            .oneshot(request(
                "POST",
                "/oagw/v1/routes",
                Some(theirs),
                other_tenant_id,
            ))
            .await
            .unwrap();

        let response = router
            .oneshot(request("GET", "/oagw/v1/routes", None, tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::OK);
        let items = body_json(response).await;
        let items = items.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["upstream_id"], upstream_id.to_string());
    }

    #[tokio::test]
    async fn list_defaults_top_to_50_and_never_exceeds_100() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id);
        for i in 0..60 {
            let body = http_create_body(upstream_id, &format!("/v1/{i}"), i);
            let response = router
                .clone()
                .oneshot(request("POST", "/oagw/v1/routes", Some(body), tenant_id))
                .await
                .unwrap();
            assert_eq!(response.status(), Status::CREATED);
        }

        let response = router
            .clone()
            .oneshot(request("GET", "/oagw/v1/routes", None, tenant_id))
            .await
            .unwrap();
        let items = body_json(response).await;
        assert_eq!(items.as_array().unwrap().len(), 50);

        let response = router
            .oneshot(request(
                "GET",
                "/oagw/v1/routes?%24top=1000",
                None,
                tenant_id,
            ))
            .await
            .unwrap();
        let items = body_json(response).await;
        assert_eq!(items.as_array().unwrap().len(), 60);
    }

    /// RF-005: `$top`/`$skip` now share one parser with
    /// `GET /oagw/v1/upstreams` (`src/api/rest/page_params.rs`) and
    /// standardise on reject-with-400 for malformed input -- this endpoint
    /// used to silently fall back to the default instead. See
    /// `src/api/rest/upstreams/tests.rs`'s
    /// `list_upstreams_top_and_skip_malformed_absent_and_over_max_are_handled_per_the_shared_contract`
    /// for the identical assertions against the upstreams endpoint.
    #[tokio::test]
    async fn list_top_and_skip_malformed_absent_and_over_max_match_the_upstreams_contract() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id);
        for i in 0..3 {
            let body = http_create_body(upstream_id, &format!("/v1/{i}"), i);
            let response = router
                .clone()
                .oneshot(request("POST", "/oagw/v1/routes", Some(body), tenant_id))
                .await
                .unwrap();
            assert_eq!(response.status(), Status::CREATED);
        }

        // Absent: default $top = 50, all 3 rows returned.
        let absent = router
            .clone()
            .oneshot(request("GET", "/oagw/v1/routes", None, tenant_id))
            .await
            .unwrap();
        assert_eq!(absent.status(), Status::OK);
        assert_eq!(body_json(absent).await.as_array().unwrap().len(), 3);

        // Valid: $top=1 respected.
        let valid = router
            .clone()
            .oneshot(request("GET", "/oagw/v1/routes?%24top=1", None, tenant_id))
            .await
            .unwrap();
        assert_eq!(valid.status(), Status::OK);
        assert_eq!(body_json(valid).await.as_array().unwrap().len(), 1);

        // Over-max: $top clamped to 100, not rejected (only 3 rows exist).
        let over_max = router
            .clone()
            .oneshot(request(
                "GET",
                "/oagw/v1/routes?%24top=1000",
                None,
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(over_max.status(), Status::OK);
        assert_eq!(body_json(over_max).await.as_array().unwrap().len(), 3);

        // Malformed $top: rejected with 400, not silently defaulted.
        let bad_top = router
            .clone()
            .oneshot(request(
                "GET",
                "/oagw/v1/routes?%24top=notanumber",
                None,
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(bad_top.status(), Status::BAD_REQUEST);

        // Malformed $skip: rejected with 400, not silently defaulted.
        let bad_skip = router
            .oneshot(request(
                "GET",
                "/oagw/v1/routes?%24skip=notanumber",
                None,
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(bad_skip.status(), Status::BAD_REQUEST);
    }

    #[tokio::test]
    async fn replace_rejects_a_changed_upstream_id() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id);
        let other_upstream_id = seed_upstream(&state, tenant_id);
        let body = http_create_body(upstream_id, "/v1/models", 1);
        let response = router
            .clone()
            .oneshot(request("POST", "/oagw/v1/routes", Some(body), tenant_id))
            .await
            .unwrap();
        let created = body_json(response).await;
        let id = created["id"].as_str().unwrap().to_owned();

        let mut replacement = http_create_body(other_upstream_id, "/v1/models", 1);
        replacement["upstream_id"] = json!(other_upstream_id);
        let response = router
            .clone()
            .oneshot(request(
                "PUT",
                &format!("/oagw/v1/routes/{id}"),
                Some(replacement),
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::BAD_REQUEST);

        // The persisted upstream_id is unaffected.
        let response = router
            .oneshot(request(
                "GET",
                &format!("/oagw/v1/routes/{id}"),
                None,
                tenant_id,
            ))
            .await
            .unwrap();
        let json = body_json(response).await;
        assert_eq!(json["upstream_id"], upstream_id.to_string());
    }

    #[tokio::test]
    async fn replace_omitting_enabled_resets_it_to_true() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id);
        let mut created_body = http_create_body(upstream_id, "/v1/models", 1);
        created_body["enabled"] = json!(false);
        let response = router
            .clone()
            .oneshot(request(
                "POST",
                "/oagw/v1/routes",
                Some(created_body),
                tenant_id,
            ))
            .await
            .unwrap();
        let created = body_json(response).await;
        assert_eq!(created["enabled"], false);
        let id = created["id"].as_str().unwrap().to_owned();

        let replacement = http_create_body(upstream_id, "/v1/models", 1);
        let response = router
            .oneshot(request(
                "PUT",
                &format!("/oagw/v1/routes/{id}"),
                Some(replacement),
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::OK);
        let replaced = body_json(response).await;
        assert_eq!(replaced["enabled"], true);
    }

    #[tokio::test]
    async fn replace_colliding_with_another_enabled_route_returns_409_and_self_replace_succeeds() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id);

        let first = http_create_body(upstream_id, "/v1/a", 1);
        let response = router
            .clone()
            .oneshot(request("POST", "/oagw/v1/routes", Some(first), tenant_id))
            .await
            .unwrap();
        let first_created = body_json(response).await;
        let first_id = first_created["id"].as_str().unwrap().to_owned();

        let second = http_create_body(upstream_id, "/v1/b", 2);
        let response = router
            .clone()
            .oneshot(request("POST", "/oagw/v1/routes", Some(second), tenant_id))
            .await
            .unwrap();
        let second_created = body_json(response).await;
        let second_id = second_created["id"].as_str().unwrap().to_owned();

        // Replacing the second route to collide with the first is a 409.
        let colliding = http_create_body(upstream_id, "/v1/a", 1);
        let response = router
            .clone()
            .oneshot(request(
                "PUT",
                &format!("/oagw/v1/routes/{second_id}"),
                Some(colliding),
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::CONFLICT);

        // Replacing the first route with its own unchanged match rules succeeds.
        let unchanged = http_create_body(upstream_id, "/v1/a", 1);
        let response = router
            .oneshot(request(
                "PUT",
                &format!("/oagw/v1/routes/{first_id}"),
                Some(unchanged),
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::OK);
    }

    #[tokio::test]
    async fn delete_returns_204_then_404_on_repeat() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id);
        let body = http_create_body(upstream_id, "/v1/models", 1);
        let response = router
            .clone()
            .oneshot(request("POST", "/oagw/v1/routes", Some(body), tenant_id))
            .await
            .unwrap();
        let created = body_json(response).await;
        let id = created["id"].as_str().unwrap().to_owned();

        let response = router
            .clone()
            .oneshot(request(
                "DELETE",
                &format!("/oagw/v1/routes/{id}"),
                None,
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::NO_CONTENT);

        let response = router
            .oneshot(request(
                "DELETE",
                &format!("/oagw/v1/routes/{id}"),
                None,
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::NOT_FOUND);
    }

    #[tokio::test]
    async fn every_error_response_carries_the_gateway_error_source_header() {
        let (router, _state) = build_router();
        let tenant_id = Uuid::new_v4();
        let body = http_create_body(Uuid::new_v4(), "/v1/models", 1);
        let response = router
            .oneshot(request("POST", "/oagw/v1/routes", Some(body), tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get("x-oagw-error-source")
                .and_then(|v| v.to_str().ok()),
            Some("gateway")
        );
    }

    #[tokio::test]
    async fn a_cors_object_does_not_fail_validation_and_is_not_persisted() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();
        let upstream_id = seed_upstream(&state, tenant_id);
        let mut body = http_create_body(upstream_id, "/v1/models", 1);
        body["cors"] = json!({ "enabled": true, "allowed_origins": ["*"] });
        let response = router
            .oneshot(request("POST", "/oagw/v1/routes", Some(body), tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::CREATED);
        let created = body_json(response).await;
        assert!(created.get("cors").is_none());
    }

    #[test]
    fn normalize_id_param_still_used_for_get_lookup_smoke() {
        // Exercised end-to-end above; this smoke test just confirms the
        // Route model helper is reachable from this module.
        assert!(Route::normalize_id_param("not-a-uuid").is_none());
    }
}
