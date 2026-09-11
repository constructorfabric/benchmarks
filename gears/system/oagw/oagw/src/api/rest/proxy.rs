//! Proxy API handler (Data Plane transport).
//!
//! `{METHOD} /oagw/v1/proxy/{alias}[/{path_suffix}][?{query}]`
//!
//! The handler owns three shapes of answer: a streamed response, a buffered
//! one, and a `101` that hands the connection over to a byte splice.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::{ConnectInfo, Extension, Path, Request};
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use hyper_util::rt::TokioIo;
use toolkit_security::SecurityContext;
use tracing::{debug, warn};

use crate::domain::error::{ErrorKind, OagwError};
use crate::domain::gts;
use crate::infra::proxy::service::{IncomingRequest, ProxyOutcome, audit_log};
use crate::infra::proxy::websocket;

use super::error::{mark_gateway, mark_upstream, problem_response};
use super::state::{OagwState, actions};

/// How long a browser may cache a preflight result.
const PREFLIGHT_MAX_AGE: &str = "86400";

/// `{METHOD} /oagw/v1/proxy/{alias}[/{*path}]`
pub async fn proxy(
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<OagwState>>,
    Path(params): Path<HashMap<String, String>>,
    request: Request,
) -> Response {
    let started = Instant::now();
    let (mut parts, body) = request.into_parts();
    let instance = parts
        .uri
        .path_and_query()
        .map_or_else(|| parts.uri.path().to_owned(), ToString::to_string);

    // A browser preflight carries no credentials, so there is no tenant to
    // resolve an upstream with; answer permissively here and enforce the
    // origin on the actual request (ADR 0004).
    if is_preflight(&parts.method, &parts.headers) {
        return preflight_response(&parts.headers);
    }

    let alias = params.get("alias").cloned().unwrap_or_default();
    let path_suffix = params
        .get("path")
        .map(|suffix| format!("/{}", suffix.trim_start_matches('/')))
        .unwrap_or_default();

    if let Err(error) = state
        .authorize(&ctx, gts::PROXY_BASE, actions::INVOKE)
        .await
    {
        return problem_response(&error, Some(&instance));
    }

    let wants_upgrade = websocket::is_upgrade_request(&parts.method, &parts.headers);
    let on_upgrade = parts.extensions.remove::<hyper::upgrade::OnUpgrade>();
    if wants_upgrade && on_upgrade.is_none() {
        return problem_response(
            &OagwError::new(
                ErrorKind::ProtocolError,
                "the inbound connection cannot be upgraded",
            ),
            Some(&instance),
        );
    }

    let payload = if wants_upgrade {
        Bytes::new()
    } else {
        match axum::body::to_bytes(body, state.config.max_request_body_bytes).await {
            Ok(bytes) => bytes,
            Err(_) => {
                return problem_response(
                    &OagwError::new(
                        ErrorKind::PayloadTooLarge,
                        format!(
                            "request body exceeds the {} byte limit",
                            state.config.max_request_body_bytes
                        ),
                    ),
                    Some(&instance),
                );
            }
        }
    };

    let query: Vec<(String, String)> = parts
        .uri
        .query()
        .map(|raw| {
            form_urlencoded::parse(raw.as_bytes())
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect()
        })
        .unwrap_or_default();

    let incoming = IncomingRequest {
        method: parts.method.clone(),
        alias: alias.clone(),
        path_suffix,
        query,
        headers: parts.headers.clone(),
        body: payload,
        client_ip: client_ip(&parts),
        instance: instance.clone(),
        wants_upgrade,
    };
    let method = parts.method.clone();
    let request_id = request_id(&parts.headers);

    let outcome = state.data_plane.execute(&ctx, incoming).await;
    let duration_ms = started.elapsed().as_millis();

    match outcome {
        Ok(ProxyOutcome::Streamed { head, body }) => {
            audit_log(
                &request_id,
                ctx.subject_tenant_id(),
                ctx.subject_id(),
                &alias,
                instance.as_str(),
                method.as_str(),
                head.status.as_u16(),
                duration_ms,
                None,
            );
            relay(head.status, head.headers, Body::from_stream(body))
        }
        Ok(ProxyOutcome::Buffered { head, body }) => {
            audit_log(
                &request_id,
                ctx.subject_tenant_id(),
                ctx.subject_id(),
                &alias,
                instance.as_str(),
                method.as_str(),
                head.status.as_u16(),
                duration_ms,
                None,
            );
            relay(head.status, head.headers, Body::from(body))
        }
        Ok(ProxyOutcome::Upgraded {
            headers,
            stream,
            leftover,
        }) => {
            let Some(on_upgrade) = on_upgrade else {
                return problem_response(
                    &OagwError::new(
                        ErrorKind::ProtocolError,
                        "the inbound connection cannot be upgraded",
                    ),
                    Some(&instance),
                );
            };
            tokio::spawn(async move {
                match on_upgrade.await {
                    Ok(upgraded) => {
                        let io = TokioIo::new(upgraded);
                        match websocket::splice(io, stream, leftover).await {
                            Ok((from_client, from_upstream)) => debug!(
                                target: "oagw.upgrade",
                                from_client,
                                from_upstream,
                                "upgraded connection closed"
                            ),
                            Err(err) => debug!(
                                target: "oagw.upgrade",
                                error = %err,
                                "upgraded connection ended"
                            ),
                        }
                    }
                    Err(err) => warn!(
                        target: "oagw.upgrade",
                        error = %err,
                        "client connection could not be upgraded"
                    ),
                }
            });
            switching_protocols(headers)
        }
        Err(error) => {
            audit_log(
                &request_id,
                ctx.subject_tenant_id(),
                ctx.subject_id(),
                &alias,
                instance.as_str(),
                method.as_str(),
                error.status(),
                duration_ms,
                Some(error.kind.gts_type()),
            );
            problem_response(&error, Some(&instance))
        }
    }
}

/// Build the client-facing response for a relayed upstream answer.
fn relay(status: StatusCode, headers: HeaderMap, body: Body) -> Response {
    let mut response = Response::new(body);
    *response.status_mut() = status;
    for (name, value) in &headers {
        response.headers_mut().append(name.clone(), value.clone());
    }
    mark_upstream(&mut response);
    response
}

/// Build the `101` that hands the connection to the splice task.
fn switching_protocols(headers: HeaderMap) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SWITCHING_PROTOCOLS;
    for (name, value) in &headers {
        response.headers_mut().append(name.clone(), value.clone());
    }
    // hyper completes the upgrade only when the response itself asks for one.
    if !response.headers().contains_key(header::CONNECTION) {
        response
            .headers_mut()
            .insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
    }
    if !response.headers().contains_key(header::UPGRADE) {
        response
            .headers_mut()
            .insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    }
    mark_upstream(&mut response);
    response
}

/// A CORS preflight is `OPTIONS` plus `Origin` plus
/// `Access-Control-Request-Method` (WHATWG Fetch).
#[must_use]
pub fn is_preflight(method: &Method, headers: &HeaderMap) -> bool {
    method == Method::OPTIONS
        && headers.contains_key(header::ORIGIN)
        && headers.contains_key(header::ACCESS_CONTROL_REQUEST_METHOD)
}

/// Permissive `204` echoing the requested origin, method and headers.
#[must_use]
pub fn preflight_response(headers: &HeaderMap) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let out = response.headers_mut();

    if let Some(origin) = headers.get(header::ORIGIN) {
        out.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin.clone());
    }
    if let Some(method) = headers.get(header::ACCESS_CONTROL_REQUEST_METHOD) {
        out.insert(header::ACCESS_CONTROL_ALLOW_METHODS, method.clone());
    }
    if let Some(request_headers) = headers.get(header::ACCESS_CONTROL_REQUEST_HEADERS) {
        out.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, request_headers.clone());
    }
    out.insert(
        header::ACCESS_CONTROL_MAX_AGE,
        HeaderValue::from_static(PREFLIGHT_MAX_AGE),
    );
    // Vary on everything the answer depends on, or a shared cache will serve
    // one origin's preflight to another.
    out.insert(
        header::VARY,
        HeaderValue::from_static(
            "Origin, Access-Control-Request-Method, Access-Control-Request-Headers",
        ),
    );
    mark_gateway(&mut response);
    response
}

/// Correlation id for the audit log: the caller's, or a fresh one.
fn request_id(headers: &HeaderMap) -> String {
    const CANDIDATES: [&str; 2] = ["x-request-id", "x-correlation-id"];
    for name in CANDIDATES {
        if let Ok(name) = HeaderName::try_from(name)
            && let Some(value) = headers.get(&name).and_then(|value| value.to_str().ok())
        {
            return value.to_owned();
        }
    }
    uuid::Uuid::new_v4().to_string()
}

/// Client address for `scope: ip` rate limiting.
fn client_ip(parts: &http::request::Parts) -> Option<String> {
    if let Some(forwarded) = parts
        .headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        && let Some(first) = forwarded.split(',').next()
    {
        let first = first.trim();
        if !first.is_empty() {
            return Some(first.to_owned());
        }
    }
    parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip().to_string())
}

#[cfg(test)]
#[path = "proxy_tests.rs"]
mod tests;
