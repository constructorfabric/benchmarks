//! The single error contract of every `/oagw/v1/...` endpoint (FEATURE
//! entry 2.5, `cpt-cf-oagw-dod-error-handling-domain-error-taxonomy`).
//!
//! One mapping layer resolves a [`DomainError`] variant onto the HTTP status,
//! the GTS error type, the retriable flag, the retry-guidance source and the
//! extension-field set of the DESIGN §3.3 table, and one serializer renders
//! the RFC 9457 `application/problem+json` body. No endpoint maps an error
//! locally and no other module in the crate renders an error body.
//!
//! # Body shape
//!
//! Exactly the five RFC 9457 members `type`, `title`, `status`, `detail` and
//! `instance`, plus the closed OAGW extension vocabulary `upstream_id`, `host`,
//! `path`, `retry_after_seconds`, `trace_id`, `referenced_by` and the
//! target-host routing family `alias`, `valid_hosts`, `invalid_value` —
//! **at the top level of the JSON object**, never nested under a `context`
//! member. An unknown member is omitted, never emitted as `null`.
//!
//! [`toolkit_canonical_errors::Problem`] nests extension members under
//! `context` and can only mint the 13 canonical categories, so it cannot carry
//! the OAGW types; [`ProblemDetails`] below is therefore the serializer of
//! this gear. The one exception is the shared canonical *permission-denied*
//! surface of a denied authorization decision, which the FEATURE requires to
//! stay on the platform's surface — no OAGW 403 type exists.
//!
//! # Headers
//!
//! * `Content-Type: application/problem+json` on every body this module
//!   renders;
//! * `X-OAGW-Error-Source: gateway` on every body this module renders, which
//!   is what makes the 2.4 emission point and this one agree by construction;
//! * `Retry-After` together with the `retry_after_seconds` member, and with
//!   the same value, only for a retriable type that carries guidance;
//! * `Vary: Origin` on the two 403 CORS rejection types.
//!
//! # Credential isolation
//!
//! No variant stores a credential value, so no body can leak one. The only
//! request-derived value ever echoed is the `X-OAGW-Target-Host` header value,
//! bounded by [`crate::domain::validation::bound_invalid_value`], emitted
//! through the JSON serializer only and never concatenated into `detail` or
//! into a header value.
// @cpt-flow:cpt-cf-oagw-flow-error-handling-disabled-upstream:p1
// @cpt-flow:cpt-cf-oagw-flow-error-handling-streaming-source:p1
// @cpt-algo:cpt-cf-oagw-algo-error-handling-source-header:p1
// @cpt-algo:cpt-cf-oagw-algo-error-handling-validation-render:p1
// @cpt-dod:cpt-cf-oagw-dod-error-handling-error-source-header:p1
// @cpt-dod:cpt-cf-oagw-dod-error-handling-upstream-passthrough:p1

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use toolkit_canonical_errors::problem::APPLICATION_PROBLEM_JSON;
use toolkit_canonical_errors::builder::{NeedsReason, ResourceAbsent, ResourceErrorBuilder};
use toolkit_canonical_errors::{resource_error, CanonicalError, Http};
use toolkit_gts::gts_uri;

use crate::domain::error::{DomainError, ReferencedBy};
use crate::domain::gts_helpers as gts;
use crate::domain::services::management::{AuthorizeError, ManagementError};

// @cpt-begin:cpt-cf-oagw-algo-error-handling-source-header:p1:inst-eh-src-1
// @cpt-begin:cpt-cf-oagw-algo-error-handling-source-header:p1:inst-eh-src-2
// @cpt-begin:cpt-cf-oagw-algo-error-handling-source-header:p1:inst-eh-src-3
// @cpt-begin:cpt-cf-oagw-algo-error-handling-source-header:p1:inst-eh-src-4
// @cpt-begin:cpt-cf-oagw-algo-error-handling-source-header:p1:inst-eh-src-5
// @cpt-begin:cpt-cf-oagw-algo-error-handling-source-header:p1:inst-eh-src-6
// @cpt-begin:cpt-cf-oagw-algo-error-handling-validation-render:p1:inst-eh-vrender-1
// @cpt-begin:cpt-cf-oagw-algo-error-handling-validation-render:p1:inst-eh-vrender-2
// @cpt-begin:cpt-cf-oagw-algo-error-handling-validation-render:p1:inst-eh-vrender-3
// @cpt-begin:cpt-cf-oagw-algo-error-handling-validation-render:p1:inst-eh-vrender-4
// @cpt-begin:cpt-cf-oagw-algo-error-handling-validation-render:p1:inst-eh-vrender-5
// @cpt-begin:cpt-cf-oagw-algo-error-handling-validation-render:p1:inst-eh-vrender-6
// @cpt-begin:cpt-cf-oagw-algo-error-handling-validation-render:p1:inst-eh-vrender-7
// @cpt-begin:cpt-cf-oagw-flow-error-handling-disabled-upstream:p1:inst-eh-dis-1
// @cpt-begin:cpt-cf-oagw-flow-error-handling-disabled-upstream:p1:inst-eh-dis-2
// @cpt-begin:cpt-cf-oagw-flow-error-handling-disabled-upstream:p1:inst-eh-dis-3
// @cpt-begin:cpt-cf-oagw-flow-error-handling-disabled-upstream:p1:inst-eh-dis-4
// @cpt-begin:cpt-cf-oagw-flow-error-handling-disabled-upstream:p1:inst-eh-dis-5
// @cpt-begin:cpt-cf-oagw-flow-error-handling-disabled-upstream:p1:inst-eh-dis-6
// @cpt-begin:cpt-cf-oagw-flow-error-handling-disabled-upstream:p1:inst-eh-dis-7
// @cpt-begin:cpt-cf-oagw-flow-error-handling-streaming-source:p1:inst-eh-stream-1
// @cpt-begin:cpt-cf-oagw-flow-error-handling-streaming-source:p1:inst-eh-stream-2
// @cpt-begin:cpt-cf-oagw-flow-error-handling-streaming-source:p1:inst-eh-stream-3
// @cpt-begin:cpt-cf-oagw-flow-error-handling-streaming-source:p1:inst-eh-stream-4
// @cpt-begin:cpt-cf-oagw-flow-error-handling-streaming-source:p1:inst-eh-stream-5
// @cpt-begin:cpt-cf-oagw-flow-error-handling-streaming-source:p1:inst-eh-stream-6
// @cpt-begin:cpt-cf-oagw-flow-error-handling-streaming-source:p1:inst-eh-stream-7
/// The resource marker every canonical error of this surface carries.
#[resource_error("gts.cf.core.oagw.upstream.v1~")]
struct UpstreamResource;
//
// @cpt-end:cpt-cf-oagw-algo-error-handling-source-header:p1:inst-eh-src-6
// @cpt-end:cpt-cf-oagw-algo-error-handling-source-header:p1:inst-eh-src-5
// @cpt-end:cpt-cf-oagw-algo-error-handling-source-header:p1:inst-eh-src-4
// @cpt-end:cpt-cf-oagw-algo-error-handling-source-header:p1:inst-eh-src-3
// @cpt-end:cpt-cf-oagw-algo-error-handling-source-header:p1:inst-eh-src-2
// @cpt-end:cpt-cf-oagw-algo-error-handling-source-header:p1:inst-eh-src-1
// @cpt-end:cpt-cf-oagw-algo-error-handling-validation-render:p1:inst-eh-vrender-7
// @cpt-end:cpt-cf-oagw-algo-error-handling-validation-render:p1:inst-eh-vrender-6
// @cpt-end:cpt-cf-oagw-algo-error-handling-validation-render:p1:inst-eh-vrender-5
// @cpt-end:cpt-cf-oagw-algo-error-handling-validation-render:p1:inst-eh-vrender-4
// @cpt-end:cpt-cf-oagw-algo-error-handling-validation-render:p1:inst-eh-vrender-3
// @cpt-end:cpt-cf-oagw-algo-error-handling-validation-render:p1:inst-eh-vrender-2
// @cpt-end:cpt-cf-oagw-algo-error-handling-validation-render:p1:inst-eh-vrender-1
// @cpt-end:cpt-cf-oagw-flow-error-handling-disabled-upstream:p1:inst-eh-dis-7
// @cpt-end:cpt-cf-oagw-flow-error-handling-disabled-upstream:p1:inst-eh-dis-6
// @cpt-end:cpt-cf-oagw-flow-error-handling-disabled-upstream:p1:inst-eh-dis-5
// @cpt-end:cpt-cf-oagw-flow-error-handling-disabled-upstream:p1:inst-eh-dis-4
// @cpt-end:cpt-cf-oagw-flow-error-handling-disabled-upstream:p1:inst-eh-dis-3
// @cpt-end:cpt-cf-oagw-flow-error-handling-disabled-upstream:p1:inst-eh-dis-2
// @cpt-end:cpt-cf-oagw-flow-error-handling-disabled-upstream:p1:inst-eh-dis-1
// @cpt-end:cpt-cf-oagw-flow-error-handling-streaming-source:p1:inst-eh-stream-7
// @cpt-end:cpt-cf-oagw-flow-error-handling-streaming-source:p1:inst-eh-stream-6
// @cpt-end:cpt-cf-oagw-flow-error-handling-streaming-source:p1:inst-eh-stream-5
// @cpt-end:cpt-cf-oagw-flow-error-handling-streaming-source:p1:inst-eh-stream-4
// @cpt-end:cpt-cf-oagw-flow-error-handling-streaming-source:p1:inst-eh-stream-3
// @cpt-end:cpt-cf-oagw-flow-error-handling-streaming-source:p1:inst-eh-stream-2
// @cpt-end:cpt-cf-oagw-flow-error-handling-streaming-source:p1:inst-eh-stream-1
//

/// The resource marker a denied *route* decision renders, so the evaluated
/// resource is the base type the denied permission names.
#[resource_error("gts.cf.core.oagw.route.v1~")]
struct RouteResource;

/// The resource markers a denied *plugin* decision renders, one per plugin
/// base type, so the evaluated resource is the base type the denied permission
/// names (`cpt-cf-oagw-dod-plugin-system-management-api`).
#[resource_error("gts.cf.core.oagw.auth_plugin.v1~")]
struct AuthPluginResource;

#[resource_error("gts.cf.core.oagw.guard_plugin.v1~")]
struct GuardPluginResource;

#[resource_error("gts.cf.core.oagw.transform_plugin.v1~")]
struct TransformPluginResource;

/// The `X-OAGW-Error-Source` value of a gateway-rendered body.
pub(crate) const ERROR_SOURCE: &str = "gateway";

/// The request header the inbound trace identifier arrives on. The value is
/// carried, never minted: minting one is entry 2.9's.
pub const TRACE_ID_HEADER: &str = "x-request-id";

/// Longest trace identifier echoed into a problem body, and the only
/// request-derived value echoed besides `invalid_value`.
const MAX_TRACE_ID_LEN: usize = 128;

/// The platform canonical *internal* category, for the two variants that
/// carry no OAGW type.
///
/// Re-exported here because the mapping table is where it is consumed.
pub const CANONICAL_INTERNAL_TYPE: &str = gts::CANONICAL_INTERNAL_TYPE;

/// The retry guidance a retriable type carries, if any
/// (`cpt-cf-oagw-algo-error-handling-retriability` step 5).
///
/// Retriability alone never produces a `Retry-After`: a type is retriable
/// *and* carries guidance, or it is retriable with no guidance at all —
/// `link.unavailable.v1` and `circuit_breaker.open.v1` are the two retriable
/// types that carry none.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RetryGuidance {
    /// Neither `Retry-After` nor `retry_after_seconds` is emitted.
    #[default]
    None,
    /// The value travels on the error itself: the rate-limit decision of
    /// entry 2.7.
    FromError,
    /// The value is the configured `proxy_timeout_secs` of [`crate::config::OagwConfig`].
    ProxyTimeout,
}

/// One resolved row of the DESIGN §3.3 error table.
///
/// The mapping is total: every [`DomainError`] variant has exactly one row,
/// so the status, the type and the retriability of a failure never depend on
/// the endpoint that raised it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorMapping {
    /// The HTTP status of the row.
    pub status: StatusCode,
    /// The GTS error type of the row.
    pub gts_type: &'static str,
    /// Whether a client may retry the request unchanged.
    pub retriable: bool,
    /// Where the `Retry-After` / `retry_after_seconds` value comes from.
    pub guidance: RetryGuidance,
}

/// The DESIGN §3.3 table, resolved.
///
/// `RouteError` and `ValidationError` share the `validation.error.v1` row;
/// the two repository-boundary variants ride the statuses of the proxy-path
/// rows they stand in for; the two 403 CORS rejection types are owned by
/// entry 2.8 and rendered here; `Internal` and `PluginInternal` carry no OAGW
/// type and are rendered as the platform canonical internal category.
// @cpt-begin:cpt-cf-oagw-algo-error-handling-status-mapping:p1:inst-eh-map-2
// `inst-eh-map-1` .. `-3`: one table, one pass, no call-site mapping — the
// status, the GTS type and the retriable flag of every DESIGN §3.3 row, plus
// the two 403 CORS types entry 2.8 owns.
// @cpt-begin:cpt-cf-oagw-algo-error-handling-status-mapping:p1:inst-eh-map-1
// @cpt-begin:cpt-cf-oagw-algo-error-handling-status-mapping:p1:inst-eh-map-10
// @cpt-begin:cpt-cf-oagw-algo-error-handling-status-mapping:p1:inst-eh-map-3
// @cpt-begin:cpt-cf-oagw-algo-error-handling-status-mapping:p1:inst-eh-map-6
// @cpt-begin:cpt-cf-oagw-algo-error-handling-status-mapping:p1:inst-eh-map-7
// @cpt-begin:cpt-cf-oagw-algo-error-handling-status-mapping:p1:inst-eh-map-8
// @cpt-begin:cpt-cf-oagw-algo-error-handling-status-mapping:p1:inst-eh-map-9
#[must_use]
pub fn mapping(error: &DomainError) -> ErrorMapping {
    let (status, gts_type, retriable, guidance) = match error {
        DomainError::ValidationError { .. } | DomainError::RouteError { .. }
        | DomainError::CorsInvalidConfig(_) => {
            (StatusCode::BAD_REQUEST, gts::ERR_VALIDATION, false, RetryGuidance::None)
        }
        DomainError::MissingTargetHost { .. } => (
            StatusCode::BAD_REQUEST,
            gts::ERR_MISSING_TARGET_HOST,
            false,
            RetryGuidance::None,
        ),
        DomainError::InvalidTargetHost { .. } => (
            StatusCode::BAD_REQUEST,
            gts::ERR_INVALID_TARGET_HOST,
            false,
            RetryGuidance::None,
        ),
        DomainError::UnknownTargetHost { .. } => (
            StatusCode::BAD_REQUEST,
            gts::ERR_UNKNOWN_TARGET_HOST,
            false,
            RetryGuidance::None,
        ),
        DomainError::AuthenticationFailed { .. } => (
            StatusCode::UNAUTHORIZED,
            gts::ERR_AUTH_FAILED,
            false,
            RetryGuidance::None,
        ),
        DomainError::RouteNotFound { .. } | DomainError::NotFound { .. } => (
            StatusCode::NOT_FOUND,
            gts::ERR_ROUTE_NOT_FOUND,
            false,
            RetryGuidance::None,
        ),
        DomainError::Conflict { .. } | DomainError::PluginInUse { .. } => (
            StatusCode::CONFLICT,
            gts::ERR_PLUGIN_IN_USE,
            false,
            RetryGuidance::None,
        ),
        DomainError::PayloadTooLarge { .. } => (
            StatusCode::PAYLOAD_TOO_LARGE,
            gts::ERR_PAYLOAD_TOO_LARGE,
            false,
            RetryGuidance::None,
        ),
        DomainError::RateLimitExceeded { .. } => (
            StatusCode::TOO_MANY_REQUESTS,
            gts::ERR_RATE_LIMIT_EXCEEDED,
            true,
            RetryGuidance::FromError,
        ),
        DomainError::SecretNotFound { .. } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            gts::ERR_SECRET_NOT_FOUND,
            false,
            RetryGuidance::None,
        ),
        DomainError::ProtocolError { .. } => (
            StatusCode::BAD_GATEWAY,
            gts::ERR_PROTOCOL,
            false,
            RetryGuidance::None,
        ),
        DomainError::DownstreamError { retriable, .. } => (
            StatusCode::BAD_GATEWAY,
            gts::ERR_DOWNSTREAM,
            *retriable,
            RetryGuidance::None,
        ),
        DomainError::StreamAborted { .. } => (
            StatusCode::BAD_GATEWAY,
            gts::ERR_STREAM_ABORTED,
            false,
            RetryGuidance::None,
        ),
        DomainError::LinkUnavailable { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            gts::ERR_LINK_UNAVAILABLE,
            true,
            RetryGuidance::None,
        ),
        DomainError::CircuitBreakerOpen { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            gts::ERR_CIRCUIT_BREAKER_OPEN,
            true,
            RetryGuidance::None,
        ),
        DomainError::PluginNotFound { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            gts::ERR_PLUGIN_NOT_FOUND,
            false,
            RetryGuidance::None,
        ),
        DomainError::ConnectionTimeout { .. } => (
            StatusCode::GATEWAY_TIMEOUT,
            gts::ERR_CONNECTION_TIMEOUT,
            true,
            RetryGuidance::ProxyTimeout,
        ),
        DomainError::RequestTimeout { .. } => (
            StatusCode::GATEWAY_TIMEOUT,
            gts::ERR_REQUEST_TIMEOUT,
            true,
            RetryGuidance::ProxyTimeout,
        ),
        DomainError::IdleTimeout { .. } => (
            StatusCode::GATEWAY_TIMEOUT,
            gts::ERR_IDLE_TIMEOUT,
            true,
            RetryGuidance::ProxyTimeout,
        ),
        DomainError::CorsOriginNotAllowed { .. } => (
            StatusCode::FORBIDDEN,
            gts::ERR_CORS_ORIGIN_NOT_ALLOWED,
            false,
            RetryGuidance::None,
        ),
        DomainError::CorsMethodNotAllowed { .. } => (
            StatusCode::FORBIDDEN,
            gts::ERR_CORS_METHOD_NOT_ALLOWED,
            false,
            RetryGuidance::None,
        ),
        DomainError::PluginInternal(_) | DomainError::Internal(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            gts::CANONICAL_INTERNAL_TYPE,
            false,
            RetryGuidance::None,
        ),
    };
    ErrorMapping { status, gts_type, retriable, guidance }
}
//
// @cpt-end:cpt-cf-oagw-algo-error-handling-status-mapping:p1:inst-eh-map-9
// @cpt-end:cpt-cf-oagw-algo-error-handling-status-mapping:p1:inst-eh-map-8
// @cpt-end:cpt-cf-oagw-algo-error-handling-status-mapping:p1:inst-eh-map-7
// @cpt-end:cpt-cf-oagw-algo-error-handling-status-mapping:p1:inst-eh-map-6
// @cpt-end:cpt-cf-oagw-algo-error-handling-status-mapping:p1:inst-eh-map-3
// @cpt-end:cpt-cf-oagw-algo-error-handling-status-mapping:p1:inst-eh-map-10
// @cpt-end:cpt-cf-oagw-algo-error-handling-status-mapping:p1:inst-eh-map-1
//
// @cpt-end:cpt-cf-oagw-algo-error-handling-status-mapping:p1:inst-eh-map-2

/// The `(GTS type, status)` pair the management surface and the pipeline
/// boundary read. A projection of [`mapping`].
pub(crate) fn mapping_of(error: &DomainError) -> (&'static str, StatusCode) {
    let mapped = mapping(error);
    (mapped.gts_type, mapped.status)
}

/// Whether the variant's GTS type is retriable
/// (`cpt-cf-oagw-algo-error-handling-retriability`).
#[must_use]
pub fn is_retriable(error: &DomainError) -> bool {
    mapping(error).retriable
}

/// The human title of an OAGW GTS error type.
fn title_of(error_type: &str) -> &'static str {
    match error_type {
        gts::ERR_VALIDATION => "Validation failed",
        gts::ERR_MISSING_TARGET_HOST => "Target host header required",
        gts::ERR_INVALID_TARGET_HOST => "Invalid target host header",
        gts::ERR_UNKNOWN_TARGET_HOST => "Unknown target host",
        gts::ERR_AUTH_FAILED => "Authentication failed",
        gts::ERR_ROUTE_NOT_FOUND => "Route not found",
        gts::ERR_PLUGIN_IN_USE => "Conflict",
        gts::ERR_PAYLOAD_TOO_LARGE => "Payload too large",
        gts::ERR_RATE_LIMIT_EXCEEDED => "Rate limit exceeded",
        gts::ERR_SECRET_NOT_FOUND => "Secret not found",
        gts::ERR_PROTOCOL => "Protocol error",
        gts::ERR_DOWNSTREAM => "Downstream error",
        gts::ERR_STREAM_ABORTED => "Stream aborted",
        gts::ERR_LINK_UNAVAILABLE => "Upstream link unavailable",
        gts::ERR_CIRCUIT_BREAKER_OPEN => "Circuit breaker open",
        gts::ERR_PLUGIN_NOT_FOUND => "Plugin not found",
        gts::ERR_CONNECTION_TIMEOUT => "Connection timeout",
        gts::ERR_REQUEST_TIMEOUT => "Request timeout",
        gts::ERR_IDLE_TIMEOUT => "Idle timeout",
        gts::ERR_CORS_ORIGIN_NOT_ALLOWED => "CORS origin not allowed",
        gts::ERR_CORS_METHOD_NOT_ALLOWED => "CORS method not allowed",
        gts::CANONICAL_INTERNAL_TYPE => "Internal error",
        _ => "Internal error",
    }
}

/// The `detail` text an occurrence with no curated text of its own carries.
///
/// The text explains *this* failure, never only its class: it names the
/// fields the row owns. A request-derived value never reaches it — the
/// `invalid_value` echo lives in its own member and the valid-host list is
/// operator configuration, not request content.
// @cpt-begin:cpt-cf-oagw-algo-error-handling-status-mapping:p1:inst-eh-map-4
// `inst-eh-map-4`: `title` comes from the mapped type and `detail` from the
// occurrence, so a body always explains the failure it reports.
#[must_use]
pub fn default_detail(error: &DomainError) -> String {
    match error {
        DomainError::ValidationError { detail, .. } | DomainError::RouteError { detail, .. }
        | DomainError::Conflict { detail, .. } | DomainError::CorsInvalidConfig(detail)
        | DomainError::PluginInternal(detail) | DomainError::Internal(detail) => detail.clone(),
        DomainError::MissingTargetHost { valid_hosts, .. } => format!(
            "the `X-OAGW-Target-Host` header is required for this upstream pool; \
             valid hosts: {}",
            host_list(valid_hosts)
        ),
        DomainError::InvalidTargetHost { .. } => {
            "the `X-OAGW-Target-Host` header value is not a bare hostname or IP address".to_owned()
        }
        DomainError::UnknownTargetHost { valid_hosts, .. } => format!(
            "the requested target host is not a configured endpoint of this upstream; \
             valid hosts: {}",
            host_list(valid_hosts)
        ),
        DomainError::AuthenticationFailed { .. } => {
            "the request was not authenticated: a valid bearer token and a resolvable \
             security context are required"
                .to_owned()
        }
        DomainError::RouteNotFound { .. } | DomainError::NotFound { .. } => {
            "no enabled route matched the request, or the addressed record does not \
             exist for the calling tenant"
                .to_owned()
        }
        DomainError::PluginInUse { .. } => {
            "the plugin is still referenced and cannot be deleted".to_owned()
        }
        DomainError::PayloadTooLarge { limit_bytes, .. } => match limit_bytes {
            Some(limit) => {
                format!("the request body exceeds the configured body-size limit of {limit} bytes")
            }
            None => "the request body exceeds the configured body-size limit".to_owned(),
        },
        DomainError::RateLimitExceeded { .. } => {
            "the rate limit of the addressed upstream is exhausted for this caller".to_owned()
        }
        DomainError::SecretNotFound { .. } => {
            "a credential reference of the resolved configuration could not be resolved".to_owned()
        }
        DomainError::ProtocolError { .. } => {
            "the upstream could not be reached over the configured protocol".to_owned()
        }
        DomainError::DownstreamError { .. } => "the upstream service failed the exchange".to_owned(),
        DomainError::StreamAborted { .. } => "the streaming session was aborted".to_owned(),
        DomainError::LinkUnavailable { .. } => {
            "the upstream is unavailable, or disabled by configuration".to_owned()
        }
        DomainError::CircuitBreakerOpen { .. } => {
            "the circuit breaker of the upstream is open".to_owned()
        }
        DomainError::PluginNotFound { plugin_ref } => {
            format!("the composed chain references a plugin that does not resolve: {plugin_ref}")
        }
        DomainError::ConnectionTimeout { .. } => {
            "establishing the upstream connection exceeded the proxy timeout".to_owned()
        }
        DomainError::RequestTimeout { .. } => {
            "the buffered exchange with the upstream exceeded the proxy timeout".to_owned()
        }
        DomainError::IdleTimeout { .. } => {
            "the open session exceeded the idle window without producing a frame".to_owned()
        }
        DomainError::CorsOriginNotAllowed { .. } => {
            "the request origin is not an allowed origin of the effective CORS configuration"
                .to_owned()
        }
        DomainError::CorsMethodNotAllowed { .. } => {
            "the request method is not an allowed method of the effective CORS configuration"
                .to_owned()
        }
    }
}

/// The comma-separated host list of a `detail`, in sorted order.
fn host_list(hosts: &[String]) -> String {
    if hosts.is_empty() {
        return "(none configured)".to_owned();
    }
    hosts.join(", ")
}
// @cpt-end:cpt-cf-oagw-algo-error-handling-status-mapping:p1:inst-eh-map-4

/// The request-scoped context a failure carries, as far as it is known.
///
/// Every member starts as `None` and is populated only when known
/// (`inst-eh-map-6`/`-7`), so a body never carries a `null` extension member.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ErrorOccurrence {
    /// The failing request path, for the `instance` member.
    pub instance: Option<String>,
    /// The resolved upstream identifier, when one was resolved.
    pub upstream_id: Option<String>,
    /// The selected target endpoint host, when one was selected.
    pub host: Option<String>,
    /// The failing request path, when the error does not carry it.
    pub path: Option<String>,
    /// The trace identifier, when one exists.
    pub trace_id: Option<String>,
}

impl ErrorOccurrence {
    /// An occurrence carrying only the failing request path.
    #[must_use]
    pub fn at_path(instance: impl Into<String>) -> Self {
        Self { instance: Some(instance.into()), ..ErrorOccurrence::default() }
    }

    /// Overlay the request-scoped values the error variant did not carry.
    ///
    /// A value the variant carries wins: the pipeline knows the failing
    /// upstream and host better than the handler does.
    fn overlay(mut self, site: &Self) -> Self {
        if self.instance.is_none() {
            self.instance = site.instance.clone();
        }
        if self.upstream_id.is_none() {
            self.upstream_id = site.upstream_id.clone();
        }
        if self.host.is_none() {
            self.host = site.host.clone();
        }
        if self.path.is_none() {
            self.path = site.path.clone();
        }
        if self.trace_id.is_none() {
            self.trace_id = site.trace_id.clone();
        }
        self
    }
}

/// The occurrence context a [`DomainError`] carries itself.
// @cpt-begin:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-3
// `inst-eh-render-3`: the occurrence context is read off the error, so a
// component that resolved an upstream or selected a target contributes it and
// a component that failed earlier contributes nothing.
// @cpt-begin:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-10
// @cpt-begin:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-11
// @cpt-begin:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-12
// @cpt-begin:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-2
// @cpt-begin:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-4
// @cpt-begin:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-5
// @cpt-begin:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-6
// @cpt-begin:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-7
// @cpt-begin:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-8
// @cpt-begin:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-9
#[must_use]
pub fn occurrence_of(error: &DomainError) -> ErrorOccurrence {
    match error {
        DomainError::ValidationError { path, trace_id, .. }
        | DomainError::CorsOriginNotAllowed { path, trace_id }
        | DomainError::CorsMethodNotAllowed { path, trace_id }
        | DomainError::RouteNotFound { path, trace_id }
        | DomainError::PayloadTooLarge { path, trace_id, .. }
        | DomainError::SecretNotFound { path, trace_id } => {
            ErrorOccurrence { path: path.clone(), trace_id: trace_id.clone(), ..ErrorOccurrence::default() }
        }
        DomainError::MissingTargetHost { upstream_id, alias, trace_id, .. } => ErrorOccurrence {
            upstream_id: upstream_id.clone(),
            host: alias.clone(),
            trace_id: trace_id.clone(),
            ..ErrorOccurrence::default()
        },
        DomainError::InvalidTargetHost { upstream_id, trace_id, .. } => {
            ErrorOccurrence { upstream_id: upstream_id.clone(), trace_id: trace_id.clone(), ..ErrorOccurrence::default() }
        }
        DomainError::UnknownTargetHost { upstream_id, trace_id, .. } => {
            ErrorOccurrence { upstream_id: upstream_id.clone(), trace_id: trace_id.clone(), ..ErrorOccurrence::default() }
        }
        DomainError::AuthenticationFailed { upstream_id, host, path, trace_id }
        | DomainError::ProtocolError { upstream_id, host, path, trace_id }
        | DomainError::DownstreamError { upstream_id, host, path, trace_id, .. }
        | DomainError::StreamAborted { upstream_id, host, path, trace_id }
        | DomainError::LinkUnavailable { upstream_id, host, path, trace_id } => ErrorOccurrence {
            upstream_id: upstream_id.clone(),
            host: host.clone(),
            path: path.clone(),
            trace_id: trace_id.clone(),
            ..ErrorOccurrence::default()
        },
        DomainError::RateLimitExceeded { upstream_id, host, trace_id, .. } => ErrorOccurrence {
            upstream_id: upstream_id.clone(),
            host: host.clone(),
            trace_id: trace_id.clone(),
            ..ErrorOccurrence::default()
        },
        DomainError::CircuitBreakerOpen { upstream_id, host, trace_id } => ErrorOccurrence {
            upstream_id: upstream_id.clone(),
            host: host.clone(),
            trace_id: trace_id.clone(),
            ..ErrorOccurrence::default()
        },
        DomainError::ConnectionTimeout { upstream_id, host, trace_id, .. }
        | DomainError::RequestTimeout { upstream_id, host, trace_id, .. }
        | DomainError::IdleTimeout { upstream_id, host, trace_id, .. } => ErrorOccurrence {
            upstream_id: upstream_id.clone(),
            host: host.clone(),
            trace_id: trace_id.clone(),
            ..ErrorOccurrence::default()
        },
        _ => ErrorOccurrence::default(),
    }
}
//
// @cpt-end:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-9
// @cpt-end:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-8
// @cpt-end:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-7
// @cpt-end:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-6
// @cpt-end:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-5
// @cpt-end:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-4
// @cpt-end:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-2
// @cpt-end:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-12
// @cpt-end:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-11
// @cpt-end:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-10
//
// @cpt-end:cpt-cf-oagw-flow-error-handling-gateway-render:p1:inst-eh-render-3

/// The RFC 9457 problem-details body this gear renders (FEATURE entry 2.5).
///
/// Every member is at the **top level**: the five standard members, the six
/// OAGW extension members and the three target-host routing members. A member
/// whose value is unknown is omitted rather than emitted as `null`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProblemDetails {
    /// The GTS error type, as a GTS URI.
    #[serde(rename = "type")]
    pub problem_type: String,
    /// The human title of the type.
    pub title: String,
    /// The HTTP status code of the response.
    pub status: u16,
    /// The explanation of this occurrence.
    pub detail: String,
    /// The failing request path, gear-relative with no `/api` segment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// OAGW extension: the resolved upstream identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,
    /// OAGW extension: the selected target endpoint host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// OAGW extension: the failing request path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// OAGW extension: retry guidance in seconds, always equal to `Retry-After`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_seconds: Option<u64>,
    /// OAGW extension: the trace identifier of the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// OAGW extension: the reference set of `plugin.in_use.v1` (409) only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub referenced_by: Option<ReferencedBy>,
    /// Routing family: the addressed alias.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Routing family: the configured endpoint hosts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub valid_hosts: Option<Vec<String>>,
    /// Routing family: the bounded echo of the `X-OAGW-Target-Host` value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invalid_value: Option<String>,
}

impl ProblemDetails {
    /// Render one [`DomainError`] occurrence as the problem body it is.
    ///
    /// `site` carries the request-scoped values the handler knows (the failing
    /// path, the trace identifier, a resolved upstream); a value the variant
    /// carries itself wins over a value the handler supplies.
    #[must_use]
    pub fn from_error(error: &DomainError, site: &ErrorOccurrence) -> Self {
        let mapped = mapping(error);
        let occurrence = occurrence_of(error).overlay(site);
        let error_type = mapped.gts_type;
        let detail = error.detail().map_or_else(
            || default_detail(error),
            |curated| curated.to_owned(),
        );
        let (alias, valid_hosts, invalid_value) = match error {
            DomainError::MissingTargetHost { alias, valid_hosts, .. } => {
                (alias.clone(), Some(valid_hosts.clone()), None)
            }
            DomainError::InvalidTargetHost { invalid_value, .. } => {
                (None, None, Some(bound_invalid_value(invalid_value)))
            }
            DomainError::UnknownTargetHost { invalid_value, valid_hosts, .. } => (
                None,
                Some(valid_hosts.clone()),
                Some(bound_invalid_value(invalid_value)),
            ),
            _ => (None, None, None),
        };
        // @cpt-begin:cpt-cf-oagw-flow-error-handling-target-host-render:p1:inst-eh-th-3
        // `inst-eh-th-3` .. `-10`: each body of the target-host family carries
        // exactly the members it owns — `alias` + `valid_hosts` when the header
        // is missing, `invalid_value` when it is malformed, `invalid_value` +
        // `valid_hosts` when it names no configured endpoint — and all three
        // carry `upstream_id` and `trace_id`. The echo is bounded here, at the
        // one place it reaches a response member, and never reaches `detail`.
        // @cpt-end:cpt-cf-oagw-flow-error-handling-target-host-render:p1:inst-eh-th-3
        Self {
            problem_type: gts_uri!(error_type),
            title: title_of(error_type).to_owned(),
            status: mapped.status.as_u16(),
            detail,
            instance: occurrence.instance,
            upstream_id: occurrence.upstream_id,
            host: occurrence.host,
            path: occurrence.path,
            retry_after_seconds: guidance_of(error),
            trace_id: occurrence.trace_id.map(|trace| bound_trace_id(&trace)),
            referenced_by: referenced_by_of(error),
            alias,
            valid_hosts,
            invalid_value,
        }
    }
}

// @cpt-begin:cpt-cf-oagw-flow-error-handling-target-host-render:p1:inst-eh-th-1
// @cpt-begin:cpt-cf-oagw-flow-error-handling-target-host-render:p1:inst-eh-th-10
// @cpt-begin:cpt-cf-oagw-flow-error-handling-target-host-render:p1:inst-eh-th-2
// @cpt-begin:cpt-cf-oagw-flow-error-handling-target-host-render:p1:inst-eh-th-4
// @cpt-begin:cpt-cf-oagw-flow-error-handling-target-host-render:p1:inst-eh-th-5
// @cpt-begin:cpt-cf-oagw-flow-error-handling-target-host-render:p1:inst-eh-th-6
// @cpt-begin:cpt-cf-oagw-flow-error-handling-target-host-render:p1:inst-eh-th-7
// @cpt-begin:cpt-cf-oagw-flow-error-handling-target-host-render:p1:inst-eh-th-8
// @cpt-begin:cpt-cf-oagw-flow-error-handling-target-host-render:p1:inst-eh-th-9
impl ProblemDetails {
    /// The one problem body a proxy failure that is not a [`DomainError`]
    /// renders: a path-matched route whose method allowlist excludes the
    /// request method. It is not a row of the DESIGN §3.3 table, so it is
    /// built here and nowhere else, and the handler adds only the `Allow`
    /// header.
    #[must_use]
    pub fn method_not_allowed(path: Option<String>, allowed: &[&str]) -> Self {
        let allowed = allowed.join(", ");
        Self {
            problem_type: gts_uri!(gts::ERR_VALIDATION),
            title: "Method not allowed".to_owned(),
            status: 405,
            detail: format!(
                "the request method is not in the matched route's allowlist: {allowed}"
            ),
            instance: path,
            upstream_id: None,
            host: None,
            path: None,
            retry_after_seconds: None,
            trace_id: None,
            referenced_by: None,
            alias: None,
            valid_hosts: None,
            invalid_value: None,
        }
    }
}
//
// @cpt-end:cpt-cf-oagw-flow-error-handling-target-host-render:p1:inst-eh-th-9
// @cpt-end:cpt-cf-oagw-flow-error-handling-target-host-render:p1:inst-eh-th-8
// @cpt-end:cpt-cf-oagw-flow-error-handling-target-host-render:p1:inst-eh-th-7
// @cpt-end:cpt-cf-oagw-flow-error-handling-target-host-render:p1:inst-eh-th-6
// @cpt-end:cpt-cf-oagw-flow-error-handling-target-host-render:p1:inst-eh-th-5
// @cpt-end:cpt-cf-oagw-flow-error-handling-target-host-render:p1:inst-eh-th-4
// @cpt-end:cpt-cf-oagw-flow-error-handling-target-host-render:p1:inst-eh-th-2
// @cpt-end:cpt-cf-oagw-flow-error-handling-target-host-render:p1:inst-eh-th-10
// @cpt-end:cpt-cf-oagw-flow-error-handling-target-host-render:p1:inst-eh-th-1
//

/// The retry guidance value of an error, or `None` when it carries none.
///
/// Emitted only for a retriable type that carries guidance: the `429` value is
/// the rate-limit decision of entry 2.7, the three `504` values are the
/// configured `proxy_timeout_secs`, and the two retriable `503` types carry
/// none (`cpt-cf-oagw-dod-error-handling-retriability`).
// @cpt-begin:cpt-cf-oagw-algo-error-handling-retriability:p1:inst-eh-retry-5
// `inst-eh-retry-1` .. `-6`: the guidance value comes from the named source of
// its family, and `link.unavailable.v1` / `circuit_breaker.open.v1` carry
// neither `retry_after_seconds` nor `Retry-After`.
// @cpt-begin:cpt-cf-oagw-algo-error-handling-retriability:p1:inst-eh-retry-1
// @cpt-begin:cpt-cf-oagw-algo-error-handling-retriability:p1:inst-eh-retry-2
// @cpt-begin:cpt-cf-oagw-algo-error-handling-retriability:p1:inst-eh-retry-3
// @cpt-begin:cpt-cf-oagw-algo-error-handling-retriability:p1:inst-eh-retry-4
// @cpt-begin:cpt-cf-oagw-algo-error-handling-retriability:p1:inst-eh-retry-6
// @cpt-begin:cpt-cf-oagw-algo-error-handling-retriability:p1:inst-eh-retry-7
#[must_use]
pub fn guidance_of(error: &DomainError) -> Option<u64> {
    match (mapping(error).guidance, error) {
        (RetryGuidance::FromError, DomainError::RateLimitExceeded { retry_after_seconds, .. }) => {
            *retry_after_seconds
        }
        (RetryGuidance::ProxyTimeout, DomainError::ConnectionTimeout { guidance_secs, .. })
        | (RetryGuidance::ProxyTimeout, DomainError::RequestTimeout { guidance_secs, .. })
        | (RetryGuidance::ProxyTimeout, DomainError::IdleTimeout { guidance_secs, .. }) => *guidance_secs,
        _ => None,
    }
}
//
// @cpt-end:cpt-cf-oagw-algo-error-handling-retriability:p1:inst-eh-retry-7
// @cpt-end:cpt-cf-oagw-algo-error-handling-retriability:p1:inst-eh-retry-6
// @cpt-end:cpt-cf-oagw-algo-error-handling-retriability:p1:inst-eh-retry-4
// @cpt-end:cpt-cf-oagw-algo-error-handling-retriability:p1:inst-eh-retry-3
// @cpt-end:cpt-cf-oagw-algo-error-handling-retriability:p1:inst-eh-retry-2
// @cpt-end:cpt-cf-oagw-algo-error-handling-retriability:p1:inst-eh-retry-1
//
// @cpt-end:cpt-cf-oagw-algo-error-handling-retriability:p1:inst-eh-retry-5

/// The reference set of a `409` plugin-in-use body, and of nothing else.
fn referenced_by_of(error: &DomainError) -> Option<ReferencedBy> {
    match error {
        DomainError::PluginInUse { referenced_by } => Some(referenced_by.clone()),
        DomainError::Conflict { referenced_by, .. } => referenced_by.clone(),
        _ => None,
    }
}

/// The bounded echo the contract permits on a routing body.
///
/// Re-applied here so the bound holds whatever the caller of the constructor
/// did: at most 128 characters, no control character, JSON serializer only.
fn bound_invalid_value(value: &str) -> String {
    crate::domain::validation::bound_invalid_value(value)
}

/// Whether the mapped type is one of the two 403 CORS rejections, which carry
/// `Vary: Origin` because the verdict is origin-dependent.
///
/// `error_type` is the GTS URI the body carries, so the bare type the mapping
/// table names is matched as its suffix.
fn is_cors_rejection(error_type: &str) -> bool {
    error_type.ends_with(gts::ERR_CORS_ORIGIN_NOT_ALLOWED)
        || error_type.ends_with(gts::ERR_CORS_METHOD_NOT_ALLOWED)
}

// @cpt-begin:cpt-cf-oagw-algo-error-handling-serialization:p1:inst-eh-ser-1
// `inst-eh-ser-1` .. `-7`: exactly the five RFC 9457 members plus the closed
// extension vocabulary at the top level, `Content-Type:
// application/problem+json`, `status` equal to the response status, and no
// `null` member anywhere.
// @cpt-begin:cpt-cf-oagw-algo-error-handling-serialization:p1:inst-eh-ser-2
// @cpt-begin:cpt-cf-oagw-algo-error-handling-serialization:p1:inst-eh-ser-3
// @cpt-begin:cpt-cf-oagw-algo-error-handling-serialization:p1:inst-eh-ser-4
// @cpt-begin:cpt-cf-oagw-algo-error-handling-serialization:p1:inst-eh-ser-5
// @cpt-begin:cpt-cf-oagw-algo-error-handling-serialization:p1:inst-eh-ser-6
// @cpt-begin:cpt-cf-oagw-algo-error-handling-serialization:p1:inst-eh-ser-7
// @cpt-begin:cpt-cf-oagw-algo-error-handling-serialization:p1:inst-eh-ser-8
impl IntoResponse for ProblemDetails {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let retry_after = self.retry_after_seconds;
        let vary_origin = is_cors_rejection(&self.problem_type);
        let body = serde_json::to_vec(&self).unwrap_or_else(|error| {
            tracing::error!(error = %error, "failed to serialize the OAGW problem body");
            fallback_body(status.as_u16()).into_bytes()
        });
        let mut response = (
            status,
            [(header::CONTENT_TYPE, APPLICATION_PROBLEM_JSON)],
            body,
        )
            .into_response();
        // Every body this serializer renders is gateway-generated, so the
        // marker is set here and the 2.4 emission point agrees by construction.
        response
            .headers_mut()
            .insert(crate::domain::headers::ERROR_SOURCE_HEADER, HeaderValue::from_static(ERROR_SOURCE));
        if let Some(seconds) = retry_after {
            response.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(seconds));
        }
        if vary_origin {
            response.headers_mut().insert(header::VARY, HeaderValue::from_static("Origin"));
        }
        response
    }
}
//
// @cpt-end:cpt-cf-oagw-algo-error-handling-serialization:p1:inst-eh-ser-8
// @cpt-end:cpt-cf-oagw-algo-error-handling-serialization:p1:inst-eh-ser-7
// @cpt-end:cpt-cf-oagw-algo-error-handling-serialization:p1:inst-eh-ser-6
// @cpt-end:cpt-cf-oagw-algo-error-handling-serialization:p1:inst-eh-ser-5
// @cpt-end:cpt-cf-oagw-algo-error-handling-serialization:p1:inst-eh-ser-4
// @cpt-end:cpt-cf-oagw-algo-error-handling-serialization:p1:inst-eh-ser-3
// @cpt-end:cpt-cf-oagw-algo-error-handling-serialization:p1:inst-eh-ser-2
//
// @cpt-end:cpt-cf-oagw-algo-error-handling-serialization:p1:inst-eh-ser-1

/// The body emitted when the problem itself cannot be serialized.
/// The body emitted when the problem itself cannot be serialized: the canonical
/// internal type, with the `status` member taken from the response status so
/// the two can never disagree.
fn fallback_body(status: u16) -> String {
    format!(
        r#"{{"type":"gts://gts.cf.core.errors.err.v1~cf.core.err.internal.v1~","title":"Internal error","status":{status},"detail":"failed to serialize the problem body"}}"#
    )
}

/// Stamp the gateway marker on a body this module rendered, whatever surface
/// it went through: the OAGW serializer sets it itself, and the two canonical
/// surfaces (the shared `403` and the PDP-unavailable `500`) are stamped here,
/// so a caller never has to ask which surface a body came from.
fn stamp_source(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(crate::domain::headers::ERROR_SOURCE_HEADER, HeaderValue::from_static(ERROR_SOURCE));
    response
}

/// The error a management or proxy handler returns.
#[derive(Debug, Clone)]
pub enum ApiError {
    /// A domain rejection.
    Domain(DomainError),
    /// An authorization outcome.
    Authorization(AuthorizeError),
    /// The unauthenticated surface, raised before payload validation.
    Authentication,
}

impl From<DomainError> for ApiError {
    fn from(error: DomainError) -> Self {
        Self::Domain(error)
    }
}

impl From<ManagementError> for ApiError {
    fn from(error: ManagementError) -> Self {
        match error {
            ManagementError::Domain(domain) => Self::Domain(domain),
            ManagementError::Authorization(authorization) => Self::Authorization(authorization),
        }
    }
}

impl ApiError {
    /// The status the error renders.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        match self {
            Self::Authentication => StatusCode::UNAUTHORIZED,
            Self::Authorization(AuthorizeError::Denied { .. }) => StatusCode::FORBIDDEN,
            Self::Authorization(AuthorizeError::Unavailable { .. }) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            Self::Domain(error) => mapping(error).status,
        }
    }

    /// The GTS error type the error renders.
    #[must_use]
    pub fn gts_type(&self) -> String {
        match self {
            Self::Authentication => gts::ERR_AUTH_FAILED.to_owned(),
            Self::Authorization(_) => {
                // The shared canonical permission-denied category: no OAGW
                // 403 type exists.
                permission_denied(
                    "gts.cf.core.oagw.upstream.v1~:read",
                    "the permission was not granted",
                )
                .gts_type()
                .to_owned()
            }
            Self::Domain(error) => mapping(error).gts_type.to_owned(),
        }
    }

    /// Render the error as the response it is
    /// (`cpt-cf-oagw-flow-error-handling-gateway-render`).
    ///
    /// Two rendering paths exist and no third: this module's
    /// [`ProblemDetails`] body, and the platform canonical
    /// permission-denied surface of a denied decision, which the FEATURE
    /// requires to stay shared.
    #[must_use]
    pub fn into_response(self) -> Response {
        match self {
            // The `401` OAGW authentication surface, raised before payload
            // validation and carrying the OAGW authentication type.
            Self::Authentication => ProblemDetails::from_error(
                &DomainError::AuthenticationFailed {
                    upstream_id: None,
                    host: None,
                    path: None,
                    trace_id: None,
                },
                &ErrorOccurrence::default(),
            )
            .into_response(),
            Self::Authorization(AuthorizeError::Denied { permission, detail }) => {
                stamp_source(permission_denied(&permission, &detail).into_response())
            }
            Self::Authorization(AuthorizeError::Unavailable { detail }) => {
                stamp_source(CanonicalError::internal(format!(
                    "the authorization decision could not be obtained: {detail}"
                ))
                .create()
                .into_response())
            }
            Self::Domain(error) => {
                ProblemDetails::from_error(&error, &ErrorOccurrence::default()).into_response()
            }
        }
    }
}

/// The canonical problem body of an unauthenticated management request.
#[must_use]
pub fn unauthenticated() -> ApiError {
    ApiError::Authentication
}

/// The canonical permission-denied surface of a denied authorization decision.
///
/// There is no OAGW 403 error type: the shared canonical
/// `permission_denied` category carries the decision, with the evaluated
/// permission as its reason and the base type the permission names as the
/// evaluated resource — `gts.cf.core.oagw.route.v1~` for a route permission,
/// `gts.cf.core.oagw.upstream.v1~` for an upstream one.
#[must_use]
pub fn permission_denied(permission: &str, detail: &str) -> CanonicalError {
    let reason = if detail.is_empty() {
        permission.to_owned()
    } else {
        format!("{permission}: {detail}")
    };
    let denied = |builder: ResourceErrorBuilder<ResourceAbsent, NeedsReason>| {
        builder
            .with_reason(reason)
            .with_override(Http::status_code(403))
            .create()
    };
    if permission.starts_with(gts::ROUTE_BASE_TYPE) {
        denied(RouteResource::permission_denied())
    } else if permission.starts_with(gts::AUTH_PLUGIN_BASE_TYPE) {
        denied(AuthPluginResource::permission_denied())
    } else if permission.starts_with(gts::GUARD_PLUGIN_BASE_TYPE) {
        denied(GuardPluginResource::permission_denied())
    } else if permission.starts_with(gts::TRANSFORM_PLUGIN_BASE_TYPE) {
        denied(TransformPluginResource::permission_denied())
    } else {
        denied(UpstreamResource::permission_denied())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        ApiError::into_response(self)
    }
}

/// The handler result type of the management surface.
pub type ApiResult<T> = Result<T, ApiError>;

/// The correlation identifier one request carries
/// (`cpt-cf-oagw-flow-observability-and-state-trace-identifiers`).
///
/// The completing pass publishes it as a request extension, so the handlers —
/// which read the request *after* the layer wrapped them — emit the audit
/// record and the metric series of the same exchange under the identifier the
/// problem body carries (`inst-os-trace-3`, `inst-os-trace-5`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceIdentifier(pub String);

impl TraceIdentifier {
    /// The identifier as the `&str` the audit `request_id` field carries.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The identifier a request with no `x-request-id` header is minted.
///
/// The minted value is gateway-generated, carries no request-derived content
/// and stays inside the echo bound the inbound validation applies.
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-trace-propagation:p1:inst-os-trace-1
// `inst-os-trace-1`: the identifier is minted once, as a server-generated
// lowercase hyphenated UUID, and an inbound `x-request-id` is carried instead,
// never rewritten; `inst-os-trace-2`: a request without one is minted a
// gateway-generated identifier, so every request is joinable, not only the ones
// a caller named.
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-trace-propagation:p1:inst-os-trace-2
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-trace-propagation:p1:inst-os-trace-3
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-trace-propagation:p1:inst-os-trace-4
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-trace-propagation:p1:inst-os-trace-5
// @cpt-begin:cpt-cf-oagw-flow-observability-and-state-trace-propagation:p1:inst-os-trace-6
#[must_use]
pub fn mint_trace_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
//
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-trace-propagation:p1:inst-os-trace-6
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-trace-propagation:p1:inst-os-trace-5
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-trace-propagation:p1:inst-os-trace-4
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-trace-propagation:p1:inst-os-trace-3
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-trace-propagation:p1:inst-os-trace-2
//
// @cpt-end:cpt-cf-oagw-flow-observability-and-state-trace-propagation:p1:inst-os-trace-1

/// Complete a rendered problem body with the request context only the router
/// knows: the failing path and the correlation identifier.
///
/// The handlers raise their errors before they can know both — a service call
/// returns through `?` — so the two members are filled here, on the way out.
/// The pass is guarded on *all three* of an error status, the
/// `X-OAGW-Error-Source: gateway` marker and the problem+json content type, so
/// an upstream passthrough body is never touched, a success response is never
/// buffered, and a route the framework answers itself is never rewritten.
///
/// The identifier itself is resolved for **every** request — carried when the
/// inbound header names one, minted otherwise — and published as the
/// [`TraceIdentifier`] extension before the handler runs, so a request that
/// never fails is still audited under an identifier
/// (`inst-os-trace-2`, `inst-os-trace-3`).
// @cpt-begin:cpt-cf-oagw-algo-error-handling-status-mapping:p1:inst-eh-map-5
// `inst-eh-map-5`: `instance` is the failing request path under the
// gear-relative `/oagw/v1` tree with no `/api` segment, and `trace_id` the
// inbound identifier the request carried; both are request-scoped, so they are
// filled at the one place every endpoint passes through.
pub async fn complete_problem_context(request: Request, next: Next) -> Response {
    let instance = request.uri().path().to_owned();
    let supplied = request
        .headers()
        .get(TRACE_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(bound_trace_id);
    let trace_id = supplied.unwrap_or_else(mint_trace_id);
    let mut request = request;
    request.extensions_mut().insert(TraceIdentifier(trace_id.clone()));
    let mut response = next.run(request).await;
    fill_problem_context(&mut response, instance, Some(trace_id)).await;
    response
}
// @cpt-end:cpt-cf-oagw-algo-error-handling-status-mapping:p1:inst-eh-map-5

/// Bound an echoed trace identifier to at most 128 characters with no control
/// character.
fn bound_trace_id(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).take(MAX_TRACE_ID_LEN).collect()
}

/// The `Content-Type` value a problem body carries.
const PROBLEM_CONTENT_TYPE: &str = "application/problem+json";

/// Fill `instance` and `trace_id` on a gateway problem body that lacks them.
// @cpt-begin:cpt-cf-oagw-flow-error-handling-upstream-passthrough:p1:inst-eh-passthru-3
// `inst-eh-passthru-3`: the completing pass reads nothing but a gateway-marked
// problem body — an upstream passthrough body, whatever format it carries, is
// forwarded unmodified, with no member added and no content type rewritten.
// @cpt-begin:cpt-cf-oagw-flow-error-handling-upstream-passthrough:p1:inst-eh-passthru-1
// @cpt-begin:cpt-cf-oagw-flow-error-handling-upstream-passthrough:p1:inst-eh-passthru-2
// @cpt-begin:cpt-cf-oagw-flow-error-handling-upstream-passthrough:p1:inst-eh-passthru-4
// @cpt-begin:cpt-cf-oagw-flow-error-handling-upstream-passthrough:p1:inst-eh-passthru-5
// @cpt-begin:cpt-cf-oagw-flow-error-handling-upstream-passthrough:p1:inst-eh-passthru-6
// @cpt-begin:cpt-cf-oagw-flow-error-handling-upstream-passthrough:p1:inst-eh-passthru-7
// @cpt-begin:cpt-cf-oagw-flow-error-handling-upstream-passthrough:p1:inst-eh-passthru-8
async fn fill_problem_context(response: &mut Response, instance: String, trace_id: Option<String>) {
    let status = response.status();
    if !status.is_client_error() && !status.is_server_error() {
        return;
    }
    let gateway = response
        .headers()
        .get(crate::domain::headers::ERROR_SOURCE_HEADER)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == ERROR_SOURCE);
    let problem = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with(PROBLEM_CONTENT_TYPE));
    if !gateway || !problem {
        return;
    }
    let (mut parts, body) = std::mem::take(response).into_parts();
    let Ok(bytes) = http_body_util::BodyExt::collect(body).await.map(|collected| collected.to_bytes())
    else {
        // The body is gone, so a `content-length` the head still carries would
        // advertise bytes the response can never deliver: the framing headers
        // are dropped with the body rather than left over an empty one.
        parts.headers.remove(header::CONTENT_LENGTH);
        parts.headers.remove(header::CONTENT_TYPE);
        parts.headers.remove(header::TRANSFER_ENCODING);
        *response = Response::from_parts(parts, Body::empty());
        return;
    };
    let Ok(mut document) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        *response = Response::from_parts(parts, Body::from(bytes));
        return;
    };
    let Some(object) = document.as_object_mut() else {
        *response = Response::from_parts(parts, Body::from(bytes));
        return;
    };
    let mut changed = false;
    if !object.contains_key("instance") {
        object.insert("instance".to_owned(), serde_json::Value::String(instance));
        changed = true;
    }
    if let Some(trace_id) = trace_id.filter(|_| !object.contains_key("trace_id")) {
        object.insert("trace_id".to_owned(), serde_json::Value::String(trace_id));
        changed = true;
    }
    let body = if changed {
        serde_json::to_vec(&document).map_or_else(|_| bytes.to_vec(), Vec::from)
    } else {
        bytes.to_vec()
    };
    if let Ok(length) = HeaderValue::from_str(&body.len().to_string()) {
        parts.headers.insert(header::CONTENT_LENGTH, length);
    }
    *response = Response::from_parts(parts, Body::from(body));
}
//
// @cpt-end:cpt-cf-oagw-flow-error-handling-upstream-passthrough:p1:inst-eh-passthru-8
// @cpt-end:cpt-cf-oagw-flow-error-handling-upstream-passthrough:p1:inst-eh-passthru-7
// @cpt-end:cpt-cf-oagw-flow-error-handling-upstream-passthrough:p1:inst-eh-passthru-6
// @cpt-end:cpt-cf-oagw-flow-error-handling-upstream-passthrough:p1:inst-eh-passthru-5
// @cpt-end:cpt-cf-oagw-flow-error-handling-upstream-passthrough:p1:inst-eh-passthru-4
// @cpt-end:cpt-cf-oagw-flow-error-handling-upstream-passthrough:p1:inst-eh-passthru-2
// @cpt-end:cpt-cf-oagw-flow-error-handling-upstream-passthrough:p1:inst-eh-passthru-1
//
// @cpt-end:cpt-cf-oagw-flow-error-handling-upstream-passthrough:p1:inst-eh-passthru-3

/// Apply the error contract to every route registered on `router`.
///
/// The gear-relative `/oagw/v1` tree carries one body shape, so the layer is
/// added once, after the routes, and covers every endpoint registered before
/// it — the management prefixes, the proxy catch-all and every later one.
pub fn error_contract(router: axum::Router) -> axum::Router {
    router.layer(axum::middleware::from_fn(complete_problem_context))
}
