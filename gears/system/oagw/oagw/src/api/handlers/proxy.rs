// Created: 2026-08-31 by Constructor Tech
//! Proxy data-plane handlers (DESIGN §3.5).
//!
//! Both registered paths (`/oagw/v1/proxy/{alias}` and
//! `/oagw/v1/proxy/{alias}/{*path_suffix}`) are served by the same handler,
//! which reads the **raw** request URI: the alias and the suffix are taken
//! still percent-encoded, so the data plane decides what a path segment means
//! instead of inheriting a decoding it cannot undo. The `SecurityContext` the
//! auth middleware injected plus the request carry everything else.

use std::sync::Arc;

use axum::Extension;
use axum::extract::Request;
use axum::response::Response;
use toolkit_security::SecurityContext;
use tracing::Instrument;

use crate::domain::proxy::service::ProxyService;
use crate::error::{OagwError, OagwErrorKind, OagwResult, ResourceKind};

/// Prefix of both proxy paths on the wire.
const PROXY_PREFIX: &str = "/oagw/v1/proxy/";

/// Proxy a request addressed to the alias root or to a path behind it.
///
/// # Errors
/// Propagated from the data plane and rendered as problem+json with
/// `X-OAGW-Error-Source: gateway`.
pub async fn proxy_alias(
    Extension(ctx): Extension<SecurityContext>,
    Extension(svc): Extension<Arc<ProxyService>>,
    request: Request,
) -> OagwResult<Response> {
    forward(&ctx, &svc, request).await
}

/// Answer a method the data plane does not register (DESIGN §3.3).
///
/// `TRACE`, `CONNECT` and extension methods are not proxied. Without this
/// fallback axum would answer them with a bare `405 Method Not Allowed`,
/// outside the problem contract of the gear. Not proxied is not unaudited: the
/// same §4.3 record the registered methods get is emitted for this class too
/// (PRD §9 asks for a complete audit trail of every proxy request), it is only
/// never dialled, which is why it has no upstream status and no size to name.
///
/// # Errors
/// Always: the single `route.not_found.v1` problem of the data plane.
pub async fn proxy_unregistered_method(
    Extension(ctx): Extension<SecurityContext>,
    request: Request,
) -> OagwResult<Response> {
    let (parts, body) = request.into_parts();
    let opened = opened(&ctx, &parts);
    drop(body);
    let error = route_not_found(&opened.alias);
    let status = error.status();
    opened
        .audit
        .emit(opened.started, status, None, Some(&error));
    opened.span.record("status", status);
    Err(stamp(
        &opened.alias,
        &opened.path,
        opened.trace_id.as_deref(),
        error,
    ))
}

/// Hand the request to the data plane and record the outcome.
///
/// One span carries the method, the alias, the tenant and the status, and the
/// audit event adds the wall-clock duration. The request and response bodies
/// are never logged, and neither is a header: the span names what the data
/// plane decided, not what the client sent.
async fn forward(
    ctx: &SecurityContext,
    svc: &ProxyService,
    request: Request,
) -> OagwResult<Response> {
    let (parts, body) = request.into_parts();
    let opened = opened(ctx, &parts);
    let (outcome, status) = async {
        match svc
            .proxy(
                ctx,
                &opened.alias,
                &opened.path,
                http::Request::from_parts(parts, body),
            )
            .await
        {
            Ok(response) => {
                let status = response.status().as_u16();
                opened.audit.emit(
                    opened.started,
                    status,
                    // A streamed body declares no length, so the field is
                    // omitted rather than counted from what happened to arrive.
                    declared_length(response.headers().get(http::header::CONTENT_LENGTH)),
                    None,
                );
                (Ok(response), status)
            }
            Err(error) => {
                let status = error.status();
                // Nothing was forwarded, and the problem document is rendered
                // after this call, so there is no response size to name.
                opened
                    .audit
                    .emit(opened.started, status, None, Some(&error));
                (
                    Err(stamp(
                        &opened.alias,
                        &opened.path,
                        opened.trace_id.as_deref(),
                        error,
                    )),
                    status,
                )
            }
        }
    }
    .instrument(opened.span.clone())
    .await;
    opened.span.record("status", status);
    outcome
}

/// The audit trail's view of one request, taken off the wire once.
///
/// Both entries of the data plane — the handler of the registered methods and
/// the router's fallback for the unregistered ones — open the same span and
/// build the same record, so a request that is refused before it is dialled is
/// still a request the trail can account for.
struct Opened {
    /// Upstream alias the request was addressed to.
    alias: String,
    /// Request path without its query.
    path: String,
    /// Span the audit event of this request is emitted in.
    span: tracing::Span,
    /// Moment the request entered the data plane.
    started: tokio::time::Instant,
    /// The record the outcome is emitted into.
    audit: Audit,
    /// Correlation id, also stamped onto a returned problem document.
    trace_id: Option<String>,
}

/// Open the span and the record of one request.
///
/// The correlation id is the one the request's trace headers name and, when the
/// client sent none, the id of the span it lives in: §4.3 wants a correlation id
/// on every record.
fn opened(ctx: &SecurityContext, parts: &http::request::Parts) -> Opened {
    let (alias, suffix) = split_request_path(&parts.uri);
    // A span names the alias the request was addressed to and nothing a client
    // invented: the raw `Host` header is a header, and headers are not logged.
    let span = tracing::info_span!(
        "oagw_proxy_request",
        method = %parts.method,
        alias = %alias,
        status = tracing::field::Empty,
        tenant_id = %ctx.subject_tenant_id(),
    );
    // The correlation id comes from the trace headers when the client sent one
    // and from the id of the span it is being served in when it did not: 4.3
    // wants a correlation id on every record. The span's own id is read off the
    // handle rather than out of the current context, so the fallback does not
    // depend on the caller having entered the span.
    let trace_id = crate::api::error::correlation_id(&parts.headers)
        .or_else(|| span.id().map(|id| id.into_u64().to_string()));
    // The declared length is the size the data plane verifies before it
    // buffers, so it is the size the gateway actually served; a request that
    // streams (chunked, no length) declares none and omits the field.
    let request_size = declared_length(parts.headers.get(http::header::CONTENT_LENGTH));
    let path = format!("/{suffix}");
    let audit = Audit {
        host: alias.clone(),
        path: path.clone(),
        method: parts.method.clone(),
        request_id: trace_id.clone(),
        tenant_id: ctx.subject_tenant_id(),
        principal_id: ctx.subject_id(),
        request_size,
    };
    Opened {
        alias,
        path,
        span,
        started: tokio::time::Instant::now(),
        audit,
        trace_id,
    }
}

/// The one audit record of a proxied request, at the level the caller chose.
///
/// A tracing event carries its level in a `static` callsite, so the level has
/// to be known to the macro and cannot come out of a variable: the field set of
/// §4.3 is therefore spelled once here and expanded once per severity, instead
/// of being emitted at one fixed level with a second, competing `level` field.
///
/// `audit` is the record, `started` the moment the request entered the data
/// plane, `status` the status the client was answered with, `response_size` the
/// length the upstream declared (absent when it streamed), `error_type` and
/// `error_message` the problem type and detail of a failure, both absent on
/// success.
macro_rules! audit_event {
    ($level:expr, $audit:expr, $started:expr, $status:expr, $response_size:expr, $error_type:expr, $error_message:expr) => {
        tracing::event!(
            $level,
            event = "oagw_proxy_request",
            request_id = $audit.request_id.as_deref(),
            tenant_id = %$audit.tenant_id,
            principal_id = %$audit.principal_id,
            host = %$audit.host,
            path = %$audit.path,
            method = %$audit.method,
            status = $status,
            duration_ms =
                u64::try_from($started.elapsed().as_millis()).unwrap_or(u64::MAX),
            request_size = $audit.request_size,
            response_size = $response_size,
            error_type = $error_type,
            error_message = $error_message,
            "proxied request"
        )
    };
}

/// One audit record of a proxied request (DESIGN §4.3).
///
/// The record carries no body, no query string and no header value: `path` is
/// the request path without its query and `host` the upstream alias. The
/// response size cannot be known when the record is built — the answer has not
/// happened yet — so [`Audit::emit`] takes it as an argument.
struct Audit {
    /// Upstream alias the request was addressed to (§4.3 `host`).
    host: String,
    /// Request path without its query.
    path: String,
    method: http::Method,
    /// Correlation id the trace headers gave the request.
    request_id: Option<String>,
    tenant_id: uuid::Uuid,
    /// The authenticated subject: the user, service or system the platform
    /// identified. §4.3 calls it `principal_id`; `SecurityContext` has no
    /// other identity than this one.
    principal_id: uuid::Uuid,
    /// Declared size of the request body, when the request declared one.
    request_size: Option<u64>,
}

impl Audit {
    /// Emit the record, at the level its outcome asks for.
    ///
    /// `response_size` is the declared length of the upstream's answer, when
    /// it declared one; only the upstream knows it, so it arrives here rather
    /// than in the record.
    ///
    /// `timestamp` and `level` are the tracing subscriber's contribution: the
    /// JSON formatter stamps both on every record, so the event carries a
    /// level as its metadata instead of a second, competing one as a field.
    fn emit(
        &self,
        started: tokio::time::Instant,
        status: u16,
        response_size: Option<u64>,
        failure: Option<&OagwError>,
    ) {
        let error_type = failure.map(OagwError::gts_type);
        let error_message = failure.map(OagwError::detail);
        match severity_of(failure.map(OagwError::kind)) {
            Severity::Info => self.answered(
                started,
                status,
                response_size,
                error_type.as_deref(),
                error_message,
            ),
            Severity::Warn => self.limited(
                started,
                status,
                response_size,
                error_type.as_deref(),
                error_message,
            ),
            Severity::Error => self.failed(
                started,
                status,
                response_size,
                error_type.as_deref(),
                error_message,
            ),
        }
    }

    /// The record of a request the gateway answered, at §4.3's `INFO`.
    fn answered(
        &self,
        started: tokio::time::Instant,
        status: u16,
        response_size: Option<u64>,
        error_type: Option<&str>,
        error_message: Option<&str>,
    ) {
        audit_event!(
            tracing::Level::INFO,
            self,
            started,
            status,
            response_size,
            error_type,
            error_message
        );
    }

    /// The record of a refusal that limited the caller, at §4.3's `WARN`.
    fn limited(
        &self,
        started: tokio::time::Instant,
        status: u16,
        response_size: Option<u64>,
        error_type: Option<&str>,
        error_message: Option<&str>,
    ) {
        audit_event!(
            tracing::Level::WARN,
            self,
            started,
            status,
            response_size,
            error_type,
            error_message
        );
    }

    /// The record of a failure an operator has to look at, at §4.3's `ERROR`.
    fn failed(
        &self,
        started: tokio::time::Instant,
        status: u16,
        response_size: Option<u64>,
        error_type: Option<&str>,
        error_message: Option<&str>,
    ) {
        audit_event!(
            tracing::Level::ERROR,
            self,
            started,
            status,
            response_size,
            error_type,
            error_message
        );
    }
}

/// The three levels an audit record can carry (DESIGN §4.3 "Log Levels").
///
/// A dedicated enum rather than `tracing::Level` because the event only has
/// three honest outcomes; `Trace` and `Debug` are not among them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Severity {
    /// The request was answered, the gateway having done what was asked.
    Info,
    /// The refusal limited the caller.
    Warn,
    /// Something an operator has to look at failed.
    Error,
}

/// Severity of one audit record, from its outcome (DESIGN §4.3 "Log Levels").
///
/// A success and an answered request — a 400, a 404, a CORS refusal, a body
/// the client oversized — are `INFO`: the gateway did what was asked of it. A
/// refusal that limits a client is `WARN`, and a failure the *operator* has to
/// look at is `ERROR`: the upstream failed or timed out, the credentials did
/// not resolve, or a dependency of the gateway itself (a bound plugin, the
/// credential store) was not there.
fn severity_of(failure: Option<&OagwErrorKind>) -> Severity {
    let Some(kind) = failure else {
        return Severity::Info;
    };
    match kind {
        OagwErrorKind::RateLimitExceeded | OagwErrorKind::CircuitBreakerOpen => Severity::Warn,
        OagwErrorKind::AuthenticationFailed
        | OagwErrorKind::SecretNotFound
        | OagwErrorKind::PluginNotFound
        | OagwErrorKind::LinkUnavailable
        | OagwErrorKind::ConnectionTimeout
        | OagwErrorKind::RequestTimeout
        | OagwErrorKind::IdleTimeout
        | OagwErrorKind::StreamAborted
        | OagwErrorKind::DownstreamError
        | OagwErrorKind::ProtocolError
        | OagwErrorKind::Internal => Severity::Error,
        OagwErrorKind::Validation
        | OagwErrorKind::MissingTargetHost
        | OagwErrorKind::InvalidTargetHost
        | OagwErrorKind::UnknownTargetHost
        | OagwErrorKind::NotFound
        | OagwErrorKind::PayloadTooLarge
        | OagwErrorKind::CorsOriginNotAllowed
        | OagwErrorKind::CorsMethodNotAllowed
        | OagwErrorKind::AliasConflict
        | OagwErrorKind::RouteConflict
        | OagwErrorKind::PluginConflict
        | OagwErrorKind::PluginInUse => Severity::Info,
    }
}

/// Declared body length of a `Content-Length` header, when it parses.
fn declared_length(header: Option<&http::HeaderValue>) -> Option<u64> {
    header?.to_str().ok()?.trim().parse().ok()
}

/// Stamp the request context onto a failure the data plane could not annotate.
///
/// `instance` is the request path and `trace_id` the correlation id the
/// management router derives from the same headers (ADR-0007).
fn stamp(alias: &str, request_path: &str, trace_id: Option<&str>, error: OagwError) -> OagwError {
    error.with_extension(|ext| {
        if ext.alias.is_none() {
            ext.alias = Some(alias.to_owned());
        }
        if ext.path.is_none() {
            ext.path = Some(request_path.to_owned());
        }
        if ext.trace_id.is_none() {
            ext.trace_id = trace_id.map(str::to_owned);
        }
        if ext.instance.is_none() {
            ext.instance = Some(request_path.to_owned());
        }
    })
}

/// Split the raw request path into the alias and the still-encoded suffix.
///
/// `/oagw/v1/proxy/api.vendor.com/v1/chat` → `api.vendor.com`, `v1/chat`; the
/// alias root has an empty suffix.
fn split_request_path(uri: &http::Uri) -> (String, String) {
    let Some(rest) = uri.path().strip_prefix(PROXY_PREFIX) else {
        return (String::new(), String::new());
    };
    match rest.split_once('/') {
        Some((alias, suffix)) => (decode_alias(alias), suffix.to_owned()),
        None => (decode_alias(rest), String::new()),
    }
}

/// Percent-decode and normalise the alias of a request.
///
/// The write path stores aliases lowercased and without a trailing dot, and
/// the data plane matches them the same way (DESIGN §3.2 "Alias Resolution").
/// Anything that decodes to a value an alias can never have simply fails to
/// resolve, which is the 404 the contract asks for.
fn decode_alias(raw: &str) -> String {
    let decoded = crate::domain::proxy::routing::percent_decode(raw);
    crate::domain::alias::normalize(&decoded)
}

/// The single 404 of the data plane, for a request the router never matched.
fn route_not_found(alias: &str) -> OagwError {
    OagwError::new(
        OagwErrorKind::NotFound,
        format!("no upstream of the calling tenant answers to the alias '{alias}'"),
    )
    .with_resource(ResourceKind::Route)
    .with_extension(|ext| {
        ext.alias = Some(alias.to_owned());
    })
}
