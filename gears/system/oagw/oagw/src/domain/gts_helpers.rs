//! GTS identifiers used by the OAGW module.
//!
//! Resource type ids are `gts.<type>`; resource *instances* are
//! `<type><uuid>`. Error ids are instances of the shared
//! `gts.cf.core.errors.err.v1~` type, per DESIGN §3.3.

use uuid::Uuid;

/// GTS type id of the `Upstream` entity.
pub const UPSTREAM_TYPE: &str = "gts.cf.core.oagw.upstream.v1~";
/// GTS type id of the `Route` entity.
pub const ROUTE_TYPE: &str = "gts.cf.core.oagw.route.v1~";
/// GTS type id of the custom (tenant-defined) `Plugin` entity.
pub const PLUGIN_TYPE: &str = "gts.cf.core.oagw.plugin.v1~";
/// GTS type id of the `AuthPlugin` catalog family.
pub const AUTH_PLUGIN_TYPE: &str = "gts.cf.core.oagw.auth_plugin.v1~";
/// GTS type id of the `GuardPlugin` catalog family.
pub const GUARD_PLUGIN_TYPE: &str = "gts.cf.core.oagw.guard_plugin.v1~";
/// GTS type id of the `TransformPlugin` catalog family.
pub const TRANSFORM_PLUGIN_TYPE: &str = "gts.cf.core.oagw.transform_plugin.v1~";
/// GTS type id of the protocol enumeration family.
pub const PROTOCOL_TYPE: &str = "gts.cf.core.oagw.protocol.v1~";

/// Shared GTS type id whose *instances* enumerate OAGW error kinds.
pub const ERROR_TYPE: &str = "gts.cf.core.errors.err.v1~";

/// Fully-qualified protocol GTS id for the HTTP upstream protocol.
pub const PROTOCOL_HTTP: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// Fully-qualified protocol GTS id for the gRPC upstream protocol.
pub const PROTOCOL_GRPC: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1";

/// Build an anonymous GTS instance id for an `Upstream`.
#[must_use]
pub fn upstream_id(uuid: Uuid) -> String {
    format!("{UPSTREAM_TYPE}{uuid}")
}

/// Build an anonymous GTS instance id for a `Route`.
#[must_use]
pub fn route_id(uuid: Uuid) -> String {
    format!("{ROUTE_TYPE}{uuid}")
}

/// Build an anonymous GTS instance id for a custom `Plugin`.
#[must_use]
pub fn plugin_id(uuid: Uuid) -> String {
    format!("{PLUGIN_TYPE}{uuid}")
}

/// Fully-qualified built-in auth plugin id.
#[must_use]
pub fn auth_plugin(name: &str) -> String {
    format!("{AUTH_PLUGIN_TYPE}cf.core.oagw.{name}.v1")
}

/// Fully-qualified built-in guard plugin id.
#[must_use]
pub fn guard_plugin(name: &str) -> String {
    format!("{GUARD_PLUGIN_TYPE}cf.core.oagw.{name}.v1")
}

/// Fully-qualified built-in transform plugin id.
#[must_use]
pub fn transform_plugin(name: &str) -> String {
    format!("{TRANSFORM_PLUGIN_TYPE}cf.core.oagw.{name}.v1")
}

/// OAGW error instance ids (DESIGN §3.3 error table).
pub mod errors {
    /// RouteError / ValidationError (400).
    pub const VALIDATION: &str = "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1";
    /// MissingTargetHost (400).
    pub const ROUTING_MISSING_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1";
    /// InvalidTargetHost (400).
    pub const ROUTING_INVALID_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1";
    /// UnknownTargetHost (400).
    pub const ROUTING_UNKNOWN_TARGET_HOST: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1";
    /// AuthenticationFailed (401).
    pub const AUTH_FAILED: &str = "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1";
    /// RouteNotFound (404).
    pub const ROUTE_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1";
    /// PluginInUse (409).
    pub const PLUGIN_IN_USE: &str = "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1";
    /// PayloadTooLarge (413).
    pub const PAYLOAD_TOO_LARGE: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.payload.too_large.v1";
    /// RateLimitExceeded (429).
    pub const RATE_LIMIT_EXCEEDED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1";
    /// SecretNotFound (500).
    pub const SECRET_NOT_FOUND: &str = "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1";
    /// ProtocolError (502).
    pub const PROTOCOL_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.protocol.error.v1";
    /// DownstreamError (502).
    pub const DOWNSTREAM_ERROR: &str = "gts.cf.core.errors.err.v1~cf.oagw.downstream.error.v1";
    /// StreamAborted (502).
    pub const STREAM_ABORTED: &str = "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1";
    /// LinkUnavailable (503).
    pub const LINK_UNAVAILABLE: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";
    /// CircuitBreakerOpen (503).
    pub const CIRCUIT_BREAKER_OPEN: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1";
    /// PluginNotFound (503).
    pub const PLUGIN_NOT_FOUND: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1";
    /// ConnectionTimeout (504).
    pub const TIMEOUT_CONNECTION: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.timeout.connection.v1";
    /// RequestTimeout (504).
    pub const TIMEOUT_REQUEST: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.request.v1";
    /// IdleTimeout (504).
    pub const TIMEOUT_IDLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1";

    /// Alias conflicts are not enumerated in the DESIGN error table; they
    /// reuse the instance-id shape so clients can still branch on `type`.
    pub const UPSTREAM_CONFLICT: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.upstream.conflict.v1";
    /// Route match-rule conflict (409), same caveat as [`UPSTREAM_CONFLICT`].
    pub const ROUTE_CONFLICT: &str = "gts.cf.core.errors.err.v1~cf.oagw.route.conflict.v1";

    /// ADR-0004 CORS rejections.
    pub const CORS_ORIGIN_NOT_ALLOWED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1";
    /// ADR-0004 CORS rejections.
    pub const CORS_METHOD_NOT_ALLOWED: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1";

    /// ADR-0009 required-headers guard.
    pub const REQUIRED_HEADER_MISSING: &str =
        "gts.cf.core.errors.err.v1~cf.oagw.required_header.missing.v1";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_ids_embed_the_uuid() {
        let uuid = Uuid::nil();
        assert_eq!(upstream_id(uuid), "gts.cf.core.oagw.upstream.v1~00000000-0000-0000-0000-000000000000");
        assert_eq!(route_id(uuid), "gts.cf.core.oagw.route.v1~00000000-0000-0000-0000-000000000000");
        assert_eq!(plugin_id(uuid), "gts.cf.core.oagw.plugin.v1~00000000-0000-0000-0000-000000000000");
    }

    #[test]
    fn built_in_plugin_ids_follow_the_catalog() {
        assert_eq!(
            auth_plugin("apikey"),
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
        );
        assert_eq!(
            guard_plugin("required_headers"),
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
        );
        assert_eq!(
            transform_plugin("request_id"),
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
        );
    }
}
