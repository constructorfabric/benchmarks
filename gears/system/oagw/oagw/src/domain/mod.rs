//! The domain layer: the model the specification describes, the rules that
//! govern it, and the service that ties them together.
//!
//! Nothing here touches HTTP. The wire layer (`crate::api`) calls
//! [`service::Service`]; [`crate::infra`] holds the outbound transport and the
//! built-in plugin implementations.

pub mod alias;
pub mod clock;
pub mod list;
pub mod model;
pub mod plugin;
pub mod query;
pub mod ratelimit;
pub mod service;
pub mod store;

pub use alias::{derive, is_ip_literal, is_standard_port, normalize, validate_hostname};
pub use model::{
    AuthConfig, Burst, CorsRule, Endpoint, GrpcMatch, HeaderRules, HttpMatch, MatchRule,
    Passthrough, PluginBinding, PluginDefinition, PluginSet, PluginType, Protocol,
    RateLimitAlgorithm, RateLimitRule, RateLimitScope, RateLimitStrategy, RateLimitWindow, Route,
    Scheme, ServerConfig, Sharing, Sustained, Upstream,
};
pub use plugin::{AuthPlugin, ControlPlane, GuardPlugin, ProxyContext, TransformPlugin};
pub use ratelimit::{Decision, RateLimiter, SharedRateLimiter};
pub use service::{EffectiveConfig, ResolvedRequest, Service};
pub use store::{SharedStore, Store};
