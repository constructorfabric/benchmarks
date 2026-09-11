//! GTS identifier constants for the OAGW gear.
//!
//! Two families live here:
//!
//! * `ERR_*` — the *instance* ids that complete the error type
//!   `gts.cf.core.errors.err.v1~<instance>` (DESIGN §3.3).
//! * Everything else — the anonymous resource and plugin identifiers
//!   (`gts.cf.core.oagw.<type>.v1~<instance>`).

// ---------------------------------------------------------------------------
// Resource identifiers (anonymous GTS ids)
// ---------------------------------------------------------------------------

/// Type part for an upstream resource id.
pub const TYPE_UPSTREAM: &str = "gts.cf.core.oagw.upstream.v1";
/// Instance part of an upstream resource id.
pub const INST_UPSTREAM: &str = "cf.core.oagw.upstream.v1";

/// Type part for a route resource id.
pub const TYPE_ROUTE: &str = "gts.cf.core.oagw.route.v1";
/// Type part for a plugin resource id.
pub const TYPE_PLUGIN: &str = "gts.cf.core.oagw.plugin.v1";

/// GTS identifier of the HTTP upstream protocol.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// GTS identifier of the gRPC upstream protocol (Phase 3 — not proxied).
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// Builds an anonymous resource id `gts.cf.core.oagw.<type>.v1~<uuid>`.
#[must_use]
pub fn resource_id(type_id: &str, uuid: &uuid::Uuid) -> String {
    format!("{type_id}~{uuid}")
}

/// Extracts the UUID part of an anonymous resource id, tolerating a bare UUID.
#[must_use]
pub fn uuid_from_resource_id(value: &str) -> Option<uuid::Uuid> {
    let instance = value.rsplit('~').next().unwrap_or(value);
    uuid::Uuid::parse_str(instance).ok()
}

// ---------------------------------------------------------------------------
// Error instance ids (DESIGN §3.3 error table)
// ---------------------------------------------------------------------------

/// 400 — general validation failure.
pub const ERR_VALIDATION: &str = "cf.oagw.validation.error.v1";
/// 400 — `X-OAGW-Target-Host` missing.
pub const ERR_MISSING_TARGET_HOST: &str = "cf.oagw.routing.missing_target_host.v1";
/// 400 — `X-OAGW-Target-Host` malformed.
pub const ERR_INVALID_TARGET_HOST: &str = "cf.oagw.routing.invalid_target_host.v1";
/// 400 — `X-OAGW-Target-Host` unknown.
pub const ERR_UNKNOWN_TARGET_HOST: &str = "cf.oagw.routing.unknown_target_host.v1";
/// 403 — CORS origin rejected.
pub const ERR_CORS_ORIGIN_NOT_ALLOWED: &str = "cf.oagw.cors.origin_not_allowed.v1";
/// 403 — CORS method rejected.
pub const ERR_CORS_METHOD_NOT_ALLOWED: &str = "cf.oagw.cors.method_not_allowed.v1";
/// 401 — upstream authentication failed.
pub const ERR_AUTH_FAILED: &str = "cf.oagw.auth.failed.v1";
/// 404 — no route matched.
pub const ERR_ROUTE_NOT_FOUND: &str = "cf.oagw.route.not_found.v1";
/// 409 — plugin still bound.
pub const ERR_PLUGIN_IN_USE: &str = "cf.oagw.plugin.in_use.v1";
/// 413 — payload too large.
pub const ERR_PAYLOAD_TOO_LARGE: &str = "cf.oagw.payload.too_large.v1";
/// 429 — rate limit exhausted.
pub const ERR_RATE_LIMIT_EXCEEDED: &str = "cf.oagw.rate_limit.exceeded.v1";
/// 500 — credential unresolvable.
pub const ERR_SECRET_NOT_FOUND: &str = "cf.oagw.secret.not_found.v1";
/// 502 — protocol-level failure.
pub const ERR_PROTOCOL_ERROR: &str = "cf.oagw.protocol.error.v1";
/// 502 — upstream service error.
pub const ERR_DOWNSTREAM_ERROR: &str = "cf.oagw.downstream.error.v1";
/// 502 — stream aborted.
pub const ERR_STREAM_ABORTED: &str = "cf.oagw.stream.aborted.v1";
/// 503 — link unavailable.
pub const ERR_LINK_UNAVAILABLE: &str = "cf.oagw.link.unavailable.v1";
/// 503 — circuit breaker open.
pub const ERR_CIRCUIT_BREAKER_OPEN: &str = "cf.oagw.circuit_breaker.open.v1";
/// 503 — plugin not found.
pub const ERR_PLUGIN_NOT_FOUND: &str = "cf.oagw.plugin.not_found.v1";
/// 504 — connect timeout.
pub const ERR_TIMEOUT_CONNECTION: &str = "cf.oagw.timeout.connection.v1";
/// 504 — request timeout.
pub const ERR_TIMEOUT_REQUEST: &str = "cf.oagw.timeout.request.v1";
/// 504 — idle timeout.
pub const ERR_TIMEOUT_IDLE: &str = "cf.oagw.timeout.idle.v1";

/// The error `type` prefix — the instance id completes it.
pub const ERROR_TYPE_PREFIX: &str = "gts.cf.core.errors.err.v1~";

/// Builds the full error `type` for an instance id.
#[must_use]
pub fn error_type(instance_id: &str) -> String {
    format!("{ERROR_TYPE_PREFIX}{instance_id}")
}

// ---------------------------------------------------------------------------
// Plugin identifiers
// ---------------------------------------------------------------------------

/// Prefix for auth-plugin identifiers.
pub const AUTH_PLUGIN_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1";
/// Prefix for guard-plugin identifiers.
pub const GUARD_PLUGIN_TYPE: &str = "gts.cf.core.oagw.guard_plugin.v1";
/// Prefix for transform-plugin identifiers.
pub const TRANSFORM_PLUGIN_TYPE: &str = "gts.cf.core.oagw.transform_plugin.v1";

/// `noop` auth plugin.
pub const AUTH_NOOP: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
/// `apikey` auth plugin.
pub const AUTH_APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// `OAuth2` client credentials, `Form` client auth.
pub const AUTH_OAUTH2_CC: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
/// `OAuth2` client credentials, `Basic` client auth.
pub const AUTH_OAUTH2_CC_BASIC: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";
/// `basic` — catalog only, no backing implementation.
pub const AUTH_BASIC: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
/// `bearer` — catalog only, no backing `AuthPlugin`.
pub const AUTH_BEARER: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";

/// `required_headers` guard — the only bindable guard identifier.
pub const GUARD_REQUIRED_HEADERS: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// `timeout` guard — catalog only (core data-plane logic).
pub const GUARD_TIMEOUT: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
/// `cors` guard — catalog only (core data-plane logic).
pub const GUARD_CORS: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";

/// `request_id` transform.
pub const TRANSFORM_REQUEST_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
/// `logging` transform — catalog only.
pub const TRANSFORM_LOGGING: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
/// `metrics` transform — catalog only.
pub const TRANSFORM_METRICS: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

/// Header used to mark gateway- vs upstream-originated responses.
pub const HEADER_ERROR_SOURCE: &str = "x-oagw-error-source";
/// Routing header consumed (and stripped) by the data plane.
pub const HEADER_TARGET_HOST: &str = "x-oagw-target-host";
/// Propagated/generated request correlation id.
pub const HEADER_REQUEST_ID: &str = "x-request-id";

/// [`HEADER_REQUEST_ID`] as a [`http::HeaderName`].
///
/// # Panics
/// Never: the constant is a known-good lowercase header name.
#[must_use]
#[allow(clippy::expect_used)] // a static, valid header name
pub fn request_id_header() -> http::HeaderName {
    http::HeaderName::from_bytes(HEADER_REQUEST_ID.as_bytes()).expect("static header name")
}

/// `X-OAGW-Error-Source` value for gateway-generated responses.
pub const ERROR_SOURCE_GATEWAY: &str = "gateway";
/// `X-OAGW-Error-Source` value for upstream passthroughs.
pub const ERROR_SOURCE_UPSTREAM: &str = "upstream";
