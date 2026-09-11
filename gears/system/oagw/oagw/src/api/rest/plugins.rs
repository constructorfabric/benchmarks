//! Plugin Management API routes.
//!
//! Implemented by DECOMPOSITION entry 2.4 (plugin-management): CRUD
//! operations for custom Plugin resources under `/oagw/v1/plugins`,
//! including the `409 PluginInUse` delete-conflict semantics and
//! immutable-after-creation enforcement.
//!
//! There is deliberately no `PUT`/`PATCH` route registered for this
//! resource (`cpt-cf-oagw-dod-plugin-immutability`): a changed plugin is a
//! new `POST /oagw/v1/plugins` resource plus a rebind, never an in-place
//! update.

mod dto;
mod handlers;

use std::sync::Arc;

use axum::http::StatusCode;
use axum::{Extension, Router};
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::{
    CORE_GLOBAL_BASE_LICENSE_FEATURE, LicenseFeature, OperationBuilder,
};

use crate::store::OagwState;
use dto::{CreatePluginRequest, PluginResponse};

const API_TAG: &str = "Plugin Management";

struct License;

impl AsRef<str> for License {
    fn as_ref(&self) -> &'static str {
        CORE_GLOBAL_BASE_LICENSE_FEATURE
    }
}

impl LicenseFeature for License {}

// @cpt-dod:cpt-cf-oagw-dod-plugin-crud-surface:p1
// @cpt-dod:cpt-cf-oagw-dod-plugin-immutability:p1
pub(crate) fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    state: Arc<OagwState>,
) -> Router {
    let mut plugin_router = Router::new();

    plugin_router = OperationBuilder::post("/oagw/v1/plugins")
        .operation_id("oagw.plugins.create")
        .summary("Create a custom plugin")
        .description(
            "Store a new tenant-defined Starlark Auth/Guard/Transform plugin and issue its \
             anonymous GTS identifier. Plugins are immutable after creation.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .json_request::<CreatePluginRequest>(openapi, "Plugin to create")
        .handler(handlers::create_plugin)
        .json_response_with_schema::<PluginResponse>(openapi, StatusCode::CREATED, "Plugin created")
        .standard_errors(openapi)
        .register(plugin_router, openapi);

    plugin_router = OperationBuilder::get("/oagw/v1/plugins")
        .operation_id("oagw.plugins.list")
        .summary("List custom plugins")
        .description(
            "List tenant-scoped custom plugins, with OData-lite $filter/$select/$top/$skip.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .query_param(
            "$filter",
            false,
            "OData-lite 'field eq value' filter over name/plugin_type",
        )
        .query_param("$select", false, "Comma-separated field projection")
        .query_param("$top", false, "Page size (default 50, max 100)")
        .query_param("$skip", false, "Number of rows to skip")
        .handler(handlers::list_plugins)
        .json_array_response_with_schema::<PluginResponse>(openapi, StatusCode::OK, "Plugins")
        .standard_errors(openapi)
        .register(plugin_router, openapi);

    plugin_router = OperationBuilder::get("/oagw/v1/plugins/{id}")
        .operation_id("oagw.plugins.get")
        .summary("Get a custom plugin by id")
        .description(
            "Fetch one stored custom plugin's metadata by its bare UUID id or its anonymous \
             GTS identifier.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Bare UUID or anonymous GTS plugin identifier")
        .handler(handlers::get_plugin)
        .json_response_with_schema::<PluginResponse>(openapi, StatusCode::OK, "Plugin")
        .standard_errors(openapi)
        .register(plugin_router, openapi);

    plugin_router = OperationBuilder::get("/oagw/v1/plugins/{id}/source")
        .operation_id("oagw.plugins.get_source")
        .summary("Get a custom plugin's raw source code")
        .description("Return the stored Starlark source_code verbatim -- never parsed or executed.")
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Bare UUID or anonymous GTS plugin identifier")
        .handler(handlers::get_plugin_source)
        .text_response(StatusCode::OK, "Raw plugin source code", "text/plain")
        .standard_errors(openapi)
        .register(plugin_router, openapi);

    plugin_router = OperationBuilder::delete("/oagw/v1/plugins/{id}")
        .operation_id("oagw.plugins.delete")
        .summary("Delete a custom plugin")
        .description(
            "Delete a stored custom plugin. Fails with 409 PluginInUse when still bound by any \
             upstream or route.",
        )
        .tag(API_TAG)
        .authenticated()
        .require_license_features::<License>([])
        .path_param("id", "Bare UUID or anonymous GTS plugin identifier")
        .handler(handlers::delete_plugin)
        .no_content_response(StatusCode::NO_CONTENT, "Plugin deleted")
        .standard_errors(openapi)
        .register(plugin_router, openapi);

    router.merge(plugin_router.layer(Extension(state)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::OagwConfig;
    use crate::model::plugin::{PluginType, plugin_gts_ref};
    use crate::model::route::{Route, RouteMatch, RoutePluginsBinding};
    use crate::model::upstream::{AuthConfig, ServerConfig, Sharing, Upstream};
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

    fn guard_create_body(name: &str) -> Value {
        json!({
            "plugin_type": "guard",
            "name": name,
            "config_schema": { "type": "object" },
            "source_code": "def guard(req): return req",
        })
    }

    /// Acceptance criterion: `POST` with a valid `{plugin_type: "guard", ...}`
    /// body returns `201` whose identifier matches
    /// `gts.cf.core.oagw.guard_plugin.v1~{uuid}`, server-generated.
    #[tokio::test]
    async fn create_returns_201_with_a_server_generated_gts_identifier() {
        let (router, _state) = build_router();
        let tenant_id = Uuid::new_v4();

        let response = router
            .oneshot(request(
                "POST",
                "/oagw/v1/plugins",
                Some(guard_create_body("my-guard")),
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::CREATED);

        let body = body_json(response).await;
        let id = Uuid::parse_str(body["id"].as_str().unwrap()).unwrap();
        assert_eq!(body["plugin_type"], "guard");
        assert!(body.get("source_code").is_none());
        assert!(body["gc_eligible_at"].is_string());
        assert!(body["last_used_at"].is_null());

        let expected_ref = plugin_gts_ref(PluginType::Guard, id);
        assert_eq!(
            expected_ref,
            format!("gts.cf.core.oagw.guard_plugin.v1~{id}")
        );
    }

    #[tokio::test]
    async fn create_with_missing_source_code_returns_400_validation_error() {
        let (router, _state) = build_router();
        let tenant_id = Uuid::new_v4();
        let body = json!({ "plugin_type": "guard", "name": "no-source" });

        let response = router
            .oneshot(request("POST", "/oagw/v1/plugins", Some(body), tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(
            body["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
    }

    #[tokio::test]
    async fn create_with_invalid_plugin_type_returns_400_validation_error() {
        let (router, _state) = build_router();
        let tenant_id = Uuid::new_v4();
        let body = json!({
            "plugin_type": "bogus",
            "name": "bad-type",
            "source_code": "def guard(req): return req",
        });

        let response = router
            .oneshot(request("POST", "/oagw/v1/plugins", Some(body), tenant_id))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::BAD_REQUEST);
        let body = body_json(response).await;
        assert_eq!(
            body["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
        );
    }

    #[tokio::test]
    async fn create_twice_with_the_same_name_for_the_same_tenant_returns_400_on_the_second_call() {
        let (router, _state) = build_router();
        let tenant_id = Uuid::new_v4();

        let first = router
            .clone()
            .oneshot(request(
                "POST",
                "/oagw/v1/plugins",
                Some(guard_create_body("dup-name")),
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(first.status(), Status::CREATED);

        let second = router
            .oneshot(request(
                "POST",
                "/oagw/v1/plugins",
                Some(guard_create_body("dup-name")),
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(second.status(), Status::BAD_REQUEST);
    }

    #[tokio::test]
    async fn list_returns_only_the_callers_tenant_plugins() {
        let (router, _state) = build_router();
        let owner_tenant = Uuid::new_v4();
        let other_tenant = Uuid::new_v4();

        let _ = router
            .clone()
            .oneshot(request(
                "POST",
                "/oagw/v1/plugins",
                Some(guard_create_body("mine")),
                owner_tenant,
            ))
            .await
            .unwrap();
        let _ = router
            .clone()
            .oneshot(request(
                "POST",
                "/oagw/v1/plugins",
                Some(guard_create_body("theirs")),
                other_tenant,
            ))
            .await
            .unwrap();

        let response = router
            .oneshot(request("GET", "/oagw/v1/plugins", None, owner_tenant))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::OK);
        let body = body_json(response).await;
        let items = body.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["name"], "mine");
    }

    #[tokio::test]
    async fn get_by_id_returns_the_stored_metadata_and_omits_source_code() {
        let (router, _state) = build_router();
        let tenant_id = Uuid::new_v4();

        let created = router
            .clone()
            .oneshot(request(
                "POST",
                "/oagw/v1/plugins",
                Some(guard_create_body("fetchable")),
                tenant_id,
            ))
            .await
            .unwrap();
        let created = body_json(created).await;
        let id = created["id"].as_str().unwrap();

        let response = router
            .oneshot(request(
                "GET",
                &format!("/oagw/v1/plugins/{id}"),
                None,
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::OK);
        let body = body_json(response).await;
        assert_eq!(body["name"], "fetchable");
        assert!(body["gc_eligible_at"].is_string());
        assert!(body["last_used_at"].is_null());
        assert!(body.get("source_code").is_none());
    }

    /// `{id}` accepts both the bare UUID and the anonymous GTS form, resolving
    /// to the same resource with an identical body.
    #[tokio::test]
    async fn get_by_id_succeeds_identically_for_both_id_forms() {
        let (router, _state) = build_router();
        let tenant_id = Uuid::new_v4();

        let created = router
            .clone()
            .oneshot(request(
                "POST",
                "/oagw/v1/plugins",
                Some(guard_create_body("both-forms")),
                tenant_id,
            ))
            .await
            .unwrap();
        let created = body_json(created).await;
        let id = created["id"].as_str().unwrap().to_owned();
        let gts_form = format!("gts.cf.core.oagw.guard_plugin.v1~{id}");

        let by_bare = router
            .clone()
            .oneshot(request(
                "GET",
                &format!("/oagw/v1/plugins/{id}"),
                None,
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(by_bare.status(), Status::OK);
        let by_bare_body = body_json(by_bare).await;

        let by_gts = router
            .oneshot(request(
                "GET",
                &format!("/oagw/v1/plugins/{gts_form}"),
                None,
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(by_gts.status(), Status::OK);
        let by_gts_body = body_json(by_gts).await;

        assert_eq!(by_bare_body, by_gts_body);
    }

    #[tokio::test]
    async fn get_by_id_for_a_named_identifier_returns_404() {
        let (router, _state) = build_router();
        let tenant_id = Uuid::new_v4();

        let response = router
            .oneshot(request(
                "GET",
                "/oagw/v1/plugins/gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                None,
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::NOT_FOUND);
        let body = body_json(response).await;
        assert!(body.get("type").is_none());
    }

    #[tokio::test]
    async fn get_by_id_for_a_syntactically_invalid_identifier_returns_400() {
        let (router, _state) = build_router();
        let tenant_id = Uuid::new_v4();

        let response = router
            .oneshot(request(
                "GET",
                "/oagw/v1/plugins/not-a-valid-identifier",
                None,
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::BAD_REQUEST);
    }

    #[tokio::test]
    async fn get_by_id_for_another_tenants_plugin_returns_404() {
        let (router, _state) = build_router();
        let owner_tenant = Uuid::new_v4();
        let other_tenant = Uuid::new_v4();

        let created = router
            .clone()
            .oneshot(request(
                "POST",
                "/oagw/v1/plugins",
                Some(guard_create_body("owners-only")),
                owner_tenant,
            ))
            .await
            .unwrap();
        let created = body_json(created).await;
        let id = created["id"].as_str().unwrap();

        let response = router
            .oneshot(request(
                "GET",
                &format!("/oagw/v1/plugins/{id}"),
                None,
                other_tenant,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_source_returns_the_verbatim_source_code_as_text_plain() {
        let (router, _state) = build_router();
        let tenant_id = Uuid::new_v4();
        let source = "def guard(req):\n    return req\n";
        let body = json!({
            "plugin_type": "guard",
            "name": "source-test",
            "source_code": source,
        });

        let created = router
            .clone()
            .oneshot(request("POST", "/oagw/v1/plugins", Some(body), tenant_id))
            .await
            .unwrap();
        let created = body_json(created).await;
        let id = created["id"].as_str().unwrap();

        let response = router
            .oneshot(request(
                "GET",
                &format!("/oagw/v1/plugins/{id}/source"),
                None,
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/plain; charset=utf-8")
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(bytes.as_ref(), source.as_bytes());
    }

    #[tokio::test]
    async fn delete_an_unreferenced_plugin_returns_204_then_get_returns_404() {
        let (router, _state) = build_router();
        let tenant_id = Uuid::new_v4();

        let created = router
            .clone()
            .oneshot(request(
                "POST",
                "/oagw/v1/plugins",
                Some(guard_create_body("deletable")),
                tenant_id,
            ))
            .await
            .unwrap();
        let created = body_json(created).await;
        let id = created["id"].as_str().unwrap().to_owned();

        let delete_response = router
            .clone()
            .oneshot(request(
                "DELETE",
                &format!("/oagw/v1/plugins/{id}"),
                None,
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(delete_response.status(), Status::NO_CONTENT);

        let get_response = router
            .oneshot(request(
                "GET",
                &format!("/oagw/v1/plugins/{id}"),
                None,
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(get_response.status(), Status::NOT_FOUND);
    }

    fn seed_upstream_bound_to_plugin(state: &OagwState, tenant_id: Uuid, plugin_ref: &str) -> Uuid {
        let id = Uuid::new_v4();
        let upstream = Upstream {
            id: Some(id),
            enabled: true,
            alias: Some(format!("svc-{id}")),
            tags: Vec::new(),
            server: ServerConfig { endpoints: vec![] },
            protocol: "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1".to_owned(),
            auth: Some(AuthConfig {
                auth_type: Some(plugin_ref.to_owned()),
                sharing: Sharing::default(),
                config: Value::Null,
            }),
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tenant_id,
        };
        state.store.upstreams().insert(id, Arc::new(upstream));
        id
    }

    fn seed_route_bound_to_plugin(
        state: &OagwState,
        tenant_id: Uuid,
        upstream_id: Uuid,
        plugin_ref: &str,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let route = Route {
            id: Some(id),
            tenant_id,
            tags: Vec::new(),
            upstream_id,
            route_match: RouteMatch::default(),
            plugins: Some(RoutePluginsBinding {
                sharing: Sharing::default(),
                items: vec![plugin_ref.to_owned()],
            }),
            rate_limit: None,
            enabled: true,
            priority: None,
        };
        state.store.routes().insert(id, Arc::new(route));
        id
    }

    /// Acceptance criterion: a plugin bound by one upstream and one route
    /// returns `409` with GTS type `cf.oagw.plugin.in_use.v1`, a `plugin_id`
    /// field, and `referenced_by.upstreams`/`referenced_by.routes` each
    /// containing exactly one matching identifier. Bindings are seeded
    /// directly into the store (not via the sibling upstream/route handlers)
    /// per this feature's testing method.
    #[tokio::test]
    async fn delete_a_plugin_bound_by_one_upstream_and_one_route_returns_409_plugin_in_use() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();

        let created = router
            .clone()
            .oneshot(request(
                "POST",
                "/oagw/v1/plugins",
                Some(guard_create_body("in-use")),
                tenant_id,
            ))
            .await
            .unwrap();
        let created = body_json(created).await;
        let id = created["id"].as_str().unwrap().to_owned();
        let plugin_uuid = Uuid::parse_str(&id).unwrap();
        let plugin_ref = plugin_gts_ref(PluginType::Guard, plugin_uuid);

        let upstream_id = seed_upstream_bound_to_plugin(&state, tenant_id, &plugin_ref);
        let route_id = seed_route_bound_to_plugin(&state, tenant_id, upstream_id, &plugin_ref);

        let response = router
            .oneshot(request(
                "DELETE",
                &format!("/oagw/v1/plugins/{id}"),
                None,
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::CONFLICT);
        let body = body_json(response).await;
        assert_eq!(
            body["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"
        );
        assert_eq!(body["plugin_id"], plugin_ref);
        let referenced_upstreams = body["referenced_by"]["upstreams"].as_array().unwrap();
        let referenced_routes = body["referenced_by"]["routes"].as_array().unwrap();
        assert_eq!(referenced_upstreams.len(), 1);
        assert_eq!(referenced_routes.len(), 1);
        assert_eq!(
            referenced_upstreams[0],
            format!("gts.cf.core.oagw.upstream.v1~{upstream_id}")
        );
        assert_eq!(
            referenced_routes[0],
            format!("gts.cf.core.oagw.route.v1~{route_id}")
        );
    }

    /// A plugin's `gc_eligible_at` is non-null at creation, becomes `null`
    /// once bound, and becomes non-null again once that binding is removed.
    #[tokio::test]
    async fn gc_eligible_at_transitions_across_the_reference_lifecycle() {
        let (router, state) = build_router();
        let tenant_id = Uuid::new_v4();

        let created = router
            .clone()
            .oneshot(request(
                "POST",
                "/oagw/v1/plugins",
                Some(guard_create_body("gc-lifecycle")),
                tenant_id,
            ))
            .await
            .unwrap();
        let created = body_json(created).await;
        let id = created["id"].as_str().unwrap().to_owned();
        assert!(created["gc_eligible_at"].is_string());
        let plugin_uuid = Uuid::parse_str(&id).unwrap();
        let plugin_ref = plugin_gts_ref(PluginType::Guard, plugin_uuid);

        // Bind it (simulating what upstream-management's write path does):
        // the delete flow's mark-phase re-evaluation is exercised via the
        // `delete` attempt below, which must observe a nonzero reference
        // count and therefore never clear `gc_eligible_at` via this feature
        // -- clearing on bind is `cpt-cf-oagw-feature-upstream-management`'s
        // write path, out of this feature's file ownership. This test
        // instead verifies the model-level algorithm directly.
        let upstream_id = seed_upstream_bound_to_plugin(&state, tenant_id, &plugin_ref);

        let plugins = state.store.plugins();
        let stored_ref = plugins.get(&plugin_uuid).unwrap();
        let stored = Arc::clone(stored_ref.value());
        drop(stored_ref);
        let references = crate::model::plugin::count_plugin_references(
            &plugin_ref,
            Some(plugin_uuid),
            tenant_id,
            state.store.upstreams(),
            state.store.routes(),
        );
        assert_eq!(references.count(), 1);
        let marked = crate::model::plugin::mark_gc_eligibility(
            stored.gc_eligible_at.as_deref(),
            references.count(),
        );
        assert_eq!(marked, None, "referenced plugin must not be GC-eligible");

        state.store.upstreams().remove(&upstream_id);
        let references_after_unbind = crate::model::plugin::count_plugin_references(
            &plugin_ref,
            Some(plugin_uuid),
            tenant_id,
            state.store.upstreams(),
            state.store.routes(),
        );
        assert_eq!(references_after_unbind.count(), 0);
        let marked_after_unbind =
            crate::model::plugin::mark_gc_eligibility(None, references_after_unbind.count());
        assert!(marked_after_unbind.is_some());
    }

    #[tokio::test]
    async fn no_put_or_patch_route_is_registered_for_plugins() {
        let (router, _state) = build_router();
        let tenant_id = Uuid::new_v4();

        let response = router
            .oneshot(request(
                "PUT",
                "/oagw/v1/plugins/00000000-0000-0000-0000-000000000000",
                Some(json!({})),
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::METHOD_NOT_ALLOWED);
    }

    /// This feature's tests never parse, interpret, or execute
    /// `source_code` -- only round-trip it verbatim -- verifying the
    /// `cpt-cf-oagw-nfr-starlark-sandbox` storage/identification-only
    /// carve-out.
    #[tokio::test]
    async fn source_code_is_never_parsed_or_executed_only_stored_and_returned() {
        let (router, _state) = build_router();
        let tenant_id = Uuid::new_v4();
        // Deliberately not valid Starlark -- if anything on this feature's
        // path ever tried to parse/execute it, this would fail loudly
        // rather than silently.
        let source = "this is not valid starlark at all {{{ ???";
        let body = json!({
            "plugin_type": "transform",
            "name": "unexecuted",
            "source_code": source,
        });

        let created = router
            .clone()
            .oneshot(request("POST", "/oagw/v1/plugins", Some(body), tenant_id))
            .await
            .unwrap();
        assert_eq!(created.status(), Status::CREATED);
        let created = body_json(created).await;
        let id = created["id"].as_str().unwrap();

        let response = router
            .oneshot(request(
                "GET",
                &format!("/oagw/v1/plugins/{id}/source"),
                None,
                tenant_id,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), Status::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(bytes.as_ref(), source.as_bytes());
    }
}
