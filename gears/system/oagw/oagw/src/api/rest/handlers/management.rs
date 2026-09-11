//! Management handlers — upstream, route and plugin CRUD.
//!
//! Every handler is tenant-scoped through [`OagwState::authorize`] plus the
//! repository boundary, so an ancestor's resources are simply not there
//! (`404`) rather than visible-but-forbidden.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Extension, Path, Query};
use axum::response::{IntoResponse, Response};
use http::{StatusCode, Uri, header};
use serde_json::Value;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{
    ListResponseDto, PluginRequestDto, PluginResponseDto, PluginSourceDto, RouteRequestDto,
    RouteResponseDto, UpstreamRequestDto, UpstreamResponseDto,
};
use crate::api::rest::error::problem_response;
use crate::api::rest::query;
use crate::api::rest::state::{OagwState, actions};
use crate::domain::error::{DomainError, DomainResult, ErrorSource};
use crate::domain::gts_helpers;

/// Result of a management handler before rendering.
type HandlerResult = DomainResult<Response>;

/// Render a handler result, turning a domain error into a problem document
/// anchored at the request path.
fn render(uri: &Uri, result: HandlerResult) -> Response {
    match result {
        Ok(response) => response,
        Err(err) => problem_response(&err, uri.path(), ErrorSource::Gateway),
    }
}

fn json_ok<T: serde::Serialize>(status: StatusCode, body: &T) -> DomainResult<Response> {
    let value = serde_json::to_value(body)
        .map_err(|err| DomainError::internal(format!("could not serialize the response: {err}")))?;
    Ok((status, axum::Json(value)).into_response())
}

fn created<T: serde::Serialize>(uri: &Uri, id: Uuid, body: &T) -> DomainResult<Response> {
    let location = format!("{}/{id}", uri.path().trim_end_matches('/'));
    let value = serde_json::to_value(body)
        .map_err(|err| DomainError::internal(format!("could not serialize the response: {err}")))?;
    Ok((
        StatusCode::CREATED,
        [(header::LOCATION, location)],
        axum::Json(value),
    )
        .into_response())
}

fn list_response(
    params: &HashMap<String, String>,
    items: Vec<Value>,
) -> DomainResult<Response> {
    let parsed = query::parse(params)?;
    let items = query::apply(&parsed, items);
    let count = items.len();
    json_ok(StatusCode::OK, &ListResponseDto { items, count })
}

fn parse_id(raw: &str, base_type: &str, label: &str) -> DomainResult<Uuid> {
    gts_helpers::parse_resource_id(raw, base_type).ok_or_else(|| {
        DomainError::validation(format!(
            "'{label}' must be a UUID or a '{base_type}{{uuid}}' identifier, got '{raw}'"
        ))
    })
}

fn encode_list<D: serde::Serialize>(items: Vec<D>) -> DomainResult<Vec<Value>> {
    items
        .into_iter()
        .map(|item| {
            serde_json::to_value(item).map_err(|err| {
                DomainError::internal(format!("could not serialize a list item: {err}"))
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/upstreams`
pub async fn create_upstream(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    axum::Json(body): axum::Json<UpstreamRequestDto>,
) -> Response {
    render(&uri.clone(), async move {
        state
            .authorize(&ctx, gts_helpers::UPSTREAM_TYPE, actions::CREATE)
            .await?;
        let upstream = state
            .control_plane
            .create_upstream(&ctx, body.into_spec()?)
            .await?;
        created(&uri, upstream.id, &UpstreamResponseDto::from(&upstream))
    }
    .await)
}

/// `GET /oagw/v1/upstreams`
pub async fn list_upstreams(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    render(&uri.clone(), async move {
        state
            .authorize(&ctx, gts_helpers::UPSTREAM_TYPE, actions::READ)
            .await?;
        let upstreams = state.control_plane.list_upstreams(&ctx).await?;
        let items = encode_list(
            upstreams.iter().map(UpstreamResponseDto::from).collect::<Vec<_>>(),
        )?;
        list_response(&params, items)
    }
    .await)
}

/// `GET /oagw/v1/upstreams/{id}`
pub async fn get_upstream(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Response {
    render(&uri.clone(), async move {
        state
            .authorize(&ctx, gts_helpers::UPSTREAM_TYPE, actions::READ)
            .await?;
        let id = parse_id(&id, gts_helpers::UPSTREAM_TYPE, "id")?;
        let upstream = state.control_plane.get_upstream(&ctx, id).await?;
        json_ok(StatusCode::OK, &UpstreamResponseDto::from(&upstream))
    }
    .await)
}

/// `PUT /oagw/v1/upstreams/{id}`
pub async fn replace_upstream(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<UpstreamRequestDto>,
) -> Response {
    render(&uri.clone(), async move {
        state
            .authorize(&ctx, gts_helpers::UPSTREAM_TYPE, actions::OVERRIDE)
            .await?;
        let id = parse_id(&id, gts_helpers::UPSTREAM_TYPE, "id")?;
        let upstream = state
            .control_plane
            .replace_upstream(&ctx, id, body.into_spec()?)
            .await?;
        json_ok(StatusCode::OK, &UpstreamResponseDto::from(&upstream))
    }
    .await)
}

/// `DELETE /oagw/v1/upstreams/{id}`
pub async fn delete_upstream(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Response {
    render(&uri.clone(), async move {
        state
            .authorize(&ctx, gts_helpers::UPSTREAM_TYPE, actions::DELETE)
            .await?;
        let id = parse_id(&id, gts_helpers::UPSTREAM_TYPE, "id")?;
        state.control_plane.delete_upstream(&ctx, id).await?;
        Ok(StatusCode::NO_CONTENT.into_response())
    }
    .await)
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/routes`
pub async fn create_route(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    axum::Json(body): axum::Json<RouteRequestDto>,
) -> Response {
    render(&uri.clone(), async move {
        state
            .authorize(&ctx, gts_helpers::ROUTE_TYPE, actions::CREATE)
            .await?;
        let route = state
            .control_plane
            .create_route(&ctx, body.into_spec()?)
            .await?;
        created(&uri, route.id, &RouteResponseDto::from(&route))
    }
    .await)
}

/// `GET /oagw/v1/routes`
pub async fn list_routes(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    render(&uri.clone(), async move {
        state
            .authorize(&ctx, gts_helpers::ROUTE_TYPE, actions::READ)
            .await?;
        let routes = state.control_plane.list_routes(&ctx).await?;
        let items = encode_list(
            routes.iter().map(RouteResponseDto::from).collect::<Vec<_>>(),
        )?;
        list_response(&params, items)
    }
    .await)
}

/// `GET /oagw/v1/routes/{id}`
pub async fn get_route(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Response {
    render(&uri.clone(), async move {
        state
            .authorize(&ctx, gts_helpers::ROUTE_TYPE, actions::READ)
            .await?;
        let id = parse_id(&id, gts_helpers::ROUTE_TYPE, "id")?;
        let route = state.control_plane.get_route(&ctx, id).await?;
        json_ok(StatusCode::OK, &RouteResponseDto::from(&route))
    }
    .await)
}

/// `PUT /oagw/v1/routes/{id}`
pub async fn replace_route(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(id): Path<String>,
    axum::Json(body): axum::Json<RouteRequestDto>,
) -> Response {
    render(&uri.clone(), async move {
        state
            .authorize(&ctx, gts_helpers::ROUTE_TYPE, actions::OVERRIDE)
            .await?;
        let id = parse_id(&id, gts_helpers::ROUTE_TYPE, "id")?;
        let route = state
            .control_plane
            .replace_route(&ctx, id, body.into_spec()?)
            .await?;
        json_ok(StatusCode::OK, &RouteResponseDto::from(&route))
    }
    .await)
}

/// `DELETE /oagw/v1/routes/{id}`
pub async fn delete_route(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Response {
    render(&uri.clone(), async move {
        state
            .authorize(&ctx, gts_helpers::ROUTE_TYPE, actions::DELETE)
            .await?;
        let id = parse_id(&id, gts_helpers::ROUTE_TYPE, "id")?;
        state.control_plane.delete_route(&ctx, id).await?;
        Ok(StatusCode::NO_CONTENT.into_response())
    }
    .await)
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// Authorization for a plugin operation is scoped to the plugin's own base
/// type, so an operator can be allowed to manage guards without also being
/// allowed to manage auth plugins.
fn plugin_resource_type(kind: crate::domain::model::PluginKind) -> &'static str {
    kind.base_type()
}

/// `POST /oagw/v1/plugins`
pub async fn create_plugin(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    axum::Json(body): axum::Json<PluginRequestDto>,
) -> Response {
    render(&uri.clone(), async move {
        let spec = body.into_spec()?;
        state
            .authorize(&ctx, plugin_resource_type(spec.plugin_type), actions::CREATE)
            .await?;
        let plugin = state.control_plane.create_plugin(&ctx, spec).await?;
        created(&uri, plugin.id, &PluginResponseDto::from(&plugin))
    }
    .await)
}

/// `GET /oagw/v1/plugins`
pub async fn list_plugins(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    render(&uri.clone(), async move {
        for base in gts_helpers::PLUGIN_TYPES {
            state.authorize(&ctx, base, actions::READ).await?;
        }
        let plugins = state.control_plane.list_plugins(&ctx).await?;
        let items = encode_list(
            plugins.iter().map(PluginResponseDto::from).collect::<Vec<_>>(),
        )?;
        list_response(&params, items)
    }
    .await)
}

/// `GET /oagw/v1/plugins/{id}`
pub async fn get_plugin(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Response {
    render(&uri.clone(), async move {
        let id = gts_helpers::parse_plugin_id(&id).ok_or_else(|| {
            DomainError::validation(format!(
                "'id' must be a UUID or a plugin GTS identifier, got '{id}'"
            ))
        })?;
        let plugin = state.control_plane.get_plugin(&ctx, id).await?;
        state
            .authorize(&ctx, plugin_resource_type(plugin.plugin_type), actions::READ)
            .await?;
        json_ok(StatusCode::OK, &PluginResponseDto::from(&plugin))
    }
    .await)
}

/// `GET /oagw/v1/plugins/{id}/source`
pub async fn get_plugin_source(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Response {
    render(&uri.clone(), async move {
        let id = gts_helpers::parse_plugin_id(&id).ok_or_else(|| {
            DomainError::validation(format!(
                "'id' must be a UUID or a plugin GTS identifier, got '{id}'"
            ))
        })?;
        let plugin = state.control_plane.get_plugin(&ctx, id).await?;
        state
            .authorize(&ctx, plugin_resource_type(plugin.plugin_type), actions::READ)
            .await?;
        json_ok(StatusCode::OK, &PluginSourceDto::from(&plugin))
    }
    .await)
}

/// `DELETE /oagw/v1/plugins/{id}`
pub async fn delete_plugin(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(id): Path<String>,
) -> Response {
    render(&uri.clone(), async move {
        let id = gts_helpers::parse_plugin_id(&id).ok_or_else(|| {
            DomainError::validation(format!(
                "'id' must be a UUID or a plugin GTS identifier, got '{id}'"
            ))
        })?;
        let plugin = state.control_plane.get_plugin(&ctx, id).await?;
        state
            .authorize(
                &ctx,
                plugin_resource_type(plugin.plugin_type),
                actions::DELETE,
            )
            .await?;
        state.control_plane.delete_plugin(&ctx, id).await?;
        Ok(StatusCode::NO_CONTENT.into_response())
    }
    .await)
}
