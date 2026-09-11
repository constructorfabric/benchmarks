//! Axum handlers.
//!
//! The management handlers are thin: parse, delegate to the Control Plane,
//! render. The proxy handler is where the transport layer earns its keep — it
//! buffers the body under the configured limit, lifts the connection-upgrade
//! handle out of the request extensions so a WebSocket can be spliced later,
//! and hands everything to the Data Plane.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Extension, Path, Request};
use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{list_envelope, plugin_view, route_view, upstream_view};
use crate::api::rest::query;
use crate::domain::dto::{PluginSpec, RouteSpec, UpstreamSpec};
use crate::domain::error::{ErrorKind, OagwError, OagwResult};
use crate::domain::gts_helpers::{ROUTE_TYPE, UPSTREAM_TYPE, parse_resource_id};
use crate::domain::services::management::ControlPlane;
use crate::infra::proxy::{DataPlane, ProxyRequest};

/// Shared state behind every OAGW route.
pub struct AppState {
    /// Configuration owner.
    pub control: Arc<ControlPlane>,
    /// Proxy orchestrator.
    pub data_plane: Arc<DataPlane>,
    /// Maximum buffered request body.
    pub max_body_bytes: usize,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AppState")
    }
}

/// Parse a JSON body into a write model, mapping deserialisation failures to
/// a `400` that names the offending member.
fn parse_body<T: serde::de::DeserializeOwned>(body: &Bytes) -> OagwResult<T> {
    if body.is_empty() {
        return Err(OagwError::validation("a JSON request body is required"));
    }
    serde_json::from_slice(body).map_err(|err| {
        OagwError::validation(format!("request body does not match the schema: {err}"))
    })
}

/// Render an error as a problem+json response with the request path as
/// `instance`.
fn render(result: OagwResult<Response>, uri: &Uri) -> Response {
    match result {
        Ok(response) => response,
        Err(err) => err.into_response_with_instance(Some(uri.path())),
    }
}

/// `201 Created` with a `Location` header and the created representation.
fn created(view: Value, uri: &Uri, id: &str) -> Response {
    let location = format!("{}/{}", uri.path().trim_end_matches('/'), id);
    (
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        axum::Json(view),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/upstreams`
pub async fn create_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<AppState>>,
    uri: Uri,
    body: Bytes,
) -> Response {
    let result = async {
        let spec: UpstreamSpec = parse_body(&body)?;
        let created_upstream = state.control.create_upstream(&ctx, spec).await?;
        let id = created_upstream.id.to_string();
        Ok(created(upstream_view(&created_upstream), &uri, &id))
    }
    .await;
    render(result, &uri)
}

/// `GET /oagw/v1/upstreams`
pub async fn list_upstreams(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<AppState>>,
    uri: Uri,
) -> Response {
    let result = async {
        let list_query = query::parse(uri.query().unwrap_or_default())?;
        let views: Vec<Value> = state
            .control
            .list_upstreams(&ctx)
            .await?
            .iter()
            .map(upstream_view)
            .collect();
        let page = query::apply(views, &list_query);
        Ok(axum::Json(list_envelope(page, list_query.top)).into_response())
    }
    .await;
    render(result, &uri)
}

/// `GET /oagw/v1/upstreams/{id}`
pub async fn get_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<AppState>>,
    Path(id): Path<String>,
    uri: Uri,
) -> Response {
    let result = async {
        let id = resource_id(UPSTREAM_TYPE, &id, "upstream")?;
        let upstream = state.control.get_upstream(&ctx, id).await?;
        Ok(axum::Json(upstream_view(&upstream)).into_response())
    }
    .await;
    render(result, &uri)
}

/// `PUT /oagw/v1/upstreams/{id}`
pub async fn replace_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<AppState>>,
    Path(id): Path<String>,
    uri: Uri,
    body: Bytes,
) -> Response {
    let result = async {
        let id = resource_id(UPSTREAM_TYPE, &id, "upstream")?;
        let spec: UpstreamSpec = parse_body(&body)?;
        let upstream = state.control.replace_upstream(&ctx, id, spec).await?;
        Ok(axum::Json(upstream_view(&upstream)).into_response())
    }
    .await;
    render(result, &uri)
}

/// `DELETE /oagw/v1/upstreams/{id}`
pub async fn delete_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<AppState>>,
    Path(id): Path<String>,
    uri: Uri,
) -> Response {
    let result = async {
        let id = resource_id(UPSTREAM_TYPE, &id, "upstream")?;
        state.control.delete_upstream(&ctx, id).await?;
        state
            .data_plane
            .limiter()
            .forget_resource("upstream", &id.to_string());
        Ok(StatusCode::NO_CONTENT.into_response())
    }
    .await;
    render(result, &uri)
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/routes`
pub async fn create_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<AppState>>,
    uri: Uri,
    body: Bytes,
) -> Response {
    let result = async {
        let spec: RouteSpec = parse_body(&body)?;
        let route = state.control.create_route(&ctx, spec).await?;
        let id = route.id.to_string();
        Ok(created(route_view(&route), &uri, &id))
    }
    .await;
    render(result, &uri)
}

/// `GET /oagw/v1/routes`
pub async fn list_routes(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<AppState>>,
    uri: Uri,
) -> Response {
    let result = async {
        let list_query = query::parse(uri.query().unwrap_or_default())?;
        let views: Vec<Value> = state
            .control
            .list_routes(&ctx)
            .await?
            .iter()
            .map(route_view)
            .collect();
        let page = query::apply(views, &list_query);
        Ok(axum::Json(list_envelope(page, list_query.top)).into_response())
    }
    .await;
    render(result, &uri)
}

/// `GET /oagw/v1/routes/{id}`
pub async fn get_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<AppState>>,
    Path(id): Path<String>,
    uri: Uri,
) -> Response {
    let result = async {
        let id = resource_id(ROUTE_TYPE, &id, "route")?;
        let route = state.control.get_route(&ctx, id).await?;
        Ok(axum::Json(route_view(&route)).into_response())
    }
    .await;
    render(result, &uri)
}

/// `PUT /oagw/v1/routes/{id}`
pub async fn replace_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<AppState>>,
    Path(id): Path<String>,
    uri: Uri,
    body: Bytes,
) -> Response {
    let result = async {
        let id = resource_id(ROUTE_TYPE, &id, "route")?;
        let spec: RouteSpec = parse_body(&body)?;
        let route = state.control.replace_route(&ctx, id, spec).await?;
        Ok(axum::Json(route_view(&route)).into_response())
    }
    .await;
    render(result, &uri)
}

/// `DELETE /oagw/v1/routes/{id}`
pub async fn delete_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<AppState>>,
    Path(id): Path<String>,
    uri: Uri,
) -> Response {
    let result = async {
        let id = resource_id(ROUTE_TYPE, &id, "route")?;
        state.control.delete_route(&ctx, id).await?;
        state
            .data_plane
            .limiter()
            .forget_resource("route", &id.to_string());
        Ok(StatusCode::NO_CONTENT.into_response())
    }
    .await;
    render(result, &uri)
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/plugins`
pub async fn create_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<AppState>>,
    uri: Uri,
    body: Bytes,
) -> Response {
    let result = async {
        let spec: PluginSpec = parse_body(&body)?;
        let plugin = state.control.create_plugin(&ctx, spec).await?;
        let id = plugin.plugin_ref();
        Ok(created(plugin_view(&plugin), &uri, &id))
    }
    .await;
    render(result, &uri)
}

/// `GET /oagw/v1/plugins`
pub async fn list_plugins(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<AppState>>,
    uri: Uri,
) -> Response {
    let result = async {
        let list_query = query::parse(uri.query().unwrap_or_default())?;
        let views: Vec<Value> = state
            .control
            .list_plugins(&ctx)
            .await?
            .iter()
            .map(plugin_view)
            .collect();
        let page = query::apply(views, &list_query);
        Ok(axum::Json(list_envelope(page, list_query.top)).into_response())
    }
    .await;
    render(result, &uri)
}

/// `GET /oagw/v1/plugins/{id}`
pub async fn get_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<AppState>>,
    Path(id): Path<String>,
    uri: Uri,
) -> Response {
    let result = async {
        let id = plugin_id(&id)?;
        let plugin = state.control.get_plugin(&ctx, id).await?;
        Ok(axum::Json(plugin_view(&plugin)).into_response())
    }
    .await;
    render(result, &uri)
}

/// `GET /oagw/v1/plugins/{id}/source`
pub async fn get_plugin_source(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<AppState>>,
    Path(id): Path<String>,
    uri: Uri,
) -> Response {
    let result = async {
        let id = plugin_id(&id)?;
        let plugin = state.control.get_plugin(&ctx, id).await?;
        Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            plugin.source_code,
        )
            .into_response())
    }
    .await;
    render(result, &uri)
}

/// `DELETE /oagw/v1/plugins/{id}`
pub async fn delete_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<AppState>>,
    Path(id): Path<String>,
    uri: Uri,
) -> Response {
    let result = async {
        let id = plugin_id(&id)?;
        state.control.delete_plugin(&ctx, id).await?;
        Ok(StatusCode::NO_CONTENT.into_response())
    }
    .await;
    render(result, &uri)
}

// ---------------------------------------------------------------------------
// Proxy
// ---------------------------------------------------------------------------

/// `{METHOD} /oagw/v1/proxy/{alias}`
pub async fn proxy_root(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<AppState>>,
    Path(alias): Path<String>,
    request: Request,
) -> Response {
    proxy(ctx, state, alias, String::new(), request).await
}

/// `{METHOD} /oagw/v1/proxy/{alias}/{*path}`
pub async fn proxy_path(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<AppState>>,
    Path((alias, path)): Path<(String, String)>,
    request: Request,
) -> Response {
    proxy(ctx, state, alias, path, request).await
}

/// Shared proxy entry point.
async fn proxy(
    ctx: SecurityContext,
    state: Arc<AppState>,
    alias: String,
    path_suffix: String,
    request: Request,
) -> Response {
    let (mut parts, body) = request.into_parts();
    let instance = parts.uri.path().to_owned();
    let query = parts.uri.query().unwrap_or_default().to_owned();
    // Lifted out of the extensions before the body is consumed: without it a
    // WebSocket could not be spliced later.
    let on_upgrade = parts.extensions.remove::<hyper::upgrade::OnUpgrade>();
    let client_ip = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip().to_string());

    // Reject an over-large body before buffering it, and cap what is read so
    // a lying Content-Length cannot exhaust memory
    // (`cpt-cf-oagw-constraint-body-limit`).
    if let Some(declared) = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<usize>().ok())
        && declared > state.max_body_bytes
    {
        return OagwError::new(
            ErrorKind::PayloadTooLarge,
            format!(
                "Content-Length {declared} exceeds the {} byte limit",
                state.max_body_bytes
            ),
        )
        .into_response_with_instance(Some(&instance));
    }
    let body = match axum::body::to_bytes(body, state.max_body_bytes).await {
        Ok(body) => body,
        Err(err) => {
            return OagwError::new(
                ErrorKind::PayloadTooLarge,
                format!(
                    "request body exceeds the {} byte limit or could not be read: {err}",
                    state.max_body_bytes
                ),
            )
            .into_response_with_instance(Some(&instance));
        }
    };

    state
        .data_plane
        .execute_proxy(ProxyRequest {
            security_context: ctx,
            method: parts.method.clone(),
            alias,
            path_suffix,
            query,
            headers: parts.headers.clone(),
            body,
            client_ip,
            instance,
            on_upgrade,
        })
        .await
}

/// Parse an upstream/route path parameter.
fn resource_id(base_type: &str, raw: &str, kind: &str) -> OagwResult<Uuid> {
    parse_resource_id(base_type, raw).ok_or_else(|| {
        OagwError::field(
            "id",
            format!("{kind} id must be a UUID or {base_type}{{uuid}}: {raw:?}"),
        )
    })
}

/// Parse a plugin path parameter, accepting a bare UUID or any of the three
/// plugin GTS base types.
fn plugin_id(raw: &str) -> OagwResult<Uuid> {
    use crate::domain::gts_helpers::{AUTH_PLUGIN_TYPE, GUARD_PLUGIN_TYPE, TRANSFORM_PLUGIN_TYPE};
    for base in [AUTH_PLUGIN_TYPE, GUARD_PLUGIN_TYPE, TRANSFORM_PLUGIN_TYPE] {
        if let Some(id) = parse_resource_id(base, raw) {
            return Ok(id);
        }
    }
    Err(OagwError::field(
        "id",
        format!(
            "plugin id must be a UUID or gts.cf.core.oagw.{{type}}_plugin.v1~{{uuid}}: {raw:?}"
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::gts_helpers::{GUARD_PLUGIN_TYPE, anonymous_id};

    #[test]
    fn resource_ids_accept_both_spellings() {
        let id = Uuid::new_v4();
        assert_eq!(
            resource_id(UPSTREAM_TYPE, &id.to_string(), "upstream").unwrap(),
            id
        );
        assert_eq!(
            resource_id(UPSTREAM_TYPE, &anonymous_id(UPSTREAM_TYPE, id), "upstream").unwrap(),
            id
        );
        // A route id offered where an upstream id belongs is a 400, not a 404.
        let err = resource_id(UPSTREAM_TYPE, &anonymous_id(ROUTE_TYPE, id), "upstream")
            .expect_err("cross-type id");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(resource_id(UPSTREAM_TYPE, "not-a-uuid", "upstream").is_err());
    }

    #[test]
    fn plugin_ids_accept_every_plugin_base_type() {
        let id = Uuid::new_v4();
        assert_eq!(plugin_id(&id.to_string()).unwrap(), id);
        assert_eq!(plugin_id(&anonymous_id(GUARD_PLUGIN_TYPE, id)).unwrap(), id);
        assert!(plugin_id("nope").is_err());
    }

    #[test]
    fn empty_bodies_are_rejected_before_parsing() {
        let err = parse_body::<UpstreamSpec>(&Bytes::new()).expect_err("empty body");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn malformed_bodies_name_the_problem() {
        let err = parse_body::<UpstreamSpec>(&Bytes::from_static(b"{\"server\": 1}"))
            .expect_err("bad body");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(err.detail().contains("schema"), "{}", err.detail());
    }
}
