// Created: 2026-09-04 by Constructor Tech
//! Data plane of the OAGW gear: the proxy engine, its header rules, rate
//! limiter, CORS handling and plugin chain
//! (`docs/DESIGN.md` §3.2, `docs/ADR/0001-request-routing.md`).
//!
//! The data plane is the only component that talks to an upstream: it resolves
//! a proxy request against the control plane, then applies the stages of
//! [`proxy`]. A streamed upstream answer is forwarded frame by frame; every
//! frame carries an idle timeout so a stalled upstream still surfaces as `504`
//! without imposing a total timeout on a live stream
//! ([`streaming`]).

pub mod cors;
pub mod headers;
pub mod plugins;
pub mod proxy;
pub mod ratelimit;
pub mod streaming;

pub use plugins::{
    ApiKeyAuthPlugin, AuthPlugin, CredentialRequest, CredentialResolver, ErrorContext,
    GuardDecision, GuardPlugin, NoopAuthPlugin, PluginRegistry, RequestContext,
    RequestIdTransformPlugin, RequiredHeadersGuardPlugin, ResponseContext, TransformPlugin,
};
pub use proxy::{CancelProbe, DataPlane, ProxyCall, never_cancelled, register_proxy_routes};
pub use ratelimit::{
    RATE_LIMIT_LIMIT_HEADER, RATE_LIMIT_REMAINING_HEADER, RATE_LIMIT_RESET_HEADER,
    RateLimitDecision, RateLimiter, ScopeIdentity,
};
pub use streaming::{InboundUpgrade, WEBSOCKET_TOKEN};

use toolkit_gts::gts_id;

/// GTS instance id of the `MissingTargetHost` gateway error
/// (`docs/DESIGN.md` §3.3).
pub const MISSING_TARGET_HOST_TYPE: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1");

/// GTS instance id of the `InvalidTargetHost` gateway error
/// (`docs/DESIGN.md` §3.3).
pub const INVALID_TARGET_HOST_TYPE: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1");

/// GTS instance id of the `UnknownTargetHost` gateway error
/// (`docs/DESIGN.md` §3.3).
pub const UNKNOWN_TARGET_HOST_TYPE: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1");

/// GTS instance id of the `CORS Origin Not Allowed` gateway error
/// (`docs/ADR/0004-cors.md` "Error Responses").
pub const CORS_ORIGIN_NOT_ALLOWED_TYPE: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.cors.origin_not_allowed.v1");

/// GTS instance id of the `CORS Method Not Allowed` gateway error
/// (`docs/ADR/0004-cors.md` "Error Responses").
pub const CORS_METHOD_NOT_ALLOWED_TYPE: &str =
    gts_id!("cf.core.errors.err.v1~cf.oagw.cors.method_not_allowed.v1");
