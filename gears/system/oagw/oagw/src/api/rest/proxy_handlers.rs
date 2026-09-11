//! REST handler of the proxy data plane (FEATURE entry 2.4, the dispatch flow
//! `cpt-cf-oagw-flow-request-proxy-dispatch`).
//!
//! The handler does five things and no more:
//!
//! 1. it detects a CORS preflight and answers it with the permissive `204`
//!    before anything else, because a preflight carries no security context,
//!    resolves no tenant and reaches no upstream
//!    (`cpt-cf-oagw-dod-request-proxy-preflight-detection`); the CORS-correct
//!    content of that answer is entry 2.8's, built through the preflight
//!    call-in of [`crate::infra::proxy::DataPlaneServiceImpl`];
//! 2. it resolves the caller identity out of the platform security context
//!    and rejects a missing one with the `401` OAGW authentication surface;
//! 3. it evaluates `gts.cf.core.oagw.proxy.v1~:invoke` before any repository
//!    access or body read;
//! 4. it validates the body framing and reads the body, so a declared body
//!    over the limit is `413` before any byte is buffered;
//! 5. it dispatches to [`crate::infra::proxy::DataPlaneServiceImpl`] and
//!    renders what comes back, stamping `X-OAGW-Error-Source` on every
//!    response it produces.
//!
//! The proxy path is a catch-all, so the handler parses the request target
//! itself rather than relying on a path extractor: the alias is the first
//! segment after the prefix and everything after it is the path suffix.
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-cors-call-in:p1
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-effective-config:p1
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-grpc-surface:p1
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-header-pipeline:p1
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-low-latency:p1
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-path-and-query:p1
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-plugin-chain-points:p1
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-rate-limit-call-in:p1
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-response-passthrough:p1
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-route-matching:p1
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-timeout-no-retry:p1
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-websocket-session:p1
// @cpt-dod:cpt-cf-oagw-dod-request-proxy-webtransport-session:p1

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;
use axum::extract::{ConnectInfo, Extension, Request};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use toolkit_security::SecurityContext;

use super::error::{ApiError, ProblemDetails, TraceIdentifier};
use crate::domain::proxy::{
    ErrorSource, ProxyBody, ProxyContext, ProxyFailure, ProxyObservation, ProxyResponse,
};
use crate::domain::services::management::{Actor, AuthorizeError};
use crate::gts::{PERM_PROXY_INVOKE, PROXY_BASE_TYPE};
use crate::infra::observability::Observability;
use crate::infra::proxy::{body_validation, DataPlaneServiceImpl};

/// The gear-relative prefix the proxy paths live under.
pub const PROXY_PREFIX: &str = "/oagw/v1/proxy/";

/// The header the inbound trace identifier arrives on. The value is carried,
/// never minted: trace-identifier propagation is entry 2.9's, and the constant
/// is owned by the error contract so the two read the same name.
pub use super::error::TRACE_ID_HEADER;


/// The `Access-Control-Max-Age` of the permissive preflight response.
pub use crate::domain::cors::MAX_AGE;

/// Whether the request is a CORS preflight
/// (`cpt-cf-oagw-flow-request-proxy-dispatch` step 4,
/// `cpt-cf-oagw-flow-cors-preflight` step 1).
// @cpt-begin:cpt-cf-oagw-flow-cors-preflight:p1:inst-cors-pf-2
// `inst-cors-pf-2`/`-6`: a preflight is an `OPTIONS` carrying both an `Origin`
// and an `Access-Control-Request-Method`; an `OPTIONS` without the full
// signature is not a preflight and falls through to the ordinary proxy path,
// where the matched route's method allowlist decides the outcome and no
// `Access-Control-*` header is added.
// @cpt-begin:cpt-cf-oagw-flow-cors-preflight:p1:inst-cors-pf-3
// @cpt-begin:cpt-cf-oagw-flow-cors-preflight:p1:inst-cors-pf-4
// @cpt-begin:cpt-cf-oagw-flow-cors-preflight:p1:inst-cors-pf-5
// @cpt-begin:cpt-cf-oagw-flow-cors-preflight:p1:inst-cors-pf-6
// @cpt-begin:cpt-cf-oagw-flow-cors-preflight:p1:inst-cors-pf-7
// @cpt-begin:cpt-cf-oagw-flow-cors-preflight:p1:inst-cors-pf-8
#[must_use]
pub fn is_preflight(method: &str, headers: &[(String, String)]) -> bool {
    method == "OPTIONS"
        && headers.iter().any(|(name, value)| name == "origin" && !value.is_empty())
        && headers
            .iter()
            .any(|(name, value)| name == "access-control-request-method" && !value.is_empty())
}
//
// @cpt-end:cpt-cf-oagw-flow-cors-preflight:p1:inst-cors-pf-8
// @cpt-end:cpt-cf-oagw-flow-cors-preflight:p1:inst-cors-pf-7
// @cpt-end:cpt-cf-oagw-flow-cors-preflight:p1:inst-cors-pf-6
// @cpt-end:cpt-cf-oagw-flow-cors-preflight:p1:inst-cors-pf-5
// @cpt-end:cpt-cf-oagw-flow-cors-preflight:p1:inst-cors-pf-4
// @cpt-end:cpt-cf-oagw-flow-cors-preflight:p1:inst-cors-pf-3
//
// @cpt-end:cpt-cf-oagw-flow-cors-preflight:p1:inst-cors-pf-2

/// The header value of `name`, compared exactly as the framework lowercased it.
#[must_use]
pub fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(header, _)| header == name).map(|(_, value)| value.as_str())
}

/// The caller identity of a proxy request.
///
/// A preflight carries no tenant context by construction, so the identity is
/// resolved only after the preflight short-circuit.
fn actor_of(security: Option<Extension<SecurityContext>>) -> Result<Actor, ApiError> {
    let Some(Extension(security)) = security else {
        return Err(super::error::unauthenticated());
    };
    let tenant_id = security.subject_tenant_id();
    if tenant_id.is_nil() {
        return Err(super::error::unauthenticated());
    }
    Ok(Actor { tenant_id, principal_id: security.subject_id() })
}

/// The alias and the path suffix of a proxy request target.
///
/// `/oagw/v1/proxy/api.vendor.com/v1/orders` yields `("api.vendor.com",
/// Some("/v1/orders"))`; `/oagw/v1/proxy/api.vendor.com` yields
/// `("api.vendor.com", None)`.
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-dispatch:p1:inst-rp-dispatch-7
// `inst-rp-dispatch-7`: the alias and the path suffix are split from the
// gear-relative target, so no `/api` prefix ever reaches the pipeline.
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-dispatch:p1:inst-rp-dispatch-3
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-dispatch:p1:inst-rp-dispatch-4
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-dispatch:p1:inst-rp-dispatch-5
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-dispatch:p1:inst-rp-dispatch-6
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-dispatch:p1:inst-rp-dispatch-8
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-dispatch:p1:inst-rp-dispatch-9
#[must_use]
pub fn target_of(path: &str) -> Option<(String, Option<String>)> {
    let rest = path.strip_prefix(PROXY_PREFIX)?;
    let (alias, suffix) = match rest.split_once('/') {
        Some((alias, suffix)) => (alias, Some(format!("/{suffix}"))),
        None => (rest, None),
    };
    if alias.is_empty() {
        return None;
    }
    Some((alias.to_owned(), suffix))
}
//
// @cpt-end:cpt-cf-oagw-flow-request-proxy-dispatch:p1:inst-rp-dispatch-9
// @cpt-end:cpt-cf-oagw-flow-request-proxy-dispatch:p1:inst-rp-dispatch-8
// @cpt-end:cpt-cf-oagw-flow-request-proxy-dispatch:p1:inst-rp-dispatch-6
// @cpt-end:cpt-cf-oagw-flow-request-proxy-dispatch:p1:inst-rp-dispatch-5
// @cpt-end:cpt-cf-oagw-flow-request-proxy-dispatch:p1:inst-rp-dispatch-4
// @cpt-end:cpt-cf-oagw-flow-request-proxy-dispatch:p1:inst-rp-dispatch-3
//
// @cpt-end:cpt-cf-oagw-flow-request-proxy-dispatch:p1:inst-rp-dispatch-7

/// Stamp the error-source header on a response the handler produced
/// (`cpt-cf-oagw-flow-request-proxy-error-source-emission`).
fn stamp_source(response: &mut Response, source: ErrorSource) {
    let value = source.as_str();
    if let Ok(value) = HeaderValue::from_str(value) {
        response.headers_mut().insert("x-oagw-error-source", value);
    }
}

/// Render a [`ProxyResponse`] as the client response.
///
/// A gateway error renders through the shared error contract; an upstream
/// response passes through with its status, headers and body unmodified.
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-error-source-emission:p1:inst-rp-errsrc-1
// `inst-rp-errsrc-1` .. `-6`: the error-source header is stamped on every
// response this handler produces — `gateway` for a gateway error, `upstream`
// for a response the external service produced — and nothing else is added to
// an upstream response.
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-error-source-emission:p1:inst-rp-errsrc-2
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-error-source-emission:p1:inst-rp-errsrc-3
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-error-source-emission:p1:inst-rp-errsrc-4
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-error-source-emission:p1:inst-rp-errsrc-5
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-error-source-emission:p1:inst-rp-errsrc-6
#[must_use]
pub fn render(response: ProxyResponse) -> Response {
    if let Some(error) = response.error {
        let mut rendered = ApiError::Domain(error).into_response();
        // The pipeline-produced headers survive the problem+json rendering: the
        // quota headers of a refused rate limit ride on the shared error body
        // (`inst-rl-str-7`/`-8`).
        append_headers(&mut rendered, &response.headers);
        stamp_source(&mut rendered, ErrorSource::Gateway);
        return rendered;
    }
    let mut headers = axum::http::HeaderMap::new();
    for (name, value) in &response.headers {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(value),
        ) {
            headers.append(name, value);
        }
    }
    let body = match response.body {
        ProxyBody::Empty => axum::body::Body::empty(),
        ProxyBody::Buffered(bytes) => axum::body::Body::from(bytes),
        // The relayed stream is handed to the client as it arrives: no buffer
        // holds the conversation.
        ProxyBody::Stream(stream) => axum::body::Body::new(http_body_util::StreamBody::new(
            stream.map(|item| item.map(hyper::body::Frame::data)),
        )),
    };
    let status =
        StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut rendered = (status, body).into_response();
    *rendered.headers_mut() = headers;
    stamp_source(&mut rendered, response.source);
    rendered
}
//
// @cpt-end:cpt-cf-oagw-flow-request-proxy-error-source-emission:p1:inst-rp-errsrc-6
// @cpt-end:cpt-cf-oagw-flow-request-proxy-error-source-emission:p1:inst-rp-errsrc-5
// @cpt-end:cpt-cf-oagw-flow-request-proxy-error-source-emission:p1:inst-rp-errsrc-4
// @cpt-end:cpt-cf-oagw-flow-request-proxy-error-source-emission:p1:inst-rp-errsrc-3
// @cpt-end:cpt-cf-oagw-flow-request-proxy-error-source-emission:p1:inst-rp-errsrc-2
//
// @cpt-end:cpt-cf-oagw-flow-request-proxy-error-source-emission:p1:inst-rp-errsrc-1

/// Render a [`ProxyFailure`] the pipeline returned
/// (`cpt-cf-oagw-dod-request-proxy-error-source`).
// @cpt-begin:cpt-cf-oagw-flow-error-handling-validation-render:p1:inst-eh-val-2
// `inst-eh-val-2` .. `-4`, `-9`: the enforcement-owned failures are rendered
// here through the one mapping layer — `400` for a body-shape or membership
// failure, `413` for a payload-size failure, `405` for the method allowlist —
// each non-retriable, gateway-sourced, and with the request left unforwarded.
// @cpt-begin:cpt-cf-oagw-flow-error-handling-validation-render:p1:inst-eh-val-1
// @cpt-begin:cpt-cf-oagw-flow-error-handling-validation-render:p1:inst-eh-val-3
// @cpt-begin:cpt-cf-oagw-flow-error-handling-validation-render:p1:inst-eh-val-4
// @cpt-begin:cpt-cf-oagw-flow-error-handling-validation-render:p1:inst-eh-val-5
// @cpt-begin:cpt-cf-oagw-flow-error-handling-validation-render:p1:inst-eh-val-6
// @cpt-begin:cpt-cf-oagw-flow-error-handling-validation-render:p1:inst-eh-val-7
// @cpt-begin:cpt-cf-oagw-flow-error-handling-validation-render:p1:inst-eh-val-8
// @cpt-begin:cpt-cf-oagw-flow-error-handling-validation-render:p1:inst-eh-val-9
#[must_use]
pub fn render_failure(failure: ProxyFailure) -> Response {
    match failure {
        ProxyFailure::Domain(error) => ApiError::Domain(error).into_response(),
        // A matched route whose method allowlist excludes the request method
        // is not a row of the shared error table, so the one serializer of the
        // error contract renders it and the handler adds only the `Allow`
        // header naming the methods the matching routes admit.
        ProxyFailure::MethodNotAllowed { path, allowed } => {
            let mut rendered = ProblemDetails::method_not_allowed(path, &allowed).into_response();
            let allowed = allowed.join(", ");
            if let Ok(value) = HeaderValue::from_str(&allowed) {
                rendered.headers_mut().insert("allow", value);
            }
            stamp_source(&mut rendered, ErrorSource::Gateway);
            rendered
        }
    }
}
//
// @cpt-end:cpt-cf-oagw-flow-error-handling-validation-render:p1:inst-eh-val-9
// @cpt-end:cpt-cf-oagw-flow-error-handling-validation-render:p1:inst-eh-val-8
// @cpt-end:cpt-cf-oagw-flow-error-handling-validation-render:p1:inst-eh-val-7
// @cpt-end:cpt-cf-oagw-flow-error-handling-validation-render:p1:inst-eh-val-6
// @cpt-end:cpt-cf-oagw-flow-error-handling-validation-render:p1:inst-eh-val-5
// @cpt-end:cpt-cf-oagw-flow-error-handling-validation-render:p1:inst-eh-val-4
// @cpt-end:cpt-cf-oagw-flow-error-handling-validation-render:p1:inst-eh-val-3
// @cpt-end:cpt-cf-oagw-flow-error-handling-validation-render:p1:inst-eh-val-1
//
// @cpt-end:cpt-cf-oagw-flow-error-handling-validation-render:p1:inst-eh-val-2

/// What one dispatch produced, for the emission that follows the response
/// (`cpt-cf-oagw-flow-observability-and-state-request-metrics`).
struct Dispatched {
    /// The correlation identifier the completing layer published.
    trace_id: Option<String>,
    /// The request method as the client sent it.
    method: String,
    /// The request target, gear-relative.
    path: String,
    /// The addressed alias, when the target named one.
    alias: Option<String>,
    /// The caller identity, and `None` for a request the authentication
    /// surface rejected.
    actor: Option<Actor>,
    /// The inbound body size in bytes.
    request_size: u64,
    /// The pipeline-boundary observation, when the pipeline produced one.
    observation: Option<ProxyObservation>,
    /// The GTS error type of a gateway failure, and `None` for a response the
    /// handler itself rendered without one.
    error_type: Option<&'static str>,
    /// Whether the exchange was refused by a rate limit.
    refused_by_rate_limit: bool,
}

/// The proxy handler of `/oagw/v1/proxy/{alias}[/{*path_suffix}]`.
///
/// The pipeline itself is [`dispatch`]; this wrapper is the observability
/// boundary of entry 2.9: it opens the in-flight gauge when the request is
/// taken, and once the response exists it records the request families and
/// emits the audit record — for every request the proxy path answers,
/// including one rejected before route matching
/// (`cpt-cf-oagw-flow-observability-and-state-request-metrics`,
/// `cpt-cf-oagw-flow-observability-and-state-audit-record`).
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation:p1:inst-os-req-1
// `inst-os-req-1`: the in-flight gauge opens when the handler takes the
// request and closes when the response is rendered, around the one dispatch
// every request runs through.
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation:p1:inst-os-req-2
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation:p1:inst-os-req-4
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation:p1:inst-os-req-5
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation:p1:inst-os-req-6
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation:p1:inst-os-req-7
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation:p1:inst-os-req-8
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation:p1:inst-os-req-8b
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation:p1:inst-os-req-9
pub async fn proxy(
    Extension(service): Extension<Arc<DataPlaneServiceImpl>>,
    observability: Option<Extension<Arc<Observability>>>,
    security: Option<Extension<SecurityContext>>,
    request: Request,
) -> Response {
    let started = Instant::now();
    let alias = target_of(request.uri().path()).map(|(alias, _)| alias);
    if let (Some(observability), Some(alias)) = (&observability, &alias) {
        observability.metrics.enter_in_flight(alias);
    }

    let (response, dispatched) = dispatch(service, security, request).await;

    if let Some(observability) = &observability {
        observe_outcome(observability, &dispatched, response.status().as_u16(), &started);
        if let Some(alias) = &alias {
            observability.metrics.leave_in_flight(alias);
        }
    }
    response
}
//
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation:p1:inst-os-req-9
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation:p1:inst-os-req-8b
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation:p1:inst-os-req-8
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation:p1:inst-os-req-7
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation:p1:inst-os-req-6
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation:p1:inst-os-req-5
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation:p1:inst-os-req-4
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation:p1:inst-os-req-2
//
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation:p1:inst-os-req-1

/// Record the families and emit the audit record of one completed exchange.
///
/// The request family is recorded for every request under the alias it
/// addressed, the phase histograms from the observation the pipeline filled,
/// and the audit record unconditionally
/// (`inst-os-req-2` .. `-5`, `inst-os-audit-1` .. `-5`).
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation:p1:inst-os-req-3
// `inst-os-req-3`: the request counter is recorded once the response status is
// known, with the alias as the `host` label, the normalized method and the
// normalized route pattern the pipeline resolved.
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-audit-record:p1:inst-os-audit-1
// `inst-os-audit-1` .. `-4`: the audit record is built from the request
// context at the point the response is complete, every field of the ADR field
// set filled, and no body, query parameter, header or credential echoed.
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-audit-record:p1:inst-os-audit-2
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-audit-record:p1:inst-os-audit-3
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-audit-record:p1:inst-os-audit-4
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-audit-record:p1:inst-os-audit-5
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-audit-record:p1:inst-os-audit-6
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-audit-record:p1:inst-os-audit-7
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-audit-record:p1:inst-os-audit-7b
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-audit-record:p1:inst-os-audit-8
fn observe_outcome(
    observability: &Observability,
    dispatched: &Dispatched,
    status: u16,
    started: &Instant,
) {
    let method = dispatched.method.as_str();
    let trace_id = dispatched.trace_id.as_deref().unwrap_or("");
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let response_size = dispatched.observation.as_ref().map_or(0, |o| o.response_size);
    if let Some(host) = dispatched.alias.as_deref() {
        match &dispatched.observation {
            Some(observation) => observability.record_observation(host, method, observation),
            None => {
                observability.metrics.record_request(host, method, None, status);
                if let Some(error_type) = dispatched.error_type {
                    observability.metrics.record_error(host, None, error_type);
                }
            }
        }
    }
    let actor = dispatched.actor.as_ref();
    match (dispatched.alias.as_deref(), actor) {
        (Some(host), Some(actor)) => observability.audit_proxy_request(
            trace_id,
            actor.tenant_id,
            actor.principal_id,
            Some(host),
            &dispatched.path,
            method,
            status,
            duration_ms,
            dispatched.request_size,
            response_size,
            dispatched.error_type,
            dispatched.refused_by_rate_limit,
        ),
        // A request the authentication surface rejected names no identity, so
        // the record is the `auth_failure` class under the identifier that
        // joins it to the rendered problem body (`inst-os-audit-2c`).
        _ => observability.audit.emit(&crate::infra::audit::AuditRecord::auth_failure(
            trace_id,
            &actor.map_or(uuid::Uuid::nil(), |a| a.tenant_id).to_string(),
            &actor.map_or(uuid::Uuid::nil(), |a| a.principal_id).to_string(),
            dispatched.alias.as_deref(),
            &dispatched.path,
            method,
            status,
            dispatched.error_type.unwrap_or("authentication.failed"),
        )),
    }
}
//
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-audit-record:p1:inst-os-audit-8
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-audit-record:p1:inst-os-audit-7b
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-audit-record:p1:inst-os-audit-7
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-audit-record:p1:inst-os-audit-6
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-audit-record:p1:inst-os-audit-5
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-audit-record:p1:inst-os-audit-4
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-audit-record:p1:inst-os-audit-3
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-audit-record:p1:inst-os-audit-2
//
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-proxy-request-instrumentation:p1:inst-os-req-3
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-audit-record:p1:inst-os-audit-1

/// The proxy pipeline of `/oagw/v1/proxy/{alias}[/{*path_suffix}]`, with the
/// outcome the observability boundary consumes.
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-dispatch:p1:inst-rp-dispatch-1
// `inst-rp-dispatch-1` .. `-4`, `-7` .. `-9`: the catch-all proxy handler —
// gear-relative, method-agnostic, authenticated, permission-checked, body
// framed, and dispatched to the data plane before any response is rendered.
async fn dispatch(
    service: Arc<DataPlaneServiceImpl>,
    security: Option<Extension<SecurityContext>>,
    mut request: Request,
) -> (Response, Dispatched) {
    let method = request.method().as_str().to_owned();
    let uri = request.uri().clone();
    let path = uri.path().to_owned();
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .map(|(name, value)| (name.as_str().to_owned(), value.to_str().unwrap_or("").to_owned()))
        .collect();
    // The `ip` scope discriminator is the connection peer address, and never a
    // client-supplied forwarding header (`inst-rl-key-9`).
    let peer_addr = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0.to_string());
    // The correlation identifier the completing layer minted or carried, which
    // the audit record and the problem body share (`inst-os-trace-3`).
    let trace_id = request
        .extensions()
        .get::<TraceIdentifier>()
        .map(|identifier| identifier.as_str().to_owned());
    let outcome = |actor: Option<Actor>,
                   request_size: u64,
                   observation: Option<ProxyObservation>,
                   error_type: Option<&'static str>,
                   refused_by_rate_limit: bool| {
        Dispatched {
            trace_id: trace_id.clone(),
            method: method.clone(),
            path: path.clone(),
            alias: target_of(&path).map(|(alias, _)| alias),
            actor,
            request_size,
            observation,
            error_type,
            refused_by_rate_limit,
        }
    };

    // Step 1: the preflight short-circuit, at the preflight-detection point of
    // the canonical order. The answer is built by the data plane's preflight
    // call-in with no plugin execution, no endpoint selection and no upstream
    // contact, and per-request auth is skipped for it by design; the caller
    // identity the platform edge already resolved is read when present so the
    // credential input can be delivered, and is never required.
    if is_preflight(&method, &headers) {
        let tenant = security.as_ref().and_then(|Extension(security)| {
            let tenant_id = security.subject_tenant_id();
            (!tenant_id.is_nil()).then_some(tenant_id)
        });
        let alias = target_of(uri.path()).map(|(alias, _)| alias);
        let response = render(service.preflight(alias.as_deref(), tenant, &headers));
        return (response, outcome(None, 0, None, None, false));
    }

    // Step 2: the caller identity.
    let actor = match actor_of(security) {
        Ok(actor) => actor,
        Err(error) => return (error.into_response(), outcome(None, 0, None, None, false)),
    };

    // Step 3: the permission gate, before any repository access or body read.
    let authorizer = service.authorizer();
    if let Err(denied) = authorizer
        .authorize(&actor, PERM_PROXY_INVOKE, PROXY_BASE_TYPE)
        .await
    {
        return (render_authorization(denied), outcome(Some(actor), 0, None, None, false));
    }

    // Step 4: the request target and the body framing.
    let Some((alias, path_suffix)) = target_of(uri.path()) else {
        return (
            ApiError::Domain(crate::domain::error::DomainError::field_rejection(
                "alias",
                "the proxy path does not address an alias",
            ))
            .into_response(),
            outcome(Some(actor), 0, None, None, false),
        );
    };
    if let Err(error) =
        body_validation::preflight(&method, &headers, service.max_body_size_bytes())
    {
        return (
            ApiError::Domain(error).into_response(),
            outcome(Some(actor), 0, None, Some(crate::gts::ERR_VALIDATION), false),
        );
    }

    // The upgrade handle is taken before the body is read: it resolves only
    // once the client's handshake completes, and the response must be returned
    // for the handshake to complete at all.
    let upgrade = crate::domain::headers::is_websocket_upgrade(&headers).then(|| {
        crate::infra::proxy::upgrade::InboundUpgrade {
            client: Box::pin(hyper::upgrade::on(&mut request)),
        }
    });

    let limit = usize::try_from(service.max_body_size_bytes()).unwrap_or(usize::MAX);
    let body = match axum::body::to_bytes(request.into_body(), limit).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return (
                ApiError::Domain(crate::domain::error::DomainError::PayloadTooLarge {
                    path: None,
                    trace_id: trace_id.clone(),
                    upstream_id: None,
                    limit_bytes: Some(service.max_body_size_bytes()),
                })
                .into_response(),
                outcome(Some(actor), 0, None, Some(crate::gts::ERR_PAYLOAD_TOO_LARGE), false),
            );
        }
    };
    // The observed framing is checked against the declared one now that the
    // body is in memory; the declared framing was checked before it was read.
    if let Err(error) =
        body_validation::validate(&method, &headers, body.len(), service.max_body_size_bytes())
    {
        return (
            ApiError::Domain(error).into_response(),
            outcome(
                Some(actor),
                body.len() as u64,
                None,
                Some(crate::gts::ERR_VALIDATION),
                false,
            ),
        );
    }

    let context = ProxyContext {
        method: method.clone(),
        alias,
        path_suffix,
        query: uri.query().map(str::to_owned),
        headers,
        body,
        tenant_id: actor.tenant_id,
        principal_id: actor.principal_id,
        peer_addr,
        trace_id: trace_id.clone(),
    };
    let context = with_trace_id(context);

    match service.execute(context, upgrade).await {
        Ok(response) => {
            let observation = response.observation.clone();
            let error_type = observation.error_type;
            let refused_by_rate_limit =
                observation.rate_limit.as_ref().is_some_and(|rate| rate.refused);
            let outcome = outcome(
                Some(actor),
                observation.request_size,
                Some(observation),
                error_type,
                refused_by_rate_limit,
            );
            (render(response), outcome)
        }
        Err(failure) => {
            let error_type = match &failure {
                ProxyFailure::Domain(error) => Some(crate::api::rest::error::mapping_of(error).0),
                ProxyFailure::MethodNotAllowed { .. } => None,
            };
            (render_failure(failure), outcome(Some(actor), 0, None, error_type, false))
        }
    }
}
// @cpt-end:cpt-cf-oagw-flow-request-proxy-dispatch:p1:inst-rp-dispatch-1

/// Append the `(name, value)` pairs the pipeline produced onto a rendered
/// response, without replacing the headers the renderer already set.
fn append_headers(response: &mut Response, headers: &[(String, String)]) {
    for (name, value) in headers {
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::try_from(name.as_str()),
            HeaderValue::from_str(value),
        ) {
            response.headers_mut().append(name, value);
        }
    }
}

/// Render an authorization outcome through the shared canonical surface.
fn render_authorization(denied: AuthorizeError) -> Response {
    ApiError::Authorization(denied).into_response()
}

/// Carry the inbound trace identifier on the context, when the caller
/// supplied one.
fn with_trace_id(mut context: ProxyContext) -> ProxyContext {
    if context.trace_id.is_none() {
        context.trace_id = header_value(&context.headers, TRACE_ID_HEADER).map(str::to_owned);
    }
    context
}

#[cfg(test)]
#[path = "proxy_observability_tests.rs"]
mod proxy_observability_tests;
