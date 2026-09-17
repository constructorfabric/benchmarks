//! REST handlers for the OAGW management plane.
//!
//! Every handler resolves the calling tenant from the injected
//! [`SecurityContext`] — never from a path or query parameter — so ancestor
//! resources are invisible by construction (`DESIGN.md` §3.3 "Tenant
//! Scoping"). Ids in path segments accept both the bare UUID a response body
//! carries and the anonymous GTS identifier the contract names.
use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Path, RawQuery},
    http::{StatusCode, Uri},
    response::{IntoResponse, Response},
};
use toolkit::api::apply_select;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{
    CreatePluginRequest, EndpointPoolDto, PluginChainDto, PluginDto, PluginSourceDto,
    ReferencedByDto, RouteDto, RouteRequest, UpstreamDto, UpstreamRequest,
};
use crate::api::rest::error::{ApiError, ApiResult};
use crate::api::rest::params::ParsedList;
use crate::domain::error::DomainError;
use crate::domain::model::{self, Endpoint, Plugin};
use crate::domain::services::{ControlPlane, ListLimits};
use crate::infra::memory::MemoryStore;

/// Control-plane handle shared by every handler through an axum `Extension`.
pub type SharedControlPlane = Arc<ControlPlane>;

/// Build the shared control-plane service for the gear.
#[must_use]
pub fn control_plane(config: &crate::config::OagwConfig) -> SharedControlPlane {
    Arc::new(ControlPlane::new(
        Arc::new(MemoryStore::new()),
        ListLimits {
            default_top: config.list.default_top,
            max_top: config.list.max_top,
        },
    ))
}

/// The calling tenant; never taken from a path or query parameter.
fn tenant(context: &SecurityContext) -> Uuid {
    context.subject_tenant_id()
}

/// Parse a path id, accepting a bare UUID or an anonymous GTS id.
fn resource_id(raw: &str) -> Result<Uuid, ApiError> {
    crate::domain::gts::resource_uuid(raw).ok_or_else(|| {
        ApiError::validation(format!(
            "'{raw}' is not a valid resource id (expected a UUID or an anonymous GTS id)"
        ))
    })
}

/// Parse a positional sub-resource selector.
fn position(raw: &str) -> Result<usize, ApiError> {
    raw.parse::<usize>()
        .map_err(|_| ApiError::validation(format!("'{raw}' is not a valid position")))
}

/// Reject a `PUT` that echoes an immutable field with a different value.
fn immutable(field: &'static str, provided: Option<Uuid>, actual: Uuid) -> Result<(), ApiError> {
    if provided.is_some_and(|provided| provided != actual) {
        return Err(ApiError::from(DomainError::ImmutableField { field }));
    }
    Ok(())
}

/// The `Location` header value of a newly created sub-resource.
fn location(base: &str, id: &Uuid) -> String {
    format!("{}/{}", base.trim_end_matches('/'), id)
}

/// Render a list page, projecting each item through `$select`.
///
/// The projection applies to the collection's elements — the page envelope
/// (`items`, `total`, `top`, `skip`) is always present in full, matching the
/// OData semantics `DESIGN.md` §3.3 documents.
fn projected_page<T: serde::Serialize>(
    items: Vec<T>,
    total: usize,
    top: usize,
    skip: usize,
    select: Option<&[String]>,
) -> serde_json::Value {
    serde_json::json!({
        "items": items
            .into_iter()
            .map(|item| apply_select(item, select))
            .collect::<Vec<_>>(),
        "total": total,
        "top": top,
        "skip": skip,
    })
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/upstreams`.
///
/// # Errors
///
/// 400 for a malformed payload, 409 for an alias conflict, 500 internally.
pub(crate) async fn create_upstream(
    uri: Uri,
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Json(body): Json<UpstreamRequest>,
) -> ApiResult<impl IntoResponse> {
    let upstream = svc.create_upstream(tenant(&context), body.into_spec()?)?;
    Ok((
        StatusCode::CREATED,
        [(
            http::header::LOCATION.as_str(),
            location(uri.path(), &upstream.id),
        )],
        Json(UpstreamDto::from_entity(&upstream)?),
    ))
}

/// `GET /oagw/v1/upstreams`.
///
/// # Errors
///
/// 400 for an invalid list query.
pub(crate) async fn list_upstreams(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    RawQuery(query): RawQuery,
) -> ApiResult<impl IntoResponse> {
    let tenant = tenant(&context);
    let parsed = ParsedList::parse(query.as_deref(), svc.limits())?;
    let items = svc.list_upstreams(tenant, &parsed.query)?;
    let total = svc.count_upstreams(tenant, &parsed.query)?;
    let mut page = Vec::with_capacity(items.len());
    for upstream in &items {
        page.push(UpstreamDto::from_entity(upstream)?);
    }
    Ok((
        StatusCode::OK,
        Json(projected_page(
            page,
            total,
            parsed.query.top.unwrap_or_default(),
            parsed.query.skip.unwrap_or_default(),
            parsed.select.as_deref(),
        )),
    ))
}

/// `GET /oagw/v1/upstreams/{id}`.
///
/// # Errors
///
/// 404 when the tenant owns no such upstream.
pub(crate) async fn get_upstream(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
) -> ApiResult<Json<UpstreamDto>> {
    let upstream = svc.get_upstream(tenant(&context), resource_id(&id)?)?;
    Ok(Json(UpstreamDto::from_entity(&upstream)?))
}

/// `PUT /oagw/v1/upstreams/{id}` — full replacement.
///
/// # Errors
///
/// 400 for a malformed payload or an immutable-field violation, 404 when the
/// upstream is absent, 409 when the alias rules are violated.
pub(crate) async fn replace_upstream(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
    Json(body): Json<UpstreamRequest>,
) -> ApiResult<Json<UpstreamDto>> {
    let tenant = tenant(&context);
    let id = resource_id(&id)?;
    immutable("id", body.id, id)?;
    immutable("tenant_id", body.tenant_id, tenant)?;
    let upstream = svc.replace_upstream(tenant, id, body.into_spec()?)?;
    Ok(Json(UpstreamDto::from_entity(&upstream)?))
}

/// `DELETE /oagw/v1/upstreams/{id}` — cascades the upstream's routes.
///
/// # Errors
///
/// 404 when the upstream is absent.
pub(crate) async fn delete_upstream(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    svc.delete_upstream(tenant(&context), resource_id(&id)?)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /oagw/v1/upstreams/{id}/enable`.
///
/// # Errors
///
/// 404 when the upstream is absent.
pub(crate) async fn enable_upstream(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
) -> ApiResult<Json<UpstreamDto>> {
    let upstream = svc.set_upstream_enabled(tenant(&context), resource_id(&id)?, true)?;
    Ok(Json(UpstreamDto::from_entity(&upstream)?))
}

/// `POST /oagw/v1/upstreams/{id}/disable`.
///
/// # Errors
///
/// 404 when the upstream is absent.
pub(crate) async fn disable_upstream(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
) -> ApiResult<Json<UpstreamDto>> {
    let upstream = svc.set_upstream_enabled(tenant(&context), resource_id(&id)?, false)?;
    Ok(Json(UpstreamDto::from_entity(&upstream)?))
}

// ---------------------------------------------------------------------------
// Endpoint pools
// ---------------------------------------------------------------------------

/// `GET /oagw/v1/upstreams/{id}/endpoints`.
///
/// # Errors
///
/// 404 when the upstream is absent.
pub(crate) async fn get_endpoints(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
) -> ApiResult<Json<EndpointPoolDto>> {
    let pool = svc.upstream_endpoints(tenant(&context), resource_id(&id)?)?;
    Ok(Json(EndpointPoolDto::from(&pool)))
}

/// `POST /oagw/v1/upstreams/{id}/endpoints` — append to the pool.
///
/// Appending an endpoint that is already pooled is a no-op that still returns
/// the current upstream.
///
/// # Errors
///
/// 400 for a malformed pool, 404 when the upstream is absent, 409 when the
/// enlarged pool would move the routing key.
pub(crate) async fn add_endpoints(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
    Json(body): Json<EndpointPoolDto>,
) -> ApiResult<Json<UpstreamDto>> {
    let tenant = tenant(&context);
    let id = resource_id(&id)?;
    let pool = model::ServerConfig::try_from(&body)?;
    let mut upstream = svc.get_upstream(tenant, id)?;
    for endpoint in pool.endpoints {
        if !upstream.server.endpoints.contains(&endpoint) {
            upstream = svc.add_upstream_endpoint(tenant, id, endpoint)?;
        }
    }
    Ok(Json(UpstreamDto::from_entity(&upstream)?))
}

/// `PUT /oagw/v1/upstreams/{id}/endpoints` — replace the whole pool.
///
/// # Errors
///
/// 400 for a malformed or empty pool, 404 when the upstream is absent, 409
/// when the new pool would move the derived alias.
pub(crate) async fn replace_endpoints(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
    Json(body): Json<EndpointPoolDto>,
) -> ApiResult<Json<UpstreamDto>> {
    let pool = model::ServerConfig::try_from(&body)?;
    let upstream =
        svc.replace_upstream_endpoints(tenant(&context), resource_id(&id)?, pool.endpoints)?;
    Ok(Json(UpstreamDto::from_entity(&upstream)?))
}

/// `DELETE /oagw/v1/upstreams/{id}/endpoints/{position}`.
///
/// # Errors
///
/// 400 for a non-numeric position, 404 when the position is out of range or
/// the upstream is absent, 409 when the remaining pool would move the routing
/// key.
pub(crate) async fn delete_endpoint(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path((id, index)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let index = position(&index)?;
    svc.delete_upstream_endpoint(tenant(&context), resource_id(&id)?, index)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Plugin chains
// ---------------------------------------------------------------------------

/// `GET /oagw/v1/upstreams/{id}/plugins`.
///
/// # Errors
///
/// 404 when the upstream is absent.
pub(crate) async fn get_upstream_plugins(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
) -> ApiResult<Json<PluginChainDto>> {
    let chain = svc.upstream_plugins(tenant(&context), resource_id(&id)?)?;
    Ok(Json(PluginChainDto::from(&chain)))
}

/// `POST /oagw/v1/upstreams/{id}/plugins` — append bindings to the chain.
///
/// # Errors
///
/// 400 for an unknown plugin reference, 404 when the upstream is absent.
pub(crate) async fn add_upstream_plugins(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
    Json(body): Json<PluginChainDto>,
) -> ApiResult<Json<UpstreamDto>> {
    let tenant = tenant(&context);
    let id = resource_id(&id)?;
    let mut chain = svc.upstream_plugins(tenant, id)?;
    chain.sharing = body.sharing.into();
    chain
        .items
        .extend(body.items.iter().map(model::PluginBinding::from));
    let upstream = svc.set_upstream_plugins(tenant, id, chain)?;
    Ok(Json(UpstreamDto::from_entity(&upstream)?))
}

/// `PUT /oagw/v1/upstreams/{id}/plugins` — replace the chain.
///
/// # Errors
///
/// 400 for an unknown plugin reference, 404 when the upstream is absent.
pub(crate) async fn put_upstream_plugins(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
    Json(body): Json<PluginChainDto>,
) -> ApiResult<Json<UpstreamDto>> {
    let upstream =
        svc.set_upstream_plugins(tenant(&context), resource_id(&id)?, chain_from(&body))?;
    Ok(Json(UpstreamDto::from_entity(&upstream)?))
}

/// `DELETE /oagw/v1/upstreams/{id}/plugins/{position}`.
///
/// # Errors
///
/// 400 for a non-numeric position, 404 when the binding position is absent.
pub(crate) async fn delete_upstream_plugin(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path((id, index)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let index = position(&index)?;
    svc.delete_upstream_plugin(tenant(&context), resource_id(&id)?, index)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/routes`.
///
/// # Errors
///
/// 400 for a malformed payload, 404 when the target upstream is absent, 409
/// for a duplicate match key.
pub(crate) async fn create_route(
    uri: Uri,
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Json(body): Json<RouteRequest>,
) -> ApiResult<impl IntoResponse> {
    let route = svc.create_route(tenant(&context), body.into_spec()?)?;
    Ok((
        StatusCode::CREATED,
        [(
            http::header::LOCATION.as_str(),
            location(uri.path(), &route.id),
        )],
        Json(RouteDto::from_entity(&route)?),
    ))
}

/// `GET /oagw/v1/routes`.
///
/// # Errors
///
/// 400 for an invalid list query.
pub(crate) async fn list_routes(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    RawQuery(query): RawQuery,
) -> ApiResult<impl IntoResponse> {
    let tenant = tenant(&context);
    let parsed = ParsedList::parse(query.as_deref(), svc.limits())?;
    let items = svc.list_routes(tenant, &parsed.query)?;
    let total = svc.count_routes(tenant, &parsed.query)?;
    let mut page = Vec::with_capacity(items.len());
    for route in &items {
        page.push(RouteDto::from_entity(route)?);
    }
    Ok((
        StatusCode::OK,
        Json(projected_page(
            page,
            total,
            parsed.query.top.unwrap_or_default(),
            parsed.query.skip.unwrap_or_default(),
            parsed.select.as_deref(),
        )),
    ))
}

/// `GET /oagw/v1/routes/{id}`.
///
/// # Errors
///
/// 404 when the tenant owns no such route.
pub(crate) async fn get_route(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
) -> ApiResult<Json<RouteDto>> {
    let route = svc.get_route(tenant(&context), resource_id(&id)?)?;
    Ok(Json(RouteDto::from_entity(&route)?))
}

/// `PUT /oagw/v1/routes/{id}`.
///
/// # Errors
///
/// 400 for a malformed payload or an immutable-field violation, 404 when the
/// route is absent, 409 for a duplicate match key.
pub(crate) async fn replace_route(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
    Json(body): Json<RouteRequest>,
) -> ApiResult<Json<RouteDto>> {
    let tenant = tenant(&context);
    let id = resource_id(&id)?;
    immutable("id", body.id, id)?;
    immutable("tenant_id", body.tenant_id, tenant)?;
    let route = svc.replace_route(tenant, id, body.into_spec()?)?;
    Ok(Json(RouteDto::from_entity(&route)?))
}

/// `DELETE /oagw/v1/routes/{id}`.
///
/// # Errors
///
/// 404 when the route is absent.
pub(crate) async fn delete_route(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    svc.delete_route(tenant(&context), resource_id(&id)?)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /oagw/v1/routes/{id}/enable`.
///
/// # Errors
///
/// 404 when the route is absent.
pub(crate) async fn enable_route(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
) -> ApiResult<Json<RouteDto>> {
    let route = svc.set_route_enabled(tenant(&context), resource_id(&id)?, true)?;
    Ok(Json(RouteDto::from_entity(&route)?))
}

/// `POST /oagw/v1/routes/{id}/disable`.
///
/// # Errors
///
/// 404 when the route is absent.
pub(crate) async fn disable_route(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
) -> ApiResult<Json<RouteDto>> {
    let route = svc.set_route_enabled(tenant(&context), resource_id(&id)?, false)?;
    Ok(Json(RouteDto::from_entity(&route)?))
}

/// `GET /oagw/v1/routes/{id}/plugins`.
///
/// # Errors
///
/// 404 when the route is absent.
pub(crate) async fn get_route_plugins(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
) -> ApiResult<Json<PluginChainDto>> {
    let chain = svc.route_plugins(tenant(&context), resource_id(&id)?)?;
    Ok(Json(PluginChainDto::from(&chain)))
}

/// `POST /oagw/v1/routes/{id}/plugins` — append bindings to the chain.
///
/// # Errors
///
/// 400 for an unknown plugin reference, 404 when the route is absent.
pub(crate) async fn add_route_plugins(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
    Json(body): Json<PluginChainDto>,
) -> ApiResult<Json<RouteDto>> {
    let tenant = tenant(&context);
    let id = resource_id(&id)?;
    let mut chain = svc.route_plugins(tenant, id)?;
    chain.sharing = body.sharing.into();
    chain
        .items
        .extend(body.items.iter().map(model::PluginBinding::from));
    let route = svc.set_route_plugins(tenant, id, chain)?;
    Ok(Json(RouteDto::from_entity(&route)?))
}

/// `PUT /oagw/v1/routes/{id}/plugins` — replace the chain.
///
/// # Errors
///
/// 400 for an unknown plugin reference, 404 when the route is absent.
pub(crate) async fn put_route_plugins(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
    Json(body): Json<PluginChainDto>,
) -> ApiResult<Json<RouteDto>> {
    let route = svc.set_route_plugins(tenant(&context), resource_id(&id)?, chain_from(&body))?;
    Ok(Json(RouteDto::from_entity(&route)?))
}

/// `DELETE /oagw/v1/routes/{id}/plugins/{position}`.
///
/// # Errors
///
/// 400 for a non-numeric position, 404 when the binding position is absent.
pub(crate) async fn delete_route_plugin(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path((id, index)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    let index = position(&index)?;
    svc.delete_route_plugin(tenant(&context), resource_id(&id)?, index)?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/plugins`.
///
/// # Errors
///
/// 400 for a malformed payload, 409 when the name is taken.
pub(crate) async fn create_plugin(
    uri: Uri,
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Json(body): Json<CreatePluginRequest>,
) -> ApiResult<impl IntoResponse> {
    let plugin = svc.create_plugin(tenant(&context), body.into_spec())?;
    Ok((
        StatusCode::CREATED,
        [(
            http::header::LOCATION.as_str(),
            location(uri.path(), &plugin.id),
        )],
        Json(PluginDto::from(&plugin)),
    ))
}

/// `GET /oagw/v1/plugins`.
///
/// # Errors
///
/// 400 for an invalid list query.
pub(crate) async fn list_plugins(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    RawQuery(query): RawQuery,
) -> ApiResult<impl IntoResponse> {
    let tenant = tenant(&context);
    let parsed = ParsedList::parse(query.as_deref(), svc.limits())?;
    let items = svc.list_plugins(tenant, &parsed.query)?;
    let total = svc.count_plugins(tenant, &parsed.query)?;
    Ok((
        StatusCode::OK,
        Json(projected_page(
            items.iter().map(PluginDto::from).collect(),
            total,
            parsed.query.top.unwrap_or_default(),
            parsed.query.skip.unwrap_or_default(),
            parsed.select.as_deref(),
        )),
    ))
}

/// `GET /oagw/v1/plugins/{id}`.
///
/// # Errors
///
/// 404 when the tenant owns no such plugin.
pub(crate) async fn get_plugin(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
) -> ApiResult<Json<PluginDto>> {
    let plugin = svc.get_plugin(tenant(&context), resource_id(&id)?)?;
    Ok(Json(PluginDto::from(&plugin)))
}

/// `GET /oagw/v1/plugins/{id}/source`.
///
/// # Errors
///
/// 404 when the plugin is absent.
pub(crate) async fn get_plugin_source(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
) -> ApiResult<Json<PluginSourceDto>> {
    let plugin = svc.plugin_source(tenant(&context), resource_id(&id)?)?;
    Ok(Json(PluginSourceDto {
        plugin_id: plugin.gts_id(),
        name: plugin.name.clone(),
        kind: plugin.kind.into(),
        source: plugin.source,
    }))
}

/// `DELETE /oagw/v1/plugins/{id}`.
///
/// Returns 204 when the plugin is unreferenced and 409 carrying the
/// referencing resources otherwise (`docs/ADR/0001-request-routing.md`).
///
/// # Errors
///
/// 404 when the plugin is absent, 409 when it is still referenced.
pub(crate) async fn delete_plugin(
    Extension(context): Extension<SecurityContext>,
    Extension(svc): Extension<SharedControlPlane>,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    match svc.delete_plugin(tenant(&context), resource_id(&id)?) {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(error) => Err(ApiError::from(error)),
    }
}

// ---------------------------------------------------------------------------
// Shared projections
// ---------------------------------------------------------------------------

/// Build a domain plugin chain from a wire chain, leaving every binding
/// unresolved so the service resolves (and rejects unknown) references.
fn chain_from(body: &PluginChainDto) -> model::PluginsConfig {
    model::PluginsConfig {
        sharing: body.sharing.into(),
        items: body.items.iter().map(model::PluginBinding::from).collect(),
    }
}

/// The plugin reference set a 409 plugin deletion reports, on the wire.
///
/// Exposed for tests, which assert on the wire shape rather than the mapper.
#[must_use]
pub fn referenced_by(references: &crate::domain::error::PluginReferences) -> ReferencedByDto {
    ReferencedByDto::from_references(references)
}

/// `true` when `plugin` is bound anywhere on `upstream` (chain or auth slot).
#[must_use]
pub fn references_upstream(upstream: &model::Upstream, plugin: &Plugin) -> bool {
    let auth = upstream
        .auth
        .as_ref()
        .and_then(|auth| auth.plugin_uuid)
        .is_some_and(|uuid| uuid == plugin.id);
    auth || references_chain(&upstream.plugins, plugin)
}

/// `true` when `plugin` is bound on `route`'s chain.
#[must_use]
pub fn references_route(route: &model::Route, plugin: &Plugin) -> bool {
    references_chain(&route.plugins, plugin)
}

fn references_chain(chain: &Option<model::PluginsConfig>, plugin: &Plugin) -> bool {
    chain.as_ref().is_some_and(|config| {
        config
            .items
            .iter()
            .any(|binding| matches_plugin(&binding.plugin_uuid, &binding.plugin_ref, plugin))
    })
}

/// `true` when a binding spells `plugin`'s identity, whether it was stored
/// resolved (ref + UUID) or as a bare reference.
fn matches_plugin(plugin_uuid: &Option<Uuid>, plugin_ref: &str, plugin: &Plugin) -> bool {
    plugin_uuid.is_some_and(|uuid| uuid == plugin.id)
        || plugin_ref == plugin.id.to_string()
        || plugin_ref == plugin.gts_id()
}

/// The endpoint pool of an upstream, as a wire DTO.
#[must_use]
pub fn endpoint_pool(upstream: &model::Upstream) -> EndpointPoolDto {
    EndpointPoolDto::from(&upstream.server)
}

/// `true` when `endpoint` is already pooled on `upstream`.
#[must_use]
pub fn endpoint_pooled(upstream: &model::Upstream, endpoint: &Endpoint) -> bool {
    upstream.server.endpoints.contains(endpoint)
}

/// Render an [`ApiError`] as a response.
///
/// A thin re-export so the route registration module and the tests share one
/// error renderer.
#[must_use]
pub fn render_error(error: ApiError) -> Response {
    error.into_response()
}

#[cfg(test)]
#[path = "handlers_tests.rs"]
mod tests;
