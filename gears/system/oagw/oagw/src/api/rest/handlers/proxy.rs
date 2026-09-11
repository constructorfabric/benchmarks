//! The proxy handler: `/oagw/v1/proxy/{alias}[/{*path}]`.
//!
//! The handler is deliberately thin. It normalises the inbound request into a
//! [`ProxyRequest`], hands it to the data plane and turns the answer back into
//! a response. The three body shapes — buffered, streamed and upgraded — are
//! decided here, because they are the only part of the proxy that belongs to
//! the transport.

use crate::api::extract::{CallerContext, is_preflight, is_upgrade, request_id_from, target_host};
use crate::api::rest::error::gateway_error_response;
use crate::domain::error::OagwError;
use crate::domain::services::proxy::{DataPlane, ProxyBody, ProxyRequest, ProxySuccess};
use crate::infra::metrics::{Metrics, RequestRecord};
use axum::body::Body;
use axum::extract::Extension;
use axum::http::header;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::Instant;

/// Everything the proxy needs from the gear.
pub struct ProxyState {
    /// The data plane.
    pub plane: Arc<dyn DataPlane>,
    /// The gear's counters.
    pub metrics: Arc<crate::infra::metrics::Metrics>,
    /// Tenant the anonymous caller resolves to.
    pub anonymous_tenant: uuid::Uuid,
}

/// The state the handler reads, shared through `Arc`.
pub type Shared = Arc<ProxyState>;

/// Extracts the inbound upgrade future, if the connection is upgradable.
///
/// The future is taken out of the extensions here so the pump task can await
/// it once the `101` has been flushed.
pub struct DownstreamUpgrade(pub Option<hyper::upgrade::OnUpgrade>);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for DownstreamUpgrade {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(parts.extensions.remove::<hyper::upgrade::OnUpgrade>()))
    }
}

/// `ALL /oagw/v1/proxy/{alias}[/{*path}]`.
///
/// # Errors
///
/// Returns the documented problem document for every gateway failure.
#[allow(clippy::too_many_lines)]
pub async fn proxy(
    Extension(state): Extension<Shared>,
    uri: Uri,
    method: Method,
    headers: HeaderMap,
    caller: CallerContext,
    DownstreamUpgrade(on_upgrade): DownstreamUpgrade,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    let request_id = request_id_from(&headers);
    let path = uri.path().to_owned();
    let mut record = RequestRecord::begin(
        "proxy_request",
        &request_id,
        &caller.tenant_id.to_string(),
        method.as_str(),
    );
    record.request_size = body.len();

    let Some(alias) = split_alias(&path) else {
        let error = OagwError::RouteNotFound("no alias in the proxy path".to_owned());
        record.failed(&error.type_id());
        record.emit();
        return gateway_error_response(&error, Some(&path));
    };
    let suffix = path_suffix(&path);

    // CORS preflight never reaches the upstream: the gateway answers it from
    // the merged configuration, permissively when none applies.
    if is_preflight(method.as_str(), &headers) {
        let config = state
            .plane
            .resolve(prepare(
                &state,
                caller.tenant_id,
                &alias,
                &suffix,
                &uri,
                &method,
                &headers,
                &body,
            ))
            .await
            .map(|resolved| resolved.effective)
            .ok();
        let response = crate::infra::cors::preflight(config.as_ref(), &headers);
        crate::api::rest::error::mark_gateway(response)
    } else {
        let request = ProxyRequest {
            tenant_id: caller.tenant_id,
            alias,
            path_suffix: suffix,
            query: uri.query().unwrap_or_default().to_owned(),
            method: method.as_str().to_owned(),
            headers: headers.clone(),
            body,
            target_host: target_host(&headers),
            request_id,
            is_upgrade: is_upgrade(method.as_str(), &headers),
            is_preflight: false,
        };
        if is_upgrade(method.as_str(), &headers) {
            upgrade(&state, request, on_upgrade, started, record, &path).await
        } else {
            plain(&state, request, started, record, &path).await
        }
    }
}

/// Assembles a request from the inbound parts, for the preflight path.
#[allow(clippy::too_many_arguments)]
fn prepare(
    _state: &Shared,
    tenant_id: uuid::Uuid,
    alias: &str,
    suffix: &str,
    uri: &Uri,
    method: &Method,
    headers: &HeaderMap,
    body: &Bytes,
) -> ProxyRequest {
    ProxyRequest {
        tenant_id,
        alias: alias.to_owned(),
        path_suffix: suffix.to_owned(),
        query: uri.query().unwrap_or_default().to_owned(),
        method: method.as_str().to_owned(),
        headers: headers.clone(),
        body: body.clone(),
        target_host: target_host(headers),
        request_id: request_id_from(headers),
        is_upgrade: false,
        is_preflight: true,
    }
}

/// A non-upgrade proxy request, end to end.
async fn plain(
    state: &Shared,
    request: ProxyRequest,
    started: Instant,
    mut record: RequestRecord,
    path: &str,
) -> Response {
    match state.plane.handle(request).await {
        Ok(success) => {
            record.finish(Some(success.status), started.elapsed());
            record.emit();
            state.metrics.stream_relayed();
            build_response(success, &state.metrics)
        }
        Err(error) => {
            record.failed(&error.type_id());
            record.finish(Some(error.status()), started.elapsed());
            record.emit();
            gateway_error_response(&error, Some(path))
        }
    }
}

/// A WebSocket upgrade: forward the handshake, answer `101`, relay.
async fn upgrade(
    state: &Shared,
    request: ProxyRequest,
    on_upgrade: Option<hyper::upgrade::OnUpgrade>,
    started: Instant,
    mut record: RequestRecord,
    path: &str,
) -> Response {
    let tunnel = match state.plane.open_tunnel(request.clone()).await {
        Ok(tunnel) => tunnel,
        Err(error) => {
            record.failed(&error.type_id());
            record.finish(Some(error.status()), started.elapsed());
            record.emit();
            return gateway_error_response(&error, Some(path));
        }
    };
    state.metrics.tunnel_opened();
    record.finish(Some(tunnel.status), started.elapsed());
    record.emit();

    let status = StatusCode::from_u16(tunnel.status).unwrap_or(StatusCode::SWITCHING_PROTOCOLS);
    let mut builder = Response::builder().status(status);
    // The bytes on this answer came from the upstream, whatever the status.
    builder = builder.header(
        crate::api::rest::error::ERROR_SOURCE_HEADER,
        crate::api::rest::error::ERROR_SOURCE_UPSTREAM,
    );
    for (name, value) in &tunnel.headers {
        if let (Ok(name), Ok(value)) = (
            header::HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            builder = builder.header(name, value);
        }
    }
    if let Some(upgrade) = on_upgrade {
        let upstream = tunnel.stream;
        tokio::spawn(async move {
            if let Ok(downstream) = upgrade.await {
                let downstream = Box::pin(crate::infra::proxy::websocket::UpgradedIo(downstream));
                crate::infra::proxy::websocket::relay(downstream, upstream).await;
            }
            // A failed upgrade means the client left before the handshake
            // completed: the upstream socket is dropped, closing the tunnel.
        });
    }
    // Nothing to relay onto: dropping the upstream socket closes it.
    builder
        .body(Body::empty())
        .unwrap_or_else(|error| (StatusCode::BAD_GATEWAY, error.to_string()).into_response())
}

/// Builds the client response from a proxied answer.
///
/// A stream the upstream cut short is counted: the client sees the connection
/// end, and the only trace left is this counter and the audit record.
fn build_response(success: ProxySuccess, metrics: &Arc<Metrics>) -> Response {
    let status = StatusCode::from_u16(success.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    for (name, value) in &success.headers {
        if name.eq_ignore_ascii_case("content-length") {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            header::HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            builder = builder.header(name, value);
        }
    }
    let body = match success.body {
        ProxyBody::Full(bytes) => Body::from(bytes),
        ProxyBody::Streaming(stream) => {
            let counters = Arc::clone(metrics);
            Body::from_stream(stream.map(move |item| {
                if item.is_err() {
                    counters.stream_aborted();
                }
                item
            }))
        }
        // An upgrade is relayed in `upgrade`, never returned from `handle`.
        ProxyBody::WebSocket(_) => Body::empty(),
    };
    builder
        .body(body)
        .unwrap_or_else(|error| (StatusCode::BAD_GATEWAY, error.to_string()).into_response())
}

/// The path segment that introduces the alias.
const PROXY_SEGMENT: &str = "/proxy/";

/// Everything a proxy route carries after its mount point.
///
/// The gear's routes are served at their full path, so the alias begins after
/// whichever prefix the router mounted the proxy under.
#[must_use]
fn after_proxy_segment(path: &str) -> Option<&str> {
    path.split_once(PROXY_SEGMENT).map(|(_, rest)| rest)
}

/// Splits the alias out of a proxy path.
#[must_use]
pub fn split_alias(path: &str) -> Option<String> {
    let rest = after_proxy_segment(path)?;
    let alias = rest.split('/').next()?;
    let alias = alias.trim();
    (!alias.is_empty()).then(|| alias.to_ascii_lowercase())
}

/// Everything after the alias, without a leading slash.
#[must_use]
pub fn path_suffix(path: &str) -> String {
    let rest = after_proxy_segment(path).unwrap_or(path);
    match rest.split_once('/') {
        Some((_, suffix)) => suffix.to_owned(),
        None => String::new(),
    }
}
