// Created: 2026-08-29 by Constructor Tech
//! Data-plane REST handlers (DESIGN §3.5 proxy flow).
//!
//! Paths are gear-relative: the gear mounts `/oagw/v1/proxy/{*tail}`, which the
//! server serves at `/api/oagw/v1/proxy/...` when the gear prefix is `/api`.
//! CORS preflights are answered by the gateway itself (ADR-0004); plain
//! requests, event streams and WebSocket upgrades all funnel through
//! [`crate::infra::proxy::ProxyEngine`].

use std::sync::Arc;

use axum::Extension;
use axum::body::{Body, HttpBody};
use axum::extract::ws::{WebSocketUpgrade, rejection::WebSocketUpgradeRejection};
use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::{HeaderMap, Method, StatusCode, Uri, request::Parts};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use toolkit_security::SecurityContext;

use crate::api::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_UPSTREAM, OagwError};
use crate::config::OagwConfig;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::model::HttpMethod;
use crate::domain::services::proxy::{ProxyService, split_proxy_path};
use crate::infra::audit;
use crate::infra::cors;
use crate::infra::hierarchy::FlatTenantHierarchy;
use crate::infra::metrics::SharedMetrics;
use crate::infra::proxy::ProxyEngine;
use crate::infra::ratelimit::RateLimiter;
use crate::infra::storage::InMemoryStore;
use uuid::Uuid;

/// The concrete proxy service the gear wires.
pub type Proxy = Arc<ProxyService<InMemoryStore, InMemoryStore, FlatTenantHierarchy, RateLimiter>>;

/// Everything one proxy request needs, layered onto the router as extensions.
#[derive(Clone)]
pub struct ProxyStack {
    /// Resolution service.
    pub service: Proxy,
    /// Outbound engine.
    pub engine: Arc<ProxyEngine>,
    /// Gear configuration.
    pub config: Arc<OagwConfig>,
    /// Data-plane metrics.
    pub metrics: SharedMetrics,
}

/// `true` when the request method is a CORS preflight.
#[must_use]
pub fn is_preflight(method: &Method) -> bool {
    method == Method::OPTIONS
}

/// `true` when the exchange is a CORS preflight in the ADR-0004 sense.
///
/// Both markers are required: an `OPTIONS` without an `Origin` and a requested
/// method is not a browser preflight and is not answered from the permissive
/// path.
#[must_use]
pub fn is_cors_preflight(method: &Method, headers: &HeaderMap) -> bool {
    is_preflight(method)
        && headers.contains_key(cors::ORIGIN)
        && headers.contains_key(cors::REQUEST_METHOD)
}

/// Reads the preflight markers the browser sent.
fn preflight_request(headers: &HeaderMap) -> (Option<&str>, Option<&str>) {
    let method = headers
        .get(cors::REQUEST_METHOD)
        .and_then(|value| value.to_str().ok());
    let requested = headers
        .get(cors::REQUEST_HEADERS)
        .and_then(|value| value.to_str().ok());
    (method, requested)
}

/// Gear-relative prefix every proxy path starts with.
pub const PROXY_PREFIX: &str = "/oagw/v1/proxy";

/// Parses the request path into the upstream alias and the proxy tail.
///
/// `/oagw/v1/proxy/api.openai.com:8080/v1/chat` →
/// `("api.openai.com:8080", "/v1/chat")`. The bare `/proxy/...` form is
/// accepted too so standalone gear mounts behave identically.
#[must_use]
pub fn proxy_target(path: &str) -> Option<(String, String)> {
    let rest = path
        .strip_prefix(PROXY_PREFIX)
        .or_else(|| path.strip_prefix("/proxy/"))?;
    let (alias, tail) = split_proxy_path(rest)?;
    Some((alias, format!("/{tail}")))
}

/// Maps the request method onto the domain verb the route matcher understands.
///
/// `OPTIONS` is handled by the CORS preflight path and never reaches routing.
#[must_use]
pub fn domain_method(method: &Method) -> Option<HttpMethod> {
    match *method {
        Method::GET => Some(HttpMethod::Get),
        Method::POST => Some(HttpMethod::Post),
        Method::PUT => Some(HttpMethod::Put),
        Method::DELETE => Some(HttpMethod::Delete),
        Method::PATCH => Some(HttpMethod::Patch),
        _ => None,
    }
}

/// The socket peer address the platform recorded, when the router provides one.
///
/// [`axum::extract::ConnectInfo`] has no `Option` extractor of its own, so the
/// optional form is read straight out of the request extensions — the same way
/// the platform's access log middleware reads it.
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub Option<std::net::SocketAddr>);

impl<S> FromRequestParts<S> for PeerAddr
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<std::net::SocketAddr>>()
                .map(|info| info.0),
        ))
    }
}

/// The client's IP, taken from the socket peer the platform recorded.
///
/// A forwarded header is never consulted: it is caller-controlled, and it is
/// the identity of a `scope: ip` rate-limit counter, so trusting it would let a
/// client mint a fresh bucket per request (or exhaust someone else's).
#[must_use]
pub fn peer_ip(peer: Option<std::net::SocketAddr>) -> String {
    peer.map(|peer| peer.ip().to_string())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Reads the buffered request body, enforcing the gear's hard limit.
///
/// # Errors
///
/// Returns `PayloadTooLarge` above `limit`.
pub async fn read_body(body: Body, limit: u64) -> DomainResult<Bytes> {
    let bytes = axum::body::to_bytes(body, usize::try_from(limit).unwrap_or(usize::MAX))
        .await
        .map_err(|_| DomainError::PayloadTooLarge { limit })?;
    Ok(bytes)
}

/// Answers a CORS preflight locally (ADR-0004 "Preflight Request Handling").
///
/// Browser preflights carry no credentials, so there is no tenant context to
/// resolve an upstream with: the gateway answers `204` by echoing the requested
/// origin, method and headers, and defers origin/method validation to the
/// actual request. No upstream resolution happens on this path.
#[must_use]
pub fn preflight(headers: &HeaderMap) -> Response {
    let origin = headers
        .get(cors::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let (request_method, request_headers) = preflight_request(headers);
    if origin.is_empty() {
        // No usable `Origin`: there is nothing to echo, and the browser treats
        // a response without `Access-Control-Allow-Origin` as a refusal.
        return (StatusCode::FORBIDDEN, "").into_response();
    }

    let mut response = (StatusCode::NO_CONTENT, "").into_response();
    for (name, value) in cors::permissive_preflight(origin, request_method, request_headers) {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::from_bytes(name.as_bytes()),
            axum::http::HeaderValue::from_str(&value),
        ) {
            response.headers_mut().insert(name, value);
        }
    }
    response.headers_mut().insert(
        axum::http::header::VARY,
        axum::http::HeaderValue::from_static(cors::VARY_VALUE),
    );
    response
}

/// The axum handler the gear mounts on `/oagw/v1/proxy/{*tail}`.
///
/// WebSocket upgrades and plain exchanges share the route: the upgrade
/// extractor succeeds only when the handshake headers are present, so a
/// rejection falls through to the buffered proxy path.
#[allow(clippy::too_many_arguments)] // the handler mirrors the HTTP exchange
pub async fn proxy_handler(
    Extension(stack): Extension<ProxyStack>,
    context: Option<Extension<SecurityContext>>,
    peer: PeerAddr,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
    body: Body,
) -> Response {
    let path = uri.path().to_owned();
    let query = uri.query().map(str::to_owned);
    let caller_ip = peer_ip(peer.0);
    let Ok((tenant_id, subject_id)) = identity(context.as_deref()) else {
        return OagwError::from(DomainError::Unauthorized(
            "the OAGW data plane requires an authenticated caller".to_owned(),
        ))
        .into_response();
    };
    let subject_id = subject_id.as_deref();

    if let Ok(upgrade) = upgrade {
        return match proxy_websocket(
            &stack,
            tenant_id,
            subject_id,
            &path,
            query.as_deref(),
            &caller_ip,
            &headers,
            upgrade,
        )
        .await
        {
            Ok(response) => response,
            Err(error) => rendered_error(error),
        };
    }

    proxy(
        &stack,
        tenant_id,
        subject_id,
        &method,
        &path,
        query.as_deref(),
        &caller_ip,
        &headers,
        body,
    )
    .await
}

/// Extracts the tenant and the authenticated subject from the platform context.
///
/// # Errors
///
/// [`DomainError::Unauthorized`] when the platform auth middleware is not
/// mounted at all: the data plane never serves an anonymous caller, and a
/// missing context can only mean the host wired the gear without it.
fn identity(context: Option<&SecurityContext>) -> DomainResult<(Uuid, Option<String>)> {
    let Some(context) = context else {
        return Err(DomainError::Unauthorized(
            "the OAGW data plane requires an authenticated caller".to_owned(),
        ));
    };
    Ok((
        context.subject_tenant_id(),
        Some(context.subject_id().to_string()),
    ))
}

/// The body length the request declared, which is the audit's request-size fact
/// even when the body itself is streamed or already drained by the handler.
#[must_use]
pub fn declared_body_size(headers: &HeaderMap) -> usize {
    headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0)
}

/// The response length the upstream declared, when the relay preserved it.
#[must_use]
fn declared_response_size(response: &Response) -> usize {
    response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0)
}

/// The main data-plane entry point.
#[allow(clippy::too_many_arguments)]
pub async fn proxy(
    stack: &ProxyStack,
    tenant_id: Uuid,
    subject_id: Option<&str>,
    method: &Method,
    path: &str,
    query: Option<&str>,
    caller_ip: &str,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    let started = std::time::Instant::now();
    let request_id = inbound_request_id(headers);
    let request_size = declared_body_size(headers);
    let Some(host) = proxy_host(path) else {
        return OagwError::from(DomainError::validation(
            "proxy path must name an upstream alias",
        ))
        .into_response();
    };
    let guard = stack.metrics.enter(&host);

    let outcome = if is_cors_preflight(method, headers) {
        // Answered locally, without upstream resolution or a tenant context
        // (ADR-0004): browser preflights carry no credentials.
        Ok(preflight(headers))
    } else if let Some(domain) = domain_method(method) {
        hop(
            stack, tenant_id, subject_id, method, &domain, path, query, caller_ip, headers, body,
        )
        .await
    } else {
        Err(DomainError::validation(format!(
            "method {method} is not proxied by OAGW"
        )))
    };

    drop(guard);
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let route = route_label(path);

    match outcome {
        Ok(response) => {
            let status = response.status().as_u16();
            // A streamed body has no size until it drains, so the response size
            // is recorded as the declared length when there is one and as the
            // buffered length otherwise.
            let response_size = usize::try_from(response.body().size_hint().exact().unwrap_or(0))
                .unwrap_or(0)
                .max(declared_response_size(&response));
            stack.metrics.record_request(
                &host,
                method.as_str(),
                &route,
                status,
                started.elapsed().as_secs_f64(),
            );
            audit::proxy_request(&audit::ProxyAudit {
                request_id: &request_id,
                tenant_id: &tenant_id.to_string(),
                principal_id: subject_id,
                host: &host,
                path,
                method: method.as_str(),
                status,
                duration_ms: elapsed_ms,
                request_size,
                response_size,
                error_type: None,
            });
            response
        }
        Err(error) => {
            // Gateway rejections are requests too: they belong in the
            // `oagw_requests_total` denominator alongside the upstream hops.
            stack.metrics.record_request(
                &host,
                method.as_str(),
                &route,
                error.status_code(),
                started.elapsed().as_secs_f64(),
            );
            if error.status_code() == 429 {
                stack.metrics.record_rate_limit(&host, &route);
            }
            stack
                .metrics
                .record_error(&host, &route, error.error_type());
            audit::proxy_request(&audit::ProxyAudit {
                request_id: &request_id,
                tenant_id: &tenant_id.to_string(),
                principal_id: subject_id,
                host: &host,
                path,
                method: method.as_str(),
                status: error.status_code(),
                duration_ms: elapsed_ms,
                request_size,
                response_size: 0,
                error_type: Some(error.error_type()),
            });
            error_response(&error)
        }
    }
}

/// The WebSocket data-plane entry point.
///
/// # Errors
///
/// Returns the resolution, policy and transport errors of the tunnel setup.
#[allow(clippy::too_many_arguments)] // one argument per caller-supplied fact
pub async fn proxy_websocket(
    stack: &ProxyStack,
    tenant_id: Uuid,
    subject_id: Option<&str>,
    path: &str,
    query: Option<&str>,
    caller_ip: &str,
    headers: &HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, OagwError> {
    let Some((alias, tail)) = proxy_target(path) else {
        return Err(OagwError::from(DomainError::validation(
            "proxy path must name an upstream alias",
        )));
    };
    let host = alias.clone();
    // The handshake's query travels with the tunnel: upstreams commonly
    // authenticate or route on it.
    let resolved = stack
        .service
        .resolve(
            tenant_id,
            &alias,
            HttpMethod::Get,
            &tail,
            query,
            target_host(headers),
        )
        .await
        .map_err(OagwError::from)?;
    cors::enforce(
        resolved.cors.as_ref(),
        request_origin(headers),
        &Method::GET,
    )
    .map_err(OagwError::from)?;
    stack
        .service
        .charge_rate(
            &resolved,
            tenant_id,
            subject_id.unwrap_or_default(),
            caller_ip,
        )
        .map_err(OagwError::from)?;
    let engine = stack.engine.clone();
    let upstream_path = crate::infra::proxy::outbound_path(&resolved);
    let tunnel = engine
        .forward_websocket(
            &resolved,
            headers,
            &upstream_path,
            &tenant_id.to_string(),
            subject_id,
        )
        .await
        .map_err(OagwError::from)?;
    audit::proxy_request(&audit::ProxyAudit {
        request_id: &inbound_request_id(headers),
        tenant_id: &tenant_id.to_string(),
        principal_id: subject_id,
        host: &host,
        path: &upstream_path,
        method: "GET",
        status: 101,
        duration_ms: 0,
        request_size: 0,
        response_size: 0,
        error_type: None,
    });
    Ok(upgrade.on_upgrade(move |socket| async move {
        relay_socket(socket, tunnel).await;
    }))
}

/// Relays frames between the client socket and the upstream tunnel.
async fn relay_socket(
    mut client: axum::extract::ws::WebSocket,
    mut upstream: tokio_tungstenite::WebSocketStream<reqwest::Upgraded>,
) {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::protocol::Message;

    loop {
        tokio::select! {
            frame = client.recv() => {
                let Some(Ok(frame)) = frame else { break };
                let message = match frame {
                    axum::extract::ws::Message::Text(text) => Message::text(text.as_str()),
                    axum::extract::ws::Message::Binary(bytes) => Message::binary(bytes),
                    axum::extract::ws::Message::Ping(bytes) => Message::Ping(bytes),
                    axum::extract::ws::Message::Pong(bytes) => Message::Pong(bytes),
                    axum::extract::ws::Message::Close(Some(close)) => Message::Close(Some(
                        tokio_tungstenite::tungstenite::protocol::CloseFrame {
                            code: close.code.into(),
                            reason: close.reason.as_str().into(),
                        },
                    )),
                    axum::extract::ws::Message::Close(None) => Message::Close(None),
                };
                if upstream.send(message).await.is_err() {
                    break;
                }
            }
            frame = upstream.next() => {
                let Some(Ok(frame)) = frame else { break };
                let message = match frame {
                    Message::Text(text) => axum::extract::ws::Message::Text(text.as_str().into()),
                    Message::Binary(bytes) => axum::extract::ws::Message::Binary(bytes),
                    Message::Ping(bytes) => axum::extract::ws::Message::Ping(bytes),
                    Message::Pong(bytes) => axum::extract::ws::Message::Pong(bytes),
                    Message::Close(close) => {
                        let close = close.map(|frame| axum::extract::ws::CloseFrame {
                            code: u16::from(frame.code),
                            reason: frame.reason.as_str().into(),
                        });
                        let _ = client.send(axum::extract::ws::Message::Close(close)).await;
                        break;
                    }
                    Message::Frame(_) => continue,
                };
                if client.send(message).await.is_err() {
                    break;
                }
            }
        }
    }
    let _ = client.send(axum::extract::ws::Message::Close(None)).await;
}

/// One full proxy hop: rate limit, plugins, upstream dial, response projection.
#[allow(clippy::too_many_arguments)]
async fn hop(
    stack: &ProxyStack,
    tenant_id: Uuid,
    subject_id: Option<&str>,
    method: &Method,
    domain: &HttpMethod,
    path: &str,
    query: Option<&str>,
    caller_ip: &str,
    headers: &HeaderMap,
    body: Body,
) -> DomainResult<Response> {
    let Some((_alias, tail)) = proxy_target(path) else {
        return Err(DomainError::validation(
            "proxy path must name an upstream alias",
        ));
    };
    // The query never joins the routing path: prefix matching and the suffix
    // are path concerns (DESIGN §4.4), and joining them corrupts both. The
    // target already carries its leading slash.
    let routing_path = tail;
    let resolved = stack
        .service
        .resolve(
            tenant_id,
            &_alias,
            *domain,
            &routing_path,
            query,
            target_host(headers),
        )
        .await?;
    // Origin/method enforcement happens after resolution and before the dial
    // (ADR-0004): preflight is permissive, the actual request is not.
    cors::enforce(resolved.cors.as_ref(), request_origin(headers), method)?;
    let verdict = stack.service.charge_rate(
        &resolved,
        tenant_id,
        subject_id.unwrap_or_default(),
        caller_ip,
    )?;
    let limit = stack.config.max_body_size_bytes;
    let buffered = read_body(body, limit).await?;
    let response = stack
        .engine
        .forward(
            &resolved,
            method,
            headers,
            Some(buffered),
            &tenant_id.to_string(),
            subject_id,
        )
        .await?;
    // ADR-0003 drafts the counter state back to the caller.
    if let Some(verdict) = verdict.filter(|_| {
        resolved
            .rate_limit
            .as_ref()
            .is_none_or(|config| config.response_headers)
    }) {
        stamped_rate_headers(response, &verdict)
    } else {
        Ok(response)
    }
}

/// Adds the `X-RateLimit-*` headers a charged request carries back.
fn stamped_rate_headers(
    response: Response,
    verdict: &crate::domain::services::proxy::RateVerdict,
) -> DomainResult<Response> {
    use axum::http::header::{HeaderName, HeaderValue};
    let mut response = response;
    for (name, value) in [
        ("x-ratelimit-limit", verdict.limit.to_string()),
        ("x-ratelimit-remaining", verdict.remaining.to_string()),
        ("x-ratelimit-reset", verdict.reset_seconds.to_string()),
    ] {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            response.headers_mut().insert(name, value);
        }
    }
    Ok(response)
}

/// The upstream alias a proxy path names.
fn proxy_host(path: &str) -> Option<String> {
    proxy_target(path).map(|(alias, _)| alias)
}

/// The correlation identifier the platform attached, if any.
fn inbound_request_id(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// The caller's `Origin` header, which the preflight path needs.
fn request_origin(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
}

/// `X-OAGW-Target-Host` from the inbound exchange, when present.
fn target_host(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(crate::infra::proxy::TARGET_HOST_HEADER)
        .and_then(|value| value.to_str().ok())
}

/// The route label metrics use; the matched prefix keeps cardinality bounded.
fn route_label(path: &str) -> String {
    match proxy_target(path) {
        Some((alias, tail)) => {
            let depth = tail
                .split('/')
                .filter(|segment| !segment.is_empty())
                .count();
            format!("/proxy/{alias}/{depth}segment")
        }
        None => String::new(),
    }
}

/// Renders a domain error with the gateway error-source header.
fn error_response(error: &DomainError) -> Response {
    rendered_error(OagwError::from(error))
}

/// Renders an already-projected error with the gateway error-source header.
fn rendered_error(error: OagwError) -> Response {
    // `from_static` requires lowercase; the constant carries the wire spelling.
    let mut response = error.into_response();
    response.headers_mut().insert(
        axum::http::HeaderName::from_static("x-oagw-error-source"),
        axum::http::HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
    );
    response
}

/// The upstream error-source header value, re-exported for the handler tests.
pub const UPSTREAM_SOURCE: &str = ERROR_SOURCE_UPSTREAM;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_paths_split_into_alias_and_tail() {
        assert_eq!(
            proxy_target("/oagw/v1/proxy/api.openai.com:8080/v1/chat"),
            Some(("api.openai.com:8080".to_owned(), "/v1/chat".to_owned()))
        );
        assert_eq!(
            proxy_target("/proxy/api.openai.com:8080"),
            Some(("api.openai.com:8080".to_owned(), "/".to_owned()))
        );
        assert_eq!(proxy_target("/oagw/v1/proxy/"), None);
        assert_eq!(proxy_target("/other/api"), None);
    }

    #[test]
    fn methods_map_onto_the_domain_vocabulary() {
        assert_eq!(domain_method(&Method::GET), Some(HttpMethod::Get));
        assert_eq!(domain_method(&Method::PATCH), Some(HttpMethod::Patch));
        assert_eq!(domain_method(&Method::OPTIONS), None);
        assert_eq!(domain_method(&Method::TRACE), None);
    }

    #[test]
    fn preflights_are_recognised() {
        assert!(is_preflight(&Method::OPTIONS));
        assert!(!is_preflight(&Method::POST));
    }

    #[test]
    fn the_client_ip_is_not_taken_from_forwarded_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "6.6.6.6".parse().expect("value"));
        headers.insert("x-oagw-client-ip", "10.0.0.9".parse().expect("value"));
        // The socket peer wins; a spoofable header never does.
        let peer: std::net::SocketAddr = "10.0.0.9:51000".parse().expect("addr");
        assert_eq!(peer_ip(Some(peer)), "10.0.0.9");
        assert_eq!(peer_ip(None), "unknown");
    }

    #[test]
    fn declared_sizes_come_from_the_wire_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("content-length", "42".parse().expect("value"));
        assert_eq!(declared_body_size(&headers), 42);
        assert_eq!(declared_body_size(&HeaderMap::new()), 0);
        let response = Response::builder()
            .header("content-length", "7")
            .body(Body::empty())
            .expect("response");
        assert_eq!(declared_response_size(&response), 7);
    }

    #[test]
    fn route_labels_keep_the_upstream_but_drop_path_detail() {
        let label = route_label("/oagw/v1/proxy/api.openai.com:8080/v1/chat/completions");
        assert_eq!(label, "/proxy/api.openai.com:8080/3segment");
        assert_eq!(route_label("/unrelated"), String::new());
    }

    #[test]
    fn identity_requires_a_security_context() {
        let error = identity(None).expect_err("unauthenticated");
        assert_eq!(error.status_code(), 401);
        let context = SecurityContext::builder()
            .subject_tenant_id(Uuid::now_v7())
            .subject_id(Uuid::now_v7())
            .build()
            .expect("context");
        let (tenant, subject) = identity(Some(&context)).expect("authenticated");
        assert!(subject.is_some());
        assert_ne!(tenant, Uuid::nil());
    }

    #[test]
    fn bodies_above_the_limit_are_rejected() {
        let result = tokio_test_block(read_body(Body::from("toolong"), 4));
        assert_eq!(result.expect_err("over the limit").status_code(), 413);
        assert!(tokio_test_block(read_body(Body::from("ok"), 4)).is_ok());
    }

    fn tokio_test_block<F: std::future::Future<Output = T> + Send + 'static, T>(future: F) -> T
    where
        T: Send + 'static,
    {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    }
}
