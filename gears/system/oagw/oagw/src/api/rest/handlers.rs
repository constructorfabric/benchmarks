//! REST handlers for the OAGW gear.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Extension, Path, Query, RawQuery};
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::control_plane::{Caller, ControlPlaneService, ListOptions};
use crate::domain::data_plane::{DataPlaneService, MAX_REQUEST_BODY, ProxyRequest};
use crate::domain::wire::{
    PluginDocument, PluginInput, RouteDocument, RouteInput, RouteListResponse, UpstreamDocument,
    UpstreamInput, UpstreamListResponse,
};
use crate::error::OagwError;
use crate::infra::storage::Services;
use crate::gts_helpers;

type Svc = Arc<Services>;

fn caller_of(ctx: &SecurityContext) -> Caller {
    Caller::from_security_context(ctx)
}

/// Parse a resource id path parameter: a bare UUID or a full GTS
/// `{type}~{uuid}` identifier of the given resource type.
fn parse_resource_id(raw: &str, resource_type: &str) -> Result<Uuid, OagwError> {
    if let Ok(id) = Uuid::parse_str(raw) {
        return Ok(id);
    }
    if let Some(rest) = raw.strip_prefix(resource_type).and_then(|r| r.strip_prefix('~')) {
        return Uuid::parse_str(rest)
            .map_err(|_| OagwError::validation("invalid resource identifier"));
    }
    Err(OagwError::validation(format!(
        "`{raw}` is not a valid UUID or {resource_type}~ identifier"
    )))
}

fn parse_id_param(raw: &str, resource_type: &str) -> Result<Uuid, OagwError> {
    parse_resource_id(raw, resource_type)
}

/// Parse OData `$top`/`$skip` query params (accepts a bare `top`/`skip` too).
fn list_options(params: &HashMap<String, String>) -> Result<ListOptions, OagwError> {
    let parse = |key: &str| -> Result<usize, OagwError> {
        let raw = params
            .get(&format!("${key}"))
            .or_else(|| params.get(key));
        match raw {
            None => Ok(0),
            Some(v) => v
                .trim()
                .parse::<usize>()
                .map_err(|_| OagwError::validation(format!("`${key}` must be a non-negative integer"))),
        }
    };
    Ok(ListOptions {
        top: parse("top")?,
        skip: parse("skip")?,
    })
}

fn json_invalid(e: JsonRejection) -> OagwError {
    OagwError::validation(format!("invalid request body: {e}"))
}

fn body<T>(b: Result<Json<T>, JsonRejection>) -> Result<T, OagwError> {
    b.map(|Json(v)| v).map_err(json_invalid)
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

pub async fn create_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    input: Result<Json<UpstreamInput>, JsonRejection>,
) -> Result<(StatusCode, Json<UpstreamDocument>), OagwError> {
    let caller = caller_of(&ctx);
    let upstream = svc.create_upstream(&caller, body(input)?).await?;
    Ok((StatusCode::CREATED, Json(UpstreamDocument::from(&upstream))))
}

pub async fn list_upstreams(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<UpstreamListResponse>, OagwError> {
    let caller = caller_of(&ctx);
    let opts = list_options(&params)?;
    let (upstreams, total) = svc.list_upstreams(&caller, &opts).await?;
    Ok(Json(UpstreamListResponse {
        items: upstreams.iter().map(Into::into).collect(),
        total,
    }))
}

pub async fn get_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path(id): Path<String>,
) -> Result<Json<UpstreamDocument>, OagwError> {
    let caller = caller_of(&ctx);
    let id = parse_id_param(&id, gts_helpers::UPSTREAM_TYPE)?;
    let upstream = svc.get_upstream(&caller, id).await?;
    Ok(Json(UpstreamDocument::from(&upstream)))
}

pub async fn update_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path(id): Path<String>,
    input: Result<Json<UpstreamInput>, JsonRejection>,
) -> Result<Json<UpstreamDocument>, OagwError> {
    let caller = caller_of(&ctx);
    let id = parse_id_param(&id, gts_helpers::UPSTREAM_TYPE)?;
    let upstream = svc.update_upstream(&caller, id, body(input)?).await?;
    Ok(Json(UpstreamDocument::from(&upstream)))
}

pub async fn delete_upstream(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path(id): Path<String>,
) -> Result<StatusCode, OagwError> {
    let caller = caller_of(&ctx);
    let id = parse_id_param(&id, gts_helpers::UPSTREAM_TYPE)?;
    svc.delete_upstream(&caller, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

pub async fn create_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    input: Result<Json<RouteInput>, JsonRejection>,
) -> Result<(StatusCode, Json<RouteDocument>), OagwError> {
    let caller = caller_of(&ctx);
    let route = svc.create_route(&caller, body(input)?).await?;
    Ok((StatusCode::CREATED, Json(RouteDocument::from(&route))))
}

pub async fn list_routes(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<RouteListResponse>, OagwError> {
    let caller = caller_of(&ctx);
    let opts = list_options(&params)?;
    let (routes, total) = svc.list_routes(&caller, &opts).await?;
    Ok(Json(RouteListResponse {
        items: routes.iter().map(Into::into).collect(),
        total,
    }))
}

pub async fn get_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path(id): Path<String>,
) -> Result<Json<RouteDocument>, OagwError> {
    let caller = caller_of(&ctx);
    let id = parse_id_param(&id, gts_helpers::ROUTE_TYPE)?;
    let route = svc.get_route(&caller, id).await?;
    Ok(Json(RouteDocument::from(&route)))
}

pub async fn update_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path(id): Path<String>,
    input: Result<Json<RouteInput>, JsonRejection>,
) -> Result<Json<RouteDocument>, OagwError> {
    let caller = caller_of(&ctx);
    let id = parse_id_param(&id, gts_helpers::ROUTE_TYPE)?;
    let route = svc.update_route(&caller, id, body(input)?).await?;
    Ok(Json(RouteDocument::from(&route)))
}

pub async fn delete_route(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path(id): Path<String>,
) -> Result<StatusCode, OagwError> {
    let caller = caller_of(&ctx);
    let id = parse_id_param(&id, gts_helpers::ROUTE_TYPE)?;
    svc.delete_route(&caller, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Custom plugins
// ---------------------------------------------------------------------------

pub async fn create_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    input: Result<Json<PluginInput>, JsonRejection>,
) -> Result<(StatusCode, Json<PluginDocument>), OagwError> {
    let caller = caller_of(&ctx);
    let plugin = svc.create_plugin(&caller, body(input)?).await?;
    Ok((StatusCode::CREATED, Json(PluginDocument::from(&plugin))))
}

pub async fn list_plugins(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<crate::domain::wire::PluginListResponse>, OagwError> {
    let caller = caller_of(&ctx);
    let opts = list_options(&params)?;
    let (plugins, total) = svc.list_plugins(&caller, &opts).await?;
    Ok(Json(crate::domain::wire::PluginListResponse {
        items: plugins.iter().map(Into::into).collect(),
        total,
    }))
}

pub async fn get_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path(id): Path<String>,
) -> Result<Json<PluginDocument>, OagwError> {
    let caller = caller_of(&ctx);
    let id = parse_plugin_id(&id)?;
    let plugin = svc.get_plugin(&caller, id).await?;
    Ok(Json(PluginDocument::from(&plugin)))
}

pub async fn delete_plugin(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path(id): Path<String>,
) -> Result<StatusCode, OagwError> {
    let caller = caller_of(&ctx);
    let id = parse_plugin_id(&id)?;
    svc.delete_plugin(&caller, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn get_plugin_source(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path(id): Path<String>,
) -> Result<Json<String>, OagwError> {
    let caller = caller_of(&ctx);
    let id = parse_plugin_id(&id)?;
    let source = svc.get_plugin_source(&caller, id).await?;
    Ok(Json(source.unwrap_or_default()))
}

/// Plugin ids accept bare UUIDs or any `{plugin_type}~{uuid}` form.
fn parse_plugin_id(raw: &str) -> Result<Uuid, OagwError> {
    if let Ok(id) = Uuid::parse_str(raw) {
        return Ok(id);
    }
    for resource_type in [
        gts_helpers::AUTH_PLUGIN_TYPE,
        gts_helpers::GUARD_PLUGIN_TYPE,
        gts_helpers::TRANSFORM_PLUGIN_TYPE,
    ] {
        if let Some(rest) = raw.strip_prefix(resource_type).and_then(|r| r.strip_prefix('~')) {
            return Uuid::parse_str(rest)
                .map_err(|_| OagwError::validation("invalid plugin identifier"));
        }
    }
    Err(OagwError::validation(format!(
        "`{raw}` is not a valid plugin UUID or GTS identifier"
    )))
}

// ---------------------------------------------------------------------------
// Proxy
// ---------------------------------------------------------------------------

/// Body validation rules (DESIGN.md): valid Content-Length matching the
/// actual size, only `chunked` transfer encoding, and the 100 MiB hard limit.
fn validate_proxy_body(headers: &HeaderMap, body: &[u8]) -> Result<(), OagwError> {
    if let Some(cl) = headers.get("content-length") {
        let cl_str = cl
            .to_str()
            .map_err(|_| OagwError::validation("Content-Length is not a valid header value"))?;
        let declared: u64 = cl_str
            .trim()
            .parse()
            .map_err(|_| OagwError::validation("Content-Length must be a valid integer"))?;
        if declared as usize != body.len() {
            return Err(OagwError::validation(format!(
                "Content-Length ({declared}) does not match the actual body size ({})",
                body.len()
            )));
        }
    }
    for te in headers.get_all("transfer-encoding").iter() {
        let val = te
            .to_str()
            .map_err(|_| OagwError::validation("Transfer-Encoding is not a valid header value"))?;
        if !val.eq_ignore_ascii_case("chunked") {
            return Err(OagwError::validation(format!(
                "unsupported Transfer-Encoding `{val}`; only chunked is supported"
            )));
        }
    }
    if body.len() > MAX_REQUEST_BODY {
        return Err(OagwError::PayloadTooLarge {
            detail: format!(
                "request body exceeds the {}-byte limit",
                MAX_REQUEST_BODY
            ),
        });
    }
    Ok(())
}

/// Split `proxy_path` (the path after `/oagw/v1/proxy/`) into `(alias, rest)`.
fn split_alias_path(proxy_path: &str) -> (String, String) {
    let trimmed = proxy_path.trim_matches('/');
    match trimmed.split_once('/') {
        Some((alias, rest)) => (alias.to_owned(), rest.to_owned()),
        None => (trimmed.to_owned(), String::new()),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn proxy(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Svc>,
    Path(proxy_path): Path<String>,
    method: Method,
    headers: HeaderMap,
    RawQuery(raw_query): RawQuery,
    body: Bytes,
) -> Result<Response, OagwError> {
    // CORS preflight is answered permissively at the handler level (ADR-0004):
    // no alias resolution, no tenant context required.
    if crate::infra::cors::is_cors_preflight(&method, &headers) {
        return Ok(crate::infra::cors::build_preflight_response(&headers));
    }

    if method == Method::OPTIONS {
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    validate_proxy_body(&headers, &body)?;

    let (alias, path_suffix) = split_alias_path(&proxy_path);
    if alias.is_empty() {
        return Err(OagwError::not_found("missing alias in proxy path"));
    }

    let caller = caller_of(&ctx);
    let request = ProxyRequest {
        method,
        path_suffix,
        raw_query,
        headers,
        body,
        client_ip: None,
    };

    let proxy_resp = svc
        .proxy_request(&caller, &alias, request)
        .await
        .map_err(|f| f.error)?;

    let mut builder = axum::response::Response::builder().status(proxy_resp.status);
    for (name, value) in proxy_resp.headers.iter() {
        builder = builder.header(name, value);
    }
    builder
        .body(axum::body::Body::new(proxy_resp.body))
        .map_err(|e| OagwError::internal(format!("failed to build proxy response: {e}")))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn proxy_body_validation() {
        // Matching Content-Length passes.
        let h = headers(&[("content-length", "3")]);
        assert!(validate_proxy_body(&h, b"abc").is_ok());

        // Mismatched Content-Length is a 400.
        let h = headers(&[("content-length", "5")]);
        let err = validate_proxy_body(&h, b"abc").unwrap_err();
        assert_eq!(err.status().as_u16(), 400);
        assert!(err.detail().contains("Content-Length"));

        // Unsupported transfer encoding is a 400.
        let h = headers(&[("transfer-encoding", "gzip")]);
        let err = validate_proxy_body(&h, b"").unwrap_err();
        assert_eq!(err.status().as_u16(), 400);
        assert!(err.detail().contains("Transfer-Encoding"));

        // chunked is accepted (body is the de-chunked payload).
        let h = headers(&[("transfer-encoding", "chunked")]);
        assert!(validate_proxy_body(&h, b"abc").is_ok());

        // Over the hard limit is a 413.
        let h = headers(&[]);
        let body = vec![0u8; MAX_REQUEST_BODY + 1];
        let err = validate_proxy_body(&h, &body).unwrap_err();
        assert_eq!(err.status().as_u16(), 413);
    }

    #[test]
    fn alias_path_splitting() {
        assert_eq!(split_alias_path("tr/api/x"), ("tr".to_owned(), "api/x".to_owned()));
        assert_eq!(split_alias_path("tr"), ("tr".to_owned(), String::new()));
        assert_eq!(split_alias_path("/tr/"), ("tr".to_owned(), String::new()));
        assert_eq!(split_alias_path(""), ("".to_owned(), String::new()));
    }

    #[test]
    fn resource_id_parsing() {
        let id = Uuid::new_v4();
        assert_eq!(parse_resource_id(&id.to_string(), "gts.cf.core.oagw.upstream.v1").unwrap(), id);
        assert_eq!(
            parse_resource_id(&format!("gts.cf.core.oagw.upstream.v1~{id}"), "gts.cf.core.oagw.upstream.v1")
                .unwrap(),
            id
        );
        assert!(parse_resource_id("not-a-uuid", "gts.cf.core.oagw.upstream.v1").is_err());
    }
}
