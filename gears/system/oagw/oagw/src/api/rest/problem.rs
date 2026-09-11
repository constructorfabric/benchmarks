//! RFC 9457 mapping of a [`DomainError`] onto an HTTP response.
//!
//! Realizes `cpt-cf-oagw-algo-error-mapping`. A gateway-sourced failure
//! becomes `application/problem+json` with the catalogue row's `type`, `title`
//! and `status`, the caller's `detail`, the request URI as `instance`, and every
//! present [`ErrorContext`] member as an extension field. An upstream-sourced
//! failure is passed through untouched — never rewritten into a problem body.

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode};
use axum::response::Response;
use serde_json::{Map, Value};
use toolkit_canonical_errors::CanonicalError;

use crate::domain::error::{DomainError, ErrorContext};

/// `X-OAGW-Error-Source` — distinguishes a gateway failure from an upstream one.
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";

/// Content type of a gateway-sourced problem document.
const PROBLEM_JSON: &str = "application/problem+json";

/// The reason the permission check states when it refuses a request.
const DENY_REASON: &str = "INSUFFICIENT_PERMISSION";

/// The detail a refused permission answers with.
///
/// The property name of the permission and the resource type are named in the
/// document's own fields; no request-body value appears anywhere in it.
const DENY_DETAIL: &str = "the bearer token lacks the permission the operation requires";

/// The reason a refused descendant override permission states.
const OVERRIDE_DENY_REASON: &str = "OVERRIDE_PERMISSION_DENIED";

/// The detail a refused descendant override permission answers with.
///
/// It names neither the family the body carried nor the mode any ancestor
/// declared: a refused caller learns nothing about which families its ancestor
/// enforces, and nothing it did not already state itself about which families
/// it tried to set.
const OVERRIDE_DENY_DETAIL: &str =
    "the operation requires a descendant permission the bearer token does not hold";

/// The detail a persistence failure answers with.
///
/// The reason is a server-side concern: it is logged with the correlation
/// identifier and never placed on the wire.
const STORAGE_DETAIL: &str = "the configuration store could not apply the operation";

/// Builds the RFC 9457 problem document for a gateway-sourced failure.
///
/// `type` is the catalogue row's GTS identifier, `title` and `status` come from
/// the row, `detail` is the caller's text verbatim, `instance` is the request
/// URI the caller passes in, and every present [`ErrorContext`] member becomes
/// an extension field.
#[must_use]
pub fn problem_document(error: &DomainError, instance: &str) -> Value {
    // @cpt-begin:cpt-cf-oagw-algo-error-mapping:p1:inst-errmap-problem
    let mut body = Map::new();
    body.insert(String::from("type"), Value::from(error.gts_type()));
    body.insert(String::from("title"), Value::from(error.kind.title()));
    body.insert(
        String::from("status"),
        Value::from(u64::from(error.http_status())),
    );
    body.insert(String::from("detail"), Value::from(error.detail.as_str()));
    body.insert(String::from("instance"), Value::from(instance));
    // @cpt-begin:cpt-cf-oagw-algo-error-mapping:p1:inst-errmap-extensions
    extend_with_context(&mut body, &error.context);
    // @cpt-end:cpt-cf-oagw-algo-error-mapping:p1:inst-errmap-extensions
    Value::Object(body)
    // @cpt-end:cpt-cf-oagw-algo-error-mapping:p1:inst-errmap-problem
}

/// Builds a gateway-sourced RFC 9457 problem response.
///
/// `Retry-After` is emitted only for the six retriable catalogue rows and only
/// when the context carries a delay; no delay is ever invented.
#[must_use]
pub fn problem_response(error: &DomainError, instance: &str) -> Response {
    // @cpt-begin:cpt-cf-oagw-algo-error-mapping:p1:inst-errmap-gateway-if
    // @cpt-begin:cpt-cf-oagw-algo-response-classify:p1:inst-cls-gateway-map
    // The gateway-sourced row of the classification: the answer is mapped
    // through `cpt-cf-oagw-algo-error-mapping`, which resolves the variant's
    // HTTP status and GTS `type` identifier, emits the problem body, and sets
    // `X-OAGW-Error-Source: gateway`.
    let status = status_code(error.http_status());
    let mut response = Response::builder()
        .status(status)
        .header(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static(PROBLEM_JSON),
        )
        .body(Body::from(problem_document(error, instance).to_string()))
        .unwrap_or_else(|_| Response::new(Body::empty()));
    // @cpt-end:cpt-cf-oagw-algo-response-classify:p1:inst-cls-gateway-map

    // @cpt-begin:cpt-cf-oagw-algo-response-classify:p1:inst-cls-gateway-context
    // The `ErrorContext` members that are present ride the problem body's
    // extension fields, per DESIGN §3.3's extension list, and a member with no
    // value is omitted rather than emitted empty.
    // @cpt-end:cpt-cf-oagw-algo-response-classify:p1:inst-cls-gateway-context

    // @cpt-begin:cpt-cf-oagw-algo-error-mapping:p1:inst-errmap-gateway-header
    response.headers_mut().insert(
        ERROR_SOURCE_HEADER,
        HeaderValue::from_static("gateway"),
    );
    // @cpt-end:cpt-cf-oagw-algo-error-mapping:p1:inst-errmap-gateway-header

    // @cpt-begin:cpt-cf-oagw-algo-response-classify:p1:inst-cls-gateway-retry
    // `Retry-After` is emitted only for the catalogue rows DESIGN §3.3 marks
    // retriable and only when `retry_after_seconds` is present, which is the
    // mapping `cpt-cf-oagw-algo-error-mapping` already performs.
    // @cpt-end:cpt-cf-oagw-algo-response-classify:p1:inst-cls-gateway-retry

    // @cpt-begin:cpt-cf-oagw-algo-error-mapping:p1:inst-errmap-retry-if
    if let Some(seconds) = error.retry_after_seconds() {
        // @cpt-begin:cpt-cf-oagw-algo-error-mapping:p1:inst-errmap-retry-emit
        apply_retry_after(&mut response, seconds);
        // @cpt-end:cpt-cf-oagw-algo-error-mapping:p1:inst-errmap-retry-emit
    }
    // @cpt-end:cpt-cf-oagw-algo-error-mapping:p1:inst-errmap-retry-if

    // @cpt-begin:cpt-cf-oagw-algo-error-mapping:p1:inst-errmap-return
    response
    // @cpt-end:cpt-cf-oagw-algo-error-mapping:p1:inst-errmap-return
    // @cpt-end:cpt-cf-oagw-algo-error-mapping:p1:inst-errmap-gateway-if
}

/// Builds an upstream-sourced response: the status, body and content type the
/// upstream produced, tagged with `X-OAGW-Error-Source: upstream`.
///
/// The invariant is that an upstream failure is never rewritten into
/// `application/problem+json` and never gains a `Retry-After`.
#[must_use]
pub fn passthrough_response(status: u16, body: Body, content_type: Option<&str>) -> Response {
    // @cpt-begin:cpt-cf-oagw-algo-error-mapping:p1:inst-errmap-else
    // @cpt-begin:cpt-cf-oagw-algo-error-mapping:p1:inst-errmap-upstream
    let mut builder = Response::builder()
        .status(status_code(status))
        .header(ERROR_SOURCE_HEADER, HeaderValue::from_static("upstream"));

    if let Some(content_type) = content_type {
        builder = builder.header(axum::http::header::CONTENT_TYPE, content_type);
    }

    builder
        .body(body)
        .unwrap_or_else(|_| Response::new(Body::empty()))
    // @cpt-end:cpt-cf-oagw-algo-error-mapping:p1:inst-errmap-upstream
    // @cpt-end:cpt-cf-oagw-algo-error-mapping:p1:inst-errmap-else
}

/// Builds the 403 response a refused permission answers with.
///
/// The permission family this feature enforces is not a row of the OAGW error
/// catalogue, so `type`, `title` and `status` come from the platform's
/// canonical catalogue and the enforced resource type is named in the context.
/// The detail is a constant: no request-body value, no tenant, no identifier.
#[must_use]
pub fn forbidden_response(resource_type: &'static str, instance: &str) -> Response {
    let document = permission_denied(resource_type);
    gateway_response(StatusCode::FORBIDDEN, &document, instance)
}

/// Builds the 403 response a refused descendant override permission answers
/// with.
///
/// The four `oagw:upstream:*` permissions are not rows of the OAGW error
/// catalogue either, so `type`, `title` and `status` come from the platform's
/// canonical catalogue and the enforced resource type is named in the context.
/// The detail is a constant that names no family and no sharing mode.
#[must_use]
pub fn forbidden_permission_response(resource_type: &'static str, instance: &str) -> Response {
    let error = toolkit_canonical_errors::ResourceErrorBuilder::__permission_denied(
        resource_type,
        OVERRIDE_DENY_DETAIL,
    )
    .with_reason(OVERRIDE_DENY_REASON)
    .create();
    let document = canonical_document(&error);
    gateway_response(StatusCode::FORBIDDEN, &document, instance)
}

/// Builds the 500 response a persistence failure answers with.
///
/// A storage failure is never a [`DomainError`] catalogue variant of this
/// feature: the document carries the platform's internal-error row, the
/// `X-OAGW-Error-Source: gateway` header, and a constant detail. The reason the
/// store produced is logged by the handler that observed it, not placed here.
#[must_use]
pub fn storage_problem_response(instance: &str) -> Response {
    let document = internal_error();
    gateway_response(StatusCode::INTERNAL_SERVER_ERROR, &document, instance)
}

/// Builds the bare 403 problem response a CORS refusal answers with.
///
/// A CORS refusal is not a [`DomainError`] catalogue variant either: DESIGN
/// §3.3's catalogue is closed at 22 variants over 21 identifiers and carries no
/// 403 row, so `type`, `title`, and `status` come from the two identifiers ADR
/// 0004 spells and the detail is the caller's own rendering of the offending
/// value. The document is written on the same problem-body path every gateway
/// answer takes and carries the `X-OAGW-Error-Source: gateway` tag of it.
#[must_use]
pub fn bare_forbidden_response(
    gts_type: &'static str,
    title: &'static str,
    detail: &str,
    instance: &str,
) -> Response {
    let mut body = Map::new();
    body.insert(String::from("type"), Value::from(gts_type));
    body.insert(String::from("title"), Value::from(title));
    body.insert(String::from("status"), Value::from(403u64));
    body.insert(String::from("detail"), Value::from(detail));
    gateway_response(StatusCode::FORBIDDEN, &Value::Object(body), instance)
}

/// The canonical permission-denied row, with the enforced resource type named.
fn permission_denied(resource_type: &'static str) -> Value {
    let error = toolkit_canonical_errors::ResourceErrorBuilder::__permission_denied(
        resource_type,
        DENY_DETAIL,
    )
    .with_reason(DENY_REASON)
    .create();
    canonical_document(&error)
}

/// The canonical internal-error row.
fn internal_error() -> Value {
    let error = CanonicalError::internal(STORAGE_DETAIL).create();
    canonical_document(&error)
}

/// The problem document of a canonical catalogue row: the row's `type`, `title`
/// and `status`, its `detail`, and its context members as extension fields.
fn canonical_document(error: &CanonicalError) -> Value {
    let mut body = Map::new();
    body.insert(String::from("type"), Value::from(error.gts_type()));
    body.insert(String::from("title"), Value::from(error.title()));
    body.insert(
        String::from("status"),
        Value::from(u64::from(error.status_code())),
    );
    body.insert(String::from("detail"), Value::from(error.detail()));
    body.insert(
        String::from("resource_type"),
        Value::from(error.resource_type().unwrap_or_default()),
    );
    Value::Object(body)
}

/// The response shape every gateway-sourced problem document shares.
fn gateway_response(status: StatusCode, document: &Value, instance: &str) -> Response {
    let mut body = match document {
        Value::Object(fields) => fields.clone(),
        _ => Map::new(),
    };
    body.insert(String::from("instance"), Value::from(instance));

    Response::builder()
        .status(status)
        .header(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static(PROBLEM_JSON),
        )
        .header(ERROR_SOURCE_HEADER, HeaderValue::from_static("gateway"))
        .body(Body::from(Value::Object(body).to_string()))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

/// The `StatusCode` of a catalogue row, falling back to `500` for a value the
/// HTTP grammar cannot express.
fn status_code(status: u16) -> StatusCode {
    StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

/// Adds every present [`ErrorContext`] member as an extension field; absent
/// members add nothing.
fn extend_with_context(body: &mut Map<String, Value>, context: &ErrorContext) {
    if let Some(upstream_id) = context.upstream_id {
        body.insert(
            String::from("upstream_id"),
            Value::from(upstream_id.to_string()),
        );
    }
    if let Some(host) = &context.host {
        body.insert(String::from("host"), Value::from(host.as_str()));
    }
    if let Some(path) = &context.path {
        body.insert(String::from("path"), Value::from(path.as_str()));
    }
    if let Some(seconds) = context.retry_after_seconds {
        body.insert(String::from("retry_after_seconds"), Value::from(seconds));
    }
    if let Some(trace_id) = &context.trace_id {
        body.insert(String::from("trace_id"), Value::from(trace_id.as_str()));
    }
}

/// Sets `Retry-After` on a response.
fn apply_retry_after(response: &mut Response, seconds: u64) {
    if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
        response
            .headers_mut()
            .insert(axum::http::header::RETRY_AFTER, value);
    }
}
