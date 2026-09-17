//! Data-Plane Proxy REST handler (feature `cpt-cf-oagw-feature-data-plane-proxy`,
//! p5; flow `cpt-cf-oagw-flow-data-plane-proxy-execute`).
//!
//! Registers the `/proxy/{alias}[/{*rest}]` surface — `POST/GET/PUT/PATCH/
//! DELETE` via [`proxy_request`] and a direct `OPTIONS` route via
//! [`proxy_preflight`] (the toolkit operation builder maps the five CRUD
//! methods plus a 405 fallback; preflight handling is registered as a real
//! `OPTIONS` route to avoid the fallback).
//!
//! # Authentication decision (documented)
//!
//! `/proxy` **is authenticated**: the handler extracts the platform-injected
//! [`SecurityContext`] (the same mechanism the Control Plane uses, DESGN
//! `gts.cf.core.oagw.proxy.v1~:invoke` checked by the Data Plane service at
//! `inst-dp-exec-authz`).  This follows the safer contract reading — proxy
//! invocation is a tenant resource action and the DESIGN gates it behind the
//! `invoke` permission; an unauthenticated proxy would bypass the tenant
//! boundary that §3.3 grants to the PDP.
//!
//! # Error-source semantics (ADR 0007)
//!
//! Gateway-generated failures are rendered as RFC 9457 problem+json with
//! `X-OAGW-Error-Source: gateway` ([`GatewayError::into_http_response`]),
//! decorated with the `X-RateLimit-*`/`Retry-After` projections on 429s and
//! the CORS actual headers when the request was already CORS-allowed.  A CORS
//! violation is served as the raw 403 with the full `cf.oagw.cors.*` GTS
//! instance (see [`CorsViolation`]).  Upstream responses (including 4xx/5xx)
//! pass through the gateway unchanged tagged `upstream`.
//!
//! # Observability (feature `cpt-cf-oagw-feature-observability-audit`)
//!
//! The handler performs the request-ID correlation (DoD
//! `cpt-cf-oagw-dod-observability-audit-correlation`, flow
//! `cpt-cf-oagw-flow-observability-audit-correlate`): a gateway request id is
//! minted when the client supplies none (`inst-ob-cor-relid`), injected into
//! the request header surface so the `request_id` transform plugin and the
//! passthrough re-use it (`inst-ob-cor-propagate`), recorded on the error
//! envelope and returned to the caller on every response (`inst-ob-cor-record`);
//! and one audit-log entry is emitted per proxy request (DESIGN §4.3 — the
//! deterministic field set, algorithm
//! `cpt-cf-oagw-algo-observability-audit-audit-log`).  Metrics are recorded
//! inside the Data Plane service; the audit entry reuses the GTS instance for
//! gateway failures and the bounded `upstream_http_error` marker for upstream
//! failures (§63).

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Extension, Path, Request};
use axum::http::header;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use serde::Deserialize;
use toolkit_security::SecurityContext;
use tracing::Level;
use uuid::Uuid;

use crate::domain::GearState;
use crate::domain::cors::{CorsViolation, is_preflight, preflight_response};
use crate::domain::error::DomainError;
use crate::domain::plugin::Headers;
use crate::domain::rate::RateLimitInfo;
use crate::domain::service::data_plane::{ProxyFailure, ProxyRequest};
use crate::infra::audit::{
    AuditEntry, EVENT_AUTH_FAILED, EVENT_PROXY_COMPLETED, EVENT_PROXY_REJECTED,
    EVENT_RATE_LIMIT_EXCEEDED, EVENT_UPSTREAM_ERROR, now_utc_timestamp,
};
use crate::infra::error_envelope::{ERROR_SOURCE_HEADER, ErrorRequestContext, GatewayError};

/// The `{alias}[/{*rest}]` path capture.  `rest` is absent on the bare
/// `/proxy/{alias}` route and `v1/...` (no leading slash) on the wildcard
/// route; the handler normalizes it to a leading-`/` rest path.
#[derive(Debug, Deserialize)]
pub struct ProxyPath {
    /// The normalized target alias.
    pub alias: String,
    /// The wildcard path remainder without a leading slash, when present.
    #[serde(default)]
    pub rest: Option<String>,
}

/// `POST/GET/PUT/PATCH/DELETE /proxy/{alias}[/{*rest}]` — the proxy hot path.
///
/// The [`SecurityContext`] and shared [`GearState`] arrive as axum
/// `Extension`s; the inbound [`Request`] is split into parts (method, path,
/// query, headers) plus the streaming body which the Data Plane forwards
/// without buffering.  The gateway request id is minted/reused here, injected
/// into the request surface, echoed to the caller, and correlated across the
/// audit entry and the error envelope.
pub async fn proxy_request(
    Path(path): Path<ProxyPath>,
    Extension(ctx): Extension<SecurityContext>,
    Extension(state): Extension<Arc<GearState>>,
    request: Request,
) -> Response {
    let started = std::time::Instant::now();
    let (parts, body) = request.into_parts();

    // Request correlation (`inst-ob-cor-relid`): reuse the client-supplied
    // `X-Request-ID` or mint a gateway id.
    let request_id = resolve_request_id(&parts.headers);
    let ectx = ErrorRequestContext {
        request_path: parts.uri.path().to_owned(),
        request_id: Some(request_id.clone()),
        trace_id: parts
            .headers
            .get("traceparent")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
    };

    let method = parts.method.as_str().to_owned();
    let alias = crate::domain::entity::alias::normalize_alias(&path.alias);
    let mut domain_headers = headers_from_request(&parts.headers);
    // Inject the correlated request id into the request surface so the
    // `request_id` transform plugin and the passthrough both re-use the
    // gateway id (`inst-ob-cor-propagate`) instead of minting their own.
    if domain_headers.get("x-request-id").is_none() {
        domain_headers.append("x-request-id", &request_id);
    }

    let proxy_request = ProxyRequest {
        method,
        path: parts.uri.path().to_owned(),
        alias: alias.clone(),
        rest_path: path
            .rest
            .as_deref()
            .map(|rest| format!("/{rest}"))
            .unwrap_or_else(|| "/".to_owned()),
        query: parse_query(parts.uri.query()),
        headers: domain_headers,
        body,
    };

    let request_size = request_length(&parts.headers);
    let (response, error_type, error_message) = match state.data.proxy(&ctx, proxy_request).await {
        Ok(response) => {
            if response.status().as_u16() >= 400 {
                // Upstream-produced failure passes through untouched
                // (ADR 0007); attribute the audit entry to the upstream.
                (response, "upstream_http_error".to_owned(), None)
            } else {
                (response, String::new(), None)
            }
        }
        Err(failure) => {
            let response = render_failure(&failure, &ectx);
            (
                response,
                failure.error.instance().to_owned(),
                Some(failure.error.to_string()),
            )
        }
    };

    emit_proxy_audit(
        &ctx,
        &ectx,
        &alias,
        parts.method.as_str(),
        parts.uri.path(),
        response.status().as_u16(),
        started.elapsed(),
        request_size,
        response_length(&response),
        &error_type,
        error_message.as_deref(),
    );

    echo_request_id(response, &request_id)
}

/// `OPTIONS /proxy/{alias}[/{*rest}]` — CORS preflight.
///
/// This route is registered directly (the operation builder covers the five
/// CRUD methods; an unhandled `OPTIONS` would otherwise hit the 405
/// fallback).  Preflights are permissive by design (flow
/// `cpt-cf-oagw-flow-cors-handling-preflight`): a request carrying both
/// `Origin` and `Access-Control-Request-Method` is answered with the 204 +
/// echo headers; any other `OPTIONS` is a 404 `route.not_found` (the proxy
/// surface does not implement bare `OPTIONS`).
pub async fn proxy_preflight(Path(path): Path<ProxyPath>, headers: HeaderMap) -> Response {
    let domain_headers = headers_from_request(&headers);
    if is_preflight("OPTIONS", &domain_headers) {
        let origin = domain_headers.get("origin").unwrap_or("*");
        let method = domain_headers
            .get("access-control-request-method")
            .unwrap_or("GET");
        let acrh = domain_headers.get("access-control-request-headers");
        let preflight = preflight_response(origin, method, acrh);
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NO_CONTENT;
        for (name, value) in preflight.headers {
            if let (Ok(name), Ok(value)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(&value),
            ) {
                response.headers_mut().insert(name, value);
            }
        }
        response
    } else {
        let ectx = ErrorRequestContext {
            request_path: format!("/proxy/{}", path.alias),
            ..Default::default()
        };
        GatewayError::from_domain(
            &DomainError::RouteNotFound {
                detail: "OPTIONS without a CORS preflight is not supported by the proxy".to_owned(),
            },
            &ectx,
        )
        .into_http_response()
    }
}

/// Reuses the client `X-Request-ID` when present, else mints a gateway
/// request id (`inst-ob-cor-relid`).
fn resolve_request_id(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string())
}

/// Echoes the correlated request id to the caller on every response
/// (`inst-ob-cor-record`).
fn echo_request_id(mut response: Response, request_id: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

/// The inbound request body length from `Content-Length`, when declared.
fn request_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
}

/// The response body length from the response `Content-Length`, when
/// declared (streamed passthrough bodies carry it when framed).
fn response_length(response: &Response) -> Option<u64> {
    response
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
}

/// Maps a completed proxy outcome onto the DESIGN §4.3 audit level and event
/// name: INFO success, WARN 4xx rejection / rate-limit, ERROR upstream
/// failures and authorization failures.
fn audit_level_event(status: u16, error_type: &str) -> (Level, &'static str) {
    if status == 429 {
        (Level::WARN, EVENT_RATE_LIMIT_EXCEEDED)
    } else if error_type.contains("access.denied") {
        (Level::ERROR, EVENT_AUTH_FAILED)
    } else if status >= 500 && error_type == "upstream_http_error" {
        (Level::ERROR, EVENT_UPSTREAM_ERROR)
    } else if status >= 500 {
        (Level::ERROR, EVENT_PROXY_REJECTED)
    } else if status >= 400 {
        (Level::WARN, EVENT_PROXY_REJECTED)
    } else {
        (Level::INFO, EVENT_PROXY_COMPLETED)
    }
}

/// Emits the single structured audit-log entry for one proxy request
/// (algorithm `cpt-cf-oagw-algo-observability-audit-audit-log`,
/// `inst-ob-al-*`; DoD `cpt-cf-oagw-dod-observability-audit-audit-log`).
#[allow(clippy::too_many_arguments)]
fn emit_proxy_audit(
    ctx: &SecurityContext,
    ectx: &ErrorRequestContext,
    alias: &str,
    method: &str,
    request_path: &str,
    status: u16,
    duration: std::time::Duration,
    request_size: Option<u64>,
    response_size: Option<u64>,
    error_type: &str,
    error_message: Option<&str>,
) {
    let (level, event) = audit_level_event(status, error_type);
    let tenant_id = opt_uuid(ctx.subject_tenant_id());
    let principal_id = opt_uuid(ctx.subject_id());
    let entry = AuditEntry::new(now_utc_timestamp(), level, event)
        .with_correlation(ectx.request_id.clone())
        .with_identity(tenant_id, principal_id)
        .with_request(
            Some(alias.to_owned()),
            Some(request_path.to_owned()),
            Some(method.to_owned()),
        )
        .with_outcome(
            status,
            Some(duration.as_millis().min(u128::from(u64::MAX)) as u64),
            request_size,
            response_size,
        )
        .with_error(error_type, error_message.map(str::to_owned));
    entry.emit();
}

/// `Some(uuid_string)` for a non-nil id (anonymous contexts are nil and
/// recorded as absent).
fn opt_uuid(id: Uuid) -> Option<String> {
    if id.is_nil() {
        None
    } else {
        Some(id.to_string())
    }
}

/// Renders a data-plane failure per ADR 0007 with the rate-limit / CORS
/// decorations (and the raw CORS 403 when a violation is present).
fn render_failure(failure: &ProxyFailure, ectx: &ErrorRequestContext) -> Response {
    if let Some(violation) = &failure.cors_violation {
        return cors_violation_response(violation);
    }
    let mut envelope = GatewayError::from_domain(&failure.error, ectx);
    if let Some(target) = &failure.target {
        envelope = envelope.with_target(
            Some(target.upstream_id.to_string()),
            Some(target.host.clone()),
            Some(target.path.clone()),
        );
    }
    let mut response = envelope.into_http_response();
    if let Some(info) = &failure.rate_limit {
        set_rate_limit_headers(&mut response, info);
    }
    apply_cors_headers(&mut response, &failure.cors_headers);
    response
}

/// Serves the raw CORS rejection (ADR 0004): 403 with the `cors.*` type, the
/// mandated `Vary: Origin`, and the `gateway` source attribution.
fn cors_violation_response(violation: &CorsViolation) -> Response {
    let body = serde_json::json!({
        "type": violation.code,
        "title": "CORS rejection",
        "status": violation.status,
        "detail": violation.detail,
    });
    let mut response = Response::new(Body::from(
        serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec()),
    ));
    *response.status_mut() =
        StatusCode::from_u16(violation.status).unwrap_or(StatusCode::FORBIDDEN);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    response.headers_mut().insert(
        HeaderName::from_static(ERROR_SOURCE_HEADER),
        HeaderValue::from_static("gateway"),
    );
    response
        .headers_mut()
        .append(header::VARY, HeaderValue::from_static("Origin"));
    response
}

/// Applies the `X-RateLimit-Limit` / `X-RateLimit-Remaining` /
/// `X-RateLimit-Reset` / `Retry-After` projections on a rate-limited 429.
fn set_rate_limit_headers(response: &mut Response, info: &RateLimitInfo) {
    let reset = info.reset_epoch(std::time::SystemTime::now());
    let header_value = |text: String| {
        HeaderValue::from_str(&text).unwrap_or_else(|_| HeaderValue::from_static("0"))
    };
    response
        .headers_mut()
        .insert("X-RateLimit-Limit", header_value(info.limit.to_string()));
    response.headers_mut().insert(
        "X-RateLimit-Remaining",
        header_value(info.remaining.to_string()),
    );
    response
        .headers_mut()
        .insert("X-RateLimit-Reset", header_value(reset.to_string()));
    response.headers_mut().insert(
        header::RETRY_AFTER,
        header_value(info.retry_after.as_secs().to_string()),
    );
}

/// Applies CORS actual-response headers to an error response (set when the
/// actual request had already been CORS-allowed).
fn apply_cors_headers(response: &mut Response, pairs: &[(String, String)]) {
    for (name, value) in pairs {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            if response.headers().contains_key(&name) {
                response.headers_mut().insert(name, value);
            } else {
                response.headers_mut().append(name, value);
            }
        }
    }
}

/// Rebuilds the domain [`Headers`] from an `http::HeaderMap` (lower-cased
/// names, multi-valued preserved).
fn headers_from_request(headers: &HeaderMap) -> Headers {
    let mut out = Headers::new();
    for (name, value) in headers {
        if let Ok(value_str) = value.to_str() {
            out.append(name.as_str(), value_str);
        }
    }
    out
}

/// Decodes the raw query string into `(name, value)` pairs.
fn parse_query(query: Option<&str>) -> Vec<(String, String)> {
    query
        .map(|q| {
            form_urlencoded::parse(q.as_bytes())
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect()
        })
        .unwrap_or_default()
}
