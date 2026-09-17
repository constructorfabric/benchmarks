//! The one handler behind `/oagw/v1/proxy/{*proxy_path}`.
//!
//! The transport layer owns exactly three concerns:
//!
//! 1. translating the axum request into a [`ProxyRequest`] — the alias and the
//!    path come from the request URI, never from a decoded path parameter, so
//!    an encoded `/` inside a path segment reaches the upstream intact;
//! 2. handing the data plane's [`ProxyResponse`] back to axum, streamed;
//! 3. bridging a WebSocket upgrade socket to socket.
//!
//! Everything else — alias resolution, route matching, plugins, rate limiting,
//! CORS, SSRF screening, dialing — is [`DataPlane::execute`]'s job.
use axum::{
    Extension,
    body::Body,
    extract::Request,
    http::{Method, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use hyper_util::rt::TokioIo;
use toolkit_security::SecurityContext;

use crate::infra::proxy::cors as proxy_cors;
use crate::infra::proxy::failure::ProxyFailure;
use crate::infra::proxy::forward;
use crate::infra::proxy::{ErrorSource, ProxyRequest, ProxyResponse, TARGET_HOST_HEADER};

use super::SharedDataPlane;
use super::error::ProxyError;

/// Path prefix the proxy endpoint is mounted at.
pub const PROXY_PREFIX: &str = "/oagw/v1/proxy/";
/// The axum route path of the proxy endpoint.
pub const PROXY_PATH: &str = "/oagw/v1/proxy/{*proxy_path}";

/// The proxy accepts every method; the data plane forwards it verbatim.
#[must_use]
pub fn proxied_methods() -> Vec<Method> {
    vec![
        Method::GET,
        Method::POST,
        Method::PUT,
        Method::DELETE,
        Method::PATCH,
        Method::HEAD,
        Method::OPTIONS,
    ]
}

/// `ANY /oagw/v1/proxy/{*proxy_path}` — the whole proxy hop.
///
/// # Errors
///
/// [`ProxyError`] for every gateway-side refusal, rendered as an RFC 9457
/// problem document carrying `X-OAGW-Error-Source: gateway`.
pub async fn proxy(
    Extension(context): Extension<SecurityContext>,
    Extension(plane): Extension<SharedDataPlane>,
    request: Request<Body>,
) -> Result<Response, ProxyError> {
    // Subscribing to an upgrade *removes* the `OnUpgrade` extension from the
    // request, so this has to happen before the request is taken apart.
    let mut request = request;
    let wants_upgrade = forward::is_upgrade_request(request.headers(), request.method());
    let inbound_upgrade = wants_upgrade.then(|| hyper::upgrade::on(&mut request));
    let (parts, body) = request.into_parts();

    let (alias, path) = split_path(parts.uri.path()).map_err(ProxyError::new)?;
    let target_host = parts
        .headers
        .get(TARGET_HOST_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let proxy_request = ProxyRequest {
        tenant_id: context.subject_tenant_id(),
        subject_id: context.subject_id().to_string(),
        subject_tenant_id: context.subject_tenant_id().to_string(),
        alias,
        path,
        query: parts.uri.query().map(str::to_owned),
        method: parts.method.as_str().to_ascii_uppercase(),
        headers: parts.headers.clone(),
        body: Some(body),
        upgrade: wants_upgrade,
        target_host,
        cors: proxy_cors::CorsRequest {
            method: parts.method.as_str().to_ascii_uppercase(),
            origin: header_value(&parts.headers, proxy_cors::ORIGIN),
            request_method: parts
                .headers
                .get(proxy_cors::REQUEST_METHOD)
                .and_then(|value| value.to_str().ok())
                .map(|value| value.trim().to_ascii_uppercase())
                .filter(|value| !value.is_empty()),
            request_headers: header_value(&parts.headers, proxy_cors::REQUEST_HEADERS),
        },
    };

    let response = plane
        .execute(proxy_request)
        .await
        .map_err(ProxyError::new)?;
    Ok(respond(response, inbound_upgrade))
}

/// The first `str` value of a header, trimmed and empty-filtered.
fn header_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Split `/oagw/v1/proxy/{alias}[/{path}]` into its two pieces.
///
/// An empty remainder means the alias itself is missing.
///
/// # Errors
///
/// [`ProxyFailure::validation`] when the request is not addressed at the proxy
/// prefix or carries no alias.
// The failure carries its own status, title, detail and header set, which puts
// it past clippy's `result_large_err` threshold; boxing it would trade a
// warning for an allocation on every refused request.
#[allow(clippy::result_large_err)]
pub fn split_path(raw: &str) -> Result<(String, String), ProxyFailure> {
    let Some(remainder) = raw.strip_prefix(PROXY_PREFIX) else {
        return Err(ProxyFailure::validation(format!(
            "proxy requests must be addressed as {PROXY_PREFIX}{{alias}}[/{{path}}]"
        )));
    };
    if remainder.is_empty() {
        return Err(ProxyFailure::validation(
            "the proxy endpoint needs an upstream alias after the prefix",
        ));
    }
    match remainder.split_once('/') {
        Some((alias, path)) => Ok((alias.to_owned(), format!("/{path}"))),
        None => Ok((remainder.to_owned(), "/".to_owned())),
    }
}

/// Render a data-plane response for the wire and hand any upgrade over.
fn respond(response: ProxyResponse, inbound: Option<hyper::upgrade::OnUpgrade>) -> Response {
    let status = StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut builder = Response::builder().status(status);
    for (name, value) in response.headers.iter() {
        builder = builder.header(name, value.clone());
    }
    let mut rendered = match builder.body(response.body) {
        Ok(response) => response,
        Err(error) => {
            return ProxyError::new(ProxyFailure::internal(format!(
                "cannot render the proxied response: {error}"
            )))
            .into_response();
        }
    };

    if response.source == ErrorSource::Upstream {
        rendered.headers_mut().insert(
            crate::api::rest::error::ERROR_SOURCE_HEADER,
            axum::http::HeaderValue::from_static(ErrorSource::Upstream.as_str()),
        );
    }
    if let (Some(upstream), Some(inbound)) = (response.upgraded, inbound) {
        bridge(inbound, upstream);
    }
    rendered
}

/// Bridge the caller's upgraded socket to the upstream's, both ways.
///
/// The task outlives the hop on purpose: a WebSocket is a long-lived
/// connection and the gateway must not be the one closing it.
fn bridge(inbound: hyper::upgrade::OnUpgrade, upstream: hyper::upgrade::Upgraded) {
    tokio::spawn(async move {
        let socket = match inbound.await {
            Ok(socket) => TokioIo::new(socket),
            Err(error) => {
                tracing::debug!(error = %error, "oagw: the caller did not complete its upgrade");
                return;
            }
        };
        let inbound = socket;
        let upstream = TokioIo::new(upstream);
        tokio::pin!(inbound, upstream);
        if let Err(error) = tokio::io::copy_bidirectional(&mut inbound, &mut upstream).await {
            tracing::debug!(error = %error, "oagw: upgraded connection closed");
        }
    });
}

/// Path and query of an inbound URI, percent-encoding preserved.
#[must_use]
pub fn uri_parts(uri: &Uri) -> (&str, Option<&str>) {
    (uri.path(), uri.query())
}

#[cfg(test)]
#[path = "handler_tests.rs"]
mod tests;
