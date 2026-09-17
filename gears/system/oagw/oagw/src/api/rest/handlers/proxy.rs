//! Data-plane proxy endpoints.
//!
//! Two routes cover the whole method/path space:
//!
//! * `ANY /oagw/v1/proxy/{alias}`
//! * `ANY /oagw/v1/proxy/{alias}/*path`
//!
//! Both are registered with `OperationBuilder::method_router` because the
//! builder otherwise picks the method-specific axum router.

use axum::{Extension, extract::{Path, Request}, response::{IntoResponse, Response}};
use form_urlencoded::parse;
use uuid::Uuid;

use crate::api::rest::error::ApiError;
use crate::api::rest::extractors::{ApiState, Tenant, client_ip};
use crate::domain::error::DomainError;
use crate::infra::proxy::ProxyRequest;

/// `ANY /oagw/v1/proxy/{alias}`
pub async fn handle_root(
    Extension(state): Extension<ApiState>,
    Tenant(tenant): Tenant,
    Path(alias): Path<String>,
    request: Request,
) -> Response {
    handle(state, tenant, alias, String::from("/"), request).await
}

/// `ANY /oagw/v1/proxy/{alias}/*path`
pub async fn handle_suffix(
    Extension(state): Extension<ApiState>,
    Tenant(tenant): Tenant,
    Path((alias, path)): Path<(String, String)>,
    request: Request,
) -> Response {
    let target = if path.starts_with('/') {
        path
    } else {
        format!("/{path}")
    };
    handle(state, tenant, alias, target, request).await
}

/// Shared pipeline for both proxy routes.
async fn handle(
    state: ApiState,
    tenant: Uuid,
    alias: String,
    path: String,
    request: Request,
) -> Response {
    let (mut parts, body) = request.into_parts();
    let method = parts.method.clone();
    // `OnUpgrade` is taken, not cloned: it is a one-shot handle for the socket.
    let on_upgrade = parts.extensions.remove::<hyper::upgrade::OnUpgrade>();
    let upgrade_requested = is_upgrade(&parts.headers) || on_upgrade.is_some();

    // A CORS preflight is answered before any route matching: the gateway
    // middleware serves it with an anonymous context (browsers never send
    // credentials on a preflight), so neither the tenant nor a route is known.
    if is_preflight(method.as_str(), &parts.headers) {
        return match preflight(&state, &alias, &parts.headers).await {
            Ok(response) => response,
            Err(err) => ApiError::from(err).into_response(),
        };
    }

    let body_limit = state.config.max_request_body_bytes;
    let content_length = parts
        .headers
        .get(http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if let Some(length) = content_length
        && length > body_limit
    {
        return ApiError::from(DomainError::PayloadTooLarge).into_response();
    }

    let buffered = match axum::body::to_bytes(body, body_limit).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::debug!(error = %err, "proxy request body could not be read");
            return ApiError::from(DomainError::PayloadTooLarge).into_response();
        }
    };

    let query = parse(parts.uri.query().unwrap_or_default().as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<(String, String)>>();
    let security_context = parts
        .extensions
        .get::<toolkit_security::SecurityContext>()
        .cloned();
    let proxy_request = ProxyRequest {
        tenant_id: tenant,
        alias,
        method,
        path,
        query,
        headers: parts.headers.clone(),
        body: buffered,
        client_ip: client_ip(
            &parts.headers,
            parts
                .extensions
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|info| info.0),
        ),
        subject: security_context.as_ref().map(|ctx| ctx.subject_id().to_string()),
        security_context,
        upgrade: upgrade_requested,
    };

    let result = match on_upgrade {
        Some(on_upgrade) if upgrade_requested => {
            state
                .proxy
                .execute_with_upgrade(proxy_request, on_upgrade)
                .await
        }
        _ => state.proxy.execute(proxy_request).await,
    };
    match result {
        Ok(response) => response,
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Answer a CORS preflight locally.
///
/// The caller's tenant is not known here (the edge middleware answers a
/// preflight with an anonymous security context), so the alias is resolved
/// without a tenant scope; only the allow/deny decision is exposed.
async fn preflight(
    state: &ApiState,
    alias: &str,
    headers: &http::HeaderMap,
) -> Result<Response, DomainError> {
    let upstream = state.upstreams.resolve_for_preflight(alias).await?;
    let Some(cors) = upstream.config.cors.as_ref().filter(|c| c.enabled) else {
        return Err(DomainError::UpstreamNotFound);
    };
    let origin = headers
        .get(http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !crate::infra::proxy::origin_allowed(cors, origin) {
        return Err(DomainError::CorsRejected("origin not allowed"));
    }
    let request_method = headers
        .get("access-control-request-method")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !crate::infra::proxy::method_allowed(cors, request_method) {
        return Err(DomainError::CorsRejected("method not allowed"));
    }

    let mut response = Response::new(axum::body::Body::empty());
    *response.status_mut() = http::StatusCode::NO_CONTENT;
    let out = response.headers_mut();
    insert(out, http::header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    insert(
        out,
        http::header::ACCESS_CONTROL_ALLOW_METHODS,
        &cors.allowed_methods.join(", "),
    );
    if let Some(requested) = headers
        .get("access-control-request-headers")
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.trim().is_empty())
    {
        insert(out, http::header::ACCESS_CONTROL_ALLOW_HEADERS, requested);
    } else if !cors.allowed_origins.is_empty() {
        insert(out, http::header::ACCESS_CONTROL_ALLOW_HEADERS, "content-type, accept");
    }
    if cors.allow_credentials {
        insert(out, http::header::ACCESS_CONTROL_ALLOW_CREDENTIALS, "true");
    }
    insert(out, http::header::ACCESS_CONTROL_MAX_AGE, "86400");
    out.append(
        http::header::VARY,
        http::HeaderValue::from_static("Origin"),
    );
    out.append(
        http::header::VARY,
        http::HeaderValue::from_static("Access-Control-Request-Method"),
    );
    out.append(
        http::header::VARY,
        http::HeaderValue::from_static("Access-Control-Request-Headers"),
    );
    Ok(response)
}

fn is_upgrade(headers: &http::HeaderMap) -> bool {
    headers
        .get(http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase().split(',').any(|t| t.trim() == "upgrade"))
        .unwrap_or(false)
}

/// `true` for a CORS preflight: `OPTIONS` carrying both `Origin` and
/// `Access-Control-Request-Method` (what the fetch spec calls a preflight).
fn is_preflight(method: &str, headers: &http::HeaderMap) -> bool {
    method == http::Method::OPTIONS.as_str()
        && headers.contains_key(http::header::ORIGIN)
        && headers.contains_key("access-control-request-method")
}

fn insert(headers: &mut http::HeaderMap, name: http::HeaderName, value: &str) {
    if let Ok(v) = http::HeaderValue::from_str(value) {
        headers.insert(name, v);
    }
}
