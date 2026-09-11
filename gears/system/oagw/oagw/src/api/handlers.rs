// Created: 2026-09-02 by Constructor Tech
//! REST handlers for the management API and the proxy data plane.
//!
//! Handlers are thin: they resolve the caller from the [`SecurityContext`]
//! injected by the api-gateway's auth middleware, hand the request to the
//! control plane or the [`ProxyService`], and let [`GatewayError`]'s own
//! `IntoResponse` render the RFC 9457 problem.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, Query, Request};
use axum::http::Method;
use axum::response::{IntoResponse, Response};
use toolkit_security::SecurityContext;

use crate::domain::model::{Plugin, Route, Upstream};
use crate::domain::query::ListQuery;
use crate::domain::service::{Caller, ControlPlane};
use crate::error::GatewayError;
use crate::infra::proxy::{ProxyRequest, ProxyService};

/// State shared by the management handlers.
#[derive(Clone)]
pub struct ManagementState {
    /// Control plane performing the CRUD.
    pub control_plane: Arc<ControlPlane>,
    /// Tenant resolver used for hierarchical lookups.
    pub resolver: Option<Arc<dyn tenant_resolver_sdk::TenantResolverClient>>,
}

/// State shared by the proxy handler.
#[derive(Clone)]
pub struct ProxyState {
    /// The data-plane service.
    pub proxy: Arc<ProxyService>,
}

/// The caller's chain, self first, ancestors from the tenant resolver.
async fn caller(security: &SecurityContext, resolver: Option<&Arc<dyn tenant_resolver_sdk::TenantResolverClient>>) -> Caller {
    let mut ancestors = Vec::new();
    if let Some(resolver) = resolver {
        let chain = crate::infra::client::ancestor_chain(Some(resolver), security.subject_tenant_id()).await;
        // `ancestor_chain` returns self first; the caller keeps ancestors only.
        ancestors = chain.into_iter().skip(1).collect();
    }
    Caller {
        tenant_id: security.subject_tenant_id(),
        ancestors,
        subject_tenant_id: security.subject_tenant_id(),
        subject_id: security.subject_id().to_string(),
    }
}

/// Parses the OData list query from the request parameters.
#[must_use]
pub fn list_query(pairs: &[(String, String)]) -> ListQuery {
    ListQuery::parse(pairs)
}

// ------------------------------------------------------------------ upstreams

/// `POST /oagw/v1/upstreams` — create an upstream.
///
/// # Errors
///
/// Propagates the control plane's validation and conflict errors.
pub async fn create_upstream(
    Extension(state): Extension<ManagementState>,
    security: axum::Extension<SecurityContext>,
    Json(spec): Json<Upstream>,
) -> Result<Response, GatewayError> {
    let caller = caller(&security, state.resolver.as_ref()).await;
    let created = state.control_plane.create_upstream(&caller, spec)?;
    Ok((axum::http::StatusCode::CREATED, Json(created)).into_response())
}

/// `GET /oagw/v1/upstreams` — list the caller's upstreams.
///
/// # Errors
///
/// [`GatewayError::Validation`] when the OData query is malformed.
pub async fn list_upstreams(
    Extension(state): Extension<ManagementState>,
    security: axum::Extension<SecurityContext>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Result<Json<Vec<serde_json::Value>>, GatewayError> {
    let caller = caller(&security, state.resolver.as_ref()).await;
    let items = state.control_plane.list_upstreams(&caller, &list_query(&pairs))?;
    Ok(Json(items))
}

/// `GET /oagw/v1/upstreams/{id}` — fetch one upstream.
///
/// # Errors
///
/// [`GatewayError::NotFound`] when the upstream is absent or invisible.
pub async fn get_upstream(
    Extension(state): Extension<ManagementState>,
    security: axum::Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<Json<Upstream>, GatewayError> {
    let caller = caller(&security, state.resolver.as_ref()).await;
    Ok(Json(state.control_plane.get_upstream(&caller, &id)?))
}

/// `PUT /oagw/v1/upstreams/{id}` — replace an upstream.
///
/// # Errors
///
/// [`GatewayError::Validation`] for an invalid spec or alias change,
/// [`GatewayError::NotFound`] when the upstream is invisible.
pub async fn replace_upstream(
    Extension(state): Extension<ManagementState>,
    security: axum::Extension<SecurityContext>,
    Path(id): Path<String>,
    Json(spec): Json<Upstream>,
) -> Result<Json<Upstream>, GatewayError> {
    let caller = caller(&security, state.resolver.as_ref()).await;
    let replaced = state.control_plane.replace_upstream(&caller, &id, spec)?;
    Ok(Json(replaced))
}

/// `DELETE /oagw/v1/upstreams/{id}` — delete an upstream.
///
/// # Errors
///
/// [`GatewayError::NotFound`] when the upstream is invisible.
pub async fn delete_upstream(
    Extension(state): Extension<ManagementState>,
    security: axum::Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<Response, GatewayError> {
    let caller = caller(&security, state.resolver.as_ref()).await;
    state.control_plane.delete_upstream(&caller, &id)?;
    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}

// --------------------------------------------------------------------- routes

/// `POST /oagw/v1/routes` — create a route.
///
/// # Errors
///
/// [`GatewayError::Validation`] for an invalid spec, [`GatewayError::Conflict`]
/// when the match rule collides.
pub async fn create_route(
    Extension(state): Extension<ManagementState>,
    security: axum::Extension<SecurityContext>,
    Json(spec): Json<Route>,
) -> Result<Response, GatewayError> {
    let caller = caller(&security, state.resolver.as_ref()).await;
    let created = state.control_plane.create_route(&caller, spec)?;
    Ok((axum::http::StatusCode::CREATED, Json(created)).into_response())
}

/// `GET /oagw/v1/routes` — list the caller's routes.
///
/// # Errors
///
/// [`GatewayError::Validation`] when the OData query is malformed.
pub async fn list_routes(
    Extension(state): Extension<ManagementState>,
    security: axum::Extension<SecurityContext>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Result<Json<Vec<serde_json::Value>>, GatewayError> {
    let caller = caller(&security, state.resolver.as_ref()).await;
    let items = state.control_plane.list_routes(&caller, &list_query(&pairs))?;
    Ok(Json(items))
}

/// `GET /oagw/v1/routes/{id}` — fetch one route.
///
/// # Errors
///
/// [`GatewayError::NotFound`] when the route is invisible.
pub async fn get_route(
    Extension(state): Extension<ManagementState>,
    security: axum::Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<Json<Route>, GatewayError> {
    let caller = caller(&security, state.resolver.as_ref()).await;
    Ok(Json(state.control_plane.get_route(&caller, &id)?))
}

/// `PUT /oagw/v1/routes/{id}` — replace a route.
///
/// # Errors
///
/// [`GatewayError::Validation`] for an invalid spec or an `upstream_id` change.
pub async fn replace_route(
    Extension(state): Extension<ManagementState>,
    security: axum::Extension<SecurityContext>,
    Path(id): Path<String>,
    Json(spec): Json<Route>,
) -> Result<Json<Route>, GatewayError> {
    let caller = caller(&security, state.resolver.as_ref()).await;
    let replaced = state.control_plane.replace_route(&caller, &id, spec)?;
    Ok(Json(replaced))
}

/// `DELETE /oagw/v1/routes/{id}` — delete a route.
///
/// # Errors
///
/// [`GatewayError::NotFound`] when the route is invisible.
pub async fn delete_route(
    Extension(state): Extension<ManagementState>,
    security: axum::Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<Response, GatewayError> {
    let caller = caller(&security, state.resolver.as_ref()).await;
    state.control_plane.delete_route(&caller, &id)?;
    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}

// -------------------------------------------------------------------- plugins

/// `POST /oagw/v1/plugins` — create a custom plugin definition.
///
/// # Errors
///
/// [`GatewayError::Validation`] for a bad name or source,
/// [`GatewayError::Conflict`] when the name is taken.
pub async fn create_plugin(
    Extension(state): Extension<ManagementState>,
    security: axum::Extension<SecurityContext>,
    Json(spec): Json<Plugin>,
) -> Result<Response, GatewayError> {
    let caller = caller(&security, state.resolver.as_ref()).await;
    let created = state.control_plane.create_plugin(&caller, spec)?;
    Ok((axum::http::StatusCode::CREATED, Json(created)).into_response())
}

/// `GET /oagw/v1/plugins` — list the caller's plugin definitions.
///
/// # Errors
///
/// [`GatewayError::Validation`] when the OData query is malformed.
pub async fn list_plugins(
    Extension(state): Extension<ManagementState>,
    security: axum::Extension<SecurityContext>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Result<Json<Vec<serde_json::Value>>, GatewayError> {
    let caller = caller(&security, state.resolver.as_ref()).await;
    let items = state.control_plane.list_plugins(&caller, &list_query(&pairs))?;
    Ok(Json(items))
}

/// `GET /oagw/v1/plugins/{id}` — fetch one plugin definition.
///
/// # Errors
///
/// [`GatewayError::NotFound`] when the plugin is invisible.
pub async fn get_plugin(
    Extension(state): Extension<ManagementState>,
    security: axum::Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<Json<Plugin>, GatewayError> {
    let caller = caller(&security, state.resolver.as_ref()).await;
    Ok(Json(state.control_plane.get_plugin(&caller, &id)?))
}

/// `GET /oagw/v1/plugins/{id}/source` — the plugin's Starlark source.
///
/// # Errors
///
/// [`GatewayError::NotFound`] when the plugin is invisible.
pub async fn get_plugin_source(
    Extension(state): Extension<ManagementState>,
    security: axum::Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<Response, GatewayError> {
    let caller = caller(&security, state.resolver.as_ref()).await;
    let source = state.control_plane.plugin_source(&caller, &id)?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/x-starlark; charset=utf-8")],
        source,
    )
        .into_response())
}

/// `DELETE /oagw/v1/plugins/{id}` — delete an unreferenced plugin.
///
/// # Errors
///
/// [`GatewayError::PluginInUse`] while an upstream or route still binds it.
pub async fn delete_plugin(
    Extension(state): Extension<ManagementState>,
    security: axum::Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Result<Response, GatewayError> {
    let caller = caller(&security, state.resolver.as_ref()).await;
    state.control_plane.delete_plugin(&caller, &id)?;
    Ok(axum::http::StatusCode::NO_CONTENT.into_response())
}

// ---------------------------------------------------------------------- proxy

/// The data-plane handler for the suffix-less proxy shape.
///
/// `DESIGN.md` §3.3 spells the proxy path `{alias}[/{path_suffix}]` — the
/// suffix is optional, and axum's `{*rest}` wildcard is not, so the two shapes
/// are registered separately and meet in [`proxy_with_suffix`].
pub async fn proxy(
    Extension(state): Extension<ProxyState>,
    security: axum::Extension<SecurityContext>,
    Path(alias): Path<String>,
    request: Request,
) -> Response {
    proxy_with_suffix(
        Extension(state),
        security,
        Path((alias, String::new())),
        request,
    )
    .await
}

/// The data-plane handler for every proxied method.
///
/// Registered once per `{alias}` / `{alias}/{*path_suffix}` shape with the
/// other verbs sharing a single method router, so the path matching stays
/// exactly the catch-all the specification describes.
///
/// # Errors
///
/// The [`GatewayError`] the proxy produces is rendered as a problem.
pub async fn proxy_with_suffix(
    Extension(state): Extension<ProxyState>,
    security: axum::Extension<SecurityContext>,
    Path((alias, path_suffix)): Path<(String, String)>,
    request: Request,
) -> Response {
    let (mut parts, body) = request.into_parts();
    // The upgrade future must be taken before the body is consumed: hyper
    // hands it out exactly once, and only for a `Connection: Upgrade` request.
    let upgrade = crate::infra::proxy::upgrade_of(&mut parts);
    let query = parts.uri.query().map(str::to_owned);
    let client_ip = client_ip(&parts.headers);

    let request = ProxyRequest {
        alias,
        path_suffix,
        query,
        method: parts.method.clone(),
        headers: parts.headers,
        body,
        tenant_id: security.subject_tenant_id(),
        subject_id: security.subject_id().to_string(),
        client_ip,
        upgrade,
    };

    match state.proxy.execute(request).await {
        Ok(response) => response,
        Err(error) => error.render_problem(None),
    }
}

/// The best-effort client address, used by the `ip` rate-limit scope.
///
/// The api-gateway forwards the peer address in the standard headers; the
/// direct socket address is not visible to a handler, so the first
/// `Forwarded`-style value wins and `unknown` is the fallback.
#[must_use]
fn client_ip(headers: &axum::http::HeaderMap) -> String {
    for name in ["x-forwarded-for", "x-real-ip"] {
        if let Some(value) = headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
        {
            let candidate = value.trim();
            if !candidate.is_empty() {
                return candidate.to_owned();
            }
        }
    }
    "unknown".to_owned()
}

/// The management method allowlist, for the proxy's OPTIONS preflight.
#[must_use]
pub fn is_preflight(method: &Method, headers: &axum::http::HeaderMap) -> bool {
    crate::infra::cors::is_preflight(method, headers)
}
