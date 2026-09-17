// Created: 2026-09-04 by Constructor Tech
//! Wire DTOs of the `/oagw/v1` management surface.
//!
//! Mirrors `docs/schemas/upstream.v1.schema.json` and
//! `docs/schemas/route.v1.schema.json` field-for-field; every conversion into
//! a Phase-1 domain object runs the domain invariants, so the control-plane
//! service only ever sees validated value objects.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::num::NonZeroU32;

use serde_json::Value;
use uuid::Uuid;

use toolkit::api::apply_select;

use crate::controlplane::service::{
    ControlPlaneService, PluginDescriptor, PluginSource, RouteUpdate,
};
use crate::domain::{
    Alias, AllowedOrigin, AuthConfig, BurstCapacity, CorsConfig, Endpoint, EndpointScheme,
    GrpcMatch, HeaderPassthrough, HeadersConfig, HttpMatch, HttpMethod, PathSuffixMode,
    PluginChain, Protocol, RateLimitAlgorithm, RateLimitConfig, RateLimitScope, RateLimitStrategy,
    RateLimitWindow, RequestHeaderRules, ResponseHeaderRules, Route, RouteMatch, RouteSpec,
    ServerConfig, SharingMode, SustainedRate, Upstream, UpstreamSpec, validate_tags,
};
use crate::error::OagwError;

// ------------------------------------------------------------------ upstreams

/// One load-balanced endpoint (`docs/schemas/upstream.v1.schema.json`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub struct EndpointDto {
    /// Endpoint scheme; defaults to `https`.
    #[serde(default)]
    pub scheme: Option<String>,
    /// Hostname or IP address.
    pub host: String,
    /// Explicit port; defaults to the scheme's standard port.
    #[serde(default)]
    pub port: Option<u16>,
}

impl EndpointDto {
    /// # Errors
    ///
    /// Returns [`OagwError::InvalidEndpoint`] for an unknown scheme or an
    /// invalid host or port.
    pub fn to_endpoint(&self) -> Result<Endpoint, OagwError> {
        let scheme = match &self.scheme {
            Some(raw) => EndpointScheme::parse(raw)?,
            None => EndpointScheme::default(),
        };
        Endpoint::new(scheme, self.host.trim(), self.port)
    }
}

/// Endpoint pool of an upstream (`server` of the upstream schema).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct ServerDto {
    /// At least one endpoint; all endpoints share one scheme and one port.
    pub endpoints: Vec<EndpointDto>,
}

impl ServerDto {
    /// # Errors
    ///
    /// Returns the [`Endpoint`] / [`ServerConfig`] validation errors.
    pub fn to_server(&self) -> Result<ServerConfig, OagwError> {
        let endpoints = self
            .endpoints
            .iter()
            .map(EndpointDto::to_endpoint)
            .collect::<Result<Vec<_>, _>>()?;
        ServerConfig::new(endpoints)
    }
}

impl From<&ServerConfig> for ServerDto {
    fn from(server: &ServerConfig) -> Self {
        Self {
            endpoints: server
                .endpoints()
                .iter()
                .map(|endpoint| EndpointDto {
                    scheme: Some(endpoint.scheme().as_str().to_owned()),
                    host: endpoint.host().to_owned(),
                    port: endpoint.port(),
                })
                .collect(),
        }
    }
}

/// Auth plugin binding of an upstream (`auth` of the upstream schema).
#[derive(Debug, Clone, PartialEq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct AuthDto {
    /// Auth plugin identifier (built-in GTS id or custom plugin UUID).
    #[serde(default, rename = "type")]
    pub plugin_type: Option<String>,
    /// Sharing mode of the credentials.
    #[serde(default)]
    pub sharing: Option<String>,
    /// Plugin configuration; may carry `cred://` references under any key.
    #[serde(default)]
    pub config: Option<Value>,
}

impl AuthDto {
    /// Resolves the bound plugin against the tenant's plugin registry.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for an unknown or non-auth plugin.
    pub fn to_config(
        &self,
        tenant_id: Uuid,
        svc: &ControlPlaneService,
    ) -> Result<Option<AuthConfig>, OagwError> {
        let plugin = match &self.plugin_type {
            None => None,
            Some(raw) => Some(svc.resolve_auth_plugin_ref(tenant_id, raw)?),
        };
        Ok(Some(AuthConfig {
            sharing: parse_sharing(self.sharing.as_deref())?,
            plugin,
            config: self.config.clone().unwrap_or(Value::Null),
        }))
    }
}

impl From<&crate::domain::AuthConfig> for AuthDto {
    fn from(auth: &crate::domain::AuthConfig) -> Self {
        Self {
            plugin_type: auth
                .plugin
                .as_ref()
                .map(|reference| reference.as_ref_str().to_string()),
            sharing: Some(sharing_str(auth.sharing).to_owned()),
            config: Some(auth.config.clone()),
        }
    }
}

/// Ordered plugin chain (`plugins` of the schemas).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct PluginChainDto {
    /// Sharing mode of the chain.
    #[serde(default)]
    pub sharing: Option<String>,
    /// Plugins in execution order.
    #[serde(default)]
    pub items: Vec<String>,
}

impl PluginChainDto {
    /// Resolves every item against the tenant's plugin registry.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for an unknown or non-bindable
    /// identifier.
    pub fn to_chain(
        &self,
        tenant_id: Uuid,
        svc: &ControlPlaneService,
    ) -> Result<Option<PluginChain>, OagwError> {
        if self.items.is_empty() {
            return Ok(None);
        }
        let mut items = Vec::with_capacity(self.items.len());
        for raw in &self.items {
            items.push(svc.resolve_plugin_ref(tenant_id, raw)?);
        }
        Ok(Some(PluginChain {
            sharing: parse_sharing(self.sharing.as_deref())?,
            items,
        }))
    }
}

impl From<&PluginChain> for PluginChainDto {
    fn from(chain: &PluginChain) -> Self {
        Self {
            sharing: Some(sharing_str(chain.sharing).to_owned()),
            items: chain
                .items
                .iter()
                .map(|reference| reference.as_ref_str().to_string())
                .collect(),
        }
    }
}

/// Sustained rate (`rate` tokens per `window`).
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub struct SustainedRateDto {
    /// Tokens replenished per window.
    pub rate: u32,
    /// Length of the window; defaults to `second`.
    #[serde(default)]
    pub window: Option<String>,
}

impl SustainedRateDto {
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for an unknown window or a zero rate.
    pub fn to_rate(&self) -> Result<SustainedRate, OagwError> {
        Ok(SustainedRate {
            rate: nonzero(self.rate, "rate_limit.sustained.rate")?,
            window: parse_window(self.window.as_deref())?,
        })
    }
}

/// Burst capacity of a rate limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[toolkit_macros::api_dto(request, response)]
pub struct BurstDto {
    /// Bucket capacity.
    pub capacity: u32,
}

/// Rate limit configuration (`rate_limit` of the schemas).
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request, response)]
pub struct RateLimitDto {
    /// Sharing mode of the limit.
    #[serde(default)]
    pub sharing: Option<String>,
    /// Token bucket or sliding window.
    #[serde(default)]
    pub algorithm: Option<String>,
    /// Sustained rate (required).
    pub sustained: SustainedRateDto,
    /// Burst capacity; defaults to the sustained rate.
    #[serde(default)]
    pub burst: Option<BurstDto>,
    /// Scope of the counters.
    #[serde(default)]
    pub scope: Option<String>,
    /// Behaviour when the limit is exceeded.
    #[serde(default)]
    pub strategy: Option<String>,
    /// Tokens consumed per request; defaults to 1.
    #[serde(default)]
    pub cost: Option<u32>,
}

impl RateLimitDto {
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for an unknown token or a zero rate.
    pub fn to_config(&self) -> Result<RateLimitConfig, OagwError> {
        Ok(RateLimitConfig {
            sharing: parse_sharing(self.sharing.as_deref())?,
            algorithm: parse_algorithm(self.algorithm.as_deref())?,
            sustained: self.sustained.to_rate()?,
            burst: match self.burst {
                None => None,
                Some(burst) => Some(BurstCapacity {
                    capacity: nonzero(burst.capacity, "rate_limit.burst.capacity")?,
                }),
            },
            scope: parse_scope(self.scope.as_deref())?,
            strategy: parse_strategy(self.strategy.as_deref())?,
            cost: match self.cost {
                None => NonZeroU32::MIN,
                Some(cost) => nonzero(cost, "rate_limit.cost")?,
            },
        })
    }
}

impl From<&RateLimitConfig> for RateLimitDto {
    fn from(config: &RateLimitConfig) -> Self {
        Self {
            sharing: Some(sharing_str(config.sharing).to_owned()),
            algorithm: Some(algorithm_str(config.algorithm).to_owned()),
            sustained: SustainedRateDto {
                rate: config.sustained.rate.get(),
                window: Some(window_str(config.sustained.window).to_owned()),
            },
            burst: config.burst.map(|burst| BurstDto {
                capacity: burst.capacity.get(),
            }),
            scope: Some(scope_str(config.scope).to_owned()),
            strategy: Some(strategy_str(config.strategy).to_owned()),
            cost: Some(config.cost.get()),
        }
    }
}

/// CORS configuration (`cors` of the schemas).
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request, response)]
pub struct CorsDto {
    /// Sharing mode of the configuration.
    #[serde(default)]
    pub sharing: Option<String>,
    /// Whether CORS handling is enabled.
    pub enabled: bool,
    /// Allowed origins (`*` or exact origins).
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Allowed methods; defaults to `GET, POST`.
    #[serde(default)]
    pub allowed_methods: Option<Vec<String>>,
    /// Headers exposed to the browser beyond the safelisted ones.
    #[serde(default)]
    pub expose_headers: Option<Vec<String>>,
    /// Whether credentials are allowed.
    #[serde(default)]
    pub allow_credentials: Option<bool>,
}

impl CorsDto {
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for an invalid origin or method.
    pub fn to_config(&self) -> Result<CorsConfig, OagwError> {
        let mut origins = Vec::new();
        for raw in &self.allowed_origins {
            origins.push(AllowedOrigin::parse(raw)?);
        }
        let mut methods = Vec::new();
        for raw in self.allowed_methods.clone().unwrap_or_else(default_methods) {
            methods.push(HttpMethod::parse(&raw)?);
        }
        Ok(CorsConfig {
            sharing: parse_sharing(self.sharing.as_deref())?,
            enabled: self.enabled,
            allowed_origins: origins,
            allowed_methods: methods,
            expose_headers: self.expose_headers.clone().unwrap_or_default(),
            allow_credentials: self.allow_credentials.unwrap_or_default(),
        })
    }
}

/// Allowed methods of a CORS configuration when the request omits them.
fn default_methods() -> Vec<String> {
    vec![String::from("GET"), String::from("POST")]
}

impl From<&CorsConfig> for CorsDto {
    fn from(config: &CorsConfig) -> Self {
        Self {
            sharing: Some(sharing_str(config.sharing).to_owned()),
            enabled: config.enabled,
            allowed_origins: config
                .allowed_origins
                .iter()
                .map(|origin| match origin {
                    AllowedOrigin::Any => String::from("*"),
                    AllowedOrigin::Exact(value) => value.clone(),
                })
                .collect(),
            allowed_methods: Some(
                config
                    .allowed_methods
                    .iter()
                    .map(|method| method.as_str().to_owned())
                    .collect(),
            ),
            expose_headers: Some(config.expose_headers.clone()),
            allow_credentials: Some(config.allow_credentials),
        }
    }
}

/// Request header rules (`headers.request` of the upstream schema).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct RequestHeadersDto {
    /// Headers to set.
    #[serde(default)]
    pub set: Option<BTreeMap<String, String>>,
    /// Headers to add.
    #[serde(default)]
    pub add: Option<BTreeMap<String, String>>,
    /// Header names to remove.
    #[serde(default)]
    pub remove: Option<Vec<String>>,
    /// Which inbound headers are forwarded.
    #[serde(default)]
    pub passthrough: Option<String>,
    /// Headers forwarded when `passthrough` is `allowlist`.
    #[serde(default)]
    pub passthrough_allowlist: Option<Vec<String>>,
}

impl RequestHeadersDto {
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for an unknown passthrough mode.
    pub fn to_rules(&self) -> Result<RequestHeaderRules, OagwError> {
        Ok(RequestHeaderRules {
            set: self.set.clone().unwrap_or_default(),
            add: self.add.clone().unwrap_or_default(),
            remove: self.remove.clone().unwrap_or_default(),
            passthrough: match self.passthrough.as_deref() {
                None => HeaderPassthrough::default(),
                Some("none") => HeaderPassthrough::None,
                Some("allowlist") => HeaderPassthrough::Allowlist,
                Some("all") => HeaderPassthrough::All,
                Some(raw) => {
                    return Err(OagwError::Validation {
                        detail: format!(
                            "unknown header passthrough mode '{raw}' (expected none, allowlist or all)"
                        ),
                    });
                }
            },
            passthrough_allowlist: self.passthrough_allowlist.clone().unwrap_or_default(),
        })
    }
}

impl From<&RequestHeaderRules> for RequestHeadersDto {
    fn from(rules: &RequestHeaderRules) -> Self {
        Self {
            set: Some(rules.set.clone()),
            add: Some(rules.add.clone()),
            remove: Some(rules.remove.clone()),
            passthrough: Some(passthrough_str(rules.passthrough).to_owned()),
            passthrough_allowlist: Some(rules.passthrough_allowlist.clone()),
        }
    }
}

/// Response header rules (`headers.response` of the upstream schema).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct ResponseHeadersDto {
    /// Headers to set on the response.
    #[serde(default)]
    pub set: Option<BTreeMap<String, String>>,
    /// Headers to add to the response.
    #[serde(default)]
    pub add: Option<BTreeMap<String, String>>,
    /// Headers stripped from the upstream response.
    #[serde(default)]
    pub remove: Option<Vec<String>>,
}

impl From<&ResponseHeaderRules> for ResponseHeadersDto {
    fn from(rules: &ResponseHeaderRules) -> Self {
        Self {
            set: Some(rules.set.clone()),
            add: Some(rules.add.clone()),
            remove: Some(rules.remove.clone()),
        }
    }
}

/// Header transformation rules of an upstream (`headers` of the schema).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct HeadersDto {
    /// Rules for the request towards the upstream.
    #[serde(default)]
    pub request: Option<RequestHeadersDto>,
    /// Rules for the response towards the client.
    #[serde(default)]
    pub response: Option<ResponseHeadersDto>,
}

impl HeadersDto {
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for an invalid header name or
    /// passthrough mode.
    pub fn to_config(&self) -> Result<HeadersConfig, OagwError> {
        Ok(HeadersConfig {
            request: match &self.request {
                None => None,
                Some(request) => Some(request.to_rules()?),
            },
            response: self.response.as_ref().map(|response| ResponseHeaderRules {
                set: response.set.clone().unwrap_or_default(),
                add: response.add.clone().unwrap_or_default(),
                remove: response.remove.clone().unwrap_or_default(),
            }),
        })
    }
}

impl From<&HeadersConfig> for HeadersDto {
    fn from(config: &HeadersConfig) -> Self {
        Self {
            request: config.request.as_ref().map(RequestHeadersDto::from),
            response: config.response.as_ref().map(ResponseHeadersDto::from),
        }
    }
}

/// Request body of `POST /oagw/v1/upstreams` and of
/// `PUT /oagw/v1/upstreams/{id}` (a full replacement, `id` and `tenant_id`
/// being immutable).
#[derive(Debug, Clone, PartialEq, Default)]
#[toolkit_macros::api_dto(request)]
pub struct UpstreamRequestDto {
    /// Whether the upstream accepts proxy traffic; defaults to `true`.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Requested alias; auto-derived for hostname-based endpoint pools.
    #[serde(default)]
    pub alias: Option<String>,
    /// Flat discovery tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    /// Endpoint pool (required).
    pub server: Option<ServerDto>,
    /// Protocol used to reach the upstream (required).
    pub protocol: Option<String>,
    /// Auth plugin binding.
    #[serde(default)]
    pub auth: Option<AuthDto>,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: Option<HeadersDto>,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: Option<PluginChainDto>,
    /// Rate limit configuration.
    #[serde(default)]
    pub rate_limit: Option<RateLimitDto>,
    /// CORS configuration.
    #[serde(default)]
    pub cors: Option<CorsDto>,
}

/// Response body of the upstream read endpoints
/// (`docs/schemas/upstream.v1.schema.json`).
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamResponseDto {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Whether the upstream accepts proxy traffic.
    pub enabled: bool,
    /// Routing identifier (derived or explicit).
    pub alias: String,
    /// Flat discovery tags.
    pub tags: Vec<String>,
    /// Endpoint pool.
    pub server: ServerDto,
    /// Protocol used to reach the upstream.
    pub protocol: String,
    /// Auth plugin binding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthDto>,
    /// Header transformation rules.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersDto>,
    /// Plugin chain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginChainDto>,
    /// Rate limit configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitDto>,
    /// CORS configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsDto>,
}

impl From<&Upstream> for UpstreamResponseDto {
    fn from(upstream: &Upstream) -> Self {
        Self {
            id: upstream.id,
            tenant_id: upstream.tenant_id,
            enabled: upstream.enabled,
            alias: upstream.alias.as_str().to_owned(),
            tags: upstream.tags.clone(),
            server: ServerDto::from(&upstream.server),
            protocol: upstream.protocol.as_str().to_owned(),
            auth: upstream.auth.as_ref().map(AuthDto::from),
            headers: upstream.headers.as_ref().map(HeadersDto::from),
            plugins: upstream.plugins.as_ref().map(PluginChainDto::from),
            rate_limit: upstream.rate_limit.as_ref().map(RateLimitDto::from),
            cors: upstream.cors.as_ref().map(CorsDto::from),
        }
    }
}

// --------------------------------------------------------------------- routes

/// HTTP match keys (`http_match` of the route schema).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct HttpMatchDto {
    /// Methods the route accepts (at least one).
    pub methods: Vec<String>,
    /// Inbound path prefix.
    pub path: String,
    /// Query parameters forwarded to the upstream; empty allows none.
    #[serde(default)]
    pub query_allowlist: Option<Vec<String>>,
    /// How the path suffix is forwarded; defaults to `append`.
    #[serde(default)]
    pub path_suffix_mode: Option<String>,
}

impl HttpMatchDto {
    /// # Errors
    ///
    /// Returns the [`HttpMatch`] validation errors.
    pub fn to_match(&self) -> Result<HttpMatch, OagwError> {
        let mut methods = Vec::with_capacity(self.methods.len());
        for raw in &self.methods {
            methods.push(HttpMethod::parse(raw)?);
        }
        HttpMatch::new(
            methods,
            self.path.clone(),
            self.query_allowlist.clone().unwrap_or_default(),
            match &self.path_suffix_mode {
                None => PathSuffixMode::default(),
                Some(raw) => PathSuffixMode::parse(raw)?,
            },
        )
    }
}

impl From<&HttpMatch> for HttpMatchDto {
    fn from(matched: &HttpMatch) -> Self {
        Self {
            methods: matched
                .methods
                .iter()
                .map(|method| method.as_str().to_owned())
                .collect(),
            path: matched.path.clone(),
            query_allowlist: Some(matched.query_allowlist.clone()),
            path_suffix_mode: Some(suffix_mode_str(matched.path_suffix_mode).to_owned()),
        }
    }
}

/// gRPC match keys (`grpc_match` of the route schema).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct GrpcMatchDto {
    /// Fully qualified protobuf service name.
    pub service: String,
    /// RPC method name.
    pub method: String,
}

impl From<&GrpcMatch> for GrpcMatchDto {
    fn from(matched: &GrpcMatch) -> Self {
        Self {
            service: matched.service.clone(),
            method: matched.method.clone(),
        }
    }
}

/// Match keys of a route (`match` of the route schema).
#[derive(Debug, Clone, PartialEq, Default)]
#[toolkit_macros::api_dto(request, response)]
pub struct RouteMatchDto {
    /// HTTP path prefix match.
    #[serde(default)]
    pub http: Option<HttpMatchDto>,
    /// gRPC service/method match.
    #[serde(default)]
    pub grpc: Option<GrpcMatchDto>,
}

impl RouteMatchDto {
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when neither or both of `http` and
    /// `grpc` are present, or when the match keys are incomplete.
    pub fn to_match(&self) -> Result<RouteMatch, OagwError> {
        match (&self.http, &self.grpc) {
            (Some(http), None) => Ok(RouteMatch::Http(http.to_match()?)),
            (None, Some(grpc)) => Ok(RouteMatch::Grpc(GrpcMatch::new(
                grpc.service.clone(),
                grpc.method.clone(),
            )?)),
            (Some(_), Some(_)) => Err(OagwError::Validation {
                detail: String::from("route match must set exactly one of http or grpc"),
            }),
            (None, None) => Err(OagwError::Validation {
                detail: String::from("route match requires either http or grpc keys"),
            }),
        }
    }
}

impl From<&RouteMatch> for RouteMatchDto {
    fn from(matched: &RouteMatch) -> Self {
        match matched {
            RouteMatch::Http(http) => Self {
                http: Some(HttpMatchDto::from(http)),
                grpc: None,
            },
            RouteMatch::Grpc(grpc) => Self {
                http: None,
                grpc: Some(GrpcMatchDto::from(grpc)),
            },
        }
    }
}

/// Request body of `POST /oagw/v1/routes`
/// (`docs/schemas/route.v1.schema.json`).
#[derive(Debug, Clone, PartialEq, Default)]
#[toolkit_macros::api_dto(request)]
pub struct CreateRouteRequestDto {
    /// Upstream the route forwards to (required).
    pub upstream_id: Option<Uuid>,
    /// Match keys (required).
    #[serde(default, rename = "match")]
    pub r#match: Option<RouteMatchDto>,
    /// Route-level plugin chain.
    #[serde(default)]
    pub plugins: Option<PluginChainDto>,
    /// Route-level rate limit.
    #[serde(default)]
    pub rate_limit: Option<RateLimitDto>,
    /// Route-level CORS configuration.
    #[serde(default)]
    pub cors: Option<CorsDto>,
    /// Whether the route participates in matching; defaults to `true`.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Flat discovery tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

/// Request body of `PUT /oagw/v1/routes/{id}`: a full replacement without the
/// immutable `upstream_id` (`docs/DESIGN.md` §3.3 "PUT (Replace)").
#[derive(Debug, Clone, PartialEq, Default)]
#[toolkit_macros::api_dto(request)]
pub struct ReplaceRouteRequestDto {
    /// Match keys (required).
    #[serde(rename = "match")]
    pub r#match: RouteMatchDto,
    /// Route-level plugin chain.
    #[serde(default)]
    pub plugins: Option<PluginChainDto>,
    /// Route-level rate limit.
    #[serde(default)]
    pub rate_limit: Option<RateLimitDto>,
    /// Route-level CORS configuration.
    #[serde(default)]
    pub cors: Option<CorsDto>,
    /// Whether the route participates in matching; defaults to `true`.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Flat discovery tags.
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

/// Response body of the route read endpoints
/// (`docs/schemas/route.v1.schema.json` plus the PRD `enabled` flag).
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct RouteResponseDto {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Upstream the route forwards to.
    pub upstream_id: Uuid,
    /// Match keys.
    #[serde(rename = "match")]
    pub r#match: RouteMatchDto,
    /// Route-level plugin chain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginChainDto>,
    /// Route-level rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitDto>,
    /// Route-level CORS configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsDto>,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Flat discovery tags.
    pub tags: Vec<String>,
}

impl From<&Route> for RouteResponseDto {
    fn from(route: &Route) -> Self {
        Self {
            id: route.id,
            tenant_id: route.tenant_id,
            upstream_id: route.upstream_id,
            r#match: RouteMatchDto::from(&route.r#match),
            plugins: route.plugins.as_ref().map(PluginChainDto::from),
            rate_limit: route.rate_limit.as_ref().map(RateLimitDto::from),
            cors: route.cors.as_ref().map(CorsDto::from),
            enabled: route.enabled,
            tags: route.tags.clone(),
        }
    }
}

impl CreateRouteRequestDto {
    /// Builds the domain spec, resolving plugin references against the tenant.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for a missing `upstream_id` / `match`
    /// or an invalid value, and [`OagwError::RouteNotFound`] for an unknown
    /// custom plugin.
    pub fn to_spec(
        &self,
        tenant_id: Uuid,
        svc: &ControlPlaneService,
    ) -> Result<RouteSpec, OagwError> {
        Ok(RouteSpec {
            tenant_id,
            upstream_id: self.upstream_id.ok_or_else(|| OagwError::Validation {
                detail: String::from("upstream_id is required"),
            })?,
            r#match: self.r#match.as_ref().map_or_else(
                || {
                    Err(OagwError::Validation {
                        detail: String::from("match is required"),
                    })
                },
                RouteMatchDto::to_match,
            )?,
            plugins: self
                .plugins
                .as_ref()
                .map_or_else(|| Ok(None), |chain| chain.to_chain(tenant_id, svc))?,
            rate_limit: match &self.rate_limit {
                None => None,
                Some(limit) => Some(limit.to_config()?),
            },
            cors: match &self.cors {
                None => None,
                Some(cors) => Some(cors.to_config()?),
            },
            enabled: self.enabled.unwrap_or(true),
            tags: self.tags.clone().unwrap_or_default(),
        })
    }
}

impl ReplaceRouteRequestDto {
    /// Builds the immutable-`upstream_id` replacement
    /// (`docs/DESIGN.md` §3.3 "PUT (Replace)").
    ///
    /// # Errors
    ///
    /// See [`CreateRouteRequestDto::to_spec`].
    pub fn to_update(
        &self,
        tenant_id: Uuid,
        svc: &ControlPlaneService,
    ) -> Result<RouteUpdate, OagwError> {
        Ok(RouteUpdate {
            r#match: self.r#match.to_match()?,
            plugins: self
                .plugins
                .as_ref()
                .map_or_else(|| Ok(None), |chain| chain.to_chain(tenant_id, svc))?,
            rate_limit: match &self.rate_limit {
                None => None,
                Some(limit) => Some(limit.to_config()?),
            },
            cors: match &self.cors {
                None => None,
                Some(cors) => Some(cors.to_config()?),
            },
            enabled: self.enabled.unwrap_or(true),
            tags: self.tags.clone().unwrap_or_default(),
        })
    }
}

// -------------------------------------------------------------------- plugins

/// Request body of `POST /oagw/v1/plugins`
/// (`docs/ADR/0002-plugin-system.md` Appendix A).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[toolkit_macros::api_dto(request)]
pub struct RegisterPluginRequestDto {
    /// Plugin kind: `auth`, `guard` or `transform`.
    pub kind: Option<String>,
    /// Human-readable name (unique per tenant).
    pub name: Option<String>,
    /// Starlark source.
    pub source: Option<String>,
}

/// Descriptor of a plugin as rendered by the read endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct PluginResponseDto {
    /// GTS instance id (built-in) or UUID (custom).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Plugin kind.
    pub kind: String,
    /// GTS type of the plugin.
    #[serde(rename = "type")]
    pub plugin_type: String,
    /// `true` for a fixed built-in plugin.
    pub builtin: bool,
    /// `true` when the identifier may be bound from `plugins.items[]`.
    pub bindable: bool,
    /// `true` once no upstream or route references the plugin.
    pub gc_eligible: bool,
}

impl From<&PluginDescriptor> for PluginResponseDto {
    fn from(descriptor: &PluginDescriptor) -> Self {
        Self {
            id: descriptor.id.clone(),
            name: descriptor.name.clone(),
            kind: descriptor.kind.as_str().to_owned(),
            plugin_type: descriptor.plugin_type.clone(),
            builtin: descriptor.builtin,
            bindable: descriptor.bindable,
            gc_eligible: descriptor.gc_eligible,
        }
    }
}

/// Response body of `GET /oagw/v1/plugins/{id}/source`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[toolkit_macros::api_dto(response)]
pub struct PluginSourceResponseDto {
    /// GTS instance id (built-in) or UUID (custom).
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Plugin kind.
    pub kind: String,
    /// GTS type of the plugin.
    #[serde(rename = "type")]
    pub plugin_type: String,
    /// `true` for a fixed built-in plugin.
    pub builtin: bool,
    /// Starlark source (or the built-in contract).
    pub source_code: String,
}

impl From<&PluginSource> for PluginSourceResponseDto {
    fn from(source: &PluginSource) -> Self {
        Self {
            id: source.descriptor.id.clone(),
            name: source.descriptor.name.clone(),
            kind: source.descriptor.kind.as_str().to_owned(),
            plugin_type: source.descriptor.plugin_type.clone(),
            builtin: source.descriptor.builtin,
            source_code: source.source_code.clone(),
        }
    }
}

// ------------------------------------------------------------ list parameters

/// OData list query of the management surface
/// (`docs/DESIGN.md` §3.3 "List Query Parameters").
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListQuery {
    /// `$filter` — `field eq value` clauses joined by `and`.
    pub filter: Option<String>,
    /// `$select` — comma-separated field names.
    pub select: Option<String>,
    /// `$orderby` — `field [asc|desc]`.
    pub orderby: Option<String>,
    /// `$top` — page size (default 50, maximum 100).
    pub top: Option<usize>,
    /// `$skip` — offset.
    pub skip: Option<usize>,
}

/// Default page size of the list endpoints.
pub const DEFAULT_PAGE_SIZE: usize = 50;

/// Maximum page size of the list endpoints.
pub const MAX_PAGE_SIZE: usize = 100;

impl ListQuery {
    /// Parses a raw query string; unknown parameters are ignored.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for a non-integer `$top`/`$skip`.
    pub fn parse(raw: Option<&str>) -> Result<Self, OagwError> {
        let mut query = Self::default();
        let Some(raw) = raw else {
            return Ok(query);
        };
        for (key, value) in form_urlencoded::parse(raw.as_bytes()) {
            match key.as_ref() {
                "$filter" => query.filter = Some(value.into_owned()),
                "$select" => query.select = Some(value.into_owned()),
                "$orderby" => query.orderby = Some(value.into_owned()),
                "$top" => query.top = Some(parse_count("$top", &value)?),
                "$skip" => query.skip = Some(parse_count("$skip", &value)?),
                _ => {}
            }
        }
        Ok(query)
    }

    /// Page size, clamped to [`MAX_PAGE_SIZE`].
    #[must_use]
    pub fn limit(&self) -> usize {
        self.top
            .map_or(DEFAULT_PAGE_SIZE, |top| top.min(MAX_PAGE_SIZE))
    }

    /// Pagination offset.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.skip.unwrap_or_default()
    }

    /// Selected field names.
    #[must_use]
    pub fn selected_fields(&self) -> Option<Vec<String>> {
        self.select.as_ref().map(|select| {
            select
                .split(',')
                .map(str::trim)
                .filter(|field| !field.is_empty())
                .map(str::to_owned)
                .collect()
        })
    }

    /// Applies `$filter`, `$orderby`, `$skip`/`$top` and `$select` to `items`.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for a filter or order field that no
    /// item carries.
    pub fn apply<T: serde::Serialize>(&self, items: &[T]) -> Result<Vec<Value>, OagwError> {
        let mut values = items
            .iter()
            .map(|item| {
                serde_json::to_value(item).map_err(|error| OagwError::Internal {
                    detail: format!("failed to serialize a list item: {error}"),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(filter) = &self.filter {
            filter_values(&mut values, filter)?;
        }
        if let Some(orderby) = &self.orderby {
            order_values(&mut values, orderby)?;
        }
        let selected = self.selected_fields();
        Ok(values
            .iter()
            .skip(self.offset())
            .take(self.limit())
            .map(|item| apply_select(item, selected.as_deref()))
            .collect())
    }
}

/// Keeps only the items matching every `field eq value` clause of `$filter`.
fn filter_values(values: &mut Vec<Value>, filter: &str) -> Result<(), OagwError> {
    let clauses = parse_filter(filter)?;
    for clause in &clauses {
        require_field(values.first(), &clause.field)?;
    }
    values.retain(|item| {
        clauses.iter().all(|clause| {
            item.get(&clause.field).and_then(value_token).as_deref() == Some(clause.value.as_str())
        })
    });
    Ok(())
}

/// Orders the items by `field [asc|desc]`.
fn order_values(values: &mut [Value], spec: &str) -> Result<(), OagwError> {
    let (field, descending) = parse_orderby(spec)?;
    require_field(values.first(), field)?;
    values.sort_by(|left, right| {
        let ordering = compare(left.get(field), right.get(field));
        if descending {
            ordering.reverse()
        } else {
            ordering
        }
    });
    Ok(())
}

/// Rejects a filter or order field that no item carries.
fn require_field(item: Option<&Value>, field: &str) -> Result<(), OagwError> {
    if let Some(item) = item {
        let unknown = !item
            .as_object()
            .is_some_and(|object| object.contains_key(field));
        if unknown {
            return Err(OagwError::Validation {
                detail: format!("'{field}' is not a filterable or sortable field"),
            });
        }
    }
    Ok(())
}

/// One `field eq value` clause of a `$filter`.
struct FilterClause {
    field: String,
    value: String,
}

/// Splits `$filter` into its `and`-joined `field eq value` clauses.
fn parse_filter(filter: &str) -> Result<Vec<FilterClause>, OagwError> {
    filter
        .split(" and ")
        .map(str::trim)
        .filter(|clause| !clause.is_empty())
        .map(|clause| {
            let (field, value) =
                clause
                    .split_once(" eq ")
                    .ok_or_else(|| OagwError::Validation {
                        detail: format!(
                            "unsupported $filter expression '{clause}' (expected 'field eq value')"
                        ),
                    })?;
            Ok(FilterClause {
                field: field.trim().to_ascii_lowercase(),
                value: unquote(value.trim()),
            })
        })
        .collect()
}

/// Splits `$orderby` into its field and direction.
fn parse_orderby(spec: &str) -> Result<(&str, bool), OagwError> {
    let mut parts = spec.split_whitespace();
    let field = parts.next().unwrap_or_default();
    if field.is_empty() {
        return Err(OagwError::Validation {
            detail: String::from("$orderby requires a field name"),
        });
    }
    let descending = match parts.next() {
        None => false,
        Some("asc") => false,
        Some("desc") => true,
        Some(direction) => {
            return Err(OagwError::Validation {
                detail: format!("unknown $orderby direction '{direction}' (expected asc or desc)"),
            });
        }
    };
    Ok((field, descending))
}

/// Strips the surrounding single quotes of an OData string literal.
fn unquote(raw: &str) -> String {
    raw.strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .map_or_else(|| raw.to_owned(), str::to_owned)
}

/// Canonical token of a JSON value, so `uuid` and `bool` literals compare.
fn value_token(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// Orders two optional JSON values of the same kind.
fn compare(left: Option<&Value>, right: Option<&Value>) -> std::cmp::Ordering {
    match (left.and_then(value_token), right.and_then(value_token)) {
        (Some(left), Some(right)) => left.cmp(&right),
        _ => Ordering::Equal,
    }
}

/// Parses a `$top` / `$skip` value.
fn parse_count(name: &str, raw: &str) -> Result<usize, OagwError> {
    raw.trim()
        .parse::<usize>()
        .map_err(|_| OagwError::Validation {
            detail: format!("{name} must be a non-negative integer, got '{raw}'"),
        })
}

// --------------------------------------------------------------- wire parsing

/// Sharing-mode token of a sharing mode.
#[must_use]
pub const fn sharing_str(sharing: SharingMode) -> &'static str {
    match sharing {
        SharingMode::Private => "private",
        SharingMode::Inherit => "inherit",
        SharingMode::Enforce => "enforce",
    }
}

/// `append` / `disabled` token of a path suffix mode.
#[must_use]
pub const fn suffix_mode_str(mode: PathSuffixMode) -> &'static str {
    match mode {
        PathSuffixMode::Append => "append",
        PathSuffixMode::Disabled => "disabled",
    }
}

/// Passthrough token of a header passthrough mode.
#[must_use]
pub const fn passthrough_str(passthrough: HeaderPassthrough) -> &'static str {
    match passthrough {
        HeaderPassthrough::None => "none",
        HeaderPassthrough::Allowlist => "allowlist",
        HeaderPassthrough::All => "all",
    }
}

/// Parses an optional sharing mode, defaulting to `private`.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] for an unknown token.
pub fn parse_sharing(raw: Option<&str>) -> Result<SharingMode, OagwError> {
    match raw {
        None => Ok(SharingMode::default()),
        Some(raw) => SharingMode::parse(raw),
    }
}

/// Parses the required upstream protocol.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] when the field is missing or unknown.
pub fn parse_protocol(raw: Option<&String>) -> Result<Protocol, OagwError> {
    let Some(raw) = raw else {
        return Err(OagwError::Validation {
            detail: String::from("protocol is required (server and protocol are mandatory)"),
        });
    };
    Protocol::parse(raw)
}

/// Parses a rate limit window.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] for an unknown token.
pub fn parse_window(raw: Option<&str>) -> Result<RateLimitWindow, OagwError> {
    match raw.unwrap_or("second").trim().to_ascii_lowercase().as_str() {
        "second" => Ok(RateLimitWindow::Second),
        "minute" => Ok(RateLimitWindow::Minute),
        "hour" => Ok(RateLimitWindow::Hour),
        "day" => Ok(RateLimitWindow::Day),
        other => Err(OagwError::Validation {
            detail: format!("unknown rate limit window '{other}'"),
        }),
    }
}

/// Parses a rate limit algorithm.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] for an unknown token.
pub fn parse_algorithm(raw: Option<&str>) -> Result<RateLimitAlgorithm, OagwError> {
    match raw
        .unwrap_or("token_bucket")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "token_bucket" => Ok(RateLimitAlgorithm::TokenBucket),
        "sliding_window" => Ok(RateLimitAlgorithm::SlidingWindow),
        other => Err(OagwError::Validation {
            detail: format!("unknown rate limit algorithm '{other}'"),
        }),
    }
}

/// Parses a rate limit counter scope.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] for an unknown token.
pub fn parse_scope(raw: Option<&str>) -> Result<RateLimitScope, OagwError> {
    match raw.unwrap_or("tenant").trim().to_ascii_lowercase().as_str() {
        "global" => Ok(RateLimitScope::Global),
        "tenant" => Ok(RateLimitScope::Tenant),
        "user" => Ok(RateLimitScope::User),
        "ip" => Ok(RateLimitScope::Ip),
        "route" => Ok(RateLimitScope::Route),
        other => Err(OagwError::Validation {
            detail: format!("unknown rate limit scope '{other}'"),
        }),
    }
}

/// Parses a rate limit strategy.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] for an unknown token.
pub fn parse_strategy(raw: Option<&str>) -> Result<RateLimitStrategy, OagwError> {
    match raw.unwrap_or("reject").trim().to_ascii_lowercase().as_str() {
        "reject" => Ok(RateLimitStrategy::Reject),
        "queue" => Ok(RateLimitStrategy::Queue),
        "degrade" => Ok(RateLimitStrategy::Degrade),
        other => Err(OagwError::Validation {
            detail: format!("unknown rate limit strategy '{other}'"),
        }),
    }
}

/// Parses a positive integer, rejecting zero.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] when `value` is zero.
pub fn nonzero(value: u32, field: &str) -> Result<NonZeroU32, OagwError> {
    NonZeroU32::new(value).ok_or_else(|| OagwError::Validation {
        detail: format!("{field} must be at least 1"),
    })
}

/// `token_bucket` / `sliding_window` token of an algorithm.
#[must_use]
pub const fn algorithm_str(algorithm: RateLimitAlgorithm) -> &'static str {
    match algorithm {
        RateLimitAlgorithm::TokenBucket => "token_bucket",
        RateLimitAlgorithm::SlidingWindow => "sliding_window",
    }
}

/// `second` / `minute` / `hour` / `day` token of a window.
#[must_use]
pub const fn window_str(window: RateLimitWindow) -> &'static str {
    match window {
        RateLimitWindow::Second => "second",
        RateLimitWindow::Minute => "minute",
        RateLimitWindow::Hour => "hour",
        RateLimitWindow::Day => "day",
    }
}

/// Scope token of a rate limit.
#[must_use]
pub const fn scope_str(scope: RateLimitScope) -> &'static str {
    match scope {
        RateLimitScope::Global => "global",
        RateLimitScope::Tenant => "tenant",
        RateLimitScope::User => "user",
        RateLimitScope::Ip => "ip",
        RateLimitScope::Route => "route",
    }
}

/// Strategy token of a rate limit.
#[must_use]
pub const fn strategy_str(strategy: RateLimitStrategy) -> &'static str {
    match strategy {
        RateLimitStrategy::Reject => "reject",
        RateLimitStrategy::Queue => "queue",
        RateLimitStrategy::Degrade => "degrade",
    }
}

/// Builds an upstream spec from a request body, resolving every plugin
/// reference against the tenant's registry.
///
/// # Errors
///
/// Returns the validation errors of the parts and the plugin resolution
/// errors of [`ControlPlaneService::resolve_plugin_ref`].
pub fn upstream_spec(
    body: &UpstreamRequestDto,
    tenant_id: Uuid,
    svc: &ControlPlaneService,
) -> Result<UpstreamSpec, OagwError> {
    let server = body
        .server
        .as_ref()
        .ok_or_else(|| OagwError::Validation {
            detail: String::from("server is required (server.endpoints.minItems = 1)"),
        })?
        .to_server()?;
    let tags = body.tags.clone().unwrap_or_default();
    validate_tags(&tags)?;
    Ok(UpstreamSpec {
        tenant_id,
        alias: match &body.alias {
            None => None,
            Some(raw) => Some(Alias::parse(raw)?),
        },
        protocol: parse_protocol(body.protocol.as_ref())?,
        enabled: body.enabled.unwrap_or(true),
        server,
        auth: match &body.auth {
            None => None,
            Some(auth) => auth.to_config(tenant_id, svc)?,
        },
        headers: match &body.headers {
            None => None,
            Some(headers) => Some(headers.to_config()?),
        },
        plugins: body
            .plugins
            .as_ref()
            .map_or_else(|| Ok(None), |chain| chain.to_chain(tenant_id, svc))?,
        rate_limit: match &body.rate_limit {
            None => None,
            Some(limit) => Some(limit.to_config()?),
        },
        cors: match &body.cors {
            None => None,
            Some(cors) => Some(cors.to_config()?),
        },
        tags,
    })
}
