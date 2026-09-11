//! GTS identifier constants of the OAGW catalogue.
//!
//! Realizes `cpt-cf-oagw-dod-gts-type-catalog`. Every identifier is written
//! as a `gts_id!` literal so the value is the catalogue row verbatim — the
//! problem `type` of an error is never synthesized from a variant name.
//!
//! Built-in plugin instance identifiers
//! (`gts.cf.core.oagw.{auth,guard,transform}_plugin.v1~cf.core.oagw.*.v1`) are
//! not written beside these: they are the twelve rows of
//! [`crate::gts::plugin_catalog`], the table the plugin-system feature owns,
//! which also decides which of them a registry backs and which are
//! catalog-only.

// @cpt-dod:cpt-cf-oagw-dod-gts-type-catalog:p1
pub mod catalog;
pub mod plugin_catalog;
pub mod provisioning;

use toolkit_gts::gts_id;
use uuid::Uuid;

/// Base type schema of the `Upstream` aggregate.
pub const UPSTREAM_TYPE: &str = gts_id!("cf.core.oagw.upstream.v1~");
/// Base type schema of the `Route` aggregate.
pub const ROUTE_TYPE: &str = gts_id!("cf.core.oagw.route.v1~");
/// Base type schema of the proxy API, whose `invoke` permission the Data Plane
/// enforces before any resolution runs.
pub const PROXY_TYPE: &str = gts_id!("cf.core.oagw.proxy.v1~");
/// Base type schema of the metrics surface
/// `cpt-cf-oagw-feature-observability` registers, whose `read` permission the
/// scrape is enforced with.
pub const METRICS_TYPE: &str = gts_id!("cf.core.oagw.metrics.v1~");
/// Base type schema of the protocol values.
pub const PROTOCOL_TYPE: &str = gts_id!("cf.core.oagw.protocol.v1~");
/// Base type schema of the auth plugins.
pub const AUTH_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.auth_plugin.v1~");
/// Base type schema of the guard plugins.
pub const GUARD_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.guard_plugin.v1~");
/// Base type schema of the transform plugins.
pub const TRANSFORM_PLUGIN_TYPE: &str = gts_id!("cf.core.oagw.transform_plugin.v1~");

/// The descendant permission to create an upstream whose alias matches an
/// ancestor's: the bind of DESIGN §3.2's four-permission table.
pub const PERMISSION_BIND: &str = "oagw:upstream:bind";
/// The descendant permission to override an `inherit` authentication family.
pub const PERMISSION_OVERRIDE_AUTH: &str = "oagw:upstream:override_auth";
/// The descendant permission to declare an own rate limit under `min()`.
pub const PERMISSION_OVERRIDE_RATE: &str = "oagw:upstream:override_rate";
/// The descendant permission to append own plugin items to an inherited chain.
pub const PERMISSION_ADD_PLUGINS: &str = "oagw:upstream:add_plugins";
/// Base type schema of the gateway errors: the namespace of every problem
/// `type`.
pub const ERROR_TYPE: &str = gts_id!("cf.core.errors.err.v1~");

/// HTTP protocol instance.
pub const PROTOCOL_HTTP: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.http.v1");
/// gRPC protocol instance.
pub const PROTOCOL_GRPC: &str = gts_id!("cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1");

/// The `type` of the CORS origin refusal ADR 0004 spells, which is a bare
/// problem answer and not a catalogue row: DESIGN §3.3's catalogue is closed
/// at 22 variants over 21 identifiers and carries no 403 row (§1.5 of the
/// FEATURE).
pub const ERR_CORS_ORIGIN_NOT_ALLOWED: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1");
/// The `type` of the CORS method refusal ADR 0004 spells, which is a bare
/// problem answer and not a catalogue row for the same reason.
pub const ERR_CORS_METHOD_NOT_ALLOWED: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1");

/// `RouteError` and `ValidationError` share this identifier.
pub const ERR_VALIDATION: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.validation.error.v1");
/// `MissingTargetHost`.
pub const ERR_MISSING_TARGET_HOST: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1");
/// `InvalidTargetHost`.
pub const ERR_INVALID_TARGET_HOST: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1");
/// `UnknownTargetHost`.
pub const ERR_UNKNOWN_TARGET_HOST: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1");
/// `AuthenticationFailed`.
pub const ERR_AUTH_FAILED: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.auth.failed.v1");
/// `RouteNotFound`.
pub const ERR_ROUTE_NOT_FOUND: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.route.not_found.v1");
/// `PluginInUse`.
pub const ERR_PLUGIN_IN_USE: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1");
/// `AliasConflict`, added per §1.5.
pub const ERR_ALIAS_CONFLICT: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.alias.conflict.v1");
/// `MatchConflict`, added per §1.5.
pub const ERR_MATCH_CONFLICT: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.match.conflict.v1");
/// `PayloadTooLarge`.
pub const ERR_PAYLOAD_TOO_LARGE: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.payload.too_large.v1");
/// `RateLimitExceeded`.
pub const ERR_RATE_LIMIT_EXCEEDED: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.rate_limit.exceeded.v1");
/// `SecretNotFound`.
pub const ERR_SECRET_NOT_FOUND: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.secret.not_found.v1");
/// `ProtocolError`.
pub const ERR_PROTOCOL_ERROR: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.protocol.error.v1");
/// `DownstreamError` (§1.5 resolves its `Depends` cell to non-retriable).
pub const ERR_DOWNSTREAM_ERROR: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.downstream.error.v1");
/// `StreamAborted`.
pub const ERR_STREAM_ABORTED: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.stream.aborted.v1");
/// `LinkUnavailable`.
pub const ERR_LINK_UNAVAILABLE: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.link.unavailable.v1");
/// `CircuitBreakerOpen`.
pub const ERR_CIRCUIT_BREAKER_OPEN: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1");
/// `PluginNotFound`.
pub const ERR_PLUGIN_NOT_FOUND: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1");
/// `ConnectionTimeout`.
pub const ERR_TIMEOUT_CONNECTION: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.connection.v1");
/// `RequestTimeout`.
pub const ERR_TIMEOUT_REQUEST: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.request.v1");
/// `IdleTimeout`.
pub const ERR_TIMEOUT_IDLE: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.timeout.idle.v1");

/// Renders the anonymous GTS instance identifier one row answers to: the base
/// type schema followed by the row's `Uuid`.
#[must_use]
pub fn gts_instance(prefix: &str, id: Uuid) -> String {
    format!("{prefix}{id}")
}

/// Parses the anonymous GTS instance identifier of one resource kind into the
/// `Uuid` it names, accepting the bare `Uuid` as well.
#[must_use]
pub fn parse_gts_instance(prefix: &str, value: &str) -> Option<Uuid> {
    value
        .strip_prefix(prefix)
        .and_then(|tail| Uuid::parse_str(tail).ok())
        .or_else(|| Uuid::parse_str(value).ok())
}
