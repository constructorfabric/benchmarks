//! The data plane: `{METHOD} /oagw/v1/proxy/{alias}[/{path}]`.
//!
//! The handlers are thin: they buffer the inbound request into a
//! [`ProxiedRequest`], keep hold of the upgrade handle for `WebSocket` calls,
//! and hand the result to the [`ProxyEngine`].

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, RawQuery, Request};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse as _;
use axum::response::Response;
use http_body_util::BodyExt as _;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::ResponseSpec;
use toolkit::api::operation_builder::OperationBuilder;
use toolkit_security::SecurityContext;

use crate::api::error::OagwError;
use crate::api::rest::state::OagwState;
use crate::config::MAX_BODY_BYTES;
use crate::infra::proxy::{ProxiedRequest, header_pairs};

/// Registers the two proxy paths, both accepting every `HTTP` method.
pub fn register(router: axum::Router, openapi: &dyn OpenApiRegistry) -> axum::Router {
    let router = register_proxy_operation(
        router,
        openapi,
        "/oagw/v1/proxy/{alias}",
        "oagw.proxy_root",
        "Proxy a request to an upstream",
        "Resolves `{alias}`, matches a route and forwards the request.",
        axum::routing::any(proxy_root),
    );
    register_proxy_operation(
        router,
        openapi,
        "/oagw/v1/proxy/{alias}/{*path}",
        "oagw.proxy_path",
        "Proxy a request with a path suffix",
        "Resolves `{alias}` and forwards the request with its path suffix appended.",
        axum::routing::any(proxy_suffix),
    )
}

/// Registers one proxy operation documented as `GET` but served by `any`.
#[allow(clippy::too_many_lines)]
fn register_proxy_operation(
    router: axum::Router,
    openapi: &dyn OpenApiRegistry,
    path: &'static str,
    operation_id: &'static str,
    summary: &'static str,
    description: &'static str,
    method_router: axum::routing::MethodRouter<()>,
) -> axum::Router {
    OperationBuilder::get(path)
        .operation_id(operation_id)
        .summary(summary)
        .description(description)
        .tag("OAGW Proxy")
        .authenticated()
        .no_license_required()
        .path_param("alias", "Upstream alias")
        .method_router(method_router)
        .response(ResponseSpec {
            status: StatusCode::PAYLOAD_TOO_LARGE.as_u16(),
            content_type: "application/problem+json",
            description: "Request body too large".to_owned(),
            schema: None,
        })
        .json_response(StatusCode::OK, "Upstream response")
        .error_400(openapi)
        .error_404(openapi)
        .error_429(openapi)
        .error_502(openapi)
        .error_503(openapi)
        .error_504(openapi)
        .register(router, openapi)
}

/// `OPTIONS /oagw/v1/proxy/{alias}` — answered locally when it is a preflight.
async fn proxy_root(
    axum::Extension(ctx): axum::Extension<SecurityContext>,
    axum::Extension(state): axum::Extension<Arc<OagwState>>,
    Path(alias): Path<String>,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    proxy_call(&ctx, &state, alias, String::new(), query, request).await
}

/// `{METHOD} /oagw/v1/proxy/{alias}/{*path}`.
async fn proxy_suffix(
    axum::Extension(ctx): axum::Extension<SecurityContext>,
    axum::Extension(state): axum::Extension<Arc<OagwState>>,
    Path((alias, path)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    request: Request,
) -> Response {
    let suffix = if path.starts_with('/') {
        path
    } else {
        format!("/{path}")
    };
    proxy_call(&ctx, &state, alias, suffix, query, request).await
}

/// Buffers the request and delegates to the proxy engine.
async fn proxy_call(
    ctx: &SecurityContext,
    state: &Arc<OagwState>,
    alias: String,
    path_suffix: String,
    query: Option<String>,
    request: Request,
) -> Response {
    let (mut parts, body) = request.into_parts();
    let upgrade = parts.extensions.remove::<hyper::upgrade::OnUpgrade>();

    if is_preflight(&parts.headers) {
        return preflight_response(&parts.headers);
    }

    let buffered = match collect_body(body).await {
        Ok(bytes) => bytes,
        Err(error) => return error.into_response(),
    };

    let proxied = ProxiedRequest {
        alias,
        method: parts.method.clone(),
        path_suffix,
        query: query_pairs(query.as_deref()),
        headers: header_pairs(&parts.headers),
        body: buffered,
        client_ip: client_ip(&parts.headers),
        is_websocket: is_websocket(&parts.headers),
    };

    state
        .engine
        .handle(ctx, proxied, upgrade)
        .await
        .into_response()
}

/// Builds the `204` preflight answer echoing the requested origin, method and
/// headers.
fn preflight_response(headers: &HeaderMap) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    let out = response.headers_mut();
    for (name, value) in [
        (
            axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
            headers.get("origin"),
        ),
        (
            axum::http::header::ACCESS_CONTROL_ALLOW_METHODS,
            headers.get(PREFLIGHT_METHOD),
        ),
        (
            axum::http::header::ACCESS_CONTROL_ALLOW_HEADERS,
            headers.get("access-control-request-headers"),
        ),
    ] {
        if let Some(raw) = value
            && let Ok(header) = axum::http::HeaderValue::from_bytes(raw.as_bytes())
        {
            out.insert(name, header);
        }
    }
    out.insert(
        axum::http::header::ACCESS_CONTROL_MAX_AGE,
        axum::http::HeaderValue::from_static("600"),
    );
    out.append(
        axum::http::header::VARY,
        axum::http::HeaderValue::from_static(
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        ),
    );
    response
}

/// The preflight method marker header.
const PREFLIGHT_METHOD: &str = "access-control-request-method";

/// True when the request is a `CORS` preflight.
fn is_preflight(headers: &HeaderMap) -> bool {
    headers.contains_key("origin") && headers.contains_key(PREFLIGHT_METHOD)
}

/// True when the request asks for a `WebSocket` upgrade.
fn is_websocket(headers: &HeaderMap) -> bool {
    headers
        .get("upgrade")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

/// The address used for `ip`-scoped rate limits, from `X-Forwarded-For`.
fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map_or_else(String::new, |value| value.trim().to_owned())
}

/// Splits a raw query string into decoded pairs.
fn query_pairs(query: Option<&str>) -> Vec<(String, String)> {
    form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// Buffers the inbound body, rejecting anything over the documented limit.
async fn collect_body(body: Body) -> Result<bytes::Bytes, OagwError> {
    let mut body = body;
    let mut collected: Vec<u8> = Vec::new();
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|error| {
            OagwError::validation(format!("failed to read the request body: {error}"))
        })?;
        if let Some(data) = frame.data_ref() {
            if collected.len() + data.len() > MAX_BODY_BYTES {
                return Err(OagwError::payload_too_large(format!(
                    "the request body exceeds the {MAX_BODY_BYTES} byte limit"
                )));
            }
            collected.extend_from_slice(data);
        }
    }
    Ok(bytes::Bytes::from(collected))
}
