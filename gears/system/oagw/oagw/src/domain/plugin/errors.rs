//! Data-plane error catalogue (`docs/DESIGN.md` §3.3).
//!
//! The management plane pins its identifiers in `crate::api::rest::error`; the
//! data plane needs the same identifiers from inside the domain (a plugin
//! failure raised in the plugin engine has to carry its own GTS `type`), so
//! the catalogue lives here instead. Every constant is the bare GTS type id —
//! the `gts.` prefix and the `~<uuid>` instance suffix are added by the
//! transport layer.

/// Full GTS `type` identifier for a bare OAGW error id.
#[must_use]
pub fn problem_type(bare: &str) -> String {
    format!("gts.cf.core.errors.err.v1~{bare}")
}

/// 400 — generic request validation failure.
pub const VALIDATION: &str = "cf.oagw.validation.error.v1";
/// 400 — `X-OAGW-Target-Host` required but absent.
pub const MISSING_TARGET_HOST: &str = "cf.oagw.routing.missing_target_host.v1";
/// 400 — `X-OAGW-Target-Host` is not a bare hostname.
pub const INVALID_TARGET_HOST: &str = "cf.oagw.routing.invalid_target_host.v1";
/// 400 — `X-OAGW-Target-Host` names no configured endpoint.
pub const UNKNOWN_TARGET_HOST: &str = "cf.oagw.routing.unknown_target_host.v1";
/// 401 — authentication to the upstream failed.
pub const AUTH_FAILED: &str = "cf.oagw.auth.failed.v1";
/// 403 — cross-origin request from a disallowed origin.
pub const CORS_ORIGIN_NOT_ALLOWED: &str = "cf.oagw.cors.origin_not_allowed.v1";
/// 403 — cross-origin request using a disallowed method.
pub const CORS_METHOD_NOT_ALLOWED: &str = "cf.oagw.cors.method_not_allowed.v1";
/// 404 — no route matched the request.
pub const ROUTE_NOT_FOUND: &str = "cf.oagw.route.not_found.v1";
/// 413 — request payload exceeds the configured limit.
pub const PAYLOAD_TOO_LARGE: &str = "cf.oagw.payload.too_large.v1";
/// 429 — rate limit exhausted.
pub const RATE_LIMIT_EXCEEDED: &str = "cf.oagw.rate_limit.exceeded.v1";
/// 500 — a `cred://` reference resolved to nothing.
pub const SECRET_NOT_FOUND: &str = "cf.oagw.secret.not_found.v1";
/// 500 — opaque internal failure.
pub const INTERNAL: &str = "cf.core.err.internal.v1";
/// 502 — the upstream spoke a malformed protocol.
pub const PROTOCOL_ERROR: &str = "cf.oagw.protocol.error.v1";
/// 502 — the upstream returned an error.
pub const DOWNSTREAM_ERROR: &str = "cf.oagw.downstream.error.v1";
/// 502 — a streamed response was cut short.
pub const STREAM_ABORTED: &str = "cf.oagw.stream.aborted.v1";
/// 503 — the upstream link could not be established.
pub const LINK_UNAVAILABLE: &str = "cf.oagw.link.unavailable.v1";
/// 503 — a bound plugin reference resolved to nothing.
pub const PLUGIN_NOT_FOUND: &str = "cf.oagw.plugin.not_found.v1";
/// 504 — dialing the upstream timed out.
pub const CONNECTION_TIMEOUT: &str = "cf.oagw.timeout.connection.v1";
/// 504 — the upstream exchange exceeded the configured timeout.
pub const REQUEST_TIMEOUT: &str = "cf.oagw.timeout.request.v1";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_bare_ids() {
        assert_eq!(
            problem_type(ROUTE_NOT_FOUND),
            "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
        );
    }

    #[test]
    fn every_catalogued_id_is_namespaced() {
        for bare in [
            VALIDATION,
            MISSING_TARGET_HOST,
            INVALID_TARGET_HOST,
            UNKNOWN_TARGET_HOST,
            AUTH_FAILED,
            CORS_ORIGIN_NOT_ALLOWED,
            CORS_METHOD_NOT_ALLOWED,
            ROUTE_NOT_FOUND,
            PAYLOAD_TOO_LARGE,
            RATE_LIMIT_EXCEEDED,
            SECRET_NOT_FOUND,
            INTERNAL,
            PROTOCOL_ERROR,
            DOWNSTREAM_ERROR,
            STREAM_ABORTED,
            LINK_UNAVAILABLE,
            PLUGIN_NOT_FOUND,
            CONNECTION_TIMEOUT,
            REQUEST_TIMEOUT,
        ] {
            assert!(
                bare.starts_with("cf."),
                "catalogue ids must carry their namespace: {bare}"
            );
        }
    }
}
