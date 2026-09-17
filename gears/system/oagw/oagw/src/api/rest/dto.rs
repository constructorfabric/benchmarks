//! REST DTOs for the OAGW control plane.
//!
//! Request DTOs mirror the domain write shapes; response DTOs are stable,
//! tenant-scoped projections of [`Upstream`], [`Route`] and [`Plugin`].
//! Every DTO is validated against the domain rules in the repository
//! (`cpt-cf-oagw-algo-control-plane-json-schema-validation`).

use crate::domain::models::{CorsConfig, RateLimitConfig, RouteHttpMatch, UpstreamScheme};

fn default_scheme() -> String {
    "https".to_owned()
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// Create/update request for an upstream (`oagw_upstream`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct UpstreamRequest {
    /// Unique alias (proxy path segment and binding key).
    #[serde(default)]
    pub alias: String,
    /// Optional human-readable name.
    #[serde(default)]
    pub name: Option<String>,
    /// Upstream host (DNS name or literal IP).
    #[serde(default)]
    pub host: String,
    /// Upstream TCP port; defaults per scheme when omitted.
    #[serde(default)]
    pub port: Option<u16>,
    /// `https` (default) or `http` (needs `allow_http_upstream`).
    #[serde(default = "default_scheme")]
    pub scheme: String,
    /// Path prefix prepended to proxied request paths.
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// Whether the upstream accepts traffic.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Per-upstream timeout override in seconds (0 = global default).
    #[serde(default)]
    pub timeout_secs: u64,
}

/// Read projection of an upstream.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamDto {
    /// Unique alias.
    pub alias: String,
    /// Human-readable name.
    pub name: String,
    /// Upstream host.
    pub host: String,
    /// Upstream TCP port.
    pub port: u16,
    /// Transport scheme.
    pub scheme: String,
    /// Path prefix.
    pub path_prefix: String,
    /// Whether the upstream accepts traffic.
    pub enabled: bool,
    /// Per-upstream timeout override in seconds.
    pub timeout_secs: u64,
}

impl UpstreamDto {
    /// Projects a domain upstream into its DTO.
    #[must_use]
    pub fn from_domain(u: &crate::domain::models::Upstream) -> Self {
        Self {
            alias: u.alias.clone(),
            name: u.name.clone(),
            host: u.host.clone(),
            port: u.port,
            scheme: u.scheme.to_string(),
            path_prefix: u.path_prefix.clone(),
            enabled: u.enabled,
            timeout_secs: u.timeout_secs,
        }
    }
}

impl UpstreamRequest {
    /// Projects the request into a domain upstream.
    ///
    /// The alias is authoritative from the path for updates: when `existing`
    /// is provided, the stored (path) alias is used and a non-empty body
    /// alias that disagrees is rejected instead of silently re-targeting the
    /// write.
    ///
    /// # Errors
    ///
    /// Returns a validation error when the scheme is unknown or the body
    /// alias conflicts with the path alias.
    pub fn into_domain(
        self,
        existing: Option<&crate::domain::models::Upstream>,
    ) -> Result<crate::domain::models::Upstream, crate::domain::error::DomainError> {
        let alias = match existing {
            Some(existing) if !self.alias.is_empty() && self.alias != existing.alias => {
                return Err(crate::domain::error::DomainError::validation(format!(
                    "body alias `{}` conflicts with path alias `{}`",
                    self.alias, existing.alias
                )));
            }
            Some(existing) => existing.alias.clone(),
            None => self.alias.clone(),
        };
        // @cpt-begin:cpt-cf-oagw-algo-control-plane-json-schema-validation:ph-1:inst-governing-schema
        // @cpt-begin:cpt-cf-oagw-algo-control-plane-json-schema-validation:ph-1:inst-validate-body
        let scheme = UpstreamScheme::parse(&self.scheme).ok_or_else(|| {
            // @cpt-end:cpt-cf-oagw-algo-control-plane-json-schema-validation:ph-1:inst-governing-schema
            // @cpt-end:cpt-cf-oagw-algo-control-plane-json-schema-validation:ph-1:inst-validate-body
            // @cpt-begin:cpt-cf-oagw-algo-control-plane-json-schema-validation:ph-1:inst-violation
            // @cpt-begin:cpt-cf-oagw-algo-control-plane-json-schema-validation:ph-1:inst-return-400
            crate::domain::error::DomainError::validation(format!(
                // @cpt-end:cpt-cf-oagw-algo-control-plane-json-schema-validation:ph-1:inst-violation
                // @cpt-end:cpt-cf-oagw-algo-control-plane-json-schema-validation:ph-1:inst-return-400
                "unknown upstream scheme `{}` (expected https|http)",
                self.scheme
            ))
        })?;
        let port = self.port.unwrap_or(match scheme {
            UpstreamScheme::Http => 80,
            UpstreamScheme::Https => 443,
        });
        let name = self
            .name
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| alias.clone())
            .clone();
        // @cpt-begin:cpt-cf-oagw-algo-control-plane-json-schema-validation:ph-1:inst-accept
        Ok(crate::domain::models::Upstream {
            // @cpt-end:cpt-cf-oagw-algo-control-plane-json-schema-validation:ph-1:inst-accept
            alias,
            name,
            host: self.host,
            port,
            scheme,
            path_prefix: self.path_prefix.unwrap_or_default(),
            enabled: self.enabled,
            timeout_secs: self.timeout_secs,
        })
        // @cpt-begin:cpt-cf-oagw-algo-control-plane-json-schema-validation:ph-1:inst-return-valid-body
    }
    // @cpt-end:cpt-cf-oagw-algo-control-plane-json-schema-validation:ph-1:inst-return-valid-body
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// Create/update request for a route (`oagw_route`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct RouteRequest {
    /// Unique route alias.
    #[serde(default)]
    pub alias: String,
    /// Upstream the route forwards to.
    #[serde(default)]
    pub upstream_alias: String,
    /// Allowed HTTP methods (`null`/empty = any).
    #[serde(default)]
    pub methods: Option<Vec<String>>,
    /// HTTP match rules applied by the data plane.
    #[serde(default)]
    pub http_matches: Vec<RouteHttpMatch>,
    /// Route-level rate limit.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS policy.
    #[serde(default)]
    pub cors: Option<CorsConfig>,
    /// Whether the route is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Priority used when multiple routes could match.
    #[serde(default)]
    pub priority: i32,
}

/// Read projection of a route.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct RouteDto {
    /// Unique route alias.
    pub alias: String,
    /// Upstream the route forwards to.
    pub upstream_alias: Option<String>,
    /// Allowed methods (`null` = any).
    pub methods: Option<Vec<String>>,
    /// HTTP match rules.
    pub http_matches: Vec<RouteHttpMatch>,
    /// Route-level rate limit.
    pub rate_limit: Option<RateLimitConfig>,
    /// Route-level CORS policy.
    pub cors: CorsConfig,
    /// Whether the route is enabled.
    pub enabled: bool,
    /// Match priority.
    pub priority: i32,
}

impl RouteDto {
    /// Projects a domain route into its DTO.
    #[must_use]
    pub fn from_domain(r: &crate::domain::models::Route) -> Self {
        Self {
            alias: r.alias.clone(),
            upstream_alias: r.upstream_alias.clone(),
            methods: r.methods.clone(),
            http_matches: r.http_matches.clone(),
            rate_limit: r.rate_limit.clone(),
            cors: r.cors.clone(),
            enabled: r.enabled,
            priority: r.priority,
        }
    }
}

impl RouteRequest {
    /// Projects the request into a domain route.
    ///
    /// The alias is authoritative from the path for updates: when
    /// `path_alias` is provided it is used and a non-empty body alias that
    /// disagrees is rejected instead of silently re-targeting the write.
    ///
    /// # Errors
    ///
    /// Returns a validation error when the body alias conflicts with the
    /// path alias.
    pub fn into_domain(
        self,
        path_alias: Option<&str>,
    ) -> Result<crate::domain::models::Route, crate::domain::error::DomainError> {
        let alias = match path_alias {
            Some(pa) if !self.alias.is_empty() && self.alias != pa => {
                return Err(crate::domain::error::DomainError::validation(format!(
                    "body alias `{}` conflicts with path alias `{pa}`",
                    self.alias
                )));
            }
            Some(pa) => pa.to_owned(),
            None => self.alias,
        };
        Ok(crate::domain::models::Route {
            alias,
            upstream_alias: Some(self.upstream_alias),
            methods: self.methods,
            http_matches: self.http_matches,
            rate_limit: self.rate_limit,
            cors: self.cors.unwrap_or_default(),
            enabled: self.enabled,
            priority: self.priority,
        })
    }
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// Create/update request for a plugin instance (`oagw_plugin`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct PluginRequest {
    /// Unique plugin alias.
    #[serde(default)]
    pub alias: String,
    /// Plugin kind (catalog key, e.g. `noop`, `apikey`, `oauth2_client_cred`,
    /// `required_headers`, `request_id`, `logging`, `metrics`).
    #[serde(default)]
    pub kind: String,
    /// Whether the plugin participates in the pipeline.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// JSON configuration payload.
    #[serde(default)]
    pub config: serde_json::Value,
}

/// Read projection of a plugin instance.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PluginDto {
    /// Unique plugin alias.
    pub alias: String,
    /// Plugin kind.
    pub kind: String,
    /// Whether the plugin participates in the pipeline.
    pub enabled: bool,
    /// JSON configuration payload.
    pub config: serde_json::Value,
}

impl PluginDto {
    /// Projects a domain plugin into its DTO.
    #[must_use]
    pub fn from_domain(p: &crate::domain::models::Plugin) -> Self {
        Self {
            alias: p.alias.clone(),
            kind: p.kind.as_str().to_owned(),
            enabled: p.enabled,
            config: p.config.clone(),
        }
    }
}

/// Bind/unbind request linking a plugin to an upstream or a route.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct BindRequest {
    /// Target upstream alias (bind to upstream).
    #[serde(default)]
    pub upstream: Option<String>,
    /// Target route alias (bind to route).
    #[serde(default)]
    pub route: Option<String>,
}
