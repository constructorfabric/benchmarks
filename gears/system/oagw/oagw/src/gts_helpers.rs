//! GTS identifier constants used across the gear.
//!
//! Every identifier the gear emits or compares on the wire is declared here so
//! the string forms exist in exactly one place.

/// Protocol identifier for a plain `HTTP` upstream.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// Protocol identifier for a `gRPC` upstream (accepted, not proxied).
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// Prefix of the upstream resource identifier.
pub const UPSTREAM_TYPE: &str = "gts.cf.core.oagw.upstream.v1~";
/// Prefix of the route resource identifier.
pub const ROUTE_TYPE: &str = "gts.cf.core.oagw.route.v1~";
/// Prefix of the proxy permission identifier.
pub const PROXY_PERMISSION: &str = "gts.cf.core.oagw.proxy.v1~";

/// Prefix of an auth plugin identifier.
pub const AUTH_PLUGIN_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// Prefix of a guard plugin identifier.
pub const GUARD_PLUGIN_TYPE: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// Prefix of a transform plugin identifier.
pub const TRANSFORM_PLUGIN_TYPE: &str = "gts.cf.core.oagw.transform_plugin.v1~";

/// Built-in auth plugin that injects nothing.
pub const AUTH_NOOP: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
/// Built-in auth plugin injecting an API key into a header or the query string.
pub const AUTH_APIKEY: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// Built-in auth plugin performing the `OAuth2` client-credentials grant with
/// `Form` client authentication.
pub const AUTH_OAUTH2_CLIENT_CRED: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1";
/// Built-in auth plugin performing the `OAuth2` client-credentials grant with
/// `Basic` client authentication.
pub const AUTH_OAUTH2_CLIENT_CRED_BASIC: &str =
    "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1";

/// Catalogue-only auth identifier with no backing implementation.
pub const AUTH_BASIC: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1";
/// Catalogue-only auth identifier with no backing implementation.
pub const AUTH_BEARER: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";

/// Built-in guard plugin requiring named request and response headers.
pub const GUARD_REQUIRED_HEADERS: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// Catalogue-only guard identifier; timeout is core data-plane configuration.
pub const GUARD_TIMEOUT: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1";
/// Catalogue-only guard identifier; CORS is the `cors` configuration field.
pub const GUARD_CORS: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1";

/// Built-in transform plugin generating and propagating a correlation id.
pub const TRANSFORM_REQUEST_ID: &str =
    "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
/// Catalogue-only transform identifier; logging is core instrumentation.
pub const TRANSFORM_LOGGING: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1";
/// Catalogue-only transform identifier; metrics are core instrumentation.
pub const TRANSFORM_METRICS: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1";

/// Prefix of every OAGW error type identifier.
pub const ERROR_TYPE_PREFIX: &str = "gts.cf.core.errors.err.v1~cf.oagw";

/// Builds the error type identifier for the given OAGW error family.
#[must_use]
pub fn error_type_id(family: &str) -> String {
    format!("{ERROR_TYPE_PREFIX}.{family}.v1")
}

#[cfg(test)]
#[path = "gts_helpers_tests.rs"]
mod tests;
