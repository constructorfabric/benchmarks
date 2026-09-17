//! REST handlers for the OAGW management API.
//!
//! Slice S1 covers upstream CRUD; slice S2b completes the control plane with
//! route and plugin management. Handlers are thin: they resolve the
//! authenticated caller, hand the body to
//! the control-plane service, and map the result to a DTO or to the OAGW
//! problem+json error surface. No business logic lives here.
//!
//! Extractors are read as `Result<_, _>` and request bodies are buffered by the
//! handler ([`buffer_body`]) so that every gateway error — including
//! `Path`/`Query` rejections, oversized bodies and malformed-JSON or
//! wrong-media-type bodies, which axum would otherwise render as plain text —
//! uses the problem+json surface mandated by DESIGN §2.1 / ADR-0007. Success
//! responses are stamped with the same error-source header
//! (`X-OAGW-Error-Source: gateway`).

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Extension, Path, Query, Request};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::dto::{
    ListQuery, PluginDto, PluginRequest, RouteDto, RouteRequest, UpstreamDto, UpstreamRequest,
    page_plugins, page_routes, page_upstreams, plugins_to_json, routes_to_json, upstreams_to_json,
};
use super::error::{invalid_body, stamp_gateway_source, unsupported_media_type};
use super::extract::{
    MAX_BODY_BYTES, buffer_body, check_body_limit_with, path_rejection, query_rejection,
    resource_id,
};
use crate::domain::services::control_plane::ControlPlaneService;
use crate::domain::types::{PluginSpec, RouteSpec, UpstreamSpec};
use crate::error::OagwError;
use crate::tenant_context::CallerContext;

/// The authenticated caller of a management request.
///
/// **Fail closed** (DESIGN §3.3 "Tenant Scoping"): a request without the
/// `SecurityContext` extension the host gateway's auth middleware inserts, or
/// with an anonymous context (no tenant), is rejected with 401. The gear never
/// guesses a tenant, so an unauthenticated request can never read, create,
/// replace or delete configuration.
///
/// # Errors
/// [`crate::error::OagwErrorKind::AuthenticationFailed`] when the request is
/// unauthenticated.
fn caller_of(security: Option<Extension<SecurityContext>>) -> Result<CallerContext, OagwError> {
    let Some(Extension(context)) = security else {
        return Err(OagwError::authentication_required());
    };

    let caller = CallerContext::from(&context);
    if caller.tenant_id().is_none() {
        return Err(OagwError::authentication_required());
    }

    Ok(caller)
}

/// Extract the resource identifier of a path parameter.
///
/// Accepts the bare UUID and the anonymous GTS identifier
/// `gts.cf.core.oagw.<type>.v1~{uuid}` (DESIGN §3.6); a malformed id is a 400,
/// never a 500.
///
/// # Errors
/// [`crate::error::OagwErrorKind::ValidationError`] when the parameter is not a
/// resource identifier.
fn path_id(
    parameter: &'static str,
    path: Result<Path<String>, PathRejection>,
) -> Result<Uuid, OagwError> {
    let Path(raw) = path.map_err(|rejection| path_rejection(parameter, rejection))?;

    resource_id(parameter, &raw)
}

/// Deserialize a JSON request body into a domain [`UpstreamSpec`], mapping
/// failures onto the OAGW error surface.
///
/// # Errors
/// [`crate::error::OagwErrorKind::ValidationError`] when the media type is not
/// JSON, the body is empty, or the payload is not a valid upstream document. An
/// over-limit body is a 413 before it reaches this function: [`buffer_body`]
/// enforces the hard limit while buffering.
fn parse_json_body(headers: &HeaderMap, bytes: &Bytes) -> Result<UpstreamSpec, OagwError> {
    parse_body_with_limit::<UpstreamRequest, UpstreamSpec>(headers, bytes, MAX_BODY_BYTES)
}

/// Deserialize a JSON request body into a domain [`RouteSpec`].
///
/// # Errors
/// As [`parse_json_body`], for a route document.
fn parse_route_body(headers: &HeaderMap, bytes: &Bytes) -> Result<RouteSpec, OagwError> {
    parse_body_with_limit::<RouteRequest, RouteSpec>(headers, bytes, MAX_BODY_BYTES)
}

/// Deserialize a JSON request body into a domain [`PluginSpec`].
///
/// # Errors
/// As [`parse_json_body`], for a plugin document.
fn parse_plugin_body(headers: &HeaderMap, bytes: &Bytes) -> Result<PluginSpec, OagwError> {
    parse_body_with_limit::<PluginRequest, PluginSpec>(headers, bytes, MAX_BODY_BYTES)
}

/// [`parse_json_body`] against an explicit body limit, so tests can exercise the
/// 413 path without allocating the 100 MB hard limit. Generic over the wire
/// document (`Raw`) and the domain specification (`Spec`) it produces, so the
/// three management resources share one media-type/size/JSON pipeline.
fn parse_body_with_limit<Raw, Spec>(
    headers: &HeaderMap,
    bytes: &Bytes,
    limit: usize,
) -> Result<Spec, OagwError>
where
    Raw: serde::de::DeserializeOwned,
    Spec: From<Raw>,
{
    // Enforced before parsing, so an oversized upload is never buffered into a
    // deserializer (DESIGN §2.2 "Body size hard limit").
    check_body_limit_with(bytes, limit)?;

    let media_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim);
    match media_type {
        Some("application/json") | Some("text/json") | None => {}
        Some(other) => return Err(unsupported_media_type(Some(other))),
    }

    if bytes.is_empty() {
        return Err(invalid_body("the request body is empty"));
    }

    let request: Raw =
        serde_json::from_slice(bytes).map_err(|err| invalid_body(&err.to_string()))?;

    Ok(Spec::from(request))
}

/// Mark a gateway-generated response with the ADR-0007 error-source header.
fn respond(response: impl IntoResponse) -> Response {
    stamp_gateway_source(response.into_response())
}

/// The handler behind `method_not_allowed_fallback` (405).
///
/// Axum renders an unregistered method on a registered path as a plain-text 405
/// with no `X-OAGW-Error-Source`; the management API answers every such request
/// with the problem document instead (DESIGN §2.1 / ADR-0007). Plugins, for
/// instance, are immutable (ADR-0002), so `PUT /oagw/v1/plugins/{id}` lands here.
///
/// # Errors
/// Always: [`crate::error::OagwErrorKind::MethodNotAllowed`].
pub async fn method_not_allowed() -> OagwError {
    OagwError::method_not_allowed(
        "the management API does not support this method on the requested resource",
    )
}

/// `POST /oagw/v1/upstreams` — create an upstream (201).
///
/// # Errors
/// Propagates [`OagwError`] rendered as a problem+json response.
pub async fn create_upstream(
    Extension(service): Extension<Arc<ControlPlaneService>>,
    security: Option<Extension<SecurityContext>>,
    mut request: Request,
) -> Result<Response, OagwError> {
    let caller = caller_of(security)?;
    let bytes = buffer_body(&mut request).await?;
    let request: UpstreamSpec = parse_json_body(request.headers(), &bytes)?;
    let created = service.create_upstream(&caller, &request)?;

    Ok(respond((
        StatusCode::CREATED,
        Json(UpstreamDto::from(&created)),
    )))
}

/// `GET /oagw/v1/upstreams/{id}` — fetch an upstream (200).
///
/// # Errors
/// Propagates [`OagwError`] rendered as a problem+json response.
pub async fn get_upstream(
    Extension(service): Extension<Arc<ControlPlaneService>>,
    security: Option<Extension<SecurityContext>>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Response, OagwError> {
    let caller = caller_of(security)?;
    let upstream = service.get_upstream(&caller, path_id("id", id)?)?;

    Ok(respond(Json(UpstreamDto::from(&upstream))))
}

/// `GET /oagw/v1/upstreams` — list upstreams of the calling tenant (200).
///
/// Returns a bare array, honoring `$filter`, `$select`, `$orderby`, `$top` and
/// `$skip`.
///
/// # Errors
/// Propagates [`OagwError`] rendered as a problem+json response.
pub async fn list_upstreams(
    Extension(service): Extension<Arc<ControlPlaneService>>,
    security: Option<Extension<SecurityContext>>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Response, OagwError> {
    let caller = caller_of(security)?;
    let Query(query) = query.map_err(query_rejection)?;
    let items = service.list_upstreams(&caller)?;
    let page = page_upstreams(&items, &query)?;

    Ok(respond(Json(Value::Array(upstreams_to_json(
        &page, &query,
    )))))
}

/// `PUT /oagw/v1/upstreams/{id}` — full replacement (200).
///
/// # Errors
/// Propagates [`OagwError`] rendered as a problem+json response.
pub async fn replace_upstream(
    Extension(service): Extension<Arc<ControlPlaneService>>,
    security: Option<Extension<SecurityContext>>,
    id: Result<Path<String>, PathRejection>,
    mut request: Request,
) -> Result<Response, OagwError> {
    let caller = caller_of(security)?;
    let bytes = buffer_body(&mut request).await?;
    let request: UpstreamSpec = parse_json_body(request.headers(), &bytes)?;
    let updated = service.replace_upstream(&caller, path_id("id", id)?, &request)?;

    Ok(respond(Json(UpstreamDto::from(&updated))))
}

/// `DELETE /oagw/v1/upstreams/{id}` — delete an upstream (204).
///
/// # Errors
/// Propagates [`OagwError`] rendered as a problem+json response.
pub async fn delete_upstream(
    Extension(service): Extension<Arc<ControlPlaneService>>,
    security: Option<Extension<SecurityContext>>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Response, OagwError> {
    let caller = caller_of(security)?;
    service.delete_upstream(&caller, path_id("id", id)?)?;

    Ok(respond(StatusCode::NO_CONTENT))
}

/// `POST /oagw/v1/routes` — create a route (201).
///
/// # Errors
/// Propagates [`OagwError`] rendered as a problem+json response.
pub async fn create_route(
    Extension(service): Extension<Arc<ControlPlaneService>>,
    security: Option<Extension<SecurityContext>>,
    mut request: Request,
) -> Result<Response, OagwError> {
    let caller = caller_of(security)?;
    let bytes = buffer_body(&mut request).await?;
    let request = parse_route_body(request.headers(), &bytes)?;
    let created = service.create_route(&caller, &request)?;

    Ok(respond((
        StatusCode::CREATED,
        Json(RouteDto::from(&created)),
    )))
}

/// `GET /oagw/v1/routes/{id}` — fetch a route (200).
///
/// # Errors
/// Propagates [`OagwError`] rendered as a problem+json response.
pub async fn get_route(
    Extension(service): Extension<Arc<ControlPlaneService>>,
    security: Option<Extension<SecurityContext>>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Response, OagwError> {
    let caller = caller_of(security)?;
    let route = service.get_route(&caller, path_id("id", id)?)?;

    Ok(respond(Json(RouteDto::from(&route))))
}

/// `GET /oagw/v1/routes` — list routes of the calling tenant (200).
///
/// Returns a bare array, honoring `$filter`, `$select`, `$orderby`, `$top` and
/// `$skip`.
///
/// # Errors
/// Propagates [`OagwError`] rendered as a problem+json response.
pub async fn list_routes(
    Extension(service): Extension<Arc<ControlPlaneService>>,
    security: Option<Extension<SecurityContext>>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Response, OagwError> {
    let caller = caller_of(security)?;
    let Query(query) = query.map_err(query_rejection)?;
    let items = service.list_routes(&caller)?;
    let page = page_routes(&items, &query)?;

    Ok(respond(Json(Value::Array(routes_to_json(&page, &query)))))
}

/// `PUT /oagw/v1/routes/{id}` — full replacement (200).
///
/// `upstream_id` is immutable: moving a route to another upstream is a 400 that
/// names the field.
///
/// # Errors
/// Propagates [`OagwError`] rendered as a problem+json response.
pub async fn replace_route(
    Extension(service): Extension<Arc<ControlPlaneService>>,
    security: Option<Extension<SecurityContext>>,
    id: Result<Path<String>, PathRejection>,
    mut request: Request,
) -> Result<Response, OagwError> {
    let caller = caller_of(security)?;
    let bytes = buffer_body(&mut request).await?;
    let request = parse_route_body(request.headers(), &bytes)?;
    let updated = service.replace_route(&caller, path_id("id", id)?, &request)?;

    Ok(respond(Json(RouteDto::from(&updated))))
}

/// `DELETE /oagw/v1/routes/{id}` — delete a route (204).
///
/// # Errors
/// Propagates [`OagwError`] rendered as a problem+json response.
pub async fn delete_route(
    Extension(service): Extension<Arc<ControlPlaneService>>,
    security: Option<Extension<SecurityContext>>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Response, OagwError> {
    let caller = caller_of(security)?;
    service.delete_route(&caller, path_id("id", id)?)?;

    Ok(respond(StatusCode::NO_CONTENT))
}

/// `POST /oagw/v1/plugins` — create a plugin (201).
///
/// # Errors
/// Propagates [`OagwError`] rendered as a problem+json response.
pub async fn create_plugin(
    Extension(service): Extension<Arc<ControlPlaneService>>,
    security: Option<Extension<SecurityContext>>,
    mut request: Request,
) -> Result<Response, OagwError> {
    let caller = caller_of(security)?;
    let bytes = buffer_body(&mut request).await?;
    let request = parse_plugin_body(request.headers(), &bytes)?;
    let created = service.create_plugin(&caller, &request)?;

    Ok(respond((
        StatusCode::CREATED,
        Json(PluginDto::from(&created)),
    )))
}

/// `GET /oagw/v1/plugins` — list plugins of the calling tenant (200).
///
/// Returns a bare array, honoring `$filter`, `$select`, `$orderby`, `$top` and
/// `$skip`.
///
/// # Errors
/// Propagates [`OagwError`] rendered as a problem+json response.
pub async fn list_plugins(
    Extension(service): Extension<Arc<ControlPlaneService>>,
    security: Option<Extension<SecurityContext>>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Response, OagwError> {
    let caller = caller_of(security)?;
    let Query(query) = query.map_err(query_rejection)?;
    let items = service.list_plugins(&caller)?;
    let page = page_plugins(&items, &query)?;

    Ok(respond(Json(Value::Array(plugins_to_json(&page, &query)))))
}

/// `GET /oagw/v1/plugins/{id}` — fetch a plugin (200).
///
/// # Errors
/// Propagates [`OagwError`] rendered as a problem+json response.
pub async fn get_plugin(
    Extension(service): Extension<Arc<ControlPlaneService>>,
    security: Option<Extension<SecurityContext>>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Response, OagwError> {
    let caller = caller_of(security)?;
    let plugin = service.get_plugin(&caller, path_id("id", id)?)?;

    Ok(respond(Json(PluginDto::from(&plugin))))
}

/// `DELETE /oagw/v1/plugins/{id}` — delete a plugin (204).
///
/// A plugin still referenced by any upstream or route of any tenant is a 409
/// `PluginInUse` carrying the referencing GTS identifiers (ADR-0001
/// Appendix A): the gear never leaves the data plane pointing at a plugin that
/// no longer exists.
///
/// # Errors
/// Propagates [`OagwError`] rendered as a problem+json response.
pub async fn delete_plugin(
    Extension(service): Extension<Arc<ControlPlaneService>>,
    security: Option<Extension<SecurityContext>>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Response, OagwError> {
    let caller = caller_of(security)?;
    service.delete_plugin(&caller, path_id("id", id)?)?;

    Ok(respond(StatusCode::NO_CONTENT))
}

/// `GET /oagw/v1/plugins/{id}/source` — the plugin's Starlark source verbatim.
///
/// The body is the exact source the operator registered, so the media type is
/// the plain-text one and not JSON: `text/plain; charset=utf-8` (the charset is
/// explicit because Starlark source is UTF-8 and the management API serves the
/// body to tooling that may not negotiate a charset).
///
/// # Errors
/// Propagates [`OagwError`] rendered as a problem+json response.
pub async fn get_plugin_source(
    Extension(service): Extension<Arc<ControlPlaneService>>,
    security: Option<Extension<SecurityContext>>,
    id: Result<Path<String>, PathRejection>,
) -> Result<Response, OagwError> {
    let caller = caller_of(security)?;
    let source = service.get_plugin_source(&caller, path_id("id", id)?)?;

    Ok(respond((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        source,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal valid upstream body.
    const BODY: &str = r#"{
        "server": { "endpoints": [ { "scheme": "https", "host": "api.example.com" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
    }"#;

    #[test]
    fn parse_json_body_accepts_json_media_types() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json; charset=utf-8"),
        );
        let parsed =
            parse_json_body(&headers, &Bytes::from_static(BODY.as_bytes())).expect("parses");

        assert!(parsed.enabled);
        assert_eq!(parsed.server.endpoints.len(), 1);
    }

    #[test]
    fn parse_json_body_rejects_other_media_types() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("text/plain"),
        );
        let parsed = parse_json_body(&headers, &Bytes::from_static(b"x"));

        let err = parsed.expect_err("unsupported media type");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert!(err.detail().contains("text/plain"));
    }

    #[test]
    fn parse_json_body_rejects_empty_and_malformed_bodies() {
        let headers = HeaderMap::new();

        let err = parse_json_body(&headers, &Bytes::new()).expect_err("empty body");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);

        let err = parse_json_body(&headers, &Bytes::from_static(b"{")).expect_err("malformed body");
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn parse_json_body_rejects_unknown_members() {
        let headers = HeaderMap::new();
        let body = r#"{
            "server": { "endpoints": [ { "scheme": "https", "host": "a.example.com" } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "id": "00000000-0000-0000-0000-000000000001"
        }"#;

        let err = parse_json_body(&headers, &Bytes::from_static(body.as_bytes()))
            .expect_err("unknown id");

        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn parse_json_body_accepts_a_body_under_the_limit() {
        let headers = HeaderMap::new();

        assert!(
            parse_body_with_limit::<UpstreamRequest, UpstreamSpec>(
                &headers,
                &Bytes::from_static(BODY.as_bytes()),
                MAX_BODY_BYTES
            )
            .is_ok()
        );
    }

    #[test]
    fn callers_fail_closed_without_a_security_context() {
        let err = caller_of(None).expect_err("no extension");

        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(err.detail(), "authentication required");
    }

    #[test]
    fn callers_fail_closed_for_an_anonymous_context() {
        let anonymous = toolkit_security::SecurityContext::anonymous();

        let err = caller_of(Some(Extension(anonymous))).expect_err("anonymous caller");
        assert_eq!(err.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn callers_resolve_the_authenticated_tenant() {
        let tenant = Uuid::new_v4();
        let context = toolkit_security::SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(tenant)
            .build()
            .expect("security context builds");

        let caller = caller_of(Some(Extension(context))).expect("authenticated");

        assert_eq!(caller.tenant_id(), Some(tenant));
        assert_ne!(caller.subject_id(), Uuid::nil());
    }

    #[test]
    fn path_ids_accept_both_spellings() {
        let id = Uuid::new_v4();
        let bare = Path(id.to_string());
        let gts = Path(format!("gts.cf.core.oagw.upstream.v1~{id}"));

        assert_eq!(path_id("id", Ok(bare)).expect("bare"), id);
        assert_eq!(path_id("id", Ok(gts)).expect("gts form"), id);
    }

    #[test]
    fn path_ids_reject_malformed_values() {
        let err = path_id(
            "id",
            Ok(Path("gts.cf.core.oagw.upstream.v1~nope".to_owned())),
        )
        .expect_err("malformed id");

        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
    }
}
