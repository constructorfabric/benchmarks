//! Domain entities mirroring the §3.1 domain model and §3.7 table shapes
//! (feature `cpt-cf-oagw-feature-domain-model-repositories`, DoD
//! `cpt-cf-oagw-dod-domain-model-repositories-entities`, flow
//! `cpt-cf-oagw-flow-domain-model-repositories-persist`).
//!
//! Entity modules are deliberately cohesive and small:
//! - [`config`] — hierarchical configuration value types (`RateLimitConfig`,
//!   `CorsConfig`, `PluginsConfig`, sharing modes, …);
//! - [`upstream`] — `Upstream`, `ServerConfig`/`Endpoint`, `AuthConfig`,
//!   vault-aware `cred://` [`SecretRef`](upstream::SecretRef);
//! - [`route`] — `Route`, `RouteMatch` (HTTP + reserved gRPC), `RouteMethod`;
//! - [`plugin`] — `Plugin`, `PluginConfig`, `PluginType`;
//! - [`alias`] — alias derivation/enforcement and the configuration-boundary
//!   SSRF guard.

pub mod alias;
pub mod config;
pub mod plugin;
pub mod route;
pub mod upstream;

pub use alias::{
    compute_derived_alias, validate_alias, validate_endpoint_url, validate_rfc1123_hostname,
};
pub use config::{
    CorsConfig, EndpointScheme, HeadersConfig, PluginBinding, PluginsConfig, RateLimitConfig,
    SharingMode, UpstreamProtocol,
};
pub use plugin::{Plugin, PluginConfig, PluginType, ReferencedBy, ReferencedByResource};
pub use route::{GrpcMatch, HttpMatch, PathSuffixMode, Route, RouteMatch, RouteMethod};
pub use upstream::{AuthConfig, Endpoint, SecretRef, ServerConfig, Upstream};
