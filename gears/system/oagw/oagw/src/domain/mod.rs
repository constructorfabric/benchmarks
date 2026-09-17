// Created: 2026-09-04 by Constructor Tech
//! Domain layer of the OAGW gear: value objects, invariants and pure
//! resolution logic (`docs/DESIGN.md` §2 DDD-Light layering).
//!
//! Nothing in this module performs I/O, holds a connection or depends on the
//! HTTP stack; the control plane and the data plane consume it.
//!
//! * [`alias`] — alias derivation, normalization, uniqueness and shadowing.
//! * [`upstream`] — the upstream aggregate and its configuration objects.
//! * [`route`] — the route aggregate and its match keys.
//! * [`plugin`] — the plugin identification model.
//! * [`resolve_policy`] — the hierarchical configuration resolution of
//!   `docs/PRD.md` §5.5.

pub mod alias;
pub mod plugin;
pub mod route;
pub mod upstream;

pub use alias::{
    Alias, AliasDerivation, AliasKey, AliasRegistration, TenantAliasScope, derive_alias,
    ensure_alias_unique, resolve_alias, resolve_shadowing,
};
pub use plugin::{Plugin, PluginInstance, PluginKind, PluginPhase, PluginRef};
pub use route::{GrpcMatch, HttpMatch, PathSuffixMode, Route, RouteMatch, RouteSpec};
pub use upstream::{
    AllowedOrigin, AuthConfig, BurstCapacity, CorsConfig, Endpoint, EndpointScheme,
    HeaderPassthrough, HeadersConfig, HttpMethod, PluginChain, Protocol, RateLimitAlgorithm,
    RateLimitConfig, RateLimitScope, RateLimitStrategy, RateLimitWindow, RequestHeaderRules,
    ResponseHeaderRules, SecretRef, ServerConfig, SharingMode, SustainedRate, Upstream,
    UpstreamSpec, validate_tags,
};

use crate::config::OagwConfig;

/// Effective value of a sharing-aware configuration slot across one
/// ancestor → descendant hop (`docs/PRD.md` §5.5).
///
/// * `private` — the ancestor value is not visible to descendants;
/// * `inherit` — the descendant value wins when specified, otherwise the
///   ancestor's is inherited;
/// * `enforce` — the descendant value wins when specified, otherwise the
///   ancestor's is inherited; the *stricter of the two* is enforced by the
///   concrete types (see [`RateLimitConfig::effective`]).
#[must_use]
pub fn resolve_slot<T: Clone>(
    ancestor: Option<&T>,
    descendant: Option<&T>,
    sharing: SharingMode,
) -> Option<T> {
    match sharing {
        SharingMode::Private => descendant.cloned(),
        SharingMode::Inherit | SharingMode::Enforce => {
            descendant.cloned().or_else(|| ancestor.cloned())
        }
    }
}

/// Add-only union of the tag sets along the configuration hierarchy
/// (`docs/PRD.md` §5.5 "Tags": descendants add tags, inherited tags cannot be
/// removed).
#[must_use]
pub fn merge_tags(sets: &[&[String]]) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for set in sets {
        for tag in *set {
            if !tags.contains(tag) {
                tags.push(tag.clone());
            }
        }
    }
    tags
}

/// Fully resolved configuration of one proxied request, computed from the
/// gear defaults, the matched upstream and the matched route
/// (`docs/PRD.md` §5.5 "Configuration Resolution").
#[derive(Debug, Clone, PartialEq)]
pub struct EffectivePolicy {
    /// Whether a plaintext upstream connection may be dialed at egress time.
    pub allow_http_upstream: bool,
    /// Upstream request timeout in seconds (`504` when exceeded).
    pub proxy_timeout_secs: u64,
    /// Plugin chain in execution order: upstream plugins first, then route
    /// plugins (`docs/PRD.md` `cpt-cf-oagw-fr-plugin-system`).
    pub plugins: Vec<PluginRef>,
    /// Effective rate limit, or `None` when no limit applies.
    pub rate_limit: Option<RateLimitConfig>,
    /// Effective CORS configuration, or `None` when CORS handling is off.
    pub cors: Option<CorsConfig>,
    /// Effective header transformation rules.
    pub headers: Option<HeadersConfig>,
    /// Auth plugin configuration of the upstream.
    pub auth: Option<AuthConfig>,
}

impl EffectivePolicy {
    /// `true` when a connection to `scheme` may be established.
    ///
    /// The `allow_http_upstream` gate is an **egress-time** data-plane check:
    /// `http` stays a legal scheme value and is never restricted by scheme
    /// validation.
    #[must_use]
    pub const fn permits_plaintext_egress(&self, scheme: EndpointScheme) -> bool {
        !scheme.is_plaintext() || self.allow_http_upstream
    }

    /// Effective proxy timeout as a `std::time::Duration`.
    #[must_use]
    pub const fn proxy_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.proxy_timeout_secs)
    }
}

/// Resolves the effective policy of a proxied request.
///
/// `gear` supplies the lowest layer of the hierarchy (`gears.oagw.config`),
/// `upstream` the upstream slot and `route` — when a route matched — the
/// route slot on top of it.
#[must_use]
pub fn resolve_policy(
    gear: &OagwConfig,
    upstream: &Upstream,
    route: Option<&Route>,
) -> EffectivePolicy {
    let (route_plugins, route_rate_limit, route_cors) = match route {
        None => (None, None, None),
        Some(matched) => (
            matched.plugins.as_ref(),
            matched.rate_limit.as_ref(),
            matched.cors.as_ref(),
        ),
    };
    let plugins = PluginChain::effective(upstream.plugins.as_ref(), route_plugins)
        .map_or_else(Vec::new, |chain| chain.items);
    EffectivePolicy {
        allow_http_upstream: gear.allow_http_upstream,
        proxy_timeout_secs: gear.proxy_timeout_secs,
        plugins,
        rate_limit: RateLimitConfig::effective(upstream.rate_limit.as_ref(), route_rate_limit),
        cors: CorsConfig::effective(upstream.cors.as_ref(), route_cors),
        headers: upstream.headers.clone(),
        auth: upstream.auth.clone(),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "policy_tests.rs"]
mod policy_tests;
