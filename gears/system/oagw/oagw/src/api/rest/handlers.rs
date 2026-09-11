//! Management API handlers (Control Plane).
//!
//! Bodies are deserialized by hand rather than through `Json<T>` so a malformed
//! or schema-violating payload answers `400 ValidationError` in the gateway's
//! own problem format, not axum's default rejection.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, Path};
use axum::response::{IntoResponse, Response};
use http::{StatusCode, Uri, header};
use serde::de::DeserializeOwned;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::{ErrorKind, OagwError, OagwResult};
use crate::domain::gts;
use crate::domain::input::{PluginInput, RouteInput, UpstreamInput};
use crate::domain::model::PluginKind;

use super::dto::{ListResponse, PluginResponse, PluginSourceResponse};
use super::error::{ApiError, mark_gateway, problem_response};
use super::query::ListParams;
use super::state::{OagwState, actions};

type Handler = Result<Response, ApiError>;

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/upstreams`
///
/// # Errors
///
/// `400` on validation failure, `403` when the policy denies, `409` on alias
/// conflict.
pub async fn create_upstream(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    body: Bytes,
) -> Handler {
    let at = uri.path().to_owned();
    let run = async {
        state
            .authorize(&ctx, gts::UPSTREAM_BASE, actions::CREATE)
            .await?;
        let input: UpstreamInput = parse_body(&body)?;
        let upstream = state
            .control
            .create_upstream(ctx.subject_tenant_id(), input)?;
        created(&upstream, &at, &upstream.id)
    };
    finish(run.await, &at)
}

/// `GET /oagw/v1/upstreams`
///
/// # Errors
///
/// `400` on a malformed query parameter, `403` when the policy denies.
pub async fn list_upstreams(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
) -> Handler {
    let at = uri.path().to_owned();
    let run = async {
        state
            .authorize(&ctx, gts::UPSTREAM_BASE, actions::READ)
            .await?;
        let params = ListParams::from_query(uri.query().unwrap_or_default())?;
        let items = state.control.list_upstreams(ctx.subject_tenant_id());
        list_response(&state, &params, &items)
    };
    finish(run.await, &at)
}

/// `GET /oagw/v1/upstreams/{id}`
///
/// # Errors
///
/// `400` on a malformed id, `403` when the policy denies, `404` when the
/// upstream is not the caller's.
pub async fn get_upstream(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(raw_id): Path<String>,
) -> Handler {
    let at = uri.path().to_owned();
    let run = async {
        state
            .authorize(&ctx, gts::UPSTREAM_BASE, actions::READ)
            .await?;
        let id = resource_id(&raw_id, gts::UPSTREAM_BASE, "upstream")?;
        state
            .control
            .get_upstream(ctx.subject_tenant_id(), id)
            .map(|upstream| json_response(StatusCode::OK, &upstream))
            .unwrap_or_else(|| Err(OagwError::not_found("upstream not found")))
    };
    finish(run.await, &at)
}

/// `PUT /oagw/v1/upstreams/{id}`
///
/// # Errors
///
/// `400` on validation failure, `403` when the policy denies, `404` when the
/// upstream is not the caller's, `409` on alias conflict.
pub async fn replace_upstream(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(raw_id): Path<String>,
    body: Bytes,
) -> Handler {
    let at = uri.path().to_owned();
    let run = async {
        state
            .authorize(&ctx, gts::UPSTREAM_BASE, actions::OVERRIDE)
            .await?;
        let id = resource_id(&raw_id, gts::UPSTREAM_BASE, "upstream")?;
        let input: UpstreamInput = parse_body(&body)?;
        let upstream = state
            .control
            .replace_upstream(ctx.subject_tenant_id(), id, input)?;
        json_response(StatusCode::OK, &upstream)
    };
    finish(run.await, &at)
}

/// `DELETE /oagw/v1/upstreams/{id}`
///
/// # Errors
///
/// `400` on a malformed id, `403` when the policy denies, `404` when the
/// upstream is not the caller's.
pub async fn delete_upstream(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(raw_id): Path<String>,
) -> Handler {
    let at = uri.path().to_owned();
    let run = async {
        state
            .authorize(&ctx, gts::UPSTREAM_BASE, actions::DELETE)
            .await?;
        let id = resource_id(&raw_id, gts::UPSTREAM_BASE, "upstream")?;
        state.control.delete_upstream(ctx.subject_tenant_id(), id)?;
        state.sweep_unlinked_plugins();
        Ok(no_content())
    };
    finish(run.await, &at)
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/routes`
///
/// # Errors
///
/// `400` on validation failure, `403` when the policy denies, `409` on a
/// duplicate match rule.
pub async fn create_route(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    body: Bytes,
) -> Handler {
    let at = uri.path().to_owned();
    let run = async {
        state
            .authorize(&ctx, gts::ROUTE_BASE, actions::CREATE)
            .await?;
        let input: RouteInput = parse_body(&body)?;
        let route = state.control.create_route(ctx.subject_tenant_id(), input)?;
        created(&route, &at, &route.id)
    };
    finish(run.await, &at)
}

/// `GET /oagw/v1/routes`
///
/// # Errors
///
/// `400` on a malformed query parameter, `403` when the policy denies.
pub async fn list_routes(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
) -> Handler {
    let at = uri.path().to_owned();
    let run = async {
        state.authorize(&ctx, gts::ROUTE_BASE, actions::READ).await?;
        let params = ListParams::from_query(uri.query().unwrap_or_default())?;
        let items = state.control.list_routes(ctx.subject_tenant_id());
        list_response(&state, &params, &items)
    };
    finish(run.await, &at)
}

/// `GET /oagw/v1/routes/{id}`
///
/// # Errors
///
/// `400` on a malformed id, `403` when the policy denies, `404` when the route
/// is not the caller's.
pub async fn get_route(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(raw_id): Path<String>,
) -> Handler {
    let at = uri.path().to_owned();
    let run = async {
        state.authorize(&ctx, gts::ROUTE_BASE, actions::READ).await?;
        let id = resource_id(&raw_id, gts::ROUTE_BASE, "route")?;
        state
            .control
            .get_route(ctx.subject_tenant_id(), id)
            .map(|route| json_response(StatusCode::OK, &route))
            .unwrap_or_else(|| Err(OagwError::not_found("route not found")))
    };
    finish(run.await, &at)
}

/// `PUT /oagw/v1/routes/{id}`
///
/// # Errors
///
/// `400` on validation failure, `403` when the policy denies, `404` when the
/// route is not the caller's, `409` on a duplicate match rule.
pub async fn replace_route(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(raw_id): Path<String>,
    body: Bytes,
) -> Handler {
    let at = uri.path().to_owned();
    let run = async {
        state
            .authorize(&ctx, gts::ROUTE_BASE, actions::OVERRIDE)
            .await?;
        let id = resource_id(&raw_id, gts::ROUTE_BASE, "route")?;
        let input: RouteInput = parse_body(&body)?;
        let route = state
            .control
            .replace_route(ctx.subject_tenant_id(), id, input)?;
        json_response(StatusCode::OK, &route)
    };
    finish(run.await, &at)
}

/// `DELETE /oagw/v1/routes/{id}`
///
/// # Errors
///
/// `400` on a malformed id, `403` when the policy denies, `404` when the route
/// is not the caller's.
pub async fn delete_route(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(raw_id): Path<String>,
) -> Handler {
    let at = uri.path().to_owned();
    let run = async {
        state
            .authorize(&ctx, gts::ROUTE_BASE, actions::DELETE)
            .await?;
        let id = resource_id(&raw_id, gts::ROUTE_BASE, "route")?;
        state.control.delete_route(ctx.subject_tenant_id(), id)?;
        state.sweep_unlinked_plugins();
        Ok(no_content())
    };
    finish(run.await, &at)
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/plugins`
///
/// # Errors
///
/// `400` on validation failure, `403` when the policy denies, `409` when the
/// name is taken.
pub async fn create_plugin(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    body: Bytes,
) -> Handler {
    let at = uri.path().to_owned();
    let run = async {
        let input: PluginInput = parse_body(&body)?;
        state
            .authorize(
                &ctx,
                OagwState::plugin_resource(input.plugin_type),
                actions::CREATE,
            )
            .await?;
        let plugin = state.control.create_plugin(ctx.subject_tenant_id(), input)?;
        created(&PluginResponse::from(&plugin), &at, &plugin.id)
    };
    finish(run.await, &at)
}

/// `GET /oagw/v1/plugins`
///
/// # Errors
///
/// `400` on a malformed query parameter, `403` when the policy denies.
pub async fn list_plugins(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
) -> Handler {
    let at = uri.path().to_owned();
    let run = async {
        state
            .authorize(&ctx, gts::GUARD_PLUGIN_BASE, actions::READ)
            .await?;
        state.sweep_unlinked_plugins();
        let params = ListParams::from_query(uri.query().unwrap_or_default())?;
        let items: Vec<PluginResponse> = state
            .control
            .list_plugins(ctx.subject_tenant_id())
            .iter()
            .map(PluginResponse::from)
            .collect();
        list_response(&state, &params, &items)
    };
    finish(run.await, &at)
}

/// `GET /oagw/v1/plugins/{id}`
///
/// # Errors
///
/// `400` on a malformed id, `403` when the policy denies, `404` when the
/// plugin is not the caller's.
pub async fn get_plugin(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(raw_id): Path<String>,
) -> Handler {
    let at = uri.path().to_owned();
    let run = async {
        let (id, kind) = plugin_id(&raw_id)?;
        state
            .authorize(
                &ctx,
                OagwState::plugin_resource(kind.unwrap_or(PluginKind::Guard)),
                actions::READ,
            )
            .await?;
        state
            .control
            .get_plugin(ctx.subject_tenant_id(), id)
            .map(|plugin| json_response(StatusCode::OK, &PluginResponse::from(&plugin)))
            .unwrap_or_else(|| Err(OagwError::not_found("plugin not found")))
    };
    finish(run.await, &at)
}

/// `GET /oagw/v1/plugins/{id}/source`
///
/// # Errors
///
/// `400` on a malformed id, `403` when the policy denies, `404` when the
/// plugin is not the caller's.
pub async fn get_plugin_source(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(raw_id): Path<String>,
) -> Handler {
    let at = uri.path().to_owned();
    let run = async {
        let (id, kind) = plugin_id(&raw_id)?;
        state
            .authorize(
                &ctx,
                OagwState::plugin_resource(kind.unwrap_or(PluginKind::Guard)),
                actions::READ,
            )
            .await?;
        state
            .control
            .get_plugin(ctx.subject_tenant_id(), id)
            .map(|plugin| json_response(StatusCode::OK, &PluginSourceResponse::from(&plugin)))
            .unwrap_or_else(|| Err(OagwError::not_found("plugin not found")))
    };
    finish(run.await, &at)
}

/// `DELETE /oagw/v1/plugins/{id}`
///
/// # Errors
///
/// `400` on a malformed id, `403` when the policy denies, `404` when the
/// plugin is not the caller's, `409` when it is still referenced.
pub async fn delete_plugin(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(raw_id): Path<String>,
) -> Handler {
    let at = uri.path().to_owned();
    let run = async {
        let (id, kind) = plugin_id(&raw_id)?;
        state
            .authorize(
                &ctx,
                OagwState::plugin_resource(kind.unwrap_or(PluginKind::Guard)),
                actions::DELETE,
            )
            .await?;
        let references = state.store.references_to_plugin(id);
        state
            .control
            .delete_plugin(ctx.subject_tenant_id(), id, references)?;
        Ok(no_content())
    };
    finish(run.await, &at)
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn finish(result: OagwResult<Response>, instance: &str) -> Handler {
    match result {
        Ok(response) => Ok(response),
        Err(error) => Err(ApiError::new(error, instance)),
    }
}

/// Deserialize a request body, mapping every failure to `400 ValidationError`.
fn parse_body<T: DeserializeOwned>(body: &Bytes) -> OagwResult<T> {
    if body.is_empty() {
        return Err(OagwError::validation("a JSON request body is required"));
    }
    serde_json::from_slice(body)
        .map_err(|err| OagwError::validation(format!("invalid request body: {err}")))
}

/// Resolve a path parameter that may be a bare UUID or an anonymous GTS id.
fn resource_id(raw: &str, base: &'static str, label: &str) -> OagwResult<Uuid> {
    gts::parse_resource_id(raw, Some(base)).ok_or_else(|| {
        OagwError::validation(format!(
            "'{raw}' is not a valid {label} identifier (expected a UUID or '{base}{{uuid}}')"
        ))
    })
}

fn plugin_id(raw: &str) -> OagwResult<(Uuid, Option<PluginKind>)> {
    let (base, id) = gts::parse_plugin_id(raw).ok_or_else(|| {
        OagwError::validation(format!(
            "'{raw}' is not a valid plugin identifier (expected a UUID or \
             'gts.cf.core.oagw.{{type}}_plugin.v1~{{uuid}}')"
        ))
    })?;
    Ok((id, base.and_then(PluginKind::from_base_type)))
}

fn json_response<T: serde::Serialize>(status: StatusCode, value: &T) -> OagwResult<Response> {
    let body = serde_json::to_vec(value)
        .map_err(|err| OagwError::internal(format!("response serialization failed: {err}")))?;
    let mut response = (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response();
    mark_gateway(&mut response);
    Ok(response)
}

fn created<T: serde::Serialize>(value: &T, at: &str, id: &Uuid) -> OagwResult<Response> {
    let mut response = json_response(StatusCode::CREATED, value)?;
    let location = format!("{}/{id}", at.trim_end_matches('/'));
    if let Ok(value) = http::HeaderValue::from_str(&location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    Ok(response)
}

fn no_content() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    mark_gateway(&mut response);
    response
}

fn list_response<T: serde::Serialize>(
    state: &OagwState,
    params: &ListParams,
    items: &[T],
) -> OagwResult<Response> {
    let values: Vec<serde_json::Value> = items
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<_, _>>()
        .map_err(|err| OagwError::internal(format!("response serialization failed: {err}")))?;
    let default_top = state.config.clamp_page_size(None);
    let (page, total) = params.apply(values, default_top);
    json_response(StatusCode::OK, &ListResponse::new(page, total))
}

/// Render a gateway error outside a handler's `Result` chain.
#[must_use]
pub fn render_error(error: &OagwError, instance: &str) -> Response {
    problem_response(error, Some(instance))
}

/// The gear's error catalog is also its `Unsupported media type` surface: a
/// body that is not JSON never reaches a handler, so this is the single place
/// that shape is decided.
#[must_use]
pub fn unsupported_media_type(instance: &str) -> Response {
    render_error(
        &OagwError::new(
            ErrorKind::ValidationError,
            "request body must be application/json",
        ),
        instance,
    )
}
