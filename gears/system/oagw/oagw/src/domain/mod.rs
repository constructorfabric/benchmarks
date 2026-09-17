//! Domain layer for the OAGW gear.
//!
//! Contains the OAGW domain model (`models`), the persistence-free
//! authoritative control-plane repository (`repository`), effective
//! configuration resolution and caches (`effective`), the plugin registry
//! (`plugins`), and the data-plane collaboration helpers: rate limiting
//! (`rate_limit`), CORS (`cors`), SSRF protection (`ssrf`) and credential
//! resolution (`credentials`).

pub mod cors;
pub mod credentials;
pub mod effective;
pub mod error;
pub mod models;
pub mod plugins;
pub mod rate_limit;
pub mod repository;
pub mod ssrf;

pub use crate::domain::error::DomainError;
pub use crate::domain::models::{
    CorsConfig, EffectiveRouteConfig, Plugin, PluginKind, RateLimitConfig, Route, RouteHttpMatch,
    Upstream, UpstreamScheme,
};
pub use crate::domain::repository::{ControlPlaneService, RepositoryError};
