//! Axum handlers for the OAGW REST surfaces.
//!
//! Management CRUD for upstreams, routes, and plugins, plus the data-plane
//! proxy endpoint. All gateway-originated errors are RFC 9457 problem+json
//! bodies with `X-OAGW-Error-Source: gateway`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{OriginalUri, Path, Query};
use axum::response::IntoResponse;
use axum::{Extension, Json};
use http::header::{HeaderName, HeaderValue};
use http::{Request, StatusCode};
use tenant_resolver_sdk::{GetAncestorsOptions, TenantId, TenantResolverClient};
use toolkit::api::canonical_prelude::{created_json, no_content};
use toolkit_security::SecurityContext;
use tracing::{Instrument, info_span, warn};
use uuid::Uuid;

use crate::api::rest::dto::{ListResponse, PluginSourceResponse};
use crate::api::rest::error::{
    ERROR_SOURCE_GATEWAY, HEADER_ERROR_SOURCE, OagwProblem, ReferencedByDto, type_ids,
};
use crate::api::rest::odata::{ListOptions, apply, parse_params};
use crate::domain::error::{ControlPlaneError, ResourceRef};
use crate::domain::models::{PluginRecord, Route, Upstream};
use crate::domain::service::ControlPlaneService;
use crate::infra::plugin::AuthPluginRegistry;
use crate::infra::proxy::proxy_request;
use crate::infra::ratelimit::RateLimiter;

type Response = axum::response::Response;

/// Correlation header (not shipped by `http` ≥ 1.0; defined locally).
const X_REQUEST_ID: HeaderName = HeaderName::from_static("x-request-id");

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Map a control-plane error to an OAGW problem response.
fn problem_from(e: ControlPlaneError) -> OagwProblem {
    let detail = e.to_string();
    match e {
        ControlPlaneError::NotFound(ref_) => {
            let instance = resource_instance(&ref_);
            OagwProblem::new(type_ids::NOT_FOUND, "Not Found", StatusCode::NOT_FOUND)
                .detail(detail)
                .instance(instance)
        }
        ControlPlaneError::Validation { details } => OagwProblem::validation(details),
        ControlPlaneError::Duplicate(kind) => OagwProblem::conflict(kind.to_string()),
        ControlPlaneError::ImmutableAlias => OagwProblem::validation(
            "alias is immutable: delete and re-create the upstream to change it",
        ),
        ControlPlaneError::ImmutableUpstreamId => {
            OagwProblem::validation("upstream_id is immutable after route creation")
        }
        ControlPlaneError::InUse(ref_, referenced) => {
            let plugin_id = match &ref_ {
                ResourceRef::Plugin(id) => id.clone(),
                _ => String::new(),
            };
            let referenced = ReferencedByDto {
                upstreams: referenced.upstreams,
                routes: referenced.routes,
            };
            OagwProblem::new(
                type_ids::PLUGIN_IN_USE,
                "Plugin In Use",
                StatusCode::CONFLICT,
            )
            .detail(detail)
            .plugin_id(plugin_id)
            .referenced_by(referenced)
        }
    }
}

fn resource_instance(ref_: &ResourceRef) -> String {
    let kind = resource_kind(ref_);
    let id = match ref_ {
        ResourceRef::Upstream(id) | ResourceRef::Route(id) | ResourceRef::Plugin(id) => id,
    };
    format!("/oagw/v1/{kind}/{id}")
}

fn resource_kind(ref_: &ResourceRef) -> &'static str {
    match ref_ {
        ResourceRef::Upstream(_) => "upstreams",
        ResourceRef::Route(_) => "routes",
        ResourceRef::Plugin(_) => "plugins",
    }
}

fn tenant_id(ctx: &SecurityContext) -> Uuid {
    ctx.subject_tenant_id()
}

/// Parse OData-lite query params into list options, mapping errors to 400.
fn list_options(params: &Query<HashMap<String, String>>) -> Result<ListOptions, Box<OagwProblem>> {
    parse_params(&params.0).map_err(|e| Box::new(OagwProblem::validation(e.message)))
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/upstreams` — create an upstream (alias derived/validated).
pub async fn create_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ControlPlaneService>>,
    OriginalUri(uri): OriginalUri,
    Json(body): Json<Upstream>,
) -> Response {
    let out = match service.create_upstream(tenant_id(&ctx), body) {
        Ok(u) => u,
        Err(e) => return problem_from(e).into_response(),
    };
    created_json(&out, &uri, &out.id.to_string()).into_response()
}

/// `GET /oagw/v1/upstreams` — list with OData-lite query support.
#[allow(clippy::implicit_hasher)] // axum's `Query` extractor fixes the hasher.
pub async fn list_upstreams(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ControlPlaneService>>,
    params: Query<HashMap<String, String>>,
) -> Response {
    let opts = match list_options(&params) {
        Ok(o) => o,
        Err(p) => return p.into_response(),
    };
    let items = service.list_upstreams(tenant_id(&ctx));
    let page = apply(&items, &opts);
    Json(ListResponse::of(page)).into_response()
}

/// `GET /oagw/v1/upstreams/{id}` — fetch one upstream.
pub async fn get_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<Uuid>,
) -> Response {
    match service.get_upstream(tenant_id(&ctx), id) {
        Ok(u) => Json(u).into_response(),
        Err(e) => problem_from(e).into_response(),
    }
}

/// `PUT /oagw/v1/upstreams/{id}` — update (alias immutable).
pub async fn update_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<Uuid>,
    Json(body): Json<Upstream>,
) -> Response {
    match service.update_upstream(tenant_id(&ctx), id, body) {
        Ok(u) => Json(u).into_response(),
        Err(e) => problem_from(e).into_response(),
    }
}

/// `DELETE /oagw/v1/upstreams/{id}` — delete one upstream (routes cascade;
/// the DP's rate buckets for the upstream are dropped so a re-created
/// upstream starts from a fresh budget).
pub async fn delete_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ControlPlaneService>>,
    Extension(rate): Extension<Option<Arc<RateLimiter>>>,
    Path(id): Path<Uuid>,
) -> Response {
    match service.delete_upstream(tenant_id(&ctx), id) {
        Ok(()) => {
            if let Some(limiter) = rate {
                limiter.clear_for_upstream(id);
            }
            no_content().into_response()
        }
        Err(e) => problem_from(e).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/routes` — create a route.
pub async fn create_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ControlPlaneService>>,
    OriginalUri(uri): OriginalUri,
    Json(body): Json<Route>,
) -> Response {
    let out = match service.create_route(tenant_id(&ctx), body) {
        Ok(r) => r,
        Err(e) => return problem_from(e).into_response(),
    };
    created_json(&out, &uri, &out.id.to_string()).into_response()
}

/// `GET /oagw/v1/routes` — list with OData-lite query support.
#[allow(clippy::implicit_hasher)] // axum's `Query` extractor fixes the hasher.
pub async fn list_routes(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ControlPlaneService>>,
    params: Query<HashMap<String, String>>,
) -> Response {
    let opts = match list_options(&params) {
        Ok(o) => o,
        Err(p) => return p.into_response(),
    };
    let items = service.list_routes(tenant_id(&ctx));
    let page = apply(&items, &opts);
    Json(ListResponse::of(page)).into_response()
}

/// `GET /oagw/v1/routes/{id}` — fetch one route.
pub async fn get_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<Uuid>,
) -> Response {
    match service.get_route(tenant_id(&ctx), id) {
        Ok(r) => Json(r).into_response(),
        Err(e) => problem_from(e).into_response(),
    }
}

/// `PUT /oagw/v1/routes/{id}` — update (`upstream_id` immutable).
pub async fn update_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<Uuid>,
    Json(body): Json<Route>,
) -> Response {
    match service.update_route(tenant_id(&ctx), id, body) {
        Ok(r) => Json(r).into_response(),
        Err(e) => problem_from(e).into_response(),
    }
}

/// `DELETE /oagw/v1/routes/{id}` — delete one route.
pub async fn delete_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<Uuid>,
) -> Response {
    match service.delete_route(tenant_id(&ctx), id) {
        Ok(()) => no_content().into_response(),
        Err(e) => problem_from(e).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/plugins` — create a custom plugin.
pub async fn create_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ControlPlaneService>>,
    OriginalUri(uri): OriginalUri,
    Json(body): Json<PluginRecord>,
) -> Response {
    let out = match service.create_plugin(tenant_id(&ctx), body) {
        Ok(p) => p,
        Err(e) => return problem_from(e).into_response(),
    };
    created_json(&out, &uri, &out.id.to_string()).into_response()
}

/// `GET /oagw/v1/plugins` — list custom plugins.
#[allow(clippy::implicit_hasher)] // axum's `Query` extractor fixes the hasher.
pub async fn list_plugins(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ControlPlaneService>>,
    params: Query<HashMap<String, String>>,
) -> Response {
    let opts = match list_options(&params) {
        Ok(o) => o,
        Err(p) => return p.into_response(),
    };
    let items = service.list_plugins(tenant_id(&ctx));
    let page = apply(&items, &opts);
    Json(ListResponse::of(page)).into_response()
}

/// `GET /oagw/v1/plugins/{id}` — fetch one plugin.
pub async fn get_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<Uuid>,
) -> Response {
    match service.get_plugin(tenant_id(&ctx), id) {
        Ok(p) => Json(p).into_response(),
        Err(e) => problem_from(e).into_response(),
    }
}

/// `GET /oagw/v1/plugins/{id}/source` — fetch the plugin's source text.
pub async fn get_plugin_source(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<Uuid>,
) -> Response {
    match service.get_plugin(tenant_id(&ctx), id) {
        Ok(p) => Json(PluginSourceResponse {
            id: p.id,
            source: p.source,
        })
        .into_response(),
        Err(e) => problem_from(e).into_response(),
    }
}

/// `DELETE /oagw/v1/plugins/{id}` — delete an unlinked plugin (409 when used).
pub async fn delete_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ControlPlaneService>>,
    Path(id): Path<Uuid>,
) -> Response {
    match service.delete_plugin(tenant_id(&ctx), id) {
        Ok(()) => no_content().into_response(),
        Err(e) => problem_from(e).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Data plane (proxy)
// ---------------------------------------------------------------------------

/// `{METHOD} /oagw/v1/proxy/{alias}/{*rest}` — proxy a request to an upstream.
///
/// Delegates to the data-plane engine (`crate::infra::proxy`): alias
/// resolution across the tenant chain, route matching, endpoint selection,
/// body validation, header transforms, and forwarding (HTTP / SSE / WebSocket).
#[allow(clippy::too_many_arguments)] // signature dictated by axum's extractors.
pub async fn proxy(
    Extension(ctx): Extension<SecurityContext>,
    Extension(service): Extension<Arc<ControlPlaneService>>,
    Extension(tenant_resolver): Extension<Option<Arc<dyn TenantResolverClient>>>,
    Extension(auth): Extension<Option<Arc<AuthPluginRegistry>>>,
    Extension(rate): Extension<Option<Arc<RateLimiter>>>,
    Path((alias, rest)): Path<(String, String)>,
    OriginalUri(uri): OriginalUri,
    mut request: Request<Body>,
) -> Response {
    // Correlation ID (caller-supplied `X-Request-ID` or a fresh UUID v4):
    // logged on this span, echoed on the response, and injected as `trace_id`
    // into gateway problem bodies below.
    let request_id = request_id_of(&request);
    let span = info_span!("oagw_proxy", request_id = %request_id);
    // Instrument the async block instead of holding `span.enter()` across the
    // `.await`s: an entered span held across a yield lets other tasks execute
    // while "inside" it, producing incorrect traces (diagnostics only). The
    // `oagw_proxy` span still records with the `request_id` field on every
    // poll, but is exited while the handler awaits.
    async {
        // Tenant chain (descendant → root) used for alias resolution; falls
        // back to the caller's own tenant when the resolver is absent/
        // unreachable.
        let chain = build_chain(&ctx, tenant_resolver.as_deref()).await;

        // Client-side WebSocket upgrade future, present only when a real hyper
        // server connection performed an upgrade (not in `oneshot` harnesses).
        let wants_upgrade = request
            .headers()
            .get(http::header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| !v.is_empty());
        let on_upgrade = if wants_upgrade
            && request
                .extensions()
                .get::<hyper::upgrade::OnUpgrade>()
                .is_some()
        {
            Some(hyper::upgrade::on(&mut request))
        } else {
            None
        };

        // `{*rest}` is the decoded path suffix; append the raw query string so
        // the proxy can route on path+query and pass the query through
        // untouched.
        let rest_with_query = match uri.query() {
            Some(q) if !q.is_empty() => format!("{rest}?{q}"),
            _ => rest,
        };

        let (parts, body) = request.into_parts();
        let response = proxy_request(
            &service,
            chain,
            alias,
            rest_with_query,
            parts.method,
            parts.headers,
            body,
            on_upgrade,
            Some(&ctx),
            auth.as_deref(),
            rate.as_deref(),
        )
        .await;
        attach_request_id(response, &request_id).await
    }
    .instrument(span)
    .await
}

/// The correlation identifier for one request: the caller's `X-Request-ID`
/// when present and usable, else a freshly generated UUID v4.
fn request_id_of(request: &Request<Body>) -> String {
    match request
        .headers()
        .get(&X_REQUEST_ID)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(v) => v.to_owned(),
        None => Uuid::new_v4().to_string(),
    }
}

/// Attach the correlation ID to a proxy response: `X-Request-ID` is set on
/// every response header, and gateway-originated (`X-OAGW-Error-Source:
/// gateway`) RFC 9457 problem bodies additionally get `trace_id` injected and
/// their `Content-Length` recomputed. Upstream passthroughs (including
/// streaming SSE bodies and 101 upgrades) never have their body touched.
async fn attach_request_id(response: Response, request_id: &str) -> Response {
    let is_gateway_problem = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("application/problem+json"))
        && response
            .headers()
            .get(HEADER_ERROR_SOURCE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|src| src == ERROR_SOURCE_GATEWAY);

    let (parts, body) = response.into_parts();
    let status = parts.status;
    let mut headers = parts.headers;

    let body = if is_gateway_problem {
        if let Ok(bytes) = axum::body::to_bytes(body, 1024 * 1024).await {
            let value = serde_json::from_slice::<serde_json::Value>(&bytes)
                .unwrap_or(serde_json::Value::Null);
            let value = with_trace_id(value, request_id);
            let encoded = serde_json::to_vec(&value).unwrap_or(bytes.to_vec());
            headers.insert(http::header::CONTENT_LENGTH, HeaderValue::from(encoded.len()));
            Body::from(encoded)
        } else {
            // Body read failed: keep the correlation header but drop the
            // body (the problem is still flagged by status + headers).
            headers.insert(http::header::CONTENT_LENGTH, HeaderValue::from(0usize));
            Body::empty()
        }
    } else {
        body
    };

    headers.insert(
        X_REQUEST_ID,
        HeaderValue::from_str(request_id).unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    let mut resp = Response::new(body);
    *resp.status_mut() = status;
    *resp.headers_mut() = headers;
    resp
}

/// Inject (or replace) the `trace_id` member of an RFC 9457 problem object.
fn with_trace_id(mut value: serde_json::Value, trace_id: &str) -> serde_json::Value {
    if let Some(map) = value.as_object_mut() {
        map.insert(
            "trace_id".to_owned(),
            serde_json::Value::String(trace_id.to_owned()),
        );
        value
    } else {
        // Not an object (defensive): wrap it so the trace id is still present.
        serde_json::json!({ "trace_id": trace_id, "payload": value })
    }
}

/// Resolve the tenant ancestry (caller + ancestors, nearest first) for
/// hierarchical alias resolution (DESIGN: descendant → root, closest wins).
async fn build_chain(
    ctx: &SecurityContext,
    resolver: Option<&dyn TenantResolverClient>,
) -> Vec<Uuid> {
    let me = ctx.subject_tenant_id();
    let Some(resolver) = resolver else {
        return vec![me];
    };
    match resolver
        .get_ancestors(ctx, TenantId(me), &GetAncestorsOptions::default())
        .await
    {
        Ok(resp) => {
            let mut chain = Vec::with_capacity(1 + resp.ancestors.len());
            chain.push(me);
            chain.extend(resp.ancestors.iter().map(|a| a.id.0));
            chain
        }
        Err(e) => {
            warn!(err = %e, "tenant resolver unavailable; proxy falls back to the caller's tenant only");
            vec![me]
        }
    }
}
