//! Proxy handler (`contracts/proxy-api.md`).
//!
//! The handler is a thin transport boundary: it extracts the request, hands it
//! to [`crate::infra::proxy::ProxyService`] and renders the outcome. CORS
//! preflights are answered here, per `DESIGN.md` § "Data plane", without
//! upstream resolution.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Extension, Request};
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use hyper_util::rt::TokioIo;
use toolkit_security::SecurityContext;

use crate::api::rest::error::{OagwError, RequestDetails};
use crate::domain::error::DomainError;

/// The error type a refused rate limit reports.
const RATE_LIMIT_EXCEEDED: &str = "cf.oagw.rate_limit.exceeded.v1";
use crate::domain::services::proxy as planning;
use crate::infra::proxy::ProxyService;
use crate::infra::proxy::service::{Io, UpstreamBody};

/// The proxy route, gear-relative.
pub const PROXY_PATH: &str = "/oagw/v1/proxy/{*rest}";

/// Proxies one request.
///
/// # Errors
/// [`OagwError`] with `X-OAGW-Error-Source: gateway` for every rejection the
/// pipeline produces.
#[allow(clippy::too_many_lines)]
pub async fn proxy(
    Extension(service): Extension<Arc<ProxyService>>,
    Extension(ctx): Extension<SecurityContext>,
    request: Request<Body>,
) -> Result<Response, OagwError> {
    let (parts, body) = request.into_parts();
    let method = parts.method.clone();
    let query = parts.uri.query().unwrap_or_default().to_owned();
    let inbound_headers = parts.headers.clone();
    // Taken before the body is consumed: it is the only way to splice the
    // client connection once the 101 has been written.
    let on_upgrade = parts.extensions.get::<hyper::upgrade::OnUpgrade>().cloned();

    // CORS preflight: answered at the handler level, permissively, without
    // touching the control plane. It is a gateway answer, not a proxied
    // request, so it stays out of the request counters.
    if planning::is_preflight(&method, &inbound_headers) {
        return Ok(preflight_response(&inbound_headers));
    }

    let rest = proxy_rest(&parts.uri);
    let (alias, suffix) = planning::split_proxy_path(&rest);
    let metrics = service.metrics().clone();
    // The gauge tracks the request for its whole stay in the pipeline and is
    // cleared when the guard drops, however the request ends.
    let _in_flight = metrics.enter(&alias);
    let started = std::time::Instant::now();

    let body = match read_body(body, &service).await {
        Ok(body) => body,
        Err(error) => {
            metrics.record_rejection(&alias, error.error_type());
            return Err(rejected(
                &metrics,
                &alias,
                &suffix,
                &method,
                &started,
                error,
                details_for(
                    &service,
                    ctx.subject_tenant_id(),
                    &alias,
                    &suffix,
                    &inbound_headers,
                ),
            ));
        }
    };

    let tenant_id = ctx.subject_tenant_id();
    let trace_id = inbound_headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);
    let input = crate::infra::proxy::service::ProxyInput {
        method: method.clone(),
        rest,
        query,
        headers: inbound_headers.clone(),
        body,
        tenant_id,
        subject_id: ctx.subject_id(),
        security: ctx,
        request_id: trace_id.clone(),
    };

    let output = service.handle(input).await.map_err(|failure| {
        rejected(
            &metrics,
            &alias,
            &suffix,
            &method,
            &started,
            failure.error,
            details_for(&service, tenant_id, &alias, &suffix, &inbound_headers),
        )
        .with_rate_limit(failure.rate)
    })?;

    metrics.observe_request_duration(
        &alias,
        method.as_str(),
        "",
        crate::infra::metrics::PHASE_TOTAL,
        started.elapsed().as_secs_f64(),
    );

    if let UpstreamBody::Upgraded(upstream_io) = output.body {
        return Ok(upgrade_response(
            output.status,
            output.headers,
            on_upgrade,
            upstream_io,
        ));
    }

    let mut response = Response::builder()
        .status(output.status)
        .body(match output.body {
            UpstreamBody::Buffered(bytes) => Body::from(bytes),
            UpstreamBody::Streamed(stream) => Body::new(http_body_util::BodyStream::new(stream)),
            UpstreamBody::Upgraded(_) => unreachable!("handled above"),
        })
        .map_err(|_| OagwError::gateway(DomainError::Internal("invalid response".to_owned())))?;
    *response.headers_mut() = output.headers;
    Ok(response)
}

/// Records the outcome of a request the gateway refused and renders the error.
///
/// A `5xx` is a gateway fault, a `4xx` a policy rejection; an open circuit
/// breaker and a refused rate limit have their own instruments.
fn rejected(
    metrics: &crate::infra::metrics::ProxyMetrics,
    alias: &str,
    path: &str,
    method: &axum::http::Method,
    started: &std::time::Instant,
    error: DomainError,
    details: RequestDetails,
) -> OagwError {
    let error_type = error.error_type();
    if error.status() >= 500 {
        if matches!(error, DomainError::CircuitBreakerOpen) {
            metrics.set_circuit_breaker_state(alias, crate::infra::metrics::CircuitState::Open);
        }
        metrics.record_error(alias, error_type);
    } else {
        if error_type == RATE_LIMIT_EXCEEDED {
            metrics.record_rate_limit_exceeded(alias, path);
        }
        metrics.record_rejection(alias, error_type);
    }
    metrics.observe_request_duration(
        alias,
        method.as_str(),
        "",
        crate::infra::metrics::PHASE_TOTAL,
        started.elapsed().as_secs_f64(),
    );
    OagwError::gateway(error).with_details(details)
}

/// The request details the documented extension members name.
///
/// Resolution is attempted again only on the error path, so a rejected request
/// can still name the upstream it would have reached.
fn details_for(
    service: &ProxyService,
    tenant_id: uuid::Uuid,
    alias: &str,
    path: &str,
    headers: &axum::http::HeaderMap,
) -> RequestDetails {
    let upstream = service.resolve_upstream(tenant_id, alias).ok();
    RequestDetails {
        upstream_id: upstream
            .as_ref()
            .and_then(|upstream| upstream.id.map(|id| id.to_string())),
        host: upstream
            .as_ref()
            .and_then(|upstream| upstream.alias.clone())
            .or_else(|| crate::domain::alias::normalize_alias(alias)),
        path: Some(path.to_owned()),
        trace_id: headers
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned),
    }
}

/// The part of the proxy path beyond `/oagw/v1/proxy`.
fn proxy_rest(uri: &axum::http::Uri) -> String {
    let path = uri.path().to_owned();
    path.strip_prefix("/oagw/v1/proxy")
        .unwrap_or(&path)
        .to_owned()
}

/// Reads the inbound body, enforcing the hard limit at the boundary.
async fn read_body(body: Body, service: &ProxyService) -> Result<Bytes, DomainError> {
    let limit = service.config_max_body_bytes();
    axum::body::to_bytes(body, limit)
        .await
        .map_err(|_| DomainError::PayloadTooLarge(limit))
}

/// Builds the permissive preflight answer.
fn preflight_response(request_headers: &axum::http::HeaderMap) -> Response {
    let origin = request_headers
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("*");
    let requested_method = request_headers
        .get(axum::http::header::ACCESS_CONTROL_REQUEST_METHOD)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("*");
    let requested_headers = request_headers
        .get(axum::http::header::ACCESS_CONTROL_REQUEST_HEADERS)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned);

    let mut response = StatusCode::NO_CONTENT.into_response();
    let headers = response.headers_mut();
    for (name, value) in [
        ("access-control-allow-origin", origin.to_owned()),
        ("access-control-allow-methods", requested_method.to_owned()),
        ("access-control-max-age", "600".to_owned()),
    ] {
        if let Ok(value) = HeaderValue::from_str(&value) {
            headers.insert(name, value);
        }
    }
    if let Some(value) = requested_headers
        && let Ok(value) = HeaderValue::from_str(&value)
    {
        headers.insert("access-control-allow-headers", value);
    }
    response
}

/// Returns the `101` and splices the two upgraded connections.
fn upgrade_response(
    status: StatusCode,
    response_headers: axum::http::HeaderMap,
    on_upgrade: Option<hyper::upgrade::OnUpgrade>,
    upstream_io: Box<dyn Io>,
) -> Response {
    let mut response = StatusCode::SWITCHING_PROTOCOLS.into_response();
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;

    if let Some(on_upgrade) = on_upgrade {
        tokio::spawn(async move {
            let Ok(client) = on_upgrade.await else {
                return;
            };
            let mut client: Box<dyn Io> = Box::new(TokioIo::new(client));
            let mut upstream = upstream_io;
            if let Err(err) = tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
                tracing::debug!(error = %err, "websocket splice ended");
            }
        });
    }
    response
}
