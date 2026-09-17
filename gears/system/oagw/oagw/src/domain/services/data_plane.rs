//! Data-plane resolution: alias → upstream → route matching.
//!
//! The proxy pipeline calls [`DataPlaneService::resolve`] to turn the
//! wire-level `(alias, method, path, X-OAGW-Target-Host)` into a
//! fully-resolved target: the shadowed upstream (walking the tenant
//! chain descendant → root), the pool member selected by the optional
//! `X-OAGW-Target-Host` header, the longest-path-prefix route, the
//! effective rate limit (min across the hierarchy, DOCS §5.1), CORS and
//! the merged header/auth/plugin configuration.
//!
//! Routing failures map onto the DOCS §8 error catalogue
//! (`route.not_found`, `missing|invalid|unknown_target_host`).

use std::sync::Arc;

use authz_resolver_sdk::pep::{AccessRequest, PolicyEnforcer, ResourceType};
use tenant_resolver_sdk::{GetAncestorsOptions, TenantId, TenantResolverClient};
use toolkit_security::{pep_properties, SecurityContext};
use uuid::Uuid;

use crate::domain::error::{DataPlaneError, ErrorExtensions};
use crate::domain::models::{
    AuthConfig, CorsConfig, Endpoint, HeadersConfig, HttpMethod, PathSuffixMode, PluginItem,
    PluginRef, RateLimitConfig, RateLimitWindow, ResolvedRateLimit, Route, Upstream,
};
use crate::domain::ratelimit::merge_effective_rate_limits;
use crate::domain::repo::{RouteRepo, UpstreamRepo};
use crate::gts_helpers;

/// Proxy resource type (data-plane invocation).
pub const PROXY_RESOURCE: ResourceType =
    ResourceType::from_static(gts_helpers::PROXY_TYPE_ID, &["owner_tenant_id", "alias"]);
/// PEP action name for data-plane invocation.
pub const ACTION_INVOKE: &str = "invoke";

/// A plugin binding resolved from an upstream/route chain, ready for the
/// proxy to instantiate through the plugin registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBinding {
    /// The referenced plugin (builtin GTS id or custom plugin uuid).
    pub plugin_ref: PluginRef,
    /// Effective binding-site configuration.
    pub config: serde_json::Value,
}

/// A fully-resolved proxy target.
#[derive(Debug, Clone)]
pub struct Resolution {
    /// The shadowing upstream (closest tenant in the chain).
    pub upstream: Upstream,
    /// The matching route, when one exists (always for a real forward).
    pub route: Option<Route>,
    /// The selected pool member.
    pub endpoint: Endpoint,
    /// Effective request path forwarded upstream.
    pub path: String,
    /// Effective rate limit (min across the tenant chain + route).
    pub rate_limit: Option<ResolvedRateLimit>,
    /// Effective CORS configuration (route overrides upstream).
    pub cors: Option<CorsConfig>,
    /// Header transformation rules.
    pub headers: Option<HeadersConfig>,
    /// The upstream auth block, when configured.
    pub auth: Option<AuthConfig>,
    /// Plugin bindings to run in order (upstream chain then route chain).
    pub plugins: Vec<ResolvedBinding>,
}

/// Data-plane resolution service.
pub struct DataPlaneService {
    upstreams: Arc<dyn UpstreamRepo>,
    routes: Arc<dyn RouteRepo>,
    enforcer: Arc<PolicyEnforcer>,
    tenant_resolver: Arc<dyn TenantResolverClient>,
}

impl DataPlaneService {
    /// Build the resolution service.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepo>,
        routes: Arc<dyn RouteRepo>,
        enforcer: Arc<PolicyEnforcer>,
        tenant_resolver: Arc<dyn TenantResolverClient>,
    ) -> Self {
        Self {
            upstreams,
            routes,
            enforcer,
            tenant_resolver,
        }
    }

    /// Resolve a proxied request to its full target.
    ///
    /// # Errors
    /// * 403 when the PEP denies invocation.
    /// * 404 `route.not_found` when no upstream owns `alias`.
    /// * 400/400/400 `missing|invalid|unknown_target_host` per the
    ///   `X-OAGW-Target-Host` matrix.
    /// * 500 on hierarchy lookup failures (fail-closed).
    pub async fn resolve(
        &self,
        ctx: &SecurityContext,
        alias: &str,
        method: &http::Method,
        path: &str,
        target_host: Option<&str>,
    ) -> Result<Resolution, DataPlaneError> {
        // PEP: data-plane invocation must be authorized.
        let request = AccessRequest::new()
            .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
            .resource_property("alias", alias)
            .require_constraints(true);
        self.enforcer
            .access_scope_with(ctx, &PROXY_RESOURCE, ACTION_INVOKE, None, &request)
            .await
            .map_err(|e| DataPlaneError::forbidden(format!("authorization failed: {e}")))?;

        let chain = self.tenant_chain(ctx).await?;

        // 1. Alias resolution (shadowing; closest tenant wins).
        let upstream = self
            .upstreams
            .resolve_alias(&chain, alias)
            .await
            .ok_or_else(|| DataPlaneError::RouteNotFound {
                detail: format!("no upstream is bound to alias '{alias}'"),
                extensions: ErrorExtensions::default()
                    .with_alias(alias.to_owned())
                    .with_path(path.to_owned()),
            })?;

        // 2. Pool member selection via X-OAGW-Target-Host.
        let endpoint = select_endpoint(&upstream, target_host, path)?;

        // 3. Route matching (longest path prefix).
        let route = self.match_route(&chain, &upstream, method, path).await;

        // 4. Effective rate limit: min across the chain's upstreams + route.
        let mut configs: Vec<ResolvedRateLimit> = Vec::new();
        for tid in &chain {
            let Some(u) = self.upstreams.resolve_alias(std::slice::from_ref(tid), alias).await
            else {
                continue;
            };
            let Some(rl) = u.rate_limit else {
                continue;
            };
            configs.push(to_resolved(&rl));
        }
        if let Some(rl) = route.as_ref().and_then(|r| r.rate_limit.as_ref()) {
            configs.push(to_resolved(rl));
        }
        let rate_limit = merge_effective_rate_limits(configs);

        // 5. Cors / headers / auth / plugin chain (route overrides
        //    upstream where both may exist).
        let cors = route
            .as_ref()
            .and_then(|r| r.cors.clone())
            .or_else(|| upstream.cors.clone());
        let mut plugins: Vec<ResolvedBinding> = Vec::new();
        if let Some(p) = &upstream.plugins {
            plugins.extend(p.items.iter().map(binding));
        }
        if let Some(p) = route.as_ref().and_then(|r| r.plugins.as_ref()) {
            plugins.extend(p.items.iter().map(binding));
        }

        let headers = upstream.headers.clone();
        let auth = upstream.auth.clone();

        Ok(Resolution {
            upstream,
            route,
            endpoint,
            path: path.to_owned(),
            rate_limit,
            cors,
            headers,
            auth,
            plugins,
        })
    }

    /// Tenant chain ordered descendant → root, starting with the caller.
    async fn tenant_chain(&self, ctx: &SecurityContext) -> Result<Vec<Uuid>, DataPlaneError> {
        let tid = ctx.subject_tenant_id();
        let mut chain = vec![tid];
        let resp = self
            .tenant_resolver
            .get_ancestors(ctx, TenantId(tid), &GetAncestorsOptions::default())
            .await
            .map_err(|e| DataPlaneError::internal(format!("tenant hierarchy lookup failed: {e}")))?;
        for ancestor in resp.ancestors {
            chain.push(ancestor.id.0);
        }
        Ok(chain)
    }

    /// Pick the `http` route on `upstream` matching (method, path) with
    /// the longest path prefix, considering every tenant in the chain
    /// (descendant routes shadow ancestor routes for the same upstream).
    async fn match_route(
        &self,
        chain: &[Uuid],
        upstream: &Upstream,
        method: &http::Method,
        path: &str,
    ) -> Option<Route> {
        let routes = self.routes.list_for_chain(chain).await;
        let mut best: Option<Route> = None;
        let mut best_len = 0usize;
        for r in routes {
            if r.upstream_id != upstream.id {
                continue;
            }
            let Some(h) = &r.match_.http else {
                continue; // gRPC routes are matched elsewhere.
            };
            let Some(want) = HttpMethod::from_http_method(method) else {
                continue; // not one of the schema's methods
            };
            if !h.methods.contains(&want) {
                continue;
            }
            let append = h.path_suffix_mode == PathSuffixMode::Append;
            let Some(matched_len) = match_path(path, &h.path, append) else {
                continue;
            };
            if matched_len > best_len {
                best = Some(r);
                best_len = matched_len;
            }
        }
        best
    }
}

/// Convert a `RateLimitConfig` to its resolved enforcement tuple.
#[must_use]
fn to_resolved(c: &RateLimitConfig) -> ResolvedRateLimit {
    let window_secs = match c.sustained.window {
        RateLimitWindow::Second => 1,
        RateLimitWindow::Minute => 60,
        RateLimitWindow::Hour => 3600,
        RateLimitWindow::Day => 86400,
    };
    ResolvedRateLimit {
        rate: c.sustained.rate,
        window_secs,
        capacity: c.burst.map_or(c.sustained.rate, |b| b.capacity),
        cost: c.cost,
    }
}

/// Convert a plugin-chain item into a resolution binding.
fn binding(item: &PluginItem) -> ResolvedBinding {
    ResolvedBinding {
        plugin_ref: item.plugin_ref().clone(),
        config: item.config(),
    }
}

/// Longest common path-prefix match. Returns the matched prefix length.
///
/// `Append` mode allows a `/suffix` after the route path; `Disabled`
/// requires the request path to equal the route path.
#[must_use]
fn match_path(request: &str, route: &str, append: bool) -> Option<usize> {
    if !request.starts_with(route) {
        return None;
    }
    let rest = &request[route.len()..];
    if rest.is_empty() {
        return Some(route.len());
    }
    // The root route matches every request path (every path begins at the
    // root segment, so the suffix is always well-formed).
    if route == "/" {
        return if append { Some(1) } else { None };
    }
    if !rest.starts_with('/') {
        return None; // not a segment boundary
    }
    if !append {
        return None; // suffix not allowed
    }
    Some(route.len())
}

/// Normalized host comparison tokens for a pool member: `host` plus the
/// `host:port` form when the port is non-standard.
fn member_tokens(e: &Endpoint) -> Vec<String> {
    let host = e.host.to_ascii_lowercase().trim_end_matches('.').to_owned();
    let mut tokens = vec![host.clone()];
    if e.effective_port() != e.scheme.default_port() {
        tokens.push(format!("{host}:{}", e.effective_port()));
    }
    tokens
}

/// Select the pool member for `X-OAGW-Target-Host` (DOCS §7.3 matrix):
///
/// * single endpoint: header optional but validated when present;
/// * multiple endpoints: header required, must name a pool member.
#[allow(clippy::result_large_err)] // rich RFC 9457 error carrier by design
fn select_endpoint(
    upstream: &Upstream,
    target_host: Option<&str>,
    path: &str,
) -> Result<Endpoint, DataPlaneError> {
    let endpoints = &upstream.server.endpoints;
    let ext = |hosts: Vec<String>| ErrorExtensions::default()
        .with_alias(upstream.alias.clone())
        .with_path(path.to_owned())
        .with_valid_hosts(hosts);

    if endpoints.is_empty() {
        return Err(DataPlaneError::Validation {
            detail: "upstream has no endpoints".to_owned(),
            extensions: ext(vec![]),
        });
    }

    let valid_hosts: Vec<String> = endpoints.iter().flat_map(member_tokens).collect();

    if endpoints.len() == 1 {
        if let Some(raw) = target_host {
            let header = normalize_header(raw);
            let ok = member_tokens(&endpoints[0]).iter().any(|t| t == &header);
            if !ok {
                let hosts = valid_hosts;
                return Err(DataPlaneError::InvalidTargetHost {
                    value: raw.trim().to_owned(),
                    valid_hosts: hosts.clone(),
                    extensions: ext(hosts),
                });
            }
        }
        return Ok(endpoints[0].clone());
    }

    let Some(raw) = target_host else {
        let hosts = valid_hosts;
        return Err(DataPlaneError::MissingTargetHost {
            valid_hosts: hosts.clone(),
            extensions: ext(hosts),
        });
    };
    let header = normalize_header(raw);
    for e in endpoints {
        if member_tokens(e).iter().any(|t| t == &header) {
            return Ok(e.clone());
        }
    }
    let hosts = valid_hosts;
    Err(DataPlaneError::UnknownTargetHost {
        value: raw.trim().to_owned(),
        valid_hosts: hosts.clone(),
        extensions: ext(hosts),
    })
}

fn normalize_header(header: &str) -> String {
    header
        .trim()
        .to_ascii_lowercase()
        .trim_end_matches('.')
        .to_owned()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::models::{EndpointScheme, ServerConfig, UpstreamProtocol};

    fn upstream_with(hosts: &[(&str, EndpointScheme)]) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            enabled: true,
            alias: "vendor".to_owned(),
            tags: vec![],
            server: ServerConfig {
                endpoints: hosts
                    .iter()
                    .map(|(h, s)| Endpoint {
                        scheme: *s,
                        host: h.to_string(),
                        port: None,
                    })
                    .collect(),
            },
            protocol: UpstreamProtocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    #[test]
    fn path_matching_honors_suffix_mode() {
        // Append mode: suffix allowed at a segment boundary.
        assert_eq!(match_path("/v1/chat/completions", "/v1/chat", true), Some(8));
        assert_eq!(match_path("/v1/chat", "/v1/chat", true), Some(8));
        // The root route is a catch-all in append mode.
        assert_eq!(match_path("/hello", "/", true), Some(1));
        assert_eq!(match_path("/", "/", true), Some(1));
        // Not a boundary.
        assert_eq!(match_path("/v1/chatter", "/v1/chat", true), None);
        // Disabled: only exact.
        assert_eq!(match_path("/v1/chat/completions", "/v1/chat", false), None);
        assert_eq!(match_path("/v1/chat", "/v1/chat", false), Some(8));
    }

    #[test]
    fn single_endpoint_target_host_validates() {
        let u = upstream_with(&[("api.vendor.com", EndpointScheme::Https)]);
        // Absent → OK (single endpoint needs no header).
        assert!(select_endpoint(&u, None, "/x").is_ok());
        // Matching header (case-insensitive) → OK.
        assert!(select_endpoint(&u, Some("Api.Vendor.COM"), "/x").is_ok());
        // Mismatched → invalid.
        let err = select_endpoint(&u, Some("evil.example.com"), "/x").unwrap_err();
        assert!(matches!(err, DataPlaneError::InvalidTargetHost { .. }));
    }

    #[test]
    fn multi_endpoint_requires_target_host() {
        let u = upstream_with(&[
            ("us.vendor.com", EndpointScheme::Https),
            ("eu.vendor.com", EndpointScheme::Https),
        ]);
        assert!(matches!(
            select_endpoint(&u, None, "/x").unwrap_err(),
            DataPlaneError::MissingTargetHost { .. }
        ));
        assert!(select_endpoint(&u, Some("eu.vendor.com"), "/x").is_ok());
        assert!(matches!(
            select_endpoint(&u, Some("other.vendor.com"), "/x").unwrap_err(),
            DataPlaneError::UnknownTargetHost { .. }
        ));
    }

    #[test]
    fn rate_limit_resolution_window_and_burst() {
        let rl = RateLimitConfig {
            sharing: crate::domain::models::SharingMode::Private,
            algorithm: crate::domain::models::RateLimitAlgorithm::TokenBucket,
            sustained: crate::domain::models::SustainedRate {
                rate: 100,
                window: RateLimitWindow::Minute,
            },
            burst: Some(crate::domain::models::BurstConfig { capacity: 500 }),
            scope: crate::domain::models::RateLimitScope::Tenant,
            strategy: crate::domain::models::RateLimitStrategy::Reject,
            cost: 1,
        };
        let r = to_resolved(&rl);
        assert_eq!(r.rate, 100);
        assert_eq!(r.window_secs, 60);
        assert_eq!(r.capacity, 500);
        assert_eq!(r.cost, 1);
    }

    #[test]
    fn member_tokens_include_standard_and_explicit() {
        let e = Endpoint {
            scheme: EndpointScheme::Https,
            host: "api.vendor.com".to_owned(),
            port: None,
        };
        assert_eq!(member_tokens(&e), vec!["api.vendor.com"]);
        let e = Endpoint {
            scheme: EndpointScheme::Https,
            host: "api.vendor.com".to_owned(),
            port: Some(8443),
        };
        let tokens = member_tokens(&e);
        assert!(tokens.contains(&"api.vendor.com".to_owned()));
        assert!(tokens.contains(&"api.vendor.com:8443".to_owned()));
    }
}
