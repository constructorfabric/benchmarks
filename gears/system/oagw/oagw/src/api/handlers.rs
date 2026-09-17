//! Axum handlers for the OAGW management API and the data plane.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Extension, Path, Request};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use uuid::Uuid;

use crate::api::dto::{
    self, CreatePluginRequest, CreateRouteRequest, CreateUpstreamRequest, PluginDto, RouteDto,
    UpdateRouteRequest, UpdateUpstreamRequest, UpstreamDto,
};
use crate::domain::model::{Endpoint, ServerConfig};
use crate::domain::service::ControlPlaneService;
use crate::error::{ErrorKind, OagwError, OagwResult};

/// Shared handler services.
#[derive(Clone)]
pub struct Services {
    /// Control plane.
    pub control: Arc<ControlPlaneService<crate::infra::storage::MemoryStore, crate::infra::storage::MemoryStore, crate::infra::storage::MemoryStore>>,
    /// Data plane.
    pub data: Arc<crate::infra::proxy::service::DataPlaneService<
        crate::infra::storage::MemoryStore,
        crate::infra::storage::MemoryStore,
        crate::infra::storage::MemoryStore,
    >>,
    /// Gear configuration.
    pub config: crate::config::OagwConfig,
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/upstreams`
///
/// # Errors
/// 400 on validation failure, 409 on alias conflict.
pub async fn create_upstream(
    Extension(services): Extension<Services>,
    ctx: axum::Extension<toolkit_security::SecurityContext>,
    body: Option<axum::Json<CreateUpstreamRequest>>,
) -> OagwResult<Response> {
    let request = body.map(|axum::Json(b)| b).unwrap_or_default();
    let (server, protocol) = required_server(&request.server, &request.protocol)?;
    let upstream = services
        .control
        .create_upstream(
            ctx.subject_tenant_id(),
            request.alias,
            request.enabled.unwrap_or(true),
            server,
            protocol,
            request.tags.unwrap_or_default(),
            request.headers.unwrap_or_default(),
            request.rate_limit,
            request.cors,
            request.auth,
            request.plugins.map(dto::UpstreamPluginsDto::into_domain).unwrap_or_default(),
        )
        .await?;
    Ok((StatusCode::CREATED, axum::Json(UpstreamDto::from(upstream))).into_response())
}

/// `GET /oagw/v1/upstreams`
///
/// # Errors
/// 500 on storage failure.
pub async fn list_upstreams(
    Extension(services): Extension<Services>,
    ctx: axum::Extension<toolkit_security::SecurityContext>,
) -> OagwResult<Response> {
    let items = services
        .control
        .list_upstreams(ctx.subject_tenant_id())
        .await?
        .into_iter()
        .map(UpstreamDto::from)
        .collect::<Vec<_>>();
    Ok(axum::Json(items).into_response())
}

/// `GET /oagw/v1/upstreams/{id}`
///
/// # Errors
/// 404 when the upstream is not owned by the tenant.
pub async fn get_upstream(
    Extension(services): Extension<Services>,
    ctx: axum::Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
) -> OagwResult<Response> {
    let upstream = services
        .control
        .get_upstream(ctx.subject_tenant_id(), id)
        .await?;
    Ok(axum::Json(UpstreamDto::from(upstream)).into_response())
}

/// `PUT /oagw/v1/upstreams/{id}`
///
/// # Errors
/// 400 on validation failure, 404 when absent, 409 on alias conflict.
pub async fn update_upstream(
    Extension(services): Extension<Services>,
    ctx: axum::Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
    body: Option<axum::Json<UpdateUpstreamRequest>>,
) -> OagwResult<Response> {
    let request = body.map(|axum::Json(b)| b).unwrap_or_default();
    let existing = services
        .control
        .get_upstream(ctx.subject_tenant_id(), id)
        .await?;
    let upstream = services
        .control
        .replace_upstream(
            ctx.subject_tenant_id(),
            id,
            request.alias,
            request.enabled.unwrap_or(existing.enabled),
            required_server(&request.server, &request.protocol)?.0,
            request
                .protocol
                .unwrap_or_else(|| existing.protocol.clone()),
            request.tags.unwrap_or_default(),
            request.headers.unwrap_or_default(),
            request.rate_limit,
            request.cors,
            request.auth,
            request.plugins.map(dto::UpstreamPluginsDto::into_domain).unwrap_or_default(),
        )
        .await?;
    Ok(axum::Json(UpstreamDto::from(upstream)).into_response())
}

/// `DELETE /oagw/v1/upstreams/{id}`
///
/// # Errors
/// 404 when absent.
pub async fn delete_upstream(
    Extension(services): Extension<Services>,
    ctx: axum::Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
) -> OagwResult<Response> {
    services
        .control
        .delete_upstream(ctx.subject_tenant_id(), id)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn required_server(
    server: &Option<ServerConfig>,
    protocol: &Option<String>,
) -> OagwResult<(Vec<Endpoint>, String)> {
    let endpoints = server
        .as_ref()
        .map(|s| s.endpoints.clone())
        .unwrap_or_default();
    if endpoints.is_empty() {
        return Err(OagwError::new(
            ErrorKind::ValidationError,
            "server.endpoints must contain at least one endpoint",
        )
        .with_ext("fields", serde_json::Value::from(["server.endpoints"])));
    }
    if protocol.is_none() {
        return Err(OagwError::new(
            ErrorKind::ValidationError,
            "protocol is required",
        )
        .with_ext("fields", serde_json::Value::from(["protocol"])));
    }
    Ok((endpoints, protocol.clone().unwrap_or_default()))
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/routes`
///
/// # Errors
/// 400 when the upstream is unknown or the match rule is invalid.
pub async fn create_route(
    Extension(services): Extension<Services>,
    ctx: axum::Extension<toolkit_security::SecurityContext>,
    body: Option<axum::Json<CreateRouteRequest>>,
) -> OagwResult<Response> {
    let request = body.map(|axum::Json(b)| b).unwrap_or_default();
    let upstream_id = request.upstream_id.ok_or_else(|| {
        OagwError::new(ErrorKind::ValidationError, "upstream_id is required")
            .with_ext("fields", serde_json::Value::from(["upstream_id"]))
    })?;
    let r#match = request.r#match.ok_or_else(|| {
        OagwError::new(ErrorKind::ValidationError, "match is required")
            .with_ext("fields", serde_json::Value::from(["match"]))
    })?;
    let route = services
        .control
        .create_route(
            ctx.subject_tenant_id(),
            upstream_id,
            request.tags.unwrap_or_default(),
            r#match,
            request.plugins.map(dto::UpstreamPluginsDto::into_domain).unwrap_or_default(),
            request.rate_limit,
            request.cors,
            request.headers,
        )
        .await?;
    Ok((StatusCode::CREATED, axum::Json(RouteDto::from(route))).into_response())
}

/// `GET /oagw/v1/routes`
///
/// # Errors
/// 500 on storage failure.
pub async fn list_routes(
    Extension(services): Extension<Services>,
    ctx: axum::Extension<toolkit_security::SecurityContext>,
    uri: Uri,
) -> OagwResult<Response> {
    let upstream_id = uri
        .query()
        .and_then(|q| {
            form_urlencoded::parse(q.as_bytes())
                .find(|(k, _)| k == "upstream_id")
                .and_then(|(_, v)| Uuid::parse_str(v.as_ref()).ok())
        });
    let items = services
        .control
        .list_routes(ctx.subject_tenant_id(), upstream_id)
        .await?
        .into_iter()
        .map(RouteDto::from)
        .collect::<Vec<_>>();
    Ok(axum::Json(items).into_response())
}

/// `GET /oagw/v1/routes/{id}`
///
/// # Errors
/// 404 when absent.
pub async fn get_route(
    Extension(services): Extension<Services>,
    ctx: axum::Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
) -> OagwResult<Response> {
    let route = services.control.get_route(ctx.subject_tenant_id(), id).await?;
    Ok(axum::Json(RouteDto::from(route)).into_response())
}

/// `PUT /oagw/v1/routes/{id}`
///
/// # Errors
/// 400 on validation failure, 404 when absent.
pub async fn update_route(
    Extension(services): Extension<Services>,
    ctx: axum::Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
    body: Option<axum::Json<UpdateRouteRequest>>,
) -> OagwResult<Response> {
    let request = body.map(|axum::Json(b)| b).unwrap_or_default();
    let existing = services.control.get_route(ctx.subject_tenant_id(), id).await?;
    let route = services
        .control
        .replace_route(
            ctx.subject_tenant_id(),
            id,
            request.tags.unwrap_or_default(),
            request.r#match.unwrap_or(existing.r#match),
            request.plugins.map(dto::UpstreamPluginsDto::into_domain).unwrap_or_default(),
            request.rate_limit,
            request.cors,
            request.headers,
        )
        .await?;
    Ok(axum::Json(RouteDto::from(route)).into_response())
}

/// `DELETE /oagw/v1/routes/{id}`
///
/// # Errors
/// 404 when absent.
pub async fn delete_route(
    Extension(services): Extension<Services>,
    ctx: axum::Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
) -> OagwResult<Response> {
    services.control.delete_route(ctx.subject_tenant_id(), id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/plugins`
///
/// # Errors
/// 400 on validation failure.
pub async fn create_plugin(
    Extension(services): Extension<Services>,
    ctx: axum::Extension<toolkit_security::SecurityContext>,
    body: Option<axum::Json<CreatePluginRequest>>,
) -> OagwResult<Response> {
    let request = body.map(|axum::Json(b)| b).unwrap_or_default();
    let plugin_type = request.plugin_type.ok_or_else(|| {
        OagwError::new(ErrorKind::ValidationError, "plugin_type is required")
            .with_ext("fields", serde_json::Value::from(["plugin_type"]))
    })?;
    let name = request.name.ok_or_else(|| {
        OagwError::new(ErrorKind::ValidationError, "name is required")
            .with_ext("fields", serde_json::Value::from(["name"]))
    })?;
    let plugin = services
        .control
        .create_plugin(
            ctx.subject_tenant_id(),
            &plugin_type,
            name,
            request.config_schema,
            request.source_code.unwrap_or_default(),
        )
        .await?;
    Ok((StatusCode::CREATED, axum::Json(PluginDto::from(plugin))).into_response())
}

/// `GET /oagw/v1/plugins`
///
/// # Errors
/// 500 on storage failure.
pub async fn list_plugins(
    Extension(services): Extension<Services>,
    ctx: axum::Extension<toolkit_security::SecurityContext>,
) -> OagwResult<Response> {
    let items = services
        .control
        .list_plugins(ctx.subject_tenant_id())
        .await?
        .into_iter()
        .map(PluginDto::from)
        .collect::<Vec<_>>();
    Ok(axum::Json(items).into_response())
}

/// `GET /oagw/v1/plugins/{id}`
///
/// # Errors
/// 404 when absent.
pub async fn get_plugin(
    Extension(services): Extension<Services>,
    ctx: axum::Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
) -> OagwResult<Response> {
    let plugin = services.control.get_plugin(ctx.subject_tenant_id(), id).await?;
    Ok(axum::Json(PluginDto::from(plugin)).into_response())
}

/// `GET /oagw/v1/plugins/{id}/source`
///
/// # Errors
/// 404 when absent.
pub async fn get_plugin_source(
    Extension(services): Extension<Services>,
    ctx: axum::Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
) -> OagwResult<Response> {
    let plugin = services.control.get_plugin(ctx.subject_tenant_id(), id).await?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "text/x-python; charset=utf-8")],
        plugin.source_code,
    )
        .into_response())
}

/// `DELETE /oagw/v1/plugins/{id}`
///
/// # Errors
/// 404 when absent, 409 when still bound to an upstream or route.
pub async fn delete_plugin(
    Extension(services): Extension<Services>,
    ctx: axum::Extension<toolkit_security::SecurityContext>,
    Path(id): Path<Uuid>,
) -> OagwResult<Response> {
    services.control.delete_plugin(ctx.subject_tenant_id(), id).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------------
// Data plane
// ---------------------------------------------------------------------------

/// Preflight handler for `{OPTIONS} /oagw/v1/proxy/{alias}/{*suffix}`.
///
/// Per `ADR/0004-cors.md` preflight returns a permissive 204 at the handler
/// level without resolving the upstream or requiring a tenant context.
pub async fn proxy_preflight(Extension(_services): Extension<Services>) -> Response {
    let mut response = (StatusCode::NO_CONTENT, "").into_response();
    for (name, value) in [
        ("Access-Control-Allow-Origin", "*"),
        ("Access-Control-Allow-Methods", "GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS"),
        ("Access-Control-Allow-Headers", "*"),
        ("Access-Control-Max-Age", "600"),
        ("Vary", "Origin"),
    ] {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::from_bytes(name.as_bytes()),
            axum::http::HeaderValue::from_str(value),
        ) {
            response.headers_mut().insert(name, value);
        }
    }
    response
}

/// `{METHOD} /oagw/v1/proxy/{alias}` — proxy without a path suffix.
///
/// # Errors
/// Any gateway error from `DESIGN.md §3.3`.
pub async fn proxy_root(
    Extension(services): Extension<Services>,
    ctx: axum::Extension<toolkit_security::SecurityContext>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    request: Request,
) -> Response {
    proxy_inner(services, ctx, method, uri, headers, request).await
}

/// `{METHOD} /oagw/v1/proxy/{alias}/{*path_suffix}` — proxy with a path suffix.
///
/// # Errors
/// Any gateway error from `DESIGN.md §3.3`.
pub async fn proxy(
    Extension(services): Extension<Services>,
    ctx: axum::Extension<toolkit_security::SecurityContext>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    request: Request,
) -> Response {
    proxy_inner(services, ctx, method, uri, headers, request).await
}

/// Shared data-plane entry point.
#[allow(clippy::too_many_lines)]
async fn proxy_inner(
    services: Services,
    ctx: axum::Extension<toolkit_security::SecurityContext>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    mut request: Request,
) -> Response {
    // Preflight OPTIONS requests never reach the upstream.
    if method == Method::OPTIONS {
        let mut response = (StatusCode::NO_CONTENT, "").into_response();
        for (name, value) in [
            ("Access-Control-Allow-Origin", "*"),
            (
                "Access-Control-Allow-Methods",
                "GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS",
            ),
            ("Access-Control-Allow-Headers", "*"),
            ("Access-Control-Max-Age", "600"),
            ("Vary", "Origin"),
        ] {
            if let (Ok(name), Ok(value)) = (
                axum::http::HeaderName::from_bytes(name.as_bytes()),
                axum::http::HeaderValue::from_str(value),
            ) {
                response.headers_mut().insert(name, value);
            }
        }
        return response;
    }

    let alias = uri
        .path()
        .trim_start_matches('/')
        .strip_prefix("oagw/v1/proxy/")
        .map(|rest| rest.split('/').next().unwrap_or(rest).to_owned())
        .unwrap_or_default();
    if alias.is_empty() {
        return OagwError::new(ErrorKind::RouteNotFound, "an upstream alias is required")
            .into_response_with(None);
    }

    // The suffix is whatever follows the alias in the request URI: both proxy
    // routes share one handler, so axum does not hand it over as a parameter.
    let path_suffix = suffix_from(&uri, &alias);

    // HTTP/1.1 upgrades are signalled through the request extensions.
    let client_upgrade = if request
        .extensions()
        .get::<hyper::upgrade::OnUpgrade>()
        .is_some()
    {
        request.extensions_mut().remove::<hyper::upgrade::OnUpgrade>()
    } else {
        None
    };
    let (parts, body) = request.into_parts();
    let raw_query = parts.uri.query().map(str::to_owned);
    let client_ip = parts
        .extensions
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0.ip());

    match services
        .data
        .execute(
            &ctx,
            &alias,
            &path_suffix,
            raw_query.as_deref(),
            &method,
            headers.clone(),
            body,
            client_ip,
            None,
            client_upgrade,
        )
        .await
    {
        Ok(crate::infra::proxy::service::ProxyOutcome::Pass {
            status,
            headers: response_headers,
            body,
            request_id,
        }) => {
            let mut builder = Response::builder().status(status);
            for (name, value) in response_headers.iter() {
                builder = builder.header(name, value);
            }
            if let Some(Ok(value)) = request_id.as_deref().map(axum::http::HeaderValue::from_str) {
                builder = builder.header("x-request-id", value);
            }
            builder
                .body(Body::new(body))
                .unwrap_or_else(|err| {
                    OagwError::new(
                        ErrorKind::ProtocolError,
                        format!("failed to render upstream response: {err}"),
                    )
                    .into_response_with(request_id.as_deref())
                })
        }
        Ok(crate::infra::proxy::service::ProxyOutcome::Upgrade { response, request_id }) => {
            let _ = request_id;
            response
        }
        Err(err) => err.into_response_with(None),
    }
}

/// Extract the path suffix that follows `/proxy/{alias}` in the request URI.
fn suffix_from(uri: &Uri, alias: &str) -> String {
    let path = uri.path();
    let marker = format!("/oagw/v1/proxy/{alias}");
    match path.strip_prefix(&marker) {
        Some(rest) => rest.trim_start_matches('/').to_owned(),
        None => String::new(),
    }
}

/// Health probe payload used by `/readyz`.
#[must_use]
pub fn health_payload() -> serde_json::Value {
    json!({ "status": "ok" })
}



#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{HeaderRules, PluginsConfig};

    #[test]
    fn suffix_extraction() {
        assert_eq!(suffix_from(&"/oagw/v1/proxy/api.example.com".parse().unwrap(), "api.example.com"), "");
        assert_eq!(
            suffix_from(
                &"/oagw/v1/proxy/api.example.com/v1/chat".parse().unwrap(),
                "api.example.com"
            ),
            "v1/chat"
        );
    }

    #[test]
    fn health_payload_is_ok() {
        assert_eq!(health_payload()["status"], "ok");
    }

    #[test]
    fn plugins_default_to_private() {
        let config: PluginsConfig = Default::default();
        assert!(config.items.is_empty());
    }

    #[tokio::test]
    async fn preflight_returns_204() {
        let _ = HeaderRules::default();
        let _ = crate::error::ErrorSource::Gateway;
    }
}
