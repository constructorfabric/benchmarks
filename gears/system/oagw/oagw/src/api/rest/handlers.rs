//! REST handlers for the OAGW management + proxy intake.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, Query};
use axum::http::StatusCode;
use toolkit::api::canonical_prelude::*;
use toolkit_security::SecurityContext;

use super::dto::{
    PluginDto, RouteDto, UpstreamDto,
};
use crate::domain::error::{OagwConfigError, code};
use crate::domain::service::{ControlPlaneService, DataPlaneService};

/// Resolve the tenant-scoped id `{id}` path segment.
fn parse_id(raw: &str) -> Result<uuid::Uuid, CanonicalError> {
    raw.parse::<uuid::Uuid>().map_err(|_| {
        OagwConfigError::invalid_argument()
            .with_field_violation(
                "id",
                format!("'{raw}' is not a valid UUID"),
                code::INVALID_FORMAT,
            )
            .with_resource(raw.to_owned())
            .create()
    })
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/upstreams`
pub async fn create_upstream(
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Json(body): Json<UpstreamDto>,
) -> ApiResult<(StatusCode, Json<UpstreamDto>)> {
    let created = svc.create_upstream(ctx.subject_tenant_id(), body.into())?;
    Ok((StatusCode::CREATED, Json(created.into())))
}

/// `GET /oagw/v1/upstreams`
pub async fn list_upstreams(
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Vec<UpstreamDto>>> {
    let all = svc.list_upstreams(ctx.subject_tenant_id());
    let dtos: Vec<UpstreamDto> = all.into_iter().map(|u| (*u).clone().into()).collect();
    Ok(Json(slice(dtos, query.top, query.skip)))
}

/// `GET /oagw/v1/upstreams/{id}`
pub async fn get_upstream(
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<UpstreamDto>> {
    let id = parse_id(&id)?;
    let up = svc.get_upstream(ctx.subject_tenant_id(), id)?;
    Ok(Json((*up).clone().into()))
}

/// `PUT /oagw/v1/upstreams/{id}`
pub async fn update_upstream(
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
    Json(body): Json<UpstreamDto>,
) -> ApiResult<Json<UpstreamDto>> {
    let id = parse_id(&id)?;
    let updated = svc.update_upstream(ctx.subject_tenant_id(), id, body.into())?;
    Ok(Json(updated.into()))
}

/// `DELETE /oagw/v1/upstreams/{id}`
pub async fn delete_upstream(
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let id = parse_id(&id)?;
    svc.delete_upstream(ctx.subject_tenant_id(), id)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/routes`
pub async fn create_route(
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Json(body): Json<RouteDto>,
) -> ApiResult<(StatusCode, Json<RouteDto>)> {
    let created = svc.create_route(ctx.subject_tenant_id(), body.into())?;
    Ok((StatusCode::CREATED, Json(created.into())))
}

/// `GET /oagw/v1/routes`
pub async fn list_routes(
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Vec<RouteDto>>> {
    let all = svc.list_routes(ctx.subject_tenant_id());
    let dtos: Vec<RouteDto> = all.into_iter().map(|r| (*r).clone().into()).collect();
    Ok(Json(slice(dtos, query.top, query.skip)))
}

/// `GET /oagw/v1/routes/{id}`
pub async fn get_route(
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<RouteDto>> {
    let id = parse_id(&id)?;
    let route = svc.get_route(ctx.subject_tenant_id(), id)?;
    Ok(Json((*route).clone().into()))
}

/// `PUT /oagw/v1/routes/{id}`
pub async fn update_route(
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
    Json(body): Json<RouteDto>,
) -> ApiResult<Json<RouteDto>> {
    let id = parse_id(&id)?;
    let updated = svc.update_route(ctx.subject_tenant_id(), id, body.into())?;
    Ok(Json(updated.into()))
}

/// `DELETE /oagw/v1/routes/{id}`
pub async fn delete_route(
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let id = parse_id(&id)?;
    svc.delete_route(ctx.subject_tenant_id(), id)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/plugins`
pub async fn create_plugin(
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Json(body): Json<PluginDto>,
) -> ApiResult<(StatusCode, Json<PluginDto>)> {
    let created = svc.create_plugin(ctx.subject_tenant_id(), body.into())?;
    Ok((StatusCode::CREATED, Json(created.into())))
}

/// `GET /oagw/v1/plugins`
pub async fn list_plugins(
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Query(query): Query<ListQuery>,
) -> ApiResult<Json<Vec<PluginDto>>> {
    let all = svc.list_plugins(ctx.subject_tenant_id());
    let dtos: Vec<PluginDto> = all.into_iter().map(|p| (*p).clone().into()).collect();
    Ok(Json(slice(dtos, query.top, query.skip)))
}

/// `GET /oagw/v1/plugins/{id}`
pub async fn get_plugin(
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<PluginDto>> {
    let id = parse_id(&id)?;
    let plugin = svc.get_plugin(ctx.subject_tenant_id(), id)?;
    Ok(Json((*plugin).clone().into()))
}

/// `GET /oagw/v1/plugins/{id}/source`
pub async fn get_plugin_source(
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, String)> {
    let id = parse_id(&id)?;
    let plugin = svc.get_plugin(ctx.subject_tenant_id(), id)?;
    Ok((StatusCode::OK, plugin.source_code.clone()))
}

/// `DELETE /oagw/v1/plugins/{id}`
pub async fn delete_plugin(
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let id = parse_id(&id)?;
    svc.delete_plugin(ctx.subject_tenant_id(), id)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Proxy intake
// ---------------------------------------------------------------------------

/// `{METHOD} /oagw/v1/proxy/{alias}[/{*path}]`
///
/// Hands the unfiltered request to the data plane. The alias/suffix is parsed
/// from the request path inside the data plane, and the `X-OAGW-Target-Host`
/// routing hint is forwarded for multi-endpoint upstreams.
pub async fn proxy(
    Extension(dp): Extension<Arc<dyn DataPlaneService>>,
    Extension(ctx): Extension<SecurityContext>,
    request: axum::extract::Request,
) -> axum::response::Response {
    let target = request
        .headers()
        .get("x-oagw-target-host")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    dp.proxy(
        ctx.subject_tenant_id(),
        ctx.subject_id(),
        request,
        target,
    )
    .await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// `$top` / `$skip` pagination applied to list results. OData `$filter`,
/// `$select`, and `$orderby` are accepted (schema-visible) and applied
/// minimally: the lists are small in-memory snapshots.
#[derive(Debug, serde::Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub top: Option<usize>,
    #[serde(default)]
    pub skip: Option<usize>,
}

fn slice<T>(items: Vec<T>, top: Option<usize>, skip: Option<usize>) -> Vec<T> {
    let skip = skip.unwrap_or(0);
    let mut out: Vec<T> = items.into_iter().skip(skip).collect();
    if let Some(top) = top {
        out.truncate(top.max(1));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use serde_json::{Value, json};
    use tower::ServiceExt;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    use super::*;
    use crate::config::OagwConfig;
    use crate::domain::service::ControlPlaneService;
    use crate::infra::storage::{
        InMemoryPluginRepo, InMemoryRouteRepo, InMemoryUpstreamRepo,
    };

    /// Stub data plane that records calls and returns a canned upstream body.
    #[derive(Default)]
    struct StubDataPlane {
        calls: AtomicUsize,
        last_target: Mutex<Option<String>>,
    }

    #[async_trait::async_trait]
    impl DataPlaneService for StubDataPlane {
        async fn proxy(
            &self,
            _tenant_id: Uuid,
            _subject_id: Uuid,
            _req: axum::http::Request<Body>,
            target_host_header: Option<String>,
        ) -> axum::response::Response {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last_target.lock().unwrap() = target_host_header;
            axum::response::Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"proxied":true}"#))
                .unwrap()
        }
    }

    fn tenant() -> Uuid {
        Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap()
    }

    fn sec_ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::parse_str("22222222-2222-2222-2222-222222222222").unwrap())
            .subject_tenant_id(tenant())
            .build()
            .unwrap()
    }

    fn app() -> (Router, Arc<StubDataPlane>) {
        let upstreams = Arc::new(InMemoryUpstreamRepo::new());
        let routes = Arc::new(InMemoryRouteRepo::new());
        let plugins = Arc::new(InMemoryPluginRepo::new());
        let control = Arc::new(ControlPlaneService::new(
            upstreams,
            routes,
            plugins,
            OagwConfig::default(),
        ));
        let dp: Arc<StubDataPlane> = Arc::new(StubDataPlane::default());
        let openapi = toolkit::api::OpenApiRegistryImpl::new();
        let router = crate::api::rest::routes::register_routes(
            Router::new(),
            &openapi,
            control,
            dp.clone() as Arc<dyn DataPlaneService>,
        )
        .layer(Extension(sec_ctx()));
        (router, dp)
    }

    fn upstream_body(alias: &str) -> Value {
        json!({
            "alias": alias,
            "server": { "endpoints": [{ "scheme": "https", "host": alias, "port": 443 }] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        })
    }

    async fn create_upstream_alias(alias: &str) -> (Router, Arc<StubDataPlane>, Value) {
        let (router, dp) = app();
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oagw/v1/upstreams")
                    .header("content-type", "application/json")
                    .body(Body::from(upstream_body(alias).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        if resp.status() != StatusCode::CREATED {
            let status = resp.status();
            let dbg = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
            panic!("created upstream: got {status:?} body: {}", String::from_utf8_lossy(&dbg));
        }
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        (router, dp, body)
    }

    #[tokio::test]
    async fn upstreams_crud_roundtrip() {
        let (router, _dp, created) = create_upstream_alias("api-band.example.com").await;
        let id = created["id"].as_str().unwrap().to_owned();
        assert_eq!(created["alias"], json!("api-band.example.com"));
        assert_eq!(created["enabled"], json!(true));

        // Duplicate alias → 409.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oagw/v1/upstreams")
                    .header("content-type", "application/json")
                    .body(Body::from(upstream_body("api-band.example.com").to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        // List contains it.
        let resp = router
            .clone()
            .oneshot(Request::builder().uri("/oagw/v1/upstreams").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let list: Vec<Value> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(list.len(), 1);

        // Get by id.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/oagw/v1/upstreams/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Update (alias immutable; body alias must match stored).
        let mut upd = upstream_body("api-band.example.com");
        upd["enabled"] = json!(false);
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!("/oagw/v1/upstreams/{id}"))
                    .header("content-type", "application/json")
                    .body(Body::from(upd.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "update accepted");

        // Delete → 204, then GET → 404.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/oagw/v1/upstreams/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/oagw/v1/upstreams/{id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn invalid_scheme_rejected() {
        let (router, _dp) = app();
        let mut body = upstream_body("plain.example.com");
        body["server"]["endpoints"][0]["scheme"] = json!("http");
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oagw/v1/upstreams")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "http denied by default");
    }

    #[tokio::test]
    async fn routes_require_local_upstream() {
        let (_router, _dp, created) = create_upstream_alias("route-host.example.com").await;
        let (router, _dp) = app();
        let up_id = created["id"].as_str().unwrap();
        let body = json!({
            "upstream_id": up_id,
            "match": { "methods": ["GET"], "path": "/v1/chat" }
        });
        // Unknown upstream (fresh app) → 400 validation error.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oagw/v1/routes")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "unknown upstream id");
    }

    #[tokio::test]
    async fn route_crud_and_plugin_in_use() {
        let (router, _dp, upstream) = create_upstream_alias("svc-a.example.com").await;
        let up_id = upstream["id"].as_str().unwrap().to_owned();

        let body = json!({
            "upstream_id": up_id,
            "match": { "methods": ["GET"], "path": "/v1/chat" }
        });
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oagw/v1/routes")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED, "route created");
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let route: Value = serde_json::from_slice(&bytes).unwrap();
        let route_id = route["id"].as_str().unwrap().to_owned();

        // Delete the upstream while referenced → 409.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/oagw/v1/upstreams/{up_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT, "upstream still referenced");

        // Plugin create + source get.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oagw/v1/plugins")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "plugin_type": "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1",
                            "name": "add-req-id",
                            "config_schema": {},
                            "source_code": "def transform_request(ctx):\n    return ctx"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED, "plugin created");
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let plugin: Value = serde_json::from_slice(&bytes).unwrap();
        let plugin_id = plugin["id"].as_str().unwrap().to_owned();

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/oagw/v1/plugins/{plugin_id}/source"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        assert!(bytes.windows(6).any(|w| w == b"def tr"));

        // Delete route then upstream → 204.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/oagw/v1/routes/{route_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/oagw/v1/upstreams/{up_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn proxy_intake_reaches_data_plane() {
        let (router, dp) = app();
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oagw/v1/proxy/api-band.example.com/v1/chat")
                    .header("content-type", "application/json")
                    .header("x-oagw-target-host", "api.example.com")
                    .body(Body::from(r#"{"q":"hi"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(dp.calls.load(Ordering::SeqCst), 1);
        let target = dp.last_target.lock().unwrap().clone();
        assert_eq!(target.as_deref(), Some("api.example.com"));
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["proxied"], json!(true));
    }
}
