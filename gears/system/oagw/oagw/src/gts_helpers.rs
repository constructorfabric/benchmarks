//! GTS identifiers used by the OAGW gear.
//!
//! Two families live here:
//!
//! * the resource types the gear owns (`gts.cf.core.oagw.<kind>.v1~`), used for
//!   permissions and for the GTS instances the gear registers in
//!   types-registry; and
//! * the problem-document `type` identifiers the gateway puts on every
//!   generated error, of the form
//!   `gts.cf.core.errors.err.v1~cf.oagw.<name>.v1` (see `docs/DESIGN.md`).

/// Builds the problem-document `type` for an OAGW error name.
///
/// # Examples
/// ```
/// # use oagw::gts_helpers::error_type;
/// assert_eq!(
///     error_type("route.not_found"),
///     "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
/// );
/// ```
#[must_use]
pub fn error_type(suffix: &str) -> String {
    format!("gts.cf.core.errors.err.v1~cf.oagw.{suffix}.v1")
}

/// Compile-time problem-document `type` for an OAGW error name.
macro_rules! oagw_error_type {
    ($suffix:literal) => {
        concat!("gts.cf.core.errors.err.v1~cf.oagw.", $suffix, ".v1")
    };
}

/// Upstream resource type.
pub const UPSTREAM_GTS_ID: &str = "gts.cf.core.oagw.upstream.v1~";
/// Route resource type.
pub const ROUTE_GTS_ID: &str = "gts.cf.core.oagw.route.v1~";
/// Auth plugin resource type.
pub const AUTH_PLUGIN_GTS_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// Guard plugin resource type.
pub const GUARD_PLUGIN_GTS_ID: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// Transform plugin resource type.
pub const TRANSFORM_PLUGIN_GTS_ID: &str = "gts.cf.core.oagw.transform_plugin.v1~";
/// Proxy invocation action type.
pub const PROXY_GTS_ID: &str = "gts.cf.core.oagw.proxy.v1~";

// Problem-document `type` identifiers, one per row of the error table in
// `docs/DESIGN.md` and `specs/001-oagw-gear-implementation/contracts/errors.md`.
pub const ERR_VALIDATION: &str = oagw_error_type!("validation.error");
pub const ERR_MISSING_TARGET_HOST: &str = oagw_error_type!("routing.missing_target_host");
pub const ERR_INVALID_TARGET_HOST: &str = oagw_error_type!("routing.invalid_target_host");
pub const ERR_UNKNOWN_TARGET_HOST: &str = oagw_error_type!("routing.unknown_target_host");
pub const ERR_AUTH_FAILED: &str = oagw_error_type!("auth.failed");
pub const ERR_ROUTE_NOT_FOUND: &str = oagw_error_type!("route.not_found");
pub const ERR_PLUGIN_IN_USE: &str = oagw_error_type!("plugin.in_use");
pub const ERR_PAYLOAD_TOO_LARGE: &str = oagw_error_type!("payload.too_large");
pub const ERR_RATE_LIMIT_EXCEEDED: &str = oagw_error_type!("rate_limit.exceeded");
pub const ERR_SECRET_NOT_FOUND: &str = oagw_error_type!("secret.not_found");
pub const ERR_PROTOCOL: &str = oagw_error_type!("protocol.error");
pub const ERR_DOWNSTREAM: &str = oagw_error_type!("downstream.error");
pub const ERR_STREAM_ABORTED: &str = oagw_error_type!("stream.aborted");
pub const ERR_LINK_UNAVAILABLE: &str = oagw_error_type!("link.unavailable");
pub const ERR_CIRCUIT_BREAKER_OPEN: &str = oagw_error_type!("circuit_breaker.open");
pub const ERR_PLUGIN_NOT_FOUND: &str = oagw_error_type!("plugin.not_found");
pub const ERR_CONNECTION_TIMEOUT: &str = oagw_error_type!("timeout.connection");
pub const ERR_REQUEST_TIMEOUT: &str = oagw_error_type!("timeout.request");
pub const ERR_IDLE_TIMEOUT: &str = oagw_error_type!("timeout.idle");
pub const ERR_CORS_ORIGIN_NOT_ALLOWED: &str = oagw_error_type!("cors.origin_not_allowed");
pub const ERR_CORS_METHOD_NOT_ALLOWED: &str = oagw_error_type!("cors.method_not_allowed");

#[cfg(test)]
mod gts_helpers_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn error_type_builds_documented_identifiers() {
        assert_eq!(
            error_type("route.not_found"),
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
        assert_eq!(error_type("validation.error"), ERR_VALIDATION);
        assert_eq!(error_type("rate_limit.exceeded"), ERR_RATE_LIMIT_EXCEEDED);
    }

    #[test]
    fn resource_types_are_documented_gts_ids() {
        assert_eq!(UPSTREAM_GTS_ID, "gts.cf.core.oagw.upstream.v1~");
        assert_eq!(ROUTE_GTS_ID, "gts.cf.core.oagw.route.v1~");
        assert_eq!(PROXY_GTS_ID, "gts.cf.core.oagw.proxy.v1~");
        assert!(AUTH_PLUGIN_GTS_ID.ends_with('~'));
    }
}
