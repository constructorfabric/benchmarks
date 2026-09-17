//! Domain layer: entities, errors, repository contracts, services and the
//! plugin traits. No transport and no storage dependency lives here.

/// Domain entities and wire shapes.
pub mod dto;
/// Domain error taxonomy (DESIGN §3.3).
pub mod error;
/// Tenant hierarchy: chain resolution and graded configuration.
pub mod hierarchy;
/// Plugin contracts and the built-in plugin registry interface.
pub mod plugin;
/// Repository contracts.
pub mod repo;
/// Control-plane services.
pub mod services;

/// GTS identifiers used across the module.
pub mod gts_helpers;

pub use dto::{
    AuthConfig, Burst, CorsConfig, Endpoint, EndpointScheme, GrpcMatch, HeaderRules, HeadersConfig,
    HttpMatch, MatchRules, Passthrough, PathSuffixMode, Plugin, PluginBindings, PluginKind,
    PluginRef, Protocol, RateAlgorithm, RateLimitConfig, RateScope, RateStrategy, RateWindow,
    Route, RouteConfig, Server, Sharing, SustainedRate, Upstream, UpstreamConfig, derive_alias,
    is_catalog_only_plugin, is_ip_literal, is_valid_alias, is_valid_hostname, parse_plugin_ref,
};
pub use error::DomainError;
pub use hierarchy::{
    ResolverChain, SingleTenantChain, StaticChain, TenantChain, TenantNode, effective_upstream,
    is_shared, shares_with_descendants,
};
pub use plugin::{
    AuthPlugin, GuardPlugin, PluginContext, PluginRegistry, PluginRequest, TransformPlugin,
};

/// Downstream caller identity, re-exported so plugin contracts can reference it
/// without a direct `toolkit-security` import.
pub use toolkit_security::SecurityContext;
pub use repo::{ListQuery, Mutated, Page, PluginRepo, RouteRepo, UpstreamRepo};
pub use services::{PluginService, RouteService, UpstreamService};
