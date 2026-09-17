//! OAGW domain layer: models, error types, repository contracts and
//! services.
//!
//! The domain is deliberately storage-agnostic. Persistence lives behind
//! the repository traits in [`crate::domain::repo`] (in-memory
//! implementation in [`crate::infra`]), and the data plane is pluggable
//! through the plugin traits in [`crate::domain::plugin`].

pub mod error;
pub mod models;
pub mod plugin;
pub mod ratelimit;
pub mod repo;
pub mod services;

pub use models::{
    AuthConfig, CorsConfig, CorsMethod, Endpoint, EndpointScheme, GrpcMatch, HeaderTransformConfig,
    HeadersConfig, HttpMatch, PassthroughMode, PathSuffixMode, Plugin, PluginConfig, PluginItem,
    PluginKind, PluginRef, PluginsConfig, RateLimitAlgorithm, RateLimitConfig, RateLimitScope,
    RateLimitStrategy, RateLimitWindow, RequestHeaderRules, ResponseHeaderRules, Route, RouteMatch,
    ServerConfig, SharingMode, SustainedRate, Upstream, UpstreamProtocol, UpstreamStatus,
};
pub use ratelimit::RateLimitDecision;
