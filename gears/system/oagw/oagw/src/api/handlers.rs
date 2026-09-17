//! REST handlers (`api/` transport layer).
//!
//! Control-plane handlers expose the tenant-scoped store as JSON; every
//! gateway failure is rendered as an RFC 9457 problem response (`ADR 0007`).
//! Data-plane handlers resolve the transport request into a [`ProxyCall`] and
//! delegate to the [`DataPlaneService`].

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::Extension;
use axum::Json;
use axum::extract::{Path, Request};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use serde::Serialize;
use serde_json::Value;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::query::{self, ListQuery};
use crate::domain::alias::normalize;
use crate::domain::error::{ErrorKind, OagwError};
use crate::domain::model::{CustomPlugin, Route, Upstream};
use crate::domain::service::{ControlPlaneService, DataPlaneService, ProxyCall, RelayedTarget};
use crate::domain::validation;

/// Error half of every handler: an already-rendered problem response.
type ApiError = Response;

/// Header the correlation identifier travels in (`ADR 0001` "Audit Log JSON
/// Format" `request_id`).
const REQUEST_ID_HEADER: &str = crate::infra::plugin::transform::REQUEST_ID_HEADER;

/// The correlation identifier of one request: the one the plugin chain minted
/// or echoed when its error hook ran, else a freshly minted UUID.
fn correlation_id(error: Option<&OagwError>) -> String {
    if let Some(error) = error
        && let Some((_, value)) = error
            .headers()
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(REQUEST_ID_HEADER))
    {
        return value.clone();
    }
    Uuid::new_v4().to_string()
}

/// The header NAMES a log line may carry, per [`crate::infra::proxy::loggable`].
///
/// Only the names are returned, never a value: a log must not become a second
/// copy of an authorization or credential header.
fn loggable_header_names(headers: &HeaderMap) -> Vec<String> {
    headers
        .iter()
        .filter_map(|(name, _)| {
            let name = name.as_str();
            crate::infra::proxy::loggable(name).then(|| name.to_owned())
        })
        .collect()
}

/// The hop one relayed-request log line reports: the request as it was seen at
/// the gateway edge, plus the configuration that answered it, when one did.
///
/// Grouped because the five members travel together from the relay into
/// whichever log line reports the outcome, and keeping them in one struct keeps
/// the log helpers to a short argument list.
#[derive(Clone, Copy)]
struct RelayedHop<'a> {
    alias: &'a str,
    method: &'a Method,
    path: &'a str,
    /// Header NAMES only, never values (`infra/proxy::loggable`).
    header_names: &'a [String],
    target: Option<RelayedTarget>,
}

/// One structured log line per relayed request (`PRD.md` observability: log all
/// proxy requests with correlation IDs).
///
/// Success is `info`, a gateway-produced failure is `warn`. The fields are the
/// ones an operator needs to follow one hop of a call: alias, method, relayed
/// path, matched configuration, status, duration, and the error class.
fn log_relayed(
    hop: &RelayedHop<'_>,
    status: u16,
    duration_ms: u128,
    trace_id: &str,
    error_code: Option<&str>,
) {
    // A relay is bounded by the configured deadline, so the elapsed time is far
    // below `u64::MAX`; saturating keeps a pathological clock from being
    // silently truncated to a small number of milliseconds.
    let duration_ms = u64::try_from(duration_ms).unwrap_or(u64::MAX);
    let RelayedHop {
        alias,
        method,
        path,
        header_names,
        target,
    } = *hop;
    if let Some(error_code) = error_code {
        tracing::warn!(
            alias = %alias,
            method = %method,
            path = %path,
            upstream_id = ?target.map(|target| target.upstream_id),
            route_id = ?target.and_then(|target| target.route_id),
            status = status,
            duration_ms = duration_ms,
            trace_id = %trace_id,
            loggable_header_names = ?header_names,
            error_code = %error_code,
            "oagw proxy request failed"
        );
    } else {
        tracing::info!(
            alias = %alias,
            method = %method,
            path = %path,
            upstream_id = ?target.map(|target| target.upstream_id),
            route_id = ?target.and_then(|target| target.route_id),
            status = status,
            duration_ms = duration_ms,
            trace_id = %trace_id,
            loggable_header_names = ?header_names,
            "oagw proxy request"
        );
    }
}

/// Render a gateway error as its wire response, carrying the correlation id.
fn render(error: &OagwError, instance: Option<&str>, trace_id: &str) -> ApiError {
    error.to_problem(instance, Some(trace_id))
}

/// Render an [`OagwError`] with the request path as the problem `instance`.
fn fail(error: &OagwError, uri: &Uri) -> ApiError {
    render(error, Some(uri.path()), &correlation_id(Some(error)))
}

/// A `route.not_found` problem for a missing control-plane resource.
fn not_found(resource: &str, id: Uuid) -> OagwError {
    OagwError::new(
        ErrorKind::RouteNotFound,
        format!("{resource} {id} does not exist"),
    )
}

/// A 400 validation problem naming the offending member.
fn invalid(field: &str, detail: impl Into<String>) -> OagwError {
    OagwError::new(ErrorKind::Validation, detail).with_context("field", serde_json::json!(field))
}

/// The UUID a `{id}` path parameter names.
///
/// The management API documents resource ids as GTS identifiers
/// (`gts.cf.core.oagw.upstream.v1~{uuid}`); a bare UUID is accepted too, so a
/// caller that only kept the instance part still resolves.
///
/// # Errors
///
/// Returns a validation error when neither form parses.
fn path_id(raw: &str) -> Result<Uuid, OagwError> {
    let instance = raw.split('~').next_back().unwrap_or(raw).trim();
    Uuid::parse_str(instance)
        .map_err(|_| invalid("id", format!("'{raw}' is not a resource identifier")))
}

/// The structured audit record of one management operation (`ADR 0001`
/// Appendix A): resource kind, instance id, owning tenant, and what happened.
fn audit(resource: &str, action: &str, id: Uuid, tenant: Uuid) {
    tracing::info!(
        resource = %resource,
        action = %action,
        id = %id,
        tenant_id = %tenant,
        "oagw management operation"
    );
}

/// The tenant a control-plane request acts on.
fn control_tenant(ctx: &SecurityContext) -> Uuid {
    ctx.subject_tenant_id()
}

/// The stored resources of one collection as JSON rows.
///
/// The resources are locally defined structs of strings, numbers, booleans, and
/// arrays of those, so `serde_json` cannot reject them; a failure would mean the
/// wire contract and the model had drifted apart, which has to be loud.
#[allow(clippy::expect_used)]
fn rows<T: Serialize>(items: impl IntoIterator<Item = T>) -> Vec<Value> {
    items
        .into_iter()
        .map(|item| {
            serde_json::to_value(item)
                .expect("an OAGW resource serializes to its documented JSON shape")
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Upstream control plane
// ---------------------------------------------------------------------------

/// List the calling tenant's upstreams.
///
/// The collection honours the OData-style query options `DESIGN.md` documents
/// for the list endpoints; a resource the caller cannot see is never in it.
///
/// # Errors
///
/// Returns a rendered problem response when the list query options do not
/// parse.
pub async fn list_upstreams(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
) -> Result<Json<Vec<Value>>, Response> {
    let fields = query::upstream_fields();
    let parsed = ListQuery::parse(&uri, fields).map_err(|error| fail(&error, &uri))?;
    let items = rows(
        svc.store()
            .upstreams_of(control_tenant(&ctx))
            .iter()
            .map(|upstream| upstream.as_ref().clone()),
    );
    Ok(Json(parsed.apply(items, fields)))
}

/// Register an upstream.
///
/// # Errors
///
/// Returns a rendered problem response when the upstream fails validation or
/// its alias cannot be derived from the endpoints.
pub async fn create_upstream(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Json(mut upstream): Json<Upstream>,
) -> Result<(StatusCode, Json<Upstream>), Response> {
    let tenant = control_tenant(&ctx);
    upstream.id = Uuid::new_v4();
    upstream.tenant_id = tenant;
    let custom_exists = |plugin_id: uuid::Uuid| svc.store().get_plugin(tenant, plugin_id).is_some();
    validation::validate_upstream(
        &upstream,
        svc.auth_plugins(),
        svc.guards(),
        svc.transforms(),
        &custom_exists,
    )
    .map_err(|error| fail(&error, &uri))?;
    upstream.alias = validation::resolve_alias(&upstream.alias, &upstream.server.endpoints)
        .map_err(|error| fail(&error, &uri))?;
    let stored = svc
        .store()
        .insert_upstream(upstream)
        .map_err(|error| fail(&error, &uri))?;
    audit("upstream", "created", stored.id, tenant);
    Ok((StatusCode::CREATED, Json(stored.as_ref().clone())))
}

/// Fetch one upstream.
///
/// # Errors
///
/// Returns a rendered problem response when the path identifier does not parse
/// or the upstream does not exist in the calling tenant.
pub async fn get_upstream(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(raw): Path<String>,
) -> Result<Json<Upstream>, Response> {
    let id = path_id(&raw).map_err(|error| fail(&error, &uri))?;
    match svc.store().get_upstream(control_tenant(&ctx), id) {
        Some(upstream) => Ok(Json(upstream.as_ref().clone())),
        None => Err(fail(&not_found("upstream", id), &uri)),
    }
}

/// Replace an upstream.
///
/// # Errors
///
/// Returns a rendered problem response when the path identifier does not parse,
/// the upstream does not exist, the replacement fails validation, or it would
/// change the immutable alias.
pub async fn replace_upstream(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(raw): Path<String>,
    Json(mut upstream): Json<Upstream>,
) -> Result<Json<Upstream>, Response> {
    let tenant = control_tenant(&ctx);
    let id = path_id(&raw).map_err(|error| fail(&error, &uri))?;
    let Some(stored_before) = svc.store().get_upstream(tenant, id) else {
        return Err(fail(&not_found("upstream", id), &uri));
    };
    upstream.id = id;
    upstream.tenant_id = tenant;
    let custom_exists = |plugin_id: uuid::Uuid| svc.store().get_plugin(tenant, plugin_id).is_some();
    validation::validate_upstream(
        &upstream,
        svc.auth_plugins(),
        svc.guards(),
        svc.transforms(),
        &custom_exists,
    )
    .map_err(|error| fail(&error, &uri))?;
    upstream.alias = validation::resolve_alias(&upstream.alias, &upstream.server.endpoints)
        .map_err(|error| fail(&error, &uri))?;
    // `PUT` replaces the definition, not its identity (`DESIGN.md` "Alias
    // Update Behavior"): the alias is immutable, because the data plane is
    // addressed by it. A caller that omitted the alias gets the derived one,
    // which must then be the alias it already had.
    if upstream.alias != stored_before.alias {
        return Err(fail(
            &invalid(
                "alias",
                format!(
                    "alias is immutable and cannot be changed from '{}' to '{}'",
                    stored_before.alias, upstream.alias
                ),
            ),
            &uri,
        ));
    }
    let stored = svc
        .store()
        .replace_upstream(upstream)
        .map_err(|error| fail(&error, &uri))?;
    audit("upstream", "replaced", stored.id, tenant);
    Ok(Json(stored.as_ref().clone()))
}

/// Delete an upstream and every route belonging to it.
///
/// # Errors
///
/// Returns a rendered problem response when the path identifier does not parse
/// or the upstream does not exist in the calling tenant.
pub async fn delete_upstream(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(raw): Path<String>,
) -> Result<StatusCode, Response> {
    let id = path_id(&raw).map_err(|error| fail(&error, &uri))?;
    let tenant = control_tenant(&ctx);
    match svc.store().delete_upstream(tenant, id) {
        Some(_) => {
            audit("upstream", "deleted", id, tenant);
            Ok(StatusCode::NO_CONTENT)
        }
        None => Err(fail(&not_found("upstream", id), &uri)),
    }
}

// ---------------------------------------------------------------------------
// Route control plane
// ---------------------------------------------------------------------------

/// List the calling tenant's routes.
///
/// # Errors
///
/// Returns a rendered problem response when the list query options do not
/// parse.
pub async fn list_routes(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
) -> Result<Json<Vec<Value>>, Response> {
    let fields = query::route_fields();
    let parsed = ListQuery::parse(&uri, fields).map_err(|error| fail(&error, &uri))?;
    let items = rows(
        svc.store()
            .routes_of(control_tenant(&ctx))
            .iter()
            .map(|route| route.as_ref().clone()),
    );
    Ok(Json(parsed.apply(items, fields)))
}

/// Create a route.
///
/// # Errors
///
/// Returns a rendered problem response when the route fails validation or names
/// an upstream that does not exist in the calling tenant.
pub async fn create_route(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Json(mut route): Json<Route>,
) -> Result<(StatusCode, Json<Route>), Response> {
    let tenant = control_tenant(&ctx);
    route.id = Uuid::new_v4();
    route.tenant_id = tenant;
    let custom_exists = |plugin_id: uuid::Uuid| svc.store().get_plugin(tenant, plugin_id).is_some();
    validation::validate_route(&route, svc.guards(), svc.transforms(), &custom_exists)
        .map_err(|error| fail(&error, &uri))?;
    if svc
        .store()
        .get_upstream(tenant, route.upstream_id)
        .is_none()
    {
        return Err(fail(
            &OagwError::new(
                ErrorKind::Validation,
                format!("upstream {} does not exist", route.upstream_id),
            ),
            &uri,
        ));
    }
    let stored = svc
        .store()
        .insert_route(route)
        .map_err(|error| fail(&error, &uri))?;
    audit("route", "created", stored.id, tenant);
    Ok((StatusCode::CREATED, Json(stored.as_ref().clone())))
}

/// Fetch one route.
///
/// # Errors
///
/// Returns a rendered problem response when the path identifier does not parse
/// or the route does not exist in the calling tenant.
pub async fn get_route(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(raw): Path<String>,
) -> Result<Json<Route>, Response> {
    let id = path_id(&raw).map_err(|error| fail(&error, &uri))?;
    match svc.store().get_route(control_tenant(&ctx), id) {
        Some(route) => Ok(Json(route.as_ref().clone())),
        None => Err(fail(&not_found("route", id), &uri)),
    }
}

/// Replace a route.
///
/// # Errors
///
/// Returns a rendered problem response when the path identifier does not parse,
/// the route does not exist, or the replacement fails validation.
pub async fn replace_route(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(raw): Path<String>,
    Json(mut route): Json<Route>,
) -> Result<Json<Route>, Response> {
    let tenant = control_tenant(&ctx);
    let id = path_id(&raw).map_err(|error| fail(&error, &uri))?;
    if svc.store().get_route(tenant, id).is_none() {
        return Err(fail(&not_found("route", id), &uri));
    }
    route.id = id;
    route.tenant_id = tenant;
    let custom_exists = |plugin_id: uuid::Uuid| svc.store().get_plugin(tenant, plugin_id).is_some();
    validation::validate_route(&route, svc.guards(), svc.transforms(), &custom_exists)
        .map_err(|error| fail(&error, &uri))?;
    let stored = svc
        .store()
        .replace_route(route)
        .map_err(|error| fail(&error, &uri))?;
    audit("route", "replaced", stored.id, tenant);
    Ok(Json(stored.as_ref().clone()))
}

/// Delete a route.
///
/// # Errors
///
/// Returns a rendered problem response when the path identifier does not parse
/// or the route does not exist in the calling tenant.
pub async fn delete_route(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(raw): Path<String>,
) -> Result<StatusCode, Response> {
    let id = path_id(&raw).map_err(|error| fail(&error, &uri))?;
    let tenant = control_tenant(&ctx);
    match svc.store().delete_route(tenant, id) {
        Some(_) => {
            audit("route", "deleted", id, tenant);
            Ok(StatusCode::NO_CONTENT)
        }
        None => Err(fail(&not_found("route", id), &uri)),
    }
}

// ---------------------------------------------------------------------------
// Plugin catalogue
// ---------------------------------------------------------------------------

/// List every catalogued plugin: built-ins plus tenant-defined plugins.
///
/// # Errors
///
/// Returns a rendered problem response when the list query options do not
/// parse.
pub async fn list_plugins(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
) -> Result<Json<Vec<Value>>, Response> {
    let fields = query::plugin_fields();
    let parsed = ListQuery::parse(&uri, fields).map_err(|error| fail(&error, &uri))?;
    let catalogue = svc.plugin_catalogue(control_tenant(&ctx));
    let items = rows(catalogue.iter().cloned());
    Ok(Json(parsed.apply(items, fields)))
}

/// Fetch one catalogued plugin by identifier (custom plugins only).
///
/// # Errors
///
/// Returns a rendered problem response when the path identifier does not parse
/// or no custom plugin of the calling tenant carries it.
pub async fn get_plugin(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(raw): Path<String>,
) -> Result<Json<CustomPlugin>, Response> {
    let id = path_id(&raw).map_err(|error| fail(&error, &uri))?;
    match svc.get_plugin(control_tenant(&ctx), id) {
        Some(plugin) => Ok(Json(plugin.as_ref().clone())),
        None => Err(fail(&not_found("plugin", id), &uri)),
    }
}

/// Register a custom plugin definition.
///
/// # Errors
///
/// Returns a rendered problem response when the plugin definition fails
/// validation or cannot be stored.
pub async fn create_plugin(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Json(mut plugin): Json<CustomPlugin>,
) -> Result<(StatusCode, Json<CustomPlugin>), Response> {
    let tenant = control_tenant(&ctx);
    plugin.id = Uuid::new_v4();
    plugin.tenant_id = tenant;
    plugin.gc_eligible = false;
    validation::validate_plugin(&plugin).map_err(|error| fail(&error, &uri))?;
    let stored = svc
        .insert_plugin(plugin)
        .map_err(|error| fail(&error, &uri))?;
    audit("plugin", "created", stored.id, tenant);
    Ok((StatusCode::CREATED, Json(stored.as_ref().clone())))
}

/// Fetch the stored source of a custom plugin.
///
/// # Errors
///
/// Returns a rendered problem response when the path identifier does not parse
/// or no custom plugin of the calling tenant carries it.
pub async fn get_plugin_source(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(raw): Path<String>,
) -> Result<Json<serde_json::Value>, Response> {
    let id = path_id(&raw).map_err(|error| fail(&error, &uri))?;
    match svc.get_plugin(control_tenant(&ctx), id) {
        Some(plugin) => Ok(Json(serde_json::json!({
            "id": crate::domain::model::gts_id(crate::domain::model::PLUGIN_GTS_BASE, plugin.id),
            "plugin_type": plugin.plugin_type,
            "source_code": plugin.source_code,
        }))),
        None => Err(fail(&not_found("plugin", id), &uri)),
    }
}

/// Delete a custom plugin, refusing while a chain still references it.
///
/// # Errors
///
/// Returns a rendered problem response when the path identifier does not parse
/// or a plugin chain still references the plugin.
pub async fn delete_plugin(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ControlPlaneService>>,
    Path(raw): Path<String>,
) -> Result<StatusCode, Response> {
    let id = path_id(&raw).map_err(|error| fail(&error, &uri))?;
    let tenant = control_tenant(&ctx);
    match svc.store().delete_plugin(tenant, id) {
        Ok(_) => {
            audit("plugin", "deleted", id, tenant);
            Ok(StatusCode::NO_CONTENT)
        }
        Err(error) => Err(fail(&error, &uri)),
    }
}

// ---------------------------------------------------------------------------
// Data plane
// ---------------------------------------------------------------------------

/// `ANY /oagw/v1/proxy/{alias}`
pub async fn proxy_alias(
    Extension(svc): Extension<Arc<DataPlaneService>>,
    caller: Option<Extension<SecurityContext>>,
    Path(alias): Path<String>,
    request: Request,
) -> Response {
    proxy(svc, alias, String::new(), caller, request).await
}

/// `ANY /oagw/v1/proxy/{alias}/{*path}`
pub async fn proxy_path(
    Extension(svc): Extension<Arc<DataPlaneService>>,
    caller: Option<Extension<SecurityContext>>,
    Path((alias, path)): Path<(String, String)>,
    request: Request,
) -> Response {
    // An axum 0.8 `{*path}` capture excludes the leading slash, while the route
    // patterns the data plane matches against are slash-prefixed, so the
    // capture is restored before the call is resolved.
    proxy(svc, alias, format!("/{path}"), caller, request).await
}

/// Shared data-plane entry point.
async fn proxy(
    svc: Arc<DataPlaneService>,
    alias: String,
    path: String,
    caller: Option<Extension<SecurityContext>>,
    request: Request,
) -> Response {
    let started = Instant::now();
    let uri: Uri = request.uri().clone();
    let method: Method = request.method().clone();
    let headers: HeaderMap = request.headers().clone();
    let client_ip: Option<IpAddr> = client_ip_of(&request);
    // Header NAMES only (`infra/proxy::loggable`): a log line may name a header
    // but must never quote its value.
    let loggable_names = loggable_header_names(&headers);
    let hop = RelayedHop {
        alias: &alias,
        method: &method,
        path: &path,
        header_names: &loggable_names,
        target: None,
    };

    // CORS preflight is answered by the gateway itself (`ADR 0004`): it is
    // never authenticated and never relayed.
    if crate::domain::service::is_preflight(&method, &headers) {
        return crate::domain::service::preflight_response(&headers);
    }

    let security = Arc::new(request_security(&svc, &alias, caller));
    let (parts, raw_body) = request.into_parts();
    let (upgrade_request, raw_body) = if is_upgrade(&headers) {
        // The upgrade token lives in the request extensions; the body of an
        // upgrade request is always empty, so it is dropped here.
        (
            Some(http::Request::from_parts(parts, axum::body::Body::empty())),
            axum::body::Body::empty(),
        )
    } else {
        (None, raw_body)
    };

    let body = match axum::body::to_bytes(raw_body, svc.config().max_request_body_bytes).await {
        Ok(bytes) => bytes,
        Err(error) => {
            let error = OagwError::new(
                ErrorKind::PayloadTooLarge,
                format!("request body could not be read: {error}"),
            );
            return logged_problem(&error, &uri, &hop, started);
        }
    };

    // Body validation (`DESIGN.md` "Body Validation Rules"): a declared length
    // must be a well-formed integer matching the body that arrived, and only
    // the chunked transfer coding is understood. Upgrade requests carry no body
    // and skip the check.
    if upgrade_request.is_none()
        && let Some(error) = validate_body_framing(&headers, body.len())
    {
        return logged_problem(&error, &uri, &hop, started);
    }

    let target_host = header_value(&headers, "x-oagw-target-host");
    let origin = header_value(&headers, "origin");
    let call = ProxyCall {
        alias: alias.clone(),
        path: path.clone(),
        query: uri.query().map(str::to_owned),
        method: method.clone(),
        headers,
        body,
        target_host,
        origin,
        security,
        client_ip,
        upgrade_request,
    };

    match svc.proxy(call).await {
        Ok(response) => {
            log_relayed(
                &RelayedHop {
                    target: response.extensions().get::<RelayedTarget>().copied(),
                    ..hop
                },
                response.status().as_u16(),
                started.elapsed().as_millis(),
                &correlation_id(None),
                None,
            );
            response
        }
        Err(error) => {
            let trace_id = correlation_id(Some(&error));
            let response = render(&error, Some(uri.path()), &trace_id);
            log_relayed(
                &hop,
                error.status().as_u16(),
                started.elapsed().as_millis(),
                &trace_id,
                Some(error.kind().error_code()),
            );
            response
        }
    }
}

/// Render a gateway problem, log it, and hand the response back.
///
/// The relay never ran for these, so no matched configuration is reported.
fn logged_problem(
    error: &OagwError,
    uri: &Uri,
    hop: &RelayedHop<'_>,
    started: Instant,
) -> Response {
    let trace_id = correlation_id(Some(error));
    let response = render(error, Some(uri.path()), &trace_id);
    log_relayed(
        hop,
        error.status().as_u16(),
        started.elapsed().as_millis(),
        &trace_id,
        Some(error.kind().error_code()),
    );
    response
}

/// Body framing validation (`DESIGN.md` "Body Validation Rules").
///
/// A declared `Content-Length` must parse as an integer and must be the length
/// of the body that was buffered, and a `Transfer-Encoding` other than
/// `chunked` is refused: the gateway does not implement `compress`, `deflate`
/// or `gzip` request coding.
///
/// # Errors
///
/// Returns a validation problem naming the offending header.
fn validate_body_framing(headers: &HeaderMap, body_len: usize) -> Option<OagwError> {
    if let Some(value) = headers.get(http::header::TRANSFER_ENCODING) {
        let declared = value.to_str().unwrap_or_default();
        let coding = declared
            .split(',')
            .map(str::trim)
            .rfind(|token| !token.is_empty())
            .unwrap_or_default();
        if !coding.eq_ignore_ascii_case("chunked") {
            return Some(invalid(
                "transfer-encoding",
                format!("unsupported transfer encoding '{declared}'; only chunked is supported"),
            ));
        }
    }
    let declared = headers.get(http::header::CONTENT_LENGTH)?;
    let declared = declared.to_str().unwrap_or_default().trim();
    let Ok(length) = declared.parse::<usize>() else {
        return Some(invalid(
            "content-length",
            format!("content-length '{declared}' is not a valid integer"),
        ));
    };
    if length != body_len {
        return Some(invalid(
            "content-length",
            format!("content-length {length} does not match the request body size {body_len}"),
        ));
    }
    None
}

/// The security context a proxy request acts under.
///
/// The alias identifies the configuration and, with it, the tenant, so a
/// caller that reaches the data plane without a platform identity is still
/// resolved against a concrete tenant scope.
fn request_security(
    svc: &DataPlaneService,
    alias: &str,
    caller: Option<Extension<SecurityContext>>,
) -> SecurityContext {
    // A validated bearer token wins: it carries the caller's identity and
    // tenant, which is what credential permission checks are evaluated against.
    if let Some(Extension(caller)) = caller
        && !caller.subject_tenant_id().is_nil()
    {
        return caller;
    }
    let tenant = svc
        .store()
        .upstream_by_alias_any(&normalize(alias))
        .map_or_else(Uuid::nil, |upstream| upstream.tenant_id);
    SecurityContext::builder()
        .subject_id(Uuid::nil())
        .subject_type("oagw-proxy")
        .subject_tenant_id(tenant)
        .build()
        .unwrap_or_else(|_| SecurityContext::anonymous())
}

/// Best-effort client address from the connection extension.
fn client_ip_of(request: &Request) -> Option<IpAddr> {
    request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|addr| addr.0.ip())
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Whether the request asks for the one upgrade this gateway can relay.
///
/// A bare `Upgrade` header is not enough: RFC 7230 §6.7 requires the token to
/// be listed in `Connection`, and the relay engine only speaks WebSocket, so
/// `h2c` and other protocols are relayed as ordinary requests instead of being
/// answered `101` with their body discarded.
fn is_upgrade(headers: &HeaderMap) -> bool {
    let upgrade_token = headers
        .get(http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    let connection_token = headers
        .get(http::header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        });
    upgrade_token && connection_token && headers.contains_key(http::header::SEC_WEBSOCKET_KEY)
}
