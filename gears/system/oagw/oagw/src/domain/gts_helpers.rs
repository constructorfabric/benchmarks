//! GTS identifiers reserved by the gateway.
//!
//! The resource and plugin identifiers are the ones the JSON Schemas and ADR-0009 use; the error
//! identifiers are the ones DESIGN §Error Response Format tabulates. A handful of plugin
//! identifiers (`basic`, `bearer`, `timeout`, `cors`, `logging`, `metrics`) are *catalog only*:
//! they appear in the catalogue response so callers cannot mistake them for available plugins, but
//! no implementation is registered for them and binding them fails validation.

/// GTS namespace prefix of a stored upstream resource.
pub const UPSTREAM_GTS: &str = "gts.cf.core.oagw.upstream.v1~";
/// GTS namespace prefix of a stored route resource.
pub const ROUTE_GTS: &str = "gts.cf.core.oagw.route.v1~";
/// GTS namespace prefix of a stored plugin resource.
pub const PLUGIN_GTS: &str = "gts.cf.core.oagw.plugin.v1~";

/// Root of the proxy link type (`...proxy.v1~<upstream-id>`).
pub const PROXY_GTS: &str = "gts.cf.core.oagw.proxy.v1~";

/// HTTP protocol identifier.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// gRPC protocol identifier (validated but not proxied; DESIGN §3.1).
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// `auth.type` accepted by the `apikey` built-in.
pub const AUTH_NOOP: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
/// `auth.type` accepted by the `apikey` built-in.
pub const AUTH_APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// OAuth2 client-credentials plugin with form client authentication.
pub const AUTH_OAUTH2_CC: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
/// OAuth2 client-credentials plugin with basic client authentication.
pub const AUTH_OAUTH2_CC_BASIC: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";

/// Catalog-only auth plugin: accepted by the schema, rejected by the data plane.
pub const AUTH_BASIC: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
/// Catalog-only auth plugin: accepted by the schema, rejected by the data plane.
pub const AUTH_BEARER: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";

/// Catalog-only guard plugin (`timeout`) — no implementation in this release.
pub const GUARD_TIMEOUT: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
/// `required_headers` guard plugin.
pub const GUARD_REQUIRED_HEADERS: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// Catalog-only guard plugin (`cors`) — CORS is built into the gear, not a plugin.
pub const GUARD_CORS: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";

/// `request_id` transform plugin.
pub const TRANSFORM_REQUEST_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
/// Catalog-only transform plugin (`logging`) — no implementation in this release.
pub const TRANSFORM_LOGGING: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
/// Catalog-only transform plugin (`metrics`) — no implementation in this release.
pub const TRANSFORM_METRICS: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

/// Error type: request or configuration failed validation.
pub const ERR_VALIDATION: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";
/// Error type: no route matched the request.
pub const ERR_ROUTE_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
/// Error type: the alias resolved to no upstream.
pub const ERR_UPSTREAM_NOT_FOUND: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.upstream.not_found.v1";
/// Error type: the referenced plugin does not exist.
pub const ERR_PLUGIN_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1";
/// Error type: the plugin is still bound to an upstream or route.
pub const ERR_PLUGIN_IN_USE: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1";
/// Error type: credential acquisition failed.
pub const ERR_AUTH_FAILED: &str = "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1";
/// Error type: a referenced secret could not be resolved.
pub const ERR_SECRET_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1";
/// Error type: the request body exceeded the configured ceiling.
pub const ERR_PAYLOAD_TOO_LARGE: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1";
/// Error type: a rate limit was exhausted.
pub const ERR_RATE_LIMIT: &str = "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
/// Error type: the upstream could not be reached.
pub const ERR_DOWNSTREAM: &str = "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1";
/// Error type: the upstream did not answer in time.
pub const ERR_TIMEOUT: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1";
/// Error type: the circuit breaker for the upstream is open.
pub const ERR_CIRCUIT_OPEN: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1";
/// Error type: the upstream answered with a protocol violation.
pub const ERR_PROTOCOL: &str = "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1";
/// Error type: a proxied stream was interrupted.
pub const ERR_STREAM_ABORTED: &str = "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1";
/// Error type: a link referenced by the request does not exist.
pub const ERR_LINK_UNAVAILABLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";
/// Error type: the multi-endpoint upstream requires `X-OAGW-Target-Host`.
pub const ERR_MISSING_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1";
/// Error type: `X-OAGW-Target-Host` was malformed.
pub const ERR_INVALID_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1";
/// Error type: `X-OAGW-Target-Host` named no configured endpoint.
pub const ERR_UNKNOWN_TARGET_HOST: &str =
    "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1";
/// Error type: the request origin is not allowed by the CORS configuration.
pub const ERR_CORS_ORIGIN: &str = "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
/// Error type: the request method is not allowed by the CORS configuration.
pub const ERR_CORS_METHOD: &str = "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";

/// Error code carried in problem bodies for a missing required header.
pub const CODE_REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

/// GTS id of the resource this crate stores.
#[must_use]
pub fn upstream_gts(id: uuid::Uuid) -> String {
    format!("{UPSTREAM_GTS}{id}")
}

/// GTS id of the route resource this crate stores.
#[must_use]
pub fn route_gts(id: uuid::Uuid) -> String {
    format!("{ROUTE_GTS}{id}")
}

/// GTS id of the plugin resource this crate stores.
#[must_use]
pub fn plugin_gts(id: uuid::Uuid) -> String {
    format!("{PLUGIN_GTS}{id}")
}

/// GTS id of the proxy link that leads from an upstream to its routes.
#[must_use]
pub fn proxy_gts(id: uuid::Uuid) -> String {
    format!("{PROXY_GTS}{id}")
}

/// True when `candidate` is one of the two protocol identifiers this crate accepts.
#[must_use]
pub fn is_known_protocol(candidate: &str) -> bool {
    candidate == PROTOCOL_HTTP || candidate == PROTOCOL_GRPC
}

/// The catalogue of auth plugin identifiers, including the ones with no implementation.
#[must_use]
pub fn auth_plugin_catalog() -> Vec<&'static str> {
    vec![
        AUTH_NOOP,
        AUTH_APIKEY,
        AUTH_OAUTH2_CC,
        AUTH_OAUTH2_CC_BASIC,
        AUTH_BASIC,
        AUTH_BEARER,
    ]
}

/// The catalogue of guard plugin identifiers, including the ones with no implementation.
#[must_use]
pub fn guard_plugin_catalog() -> Vec<&'static str> {
    vec![GUARD_REQUIRED_HEADERS, GUARD_TIMEOUT, GUARD_CORS]
}

/// The catalogue of transform plugin identifiers, including the ones with no implementation.
#[must_use]
pub fn transform_plugin_catalog() -> Vec<&'static str> {
    vec![TRANSFORM_REQUEST_ID, TRANSFORM_LOGGING, TRANSFORM_METRICS]
}

#[cfg(test)]
#[path = "gts_helpers_tests.rs"]
mod tests;

/// The whole catalogue of built-in plugins as `(identifier, kind)` pairs.
#[must_use]
pub fn builtin_catalogue() -> Vec<(&'static str, &'static str)> {
    let mut all: Vec<(&'static str, &'static str)> = auth_plugin_catalog()
        .into_iter()
        .map(|id| (id, "auth"))
        .collect();
    all.extend(guard_plugin_catalog().into_iter().map(|id| (id, "guard")));
    all.extend(
        transform_plugin_catalog()
            .into_iter()
            .map(|id| (id, "transform")),
    );
    all
}
