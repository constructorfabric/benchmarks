// Created: 2026-09-02 by Constructor Tech
//! GTS identifier catalog for the outbound API gateway.
//!
//! Every identifier the gateway exposes on the wire lives here so the error
//! table in `DESIGN.md` §3.3, the plugin catalogue in `PRD.md` §5.3 and the
//! resource types in `DESIGN.md` §3.1 have exactly one source of truth.
//!
//! These identifiers are **catalogued, not registered**: `DESIGN.md` §3.2 states
//! that `basic`/`bearer`/`timeout`/`cors`/`logging`/`metrics` "exist for
//! types-registry cataloging only", and the plugin identifiers are resolved
//! through in-process registries rather than through the types-registry. The
//! gear therefore declares the catalogue as constants and leaves
//! `#[gts_type_schema]` provisioning to the platform's `type_provisioning`
//! path, which keeps `types-registry`'s ready-phase chain validation (which
//! fails the boot) out of the gateway's critical path.

use toolkit_gts::gts_id;

/// Base type id of an upstream configuration object.
pub const UPSTREAM_TYPE: &str = gts_id!("cf.core.oagw.upstream.v1~");
/// Base type id of a route configuration object.
pub const ROUTE_TYPE: &str = gts_id!("cf.core.oagw.route.v1~");
/// Base type id of an auth plugin.
pub const AUTH_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.auth_plugin.v1~");
/// Base type id of a guard plugin.
pub const GUARD_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.guard_plugin.v1~");
/// Base type id of a transform plugin.
pub const TRANSFORM_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.transform_plugin.v1~");
/// Base type id of a custom (Starlark) plugin of any of the three kinds.
pub const CUSTOM_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.plugin.v1~");

/// Upstream protocol: HTTP/1.1 and HTTP/2 request/response proxying.
pub const PROTOCOL_HTTP: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.http.v1");
/// Upstream protocol: gRPC (Phase 3, no proxy code path is reachable today).
pub const PROTOCOL_GRPC: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1");

/// Auth plugin: no credential injection.
pub const AUTH_NOOP: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1");
/// Auth plugin: API key injection (header or query).
pub const AUTH_APIKEY: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1");
/// Auth plugin: OAuth2 client credentials, form-encoded token request (ADR-0008).
pub const AUTH_OAUTH2_CC: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1");
/// Auth plugin: OAuth2 client credentials with Basic-auth token request (ADR-0008).
pub const AUTH_OAUTH2_CC_BASIC: &str =
    gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1");
/// Catalog-only: HTTP Basic authentication has no backing `AuthPlugin`.
pub const AUTH_BASIC: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1");
/// Catalog-only: bearer injection has no backing `AuthPlugin`.
pub const AUTH_BEARER: &str = gts_id!("cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1");

/// Guard plugin: required request/response header enforcement (ADR-0009).
pub const GUARD_REQUIRED_HEADERS: &str =
    gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1");
/// Catalog-only: request timeout is core data-plane configuration, not a plugin.
pub const GUARD_TIMEOUT: &str = gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1");
/// Catalog-only: CORS is the dedicated `cors` field, not a plugin.
pub const GUARD_CORS: &str = gts_id!("cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1");

/// Transform plugin: `X-Request-ID` injection and propagation.
pub const TRANSFORM_REQUEST_ID: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1");
/// Catalog-only: logging is core data-plane instrumentation.
pub const TRANSFORM_LOGGING: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1");
/// Catalog-only: metrics are core data-plane instrumentation.
pub const TRANSFORM_METRICS: &str =
    gts_id!("cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1");

/// GTS *instance* ids for every gateway error in the `DESIGN.md` §3.3 table.
///
/// The full id is `gts.cf.core.errors.err.v1~cf.oagw.<area>.<name>.v1`, so each
/// constant below carries only the `cf.oagw...` instance segment.
pub mod errors {
    use super::gts_id;

    /// 400 — general request/route validation failure.
    pub const VALIDATION: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.validation.error.v1");
    /// 400 — `X-OAGW-Target-Host` missing for a common-suffix pool.
    pub const ROUTING_MISSING_TARGET_HOST: &str =
        gts_id!("cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1");
    /// 400 — `X-OAGW-Target-Host` not a bare hostname or IP.
    pub const ROUTING_INVALID_TARGET_HOST: &str =
        gts_id!("cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1");
    /// 400 — `X-OAGW-Target-Host` names no configured endpoint.
    pub const ROUTING_UNKNOWN_TARGET_HOST: &str =
        gts_id!("cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1");
    /// 401 — upstream authentication failed (including secret resolution).
    pub const AUTH_FAILED: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.auth.failed.v1");
    /// 403 — SSRF policy rejected the upstream target.
    pub const SSRF_BLOCKED: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.ssrf.blocked.v1");
    /// 400 — plaintext upstream dialled while `allow_http_upstream` is off.
    pub const INSECURE_UPSTREAM: &str =
        gts_id!("cf.core.errors.err.v1~cf.oagw.upstream.insecure.v1");
    /// 404 — no route matched the request.
    pub const ROUTE_NOT_FOUND: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.route.not_found.v1");
    /// 404 — management resource does not exist (or is invisible to the caller).
    pub const RESOURCE_NOT_FOUND: &str =
        gts_id!("cf.core.errors.err.v1~cf.oagw.resource.not_found.v1");
    /// 409 — alias / route-match conflict, or an immutable-field change.
    pub const CONFLICT: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.conflict.state.v1");
    /// 409 — plugin still referenced by an upstream or route.
    pub const PLUGIN_IN_USE: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1");
    /// 413 — request body above the hard limit.
    pub const PAYLOAD_TOO_LARGE: &str =
        gts_id!("cf.core.errors.err.v1~cf.oagw.payload.too_large.v1");
    /// 429 — rate limit exceeded (retriable).
    pub const RATE_LIMIT_EXCEEDED: &str =
        gts_id!("cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1");
    /// 500 — referenced secret is not resolvable through the credential store.
    pub const SECRET_NOT_FOUND: &str =
        gts_id!("cf.core.errors.err.v1~cf.oagw.secret.not_found.v1");
    /// 502 — protocol-level error talking to the upstream.
    pub const PROTOCOL_ERROR: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.protocol.error.v1");
    /// 502 — upstream returned an error response (passthrough).
    pub const DOWNSTREAM_ERROR: &str =
        gts_id!("cf.core.errors.err.v1~cf.oagw.downstream.error.v1");
    /// 502 — a proxied stream aborted mid-flight.
    pub const STREAM_ABORTED: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.stream.aborted.v1");
    /// 503 — upstream unreachable or disabled (retriable).
    pub const LINK_UNAVAILABLE: &str =
        gts_id!("cf.core.errors.err.v1~cf.oagw.link.unavailable.v1");
    /// 503 — circuit breaker open (retriable).
    pub const CIRCUIT_BREAKER_OPEN: &str =
        gts_id!("cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1");
    /// 503 — a referenced plugin cannot be resolved.
    pub const PLUGIN_NOT_FOUND: &str =
        gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1");
    /// 504 — connect timeout (retriable).
    pub const TIMEOUT_CONNECTION: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.connection.v1");
    /// 504 — upstream response timeout (retriable).
    pub const TIMEOUT_REQUEST: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.request.v1");
    /// 504 — streaming idle timeout (retriable).
    pub const TIMEOUT_IDLE: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.idle.v1");
}

/// CORS problem type: the request origin is not allowed.
pub const CORS_ORIGIN_NOT_ALLOWED: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1");
/// CORS problem type: the request method is not allowed.
pub const CORS_METHOD_NOT_ALLOWED: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1");

/// The `X-OAGW-Target-Host` routing header (consumed by the gateway, never forwarded).
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";
/// The `X-OAGW-Error-Source` response header (ADR-0007).
pub const ERROR_SOURCE_HEADER: &str = "x-oagw-error-source";
/// The `X-Request-ID` propagation header (transform plugin + audit trail).
pub const REQUEST_ID_HEADER: &str = "x-request-id";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plugin_catalogue_matches_the_prd_tables() {
        // Built-in, registry-resolvable identifiers.
        assert!(AUTH_NOOP.ends_with("cf.core.oagw.noop.v1"));
        assert!(AUTH_APIKEY.ends_with("cf.core.oagw.apikey.v1"));
        assert!(AUTH_OAUTH2_CC.ends_with("cf.core.oagw.oauth2_client_cred.v1"));
        assert!(AUTH_OAUTH2_CC_BASIC.ends_with("cf.core.oagw.oauth2_client_cred_basic.v1"));
        assert!(GUARD_REQUIRED_HEADERS.ends_with("cf.core.oagw.required_headers.v1"));
        assert!(TRANSFORM_REQUEST_ID.ends_with("cf.core.oagw.request_id.v1"));
        // Catalog-only identifiers.
        assert!(AUTH_BASIC.ends_with("cf.core.oagw.basic.v1"));
        assert!(AUTH_BEARER.ends_with("cf.core.oagw.bearer.v1"));
        assert!(GUARD_TIMEOUT.ends_with("cf.core.oagw.timeout.v1"));
        assert!(GUARD_CORS.ends_with("cf.core.oagw.cors.v1"));
        assert!(TRANSFORM_LOGGING.ends_with("cf.core.oagw.logging.v1"));
        assert!(TRANSFORM_METRICS.ends_with("cf.core.oagw.metrics.v1"));
    }

    #[test]
    fn identifiers_carry_the_gts_prefix() {
        for id in [
            UPSTREAM_TYPE,
            ROUTE_TYPE,
            AUTH_PLUGIN_TYPE,
            GUARD_PLUGIN_TYPE,
            TRANSFORM_PLUGIN_TYPE,
            PROTOCOL_HTTP,
            PROTOCOL_GRPC,
            AUTH_NOOP,
        ] {
            assert!(id.starts_with("gts."), "{id} must be a full GTS id");
        }
    }

    #[test]
    fn protocol_ids_are_the_schema_enums() {
        assert_eq!(
            PROTOCOL_HTTP,
            "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        );
        assert_eq!(
            PROTOCOL_GRPC,
            "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1"
        );
    }

    #[test]
    fn error_types_share_the_canonical_error_base() {
        for ty in [
            errors::VALIDATION,
            errors::ROUTE_NOT_FOUND,
            errors::RATE_LIMIT_EXCEEDED,
            errors::TIMEOUT_REQUEST,
        ] {
            assert!(
                ty.starts_with("gts.cf.core.errors.err.v1~cf.oagw."),
                "{ty}"
            );
        }
    }
}
