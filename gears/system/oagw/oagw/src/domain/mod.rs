//! Domain layer: the OAGW control-plane model, its invariants, the
//! repository ports its services depend on, and the plugin SPI the data
//! plane executes.
//!
//! Nothing in this module may depend on a transport (axum) or on the
//! concrete storage implementation; `crate::infra` implements the ports.
//! [`plugin`] is the plugin SPI of `docs/ADR/0002-plugin-system.md`: it uses
//! the canonical `http::HeaderMap` and `serde_json::Value` so that a plugin
//! written outside this crate can speak to the data plane without pulling in
//! axum.
pub mod alias;
pub mod error;
pub mod gts;
pub mod model;
pub mod plugin;
pub mod query;
pub mod reason;
pub mod repo;
pub mod services;

pub use error::{DomainError, PluginReferences};
pub use model::{
    AuthConfig, BurstConfig, CorsConfig, Endpoint, EndpointScheme, GrpcMatch, HeaderPassthrough,
    HeaderSetting, HeadersConfig, HttpMatch, PathSuffixMode, Plugin, PluginBinding, PluginKind,
    PluginsConfig, Protocol, RateAlgorithm, RateLimitConfig, RateScope, RateStrategy, RateWindow,
    RequestHeaders, ResponseHeaders, Route, RouteMatcher, ServerConfig, SharingMode, SustainedRate,
    Upstream, now_millis,
};
pub use query::{CompareOp, FilterExpr, ListQuery, OrderSpec, StringFunc};
pub use repo::{ConfigStore, ControlPlaneSnapshot};
