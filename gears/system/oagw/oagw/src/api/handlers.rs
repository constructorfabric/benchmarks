// Created: 2026-09-04 by Constructor Tech
//! HTTP handlers of the `/oagw/v1` management surface.
//!
//! Each handler is a thin projection: it converts the wire DTO into a domain
//! spec, calls the synchronous [`ControlPlaneService`], and renders either the
//! response DTO or the RFC 9457 problem body of the single [`OagwError`]
//! model. No handler holds a store lock, and none performs I/O.

use std::sync::Arc;

use axum::extract::{Extension, Path, RawQuery};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::Value;
use toolkit::api::canonical_prelude::{ApiResult, Json, created_json, ok_json};
use toolkit_canonical_errors::problem::APPLICATION_PROBLEM_JSON;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::dto::{
    CreateRouteRequestDto, ListQuery, PluginResponseDto, PluginSourceResponseDto,
    RegisterPluginRequestDto, ReplaceRouteRequestDto, RouteResponseDto, UpstreamRequestDto,
    UpstreamResponseDto,
};
use crate::controlplane::service::{ControlPlaneService, PluginDescriptor};
use crate::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER, OagwError, ProblemExtensions};

/// Alias of the concrete control-plane service the handlers extract.
pub(crate) type Service = ControlPlaneService;

/// RFC 9457 body of a control-plane error
/// (`docs/DESIGN.md` §3.3 "Error Response Format").
///
/// The body is the Phase-1 problem projection (which carries the PRD error
/// code in `error_code`), re-exposed under the `code` key the PRD documents,
/// and tagged as a *gateway* error through `x-oagw-error-source` (ADR 0007).
#[must_use]
pub(crate) fn problem_response(error: OagwError, uri: &Uri) -> axum::response::Response {
    let extensions = ProblemExtensions {
        instance: Some(uri.path().to_owned()),
        ..ProblemExtensions::default()
    };
    let problem = error.problem(extensions);
    let status = StatusCode::from_u16(problem.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut body = serde_json::to_value(&problem).unwrap_or(Value::Null);
    if let Some(code) = problem.error_code {
        body["code"] = Value::String(code);
    }
    let mut response = (
        status,
        [(axum::http::header::CONTENT_TYPE, APPLICATION_PROBLEM_JSON)],
        body.to_string(),
    )
        .into_response();
    response.headers_mut().insert(
        ERROR_SOURCE_HEADER,
        axum::http::HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
    );
    if let Some(seconds) = error.retry_after_secs() {
        response.headers_mut().insert(
            axum::http::header::RETRY_AFTER,
            axum::http::HeaderValue::from(seconds),
        );
    }
    response
}

/// `201 Created` + `Location`, or the problem body of the domain error.
fn created_or_problem<T: Serialize>(
    result: Result<(T, Uuid), OagwError>,
    uri: &Uri,
) -> ApiResult<Response> {
    match result {
        Ok((body, id)) => {
            Ok(created_json(body, uri, id.as_simple().to_string().as_str()).into_response())
        }
        Err(error) => Ok(problem_response(error, uri)),
    }
}

/// `200 OK` with the projected JSON body, or the problem body of the domain
/// error.
fn ok_or_problem<T: Serialize>(result: Result<T, OagwError>, uri: &Uri) -> ApiResult<Response> {
    match result {
        Ok(body) => Ok(ok_json(body).into_response()),
        Err(error) => Ok(problem_response(error, uri)),
    }
}

/// `204 No Content`, or the problem body of the domain error.
fn deleted_or_problem(result: Result<(), OagwError>, uri: &Uri) -> ApiResult<Response> {
    match result {
        Ok(()) => Ok(StatusCode::NO_CONTENT.into_response()),
        Err(error) => Ok(problem_response(error, uri)),
    }
}

// ------------------------------------------------------------------ upstreams

/// `POST /oagw/v1/upstreams`
///
/// # Errors
///
/// Returns a problem body on validation failure (400) or alias collision
/// (409).
pub async fn create_upstream(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Service>>,
    Json(body): Json<UpstreamRequestDto>,
) -> ApiResult<Response> {
    let tenant_id = ctx.subject_tenant_id();
    let spec = crate::api::dto::upstream_spec(&body, tenant_id, &svc);
    let created = spec.and_then(|spec| {
        svc.create_upstream(&spec)
            .map(|upstream| (UpstreamResponseDto::from(&upstream), upstream.id))
    });
    created_or_problem(created, &uri)
}

/// `GET /oagw/v1/upstreams`
///
/// # Errors
///
/// Returns a problem body for an invalid OData parameter (400).
pub async fn list_upstreams(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Service>>,
    RawQuery(query): RawQuery,
) -> ApiResult<Response> {
    let items: Vec<UpstreamResponseDto> = svc
        .list_upstreams(ctx.subject_tenant_id())
        .iter()
        .map(UpstreamResponseDto::from)
        .collect();
    let page = ListQuery::parse(query.as_deref()).and_then(|query| query.apply(&items));
    ok_or_problem(page, &uri)
}

/// `GET /oagw/v1/upstreams/{id}`
///
/// # Errors
///
/// Returns a problem body for an unknown id (404).
pub async fn get_upstream(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Service>>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let found = parse_id(&id).and_then(|id| {
        svc.upstream(ctx.subject_tenant_id(), id)
            .map(|upstream| UpstreamResponseDto::from(&upstream))
    });
    ok_or_problem(found, &uri)
}

/// `PUT /oagw/v1/upstreams/{id}`
///
/// # Errors
///
/// Returns a problem body for validation failure (400), an unknown id (404)
/// or an alias collision (409).
pub async fn replace_upstream(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Service>>,
    Path(id): Path<String>,
    Json(body): Json<UpstreamRequestDto>,
) -> ApiResult<Response> {
    let tenant_id = ctx.subject_tenant_id();
    let spec = crate::api::dto::upstream_spec(&body, tenant_id, &svc);
    let replaced = spec.and_then(|spec| {
        svc.replace_upstream(tenant_id, parse_id(&id)?, &spec)
            .map(|upstream| UpstreamResponseDto::from(&upstream))
    });
    ok_or_problem(replaced, &uri)
}

/// `DELETE /oagw/v1/upstreams/{id}`
///
/// # Errors
///
/// Returns a problem body for an unknown id (404) or a still-referenced
/// upstream (409).
pub async fn delete_upstream(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Service>>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let deleted = parse_id(&id).and_then(|id| svc.delete_upstream(ctx.subject_tenant_id(), id));
    deleted_or_problem(deleted, &uri)
}

// --------------------------------------------------------------------- routes

/// `POST /oagw/v1/routes`
///
/// # Errors
///
/// Returns a problem body on validation failure (400), an unknown upstream
/// (404) or a duplicate match rule (409).
pub async fn create_route(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Service>>,
    Json(body): Json<CreateRouteRequestDto>,
) -> ApiResult<Response> {
    let tenant_id = ctx.subject_tenant_id();
    let spec = body.to_spec(tenant_id, &svc);
    let created = spec.and_then(|spec| {
        svc.create_route(&spec)
            .map(|route| (RouteResponseDto::from(&route), route.id))
    });
    created_or_problem(created, &uri)
}

/// `GET /oagw/v1/routes`
///
/// # Errors
///
/// Returns a problem body for an invalid OData parameter (400).
pub async fn list_routes(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Service>>,
    RawQuery(query): RawQuery,
) -> ApiResult<Response> {
    let items: Vec<RouteResponseDto> = svc
        .list_routes(ctx.subject_tenant_id())
        .iter()
        .map(RouteResponseDto::from)
        .collect();
    let page = ListQuery::parse(query.as_deref()).and_then(|query| query.apply(&items));
    ok_or_problem(page, &uri)
}

/// `GET /oagw/v1/routes/{id}`
///
/// # Errors
///
/// Returns a problem body for an unknown id (404).
pub async fn get_route(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Service>>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let found = parse_id(&id).and_then(|id| {
        svc.route(ctx.subject_tenant_id(), id)
            .map(|route| RouteResponseDto::from(&route))
    });
    ok_or_problem(found, &uri)
}

/// `PUT /oagw/v1/routes/{id}`
///
/// # Errors
///
/// Returns a problem body for validation failure (400), an unknown id (404)
/// or a duplicate match rule (409).
pub async fn replace_route(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Service>>,
    Path(id): Path<String>,
    Json(body): Json<ReplaceRouteRequestDto>,
) -> ApiResult<Response> {
    let tenant_id = ctx.subject_tenant_id();
    let update = body.to_update(tenant_id, &svc);
    let replaced = update.and_then(|update| {
        svc.replace_route(tenant_id, parse_id(&id)?, &update)
            .map(|route| RouteResponseDto::from(&route))
    });
    ok_or_problem(replaced, &uri)
}

/// `DELETE /oagw/v1/routes/{id}`
///
/// # Errors
///
/// Returns a problem body for an unknown id (404).
pub async fn delete_route(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Service>>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let deleted = parse_id(&id).and_then(|id| svc.delete_route(ctx.subject_tenant_id(), id));
    deleted_or_problem(deleted, &uri)
}

// -------------------------------------------------------------------- plugins

/// `POST /oagw/v1/plugins`
///
/// # Errors
///
/// Returns a problem body on validation failure (400) or a duplicate name
/// (409).
pub async fn register_plugin(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Service>>,
    Json(body): Json<RegisterPluginRequestDto>,
) -> ApiResult<Response> {
    let tenant_id = ctx.subject_tenant_id();
    let registered = parse_plugin_kind(body.kind.as_deref()).and_then(|kind| {
        svc.register_plugin(
            tenant_id,
            kind,
            body.name.clone().unwrap_or_default(),
            body.source.clone().unwrap_or_default(),
        )
        .map(|plugin| {
            (
                PluginResponseDto::from(&PluginDescriptor::from(&plugin)),
                plugin.id,
            )
        })
    });
    created_or_problem(registered, &uri)
}

/// `GET /oagw/v1/plugins` — the built-in catalog plus the registered custom
/// plugins.
///
/// # Errors
///
/// Returns a problem body for an invalid OData parameter (400).
pub async fn list_plugins(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Service>>,
    RawQuery(query): RawQuery,
) -> ApiResult<Response> {
    let items: Vec<PluginResponseDto> = svc
        .list_plugins(ctx.subject_tenant_id())
        .iter()
        .map(PluginResponseDto::from)
        .collect();
    let page = ListQuery::parse(query.as_deref()).and_then(|query| query.apply(&items));
    ok_or_problem(page, &uri)
}

/// `GET /oagw/v1/plugins/{id}`
///
/// # Errors
///
/// Returns a problem body for an unknown id (404) or a malformed identifier
/// (400).
pub async fn get_plugin(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Service>>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let found = svc
        .plugin_descriptor(ctx.subject_tenant_id(), &id)
        .map(|descriptor| PluginResponseDto::from(&descriptor));
    ok_or_problem(found, &uri)
}

/// `DELETE /oagw/v1/plugins/{id}`
///
/// # Errors
///
/// Returns a problem body for an unknown id (404) or a plugin that is still
/// referenced (409).
pub async fn delete_plugin(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Service>>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let deleted = svc.delete_plugin(ctx.subject_tenant_id(), &id);
    deleted_or_problem(deleted, &uri)
}

/// `GET /oagw/v1/plugins/{id}/source`
///
/// # Errors
///
/// Returns a problem body for an unknown id (404) or a malformed identifier
/// (400).
pub async fn get_plugin_source(
    uri: Uri,
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<Service>>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let found = svc
        .plugin_source(ctx.subject_tenant_id(), &id)
        .map(|source| PluginSourceResponseDto::from(&source));
    ok_or_problem(found, &uri)
}

// --------------------------------------------------------------------- shared

/// Parses a path `{id}` into a UUID.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] for a non-UUID path segment.
fn parse_id(raw: &str) -> Result<Uuid, OagwError> {
    Uuid::parse_str(raw.trim()).map_err(|_| OagwError::Validation {
        detail: format!("'{raw}' is not a valid resource identifier"),
    })
}

/// Parses the `kind` of a plugin registration request.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] when the kind is missing or unknown.
fn parse_plugin_kind(raw: Option<&str>) -> Result<crate::domain::PluginKind, OagwError> {
    let Some(raw) = raw else {
        return Err(OagwError::Validation {
            detail: String::from("kind is required (auth, guard or transform)"),
        });
    };
    crate::domain::PluginKind::parse(raw)
}
