//! Domain layer: models, errors, alias rules, plugin contracts, repositories.

pub mod alias;
pub mod error;
pub mod gts_helpers;
pub mod model;
pub mod plugin;
pub mod repo;
pub mod service;

pub use error::{DomainError, DomainResult, ErrorKind};
pub use model::{
    AuthConfig, CorsConfig, Endpoint, EndpointScheme, GrpcMatch, HeaderRules, HeadersConfig,
    HttpMatch, PassthroughMode, PathSuffixMode, PluginBinding, PluginsConfig, Protocol,
    RateAlgorithm, RateLimitConfig, RateScope, RateStrategy, RateWindow, Route, RouteMatch,
    ServerConfig, SharingMode, SustainedRate, Upstream,
};
pub use repo::ListFilter;
