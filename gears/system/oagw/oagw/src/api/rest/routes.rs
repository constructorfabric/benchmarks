//! Route registrations of the OAGW management surface (DESIGN §3.3).
//!
//! Paths are gear-relative: the gear mounts them under its configured prefix,
//! so `/oagw/v1/...` is what the server serves when the prefix is empty.

use http::StatusCode;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;

use super::handlers;
use super::{API_TAG, dto};
use crate::domain::model::{Plugin, Route, Upstream};

/// Registers the upstream, route and plugin management routes.
pub fn register_routes(mut router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    // POST /oagw/v1/upstreams
    router = OperationBuilder::post("/oagw/v1/upstreams")
        .operation_id("oagw.create_upstream")
        .summary("Create upstream")
        .description(
            "Creates an upstream. The identifier and timestamps are server-generated and the \
             alias is derived from the endpoint pool unless the pool forces an explicit one.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<Upstream>(openapi, "Upstream to create")
        .handler(handlers::create_upstream)
        .json_response_with_schema::<Upstream>(openapi, StatusCode::CREATED, "Created upstream")
        .error_400(openapi)
        .error_401(openapi)
        .error_409(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .error_502(openapi)
        .error_503(openapi)
        .error_504(openapi)
        .register(router, openapi);

    // GET /oagw/v1/upstreams
    router = OperationBuilder::get("/oagw/v1/upstreams")
        .operation_id("oagw.list_upstreams")
        .summary("List upstreams")
        .description("Lists the calling tenant's own upstreams with OData query options.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param("$filter", false, "OData filter expression")
        .query_param("$orderby", false, "Sort order")
        .query_param("$select", false, "Fields to return")
        .query_param_typed("$top", false, "Maximum number of results", "integer")
        .query_param_typed("$skip", false, "Offset for pagination", "integer")
        .handler(handlers::list_upstreams)
        .json_response_with_schema::<dto::UpstreamList>(
            openapi,
            StatusCode::OK,
            "List of upstreams",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // GET /oagw/v1/upstreams/{id}
    router = OperationBuilder::get("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.get_upstream")
        .summary("Get upstream by ID")
        .description("Fetches one own upstream by its identifier.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID")
        .handler(handlers::get_upstream)
        .json_response_with_schema::<Upstream>(openapi, StatusCode::OK, "Upstream found")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // PUT /oagw/v1/upstreams/{id}
    router = OperationBuilder::put("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.replace_upstream")
        .summary("Replace upstream")
        .description(
            "Full replacement of an upstream. Alias transitions follow the derivation matrix \
             of DESIGN §3.2.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID")
        .json_request::<Upstream>(openapi, "Replacement upstream")
        .handler(handlers::replace_upstream)
        .json_response_with_schema::<Upstream>(openapi, StatusCode::OK, "Replaced upstream")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // DELETE /oagw/v1/upstreams/{id}
    router = OperationBuilder::delete("/oagw/v1/upstreams/{id}")
        .operation_id("oagw.delete_upstream")
        .summary("Delete upstream")
        .description("Deletes an upstream together with its routes.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Upstream UUID")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Upstream deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // POST /oagw/v1/routes
    router = OperationBuilder::post("/oagw/v1/routes")
        .operation_id("oagw.create_route")
        .summary("Create route")
        .description("Creates a route under an own upstream and validates its match rule.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<Route>(openapi, "Route to create")
        .handler(handlers::create_route)
        .json_response_with_schema::<Route>(openapi, StatusCode::CREATED, "Created route")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // GET /oagw/v1/routes
    router = OperationBuilder::get("/oagw/v1/routes")
        .operation_id("oagw.list_routes")
        .summary("List routes")
        .description("Lists the calling tenant's own routes with OData query options.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param_typed("upstream_id", false, "Restrict to this upstream", "string")
        .query_param("$filter", false, "OData filter expression")
        .query_param("$orderby", false, "Sort order")
        .query_param("$select", false, "Fields to return")
        .query_param_typed("$top", false, "Maximum number of results", "integer")
        .query_param_typed("$skip", false, "Offset for pagination", "integer")
        .handler(handlers::list_routes)
        .json_response_with_schema::<dto::RouteList>(openapi, StatusCode::OK, "List of routes")
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // GET /oagw/v1/routes/{id}
    router = OperationBuilder::get("/oagw/v1/routes/{id}")
        .operation_id("oagw.get_route")
        .summary("Get route by ID")
        .description("Fetches one own route by its identifier.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID")
        .handler(handlers::get_route)
        .json_response_with_schema::<Route>(openapi, StatusCode::OK, "Route found")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // PUT /oagw/v1/routes/{id}
    router = OperationBuilder::put("/oagw/v1/routes/{id}")
        .operation_id("oagw.replace_route")
        .summary("Replace route")
        .description("Full replacement of a route; `upstream_id` is immutable.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID")
        .json_request::<Route>(openapi, "Replacement route")
        .handler(handlers::replace_route)
        .json_response_with_schema::<Route>(openapi, StatusCode::OK, "Replaced route")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // DELETE /oagw/v1/routes/{id}
    router = OperationBuilder::delete("/oagw/v1/routes/{id}")
        .operation_id("oagw.delete_route")
        .summary("Delete route")
        .description("Deletes a route.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Route UUID")
        .handler(handlers::delete_route)
        .no_content_response(StatusCode::NO_CONTENT, "Route deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // POST /oagw/v1/plugins
    router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.create_plugin")
        .summary("Create plugin")
        .description("Creates a custom Starlark plugin; plugins are immutable once created.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .json_request::<Plugin>(openapi, "Plugin to create")
        .handler(handlers::create_plugin)
        .json_response_with_schema::<Plugin>(openapi, StatusCode::CREATED, "Created plugin")
        .error_400(openapi)
        .error_401(openapi)
        .error_409(openapi)
        .error_429(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // GET /oagw/v1/plugins
    router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.list_plugins")
        .summary("List plugins")
        .description("Lists the calling tenant's own plugins with OData query options.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "plugin_type",
            false,
            "Restrict to `auth`, `guard` or `transform`",
        )
        .query_param("$filter", false, "OData filter expression")
        .query_param("$orderby", false, "Sort order")
        .query_param("$select", false, "Fields to return")
        .query_param_typed("$top", false, "Maximum number of results", "integer")
        .query_param_typed("$skip", false, "Offset for pagination", "integer")
        .handler(handlers::list_plugins)
        .json_response_with_schema::<dto::PluginList>(openapi, StatusCode::OK, "List of plugins")
        .error_400(openapi)
        .error_401(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // GET /oagw/v1/plugins/{id}
    router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.get_plugin")
        .summary("Get plugin by ID")
        .description("Fetches one own plugin by its identifier.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID")
        .handler(handlers::get_plugin)
        .json_response_with_schema::<Plugin>(openapi, StatusCode::OK, "Plugin found")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // DELETE /oagw/v1/plugins/{id}
    router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.delete_plugin")
        .summary("Delete plugin")
        .description("Deletes a plugin that is no longer referenced by any upstream or route.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_409(openapi)
        .error_500(openapi)
        .register(router, openapi);

    // GET /oagw/v1/plugins/{id}/source
    router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.get_plugin_source")
        .summary("Get plugin Starlark source")
        .description("Returns the Starlark source of a custom plugin.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Plugin UUID")
        .handler(handlers::get_plugin_source)
        .json_response_with_schema::<dto::PluginSource>(
            openapi,
            StatusCode::OK,
            "Plugin source code",
        )
        .error_400(openapi)
        .error_401(openapi)
        .error_404(openapi)
        .error_500(openapi)
        .register(router, openapi);

    router
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OagwConfig;
    use crate::domain::services::management::ManagementService;
    use crate::infra::storage::InMemoryStore;
    use toolkit_security::SecurityContext;

    struct NoopRegistry;

    impl toolkit::api::OpenApiRegistry for NoopRegistry {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }

        fn register_operation(&self, _spec: &toolkit::api::OperationSpec) {}

        fn ensure_schema_raw(
            &self,
            name: &str,
            _schemas: Vec<(
                String,
                utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
            )>,
        ) -> String {
            name.to_owned()
        }
    }

    fn security_context(tenant: uuid::Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(uuid::Uuid::new_v4())
            .subject_type("user")
            .subject_tenant_id(tenant)
            .build()
            .expect("security context builds")
    }

    fn management_router() -> axum::Router {
        let store = InMemoryStore::default();
        let config = OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        };
        let service = ManagementService::new(
            std::sync::Arc::new(store.clone()),
            std::sync::Arc::new(store),
            std::sync::Arc::new(InMemoryStore::default()),
            std::sync::Arc::new(config),
        );
        register_routes(axum::Router::new(), &NoopRegistry)
            .layer(axum::Extension(std::sync::Arc::new(service)))
            .layer(axum::Extension(security_context(uuid::Uuid::new_v4())))
    }

    #[tokio::test]
    async fn upstream_crud_round_trips_over_the_router() {
        use tower::ServiceExt;
        let app = management_router();
        let body = serde_json::json!({
            "alias": "api.openai.com",
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com"}]}
        });
        let response = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/oagw/v1/upstreams")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body.to_string()))
                    .expect("request"),
            )
            .await
            .expect("response");
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(
            status,
            StatusCode::CREATED,
            "body: {}",
            String::from_utf8_lossy(&bytes)
        );
        let created: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        let id = created["id"].as_str().expect("id").to_owned();
        assert_eq!(created["alias"], "api.openai.com");

        let listed = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/oagw/v1/upstreams?$filter=alias%20eq%20%27api.openai.com%27&$select=alias")
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(listed.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(listed.into_body(), usize::MAX)
            .await
            .expect("body");
        let listed: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(listed["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(listed["items"][0]["alias"], "api.openai.com");

        let fetched = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/oagw/v1/upstreams/{id}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(fetched.status(), StatusCode::OK);

        let deleted = app
            .oneshot(
                axum::http::Request::builder()
                    .method("DELETE")
                    .uri(format!("/oagw/v1/upstreams/{id}"))
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn invalid_bodies_render_the_oagw_problem_document() {
        use tower::ServiceExt;
        let app = management_router();
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/oagw/v1/upstreams")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        serde_json::json!({"server": {"endpoints": []}}).to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response
                .headers()
                .get("x-oagw-error-source")
                .expect("error source"),
            "gateway"
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let problem: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(problem["type"], crate::domain::error::codes::VALIDATION);
        assert_eq!(problem["status"], 400);
    }
}
