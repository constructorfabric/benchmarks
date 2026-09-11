//! Map a Gateway Failure to Its Response
//! (`cpt-cf-oagw-algo-proxy-map-error`).
//!
//! Every mapping here renders through [`crate::error::OagwError`] /
//! [`crate::error::OagwProblem`] -- the shared RFC 9457 renderer gear
//! foundation (2.1) provides -- so this is not a second envelope
//! implementation. The three target-host errors carry extension fields
//! (`alias`, `valid_hosts`, `invalid_value`) that `OagwProblem`'s fixed
//! field set does not express; [`render_with_extensions`] merges them onto
//! the rendered envelope's JSON body without inventing a parallel renderer.

use axum::http::{HeaderName, HeaderValue};
use axum::response::{IntoResponse, Response};
use serde_json::Value;

use crate::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER_NAME, OagwError, OagwErrorKind};
use crate::proxy::endpoint::TargetHostError;
use crate::proxy::forward::ForwardError;

/// Request-scoped fields known at failure time, populated wherever
/// available and omitted otherwise (`inst-proxy-err-extension-fields`).
#[derive(Debug, Clone, Default)]
pub(crate) struct ErrorFields {
    pub instance: String,
    pub upstream_id: Option<String>,
    pub host: Option<String>,
    pub path: Option<String>,
    pub trace_id: Option<String>,
}

// @cpt-begin:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-standard-fields
// @cpt-begin:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-extension-fields
fn base_error(kind: OagwErrorKind, detail: impl Into<String>, fields: &ErrorFields) -> OagwError {
    // `OagwError::render` (called by `IntoResponse`) populates the standard
    // RFC 9457 fields (`type`/`title`/`status`/`detail`/`instance`) from the
    // catalog and `detail`/`instance` below; the extension fields
    // (`upstream_id`/`host`/`path`/`trace_id`) are populated only when known
    // at failure time, never as a placeholder.
    let mut error = OagwError::new(kind, detail).with_instance(fields.instance.clone());
    if let Some(id) = &fields.upstream_id {
        error = error.with_upstream_id(id.clone());
    }
    if let Some(host) = &fields.host {
        error = error.with_host(host.clone());
    }
    if let Some(path) = &fields.path {
        error = error.with_path(path.clone());
    }
    if let Some(trace_id) = &fields.trace_id {
        error = error.with_trace_id(trace_id.clone());
    }
    error
}
// @cpt-end:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-extension-fields
// @cpt-end:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-standard-fields

/// Render `error`, then merge `extensions` onto the resulting JSON body --
/// the mechanism `inst-proxy-err-target-host` uses to carry `alias`,
/// `valid_hosts` and `invalid_value`, which `OagwProblem` has no field for.
fn render_with_extensions(error: OagwError, extensions: Value) -> Response {
    let problem = error.render();
    let status = problem.status;
    let mut body =
        serde_json::to_value(&problem).unwrap_or_else(|_| Value::Object(Default::default()));
    if let (Value::Object(body_map), Value::Object(ext_map)) = (&mut body, extensions) {
        for (key, value) in ext_map {
            body_map.insert(key, value);
        }
    }
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    let mut response = (
        axum::http::StatusCode::from_u16(status)
            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR),
        [(axum::http::header::CONTENT_TYPE, "application/problem+json")],
        bytes,
    )
        .into_response();
    response.headers_mut().insert(
        HeaderName::from_static(ERROR_SOURCE_HEADER_NAME),
        HeaderValue::from_static(ERROR_SOURCE_GATEWAY),
    );
    response
}

// @cpt-algo:cpt-cf-oagw-algo-proxy-map-error:p2
// @cpt-begin:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-route-error
pub(crate) fn bad_alias(fields: &ErrorFields, detail: &str) -> Response {
    base_error(OagwErrorKind::RouteError, detail.to_owned(), fields).into_response()
}
// @cpt-end:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-route-error

// @cpt-begin:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-validation
// @cpt-begin:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-render
// @cpt-begin:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-return
pub(crate) fn validation_error(fields: &ErrorFields, detail: &str) -> Response {
    // `OagwError::into_response` (gear foundation's shared renderer) emits
    // this as `application/problem+json` with `X-OAGW-Error-Source: gateway`
    // (`inst-proxy-err-render`), and the resulting `Response` is this
    // function's return value (`inst-proxy-err-return`).
    base_error(OagwErrorKind::ValidationError, detail.to_owned(), fields).into_response()
}
// @cpt-end:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-return
// @cpt-end:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-render
// @cpt-end:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-validation

// @cpt-begin:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-not-found
pub(crate) fn route_not_found(fields: &ErrorFields) -> Response {
    base_error(
        OagwErrorKind::RouteNotFound,
        "no upstream and enabled route matched this alias, method and path",
        fields,
    )
    .into_response()
}
// @cpt-end:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-not-found

// @cpt-begin:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-unavailable
pub(crate) fn upstream_disabled(fields: &ErrorFields) -> Response {
    base_error(
        OagwErrorKind::LinkUnavailable,
        "the resolved upstream's effective enabled state is false",
        fields,
    )
    .into_response()
}
// @cpt-end:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-unavailable

// @cpt-begin:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-too-large
pub(crate) fn payload_too_large(fields: &ErrorFields) -> Response {
    base_error(
        OagwErrorKind::PayloadTooLarge,
        "request body exceeds the 100 MB hard limit",
        fields,
    )
    .into_response()
}
// @cpt-end:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-too-large

// @cpt-begin:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-target-host
pub(crate) fn target_host_error(
    fields: &ErrorFields,
    alias: &str,
    error: &TargetHostError,
) -> Response {
    match error {
        TargetHostError::Missing { valid_hosts } => render_with_extensions(
            base_error(
                OagwErrorKind::MissingTargetHost,
                "X-OAGW-Target-Host is required to disambiguate this endpoint pool",
                fields,
            ),
            serde_json::json!({ "alias": alias, "valid_hosts": valid_hosts }),
        ),
        TargetHostError::Invalid { invalid_value } => render_with_extensions(
            base_error(
                OagwErrorKind::InvalidTargetHost,
                "X-OAGW-Target-Host is not a bare hostname or IP literal",
                fields,
            ),
            serde_json::json!({ "invalid_value": invalid_value }),
        ),
        TargetHostError::Unknown {
            invalid_value,
            valid_hosts,
        } => render_with_extensions(
            base_error(
                OagwErrorKind::UnknownTargetHost,
                "X-OAGW-Target-Host does not match any endpoint configured on this upstream",
                fields,
            ),
            serde_json::json!({ "invalid_value": invalid_value, "valid_hosts": valid_hosts }),
        ),
    }
}
// @cpt-end:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-target-host

// @cpt-begin:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-protocol
// @cpt-begin:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-downstream
// @cpt-begin:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-timeouts
pub(crate) fn forward_error(fields: &ErrorFields, error: &ForwardError) -> Response {
    let (kind, detail): (OagwErrorKind, &str) = match error {
        ForwardError::PlaintextRefused => (
            OagwErrorKind::ProtocolError,
            "plaintext upstream connections are refused by this gear's configuration",
        ),
        ForwardError::ConnectionTimeout => (
            OagwErrorKind::ConnectionTimeout,
            "the proxy_timeout_secs deadline expired before the connection was established",
        ),
        ForwardError::RequestTimeout => (
            OagwErrorKind::RequestTimeout,
            "the proxy_timeout_secs deadline expired before response headers were received",
        ),
        ForwardError::IdleTimeout => (
            OagwErrorKind::IdleTimeout,
            "the proxy_timeout_secs deadline expired while the response body was in flight",
        ),
        ForwardError::LinkUnavailable => (
            OagwErrorKind::LinkUnavailable,
            "the connection was refused, reset, or could not be established",
        ),
        ForwardError::ProtocolError => (
            OagwErrorKind::ProtocolError,
            "the upstream violated the HTTP protocol or returned an unparseable response",
        ),
        ForwardError::DownstreamBodyTooLarge => (
            OagwErrorKind::DownstreamError,
            "the upstream response could not be relayed intact",
        ),
    };
    base_error(kind, detail, fields).into_response()
}
// @cpt-end:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-timeouts
// @cpt-end:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-downstream
// @cpt-end:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-protocol

// @cpt-begin:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-not-mine
// This path never raises `401 AuthenticationFailed`, `429
// RateLimitExceeded`, `500 SecretNotFound`, `502 StreamAborted`, `503
// CircuitBreakerOpen`, `503 PluginNotFound` or `409 PluginInUse`: those
// belong to plugin execution (2.9), rate limiting (2.8), streaming (2.6)
// and plugin management (2.4) respectively, or (for `SecretNotFound`) are
// never raised by any feature in this decomposition round at all.
#[cfg(test)]
const NOT_RAISED_BY_THIS_PATH: [OagwErrorKind; 7] = [
    OagwErrorKind::AuthenticationFailed,
    OagwErrorKind::RateLimitExceeded,
    OagwErrorKind::SecretNotFound,
    OagwErrorKind::StreamAborted,
    OagwErrorKind::CircuitBreakerOpen,
    OagwErrorKind::PluginNotFound,
    OagwErrorKind::PluginInUse,
];
// @cpt-end:cpt-cf-oagw-algo-proxy-map-error:p2:inst-proxy-err-not-mine

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[test]
    fn not_raised_by_this_path_names_seven_distinct_kinds_owned_elsewhere() {
        let mut seen = std::collections::HashSet::new();
        for kind in NOT_RAISED_BY_THIS_PATH {
            assert!(
                seen.insert(format!("{kind:?}")),
                "duplicate kind in catalog"
            );
        }
        assert_eq!(NOT_RAISED_BY_THIS_PATH.len(), 7);
    }

    #[tokio::test]
    async fn target_host_missing_carries_alias_and_valid_hosts() {
        let fields = ErrorFields {
            instance: "/oagw/v1/proxy/vendor.com".to_owned(),
            ..Default::default()
        };
        let error = TargetHostError::Missing {
            valid_hosts: vec!["us.vendor.com".to_owned(), "eu.vendor.com".to_owned()],
        };
        let response = target_host_error(&fields, "vendor.com", &error);
        assert_eq!(response.status().as_u16(), 400);
        assert_eq!(
            response
                .headers()
                .get(ERROR_SOURCE_HEADER_NAME)
                .and_then(|v| v.to_str().ok()),
            Some(ERROR_SOURCE_GATEWAY)
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["alias"], "vendor.com");
        assert_eq!(
            json["valid_hosts"],
            serde_json::json!(["us.vendor.com", "eu.vendor.com"])
        );
        assert_eq!(
            json["type"],
            "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
        );
    }

    #[tokio::test]
    async fn target_host_invalid_carries_invalid_value() {
        let fields = ErrorFields::default();
        let error = TargetHostError::Invalid {
            invalid_value: "host:8443".to_owned(),
        };
        let response = target_host_error(&fields, "alias", &error);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["invalid_value"], "host:8443");
    }

    #[tokio::test]
    async fn forward_error_timeouts_map_to_documented_types() {
        let fields = ErrorFields::default();
        let response = forward_error(&fields, &ForwardError::RequestTimeout);
        assert_eq!(response.status().as_u16(), 504);
    }
}
