//! REST DTOs for the OAGW control plane.
//!
//! Requests are subsets of the domain configuration; responses mirror
//! `docs/schemas/upstream.v1.schema.json` and `route.v1.schema.json`. Secret
//! material is redacted on the way out.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::dto::{
    AuthConfig, CorsConfig, EndpointScheme, HeadersConfig, PluginBindings, Protocol, RateLimitConfig,
    Route, RouteConfig, Server, Upstream, UpstreamConfig,
};

/// Secret-looking configuration keys redacted in every response.
pub const REDACTED_KEYS: [&str; 8] = [
    "value",
    "token",
    "secret",
    "secret_value",
    "api_key",
    "client_secret",
    "password",
    "client_secret_ref",
];

/// Redaction marker written in place of a secret.
pub const REDACTED: &str = "[REDACTED]";

/// Request payload for `POST /upstreams` and `PUT /upstreams/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UpstreamRequestDto {
    /// Whether this upstream accepts traffic. Defaults to `true`.
    #[serde(default = "crate::domain::dto::default_true")]
    pub enabled: bool,
    /// Explicit routing alias; omitted → derived.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Flat categorisation tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Upstream endpoints.
    pub server: Server,
    /// Upstream protocol.
    #[serde(default)]
    pub protocol: Protocol,
    /// Outbound authentication plugin.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: HeadersConfig,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: PluginBindings,
    /// Rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl From<UpstreamRequestDto> for UpstreamConfig {
    fn from(dto: UpstreamRequestDto) -> Self {
        UpstreamConfig {
            enabled: dto.enabled,
            alias: dto.alias,
            tags: dto.tags,
            server: dto.server,
            protocol: dto.protocol,
            auth: dto.auth,
            headers: dto.headers,
            plugins: dto.plugins,
            rate_limit: dto.rate_limit,
            cors: dto.cors,
        }
    }
}

impl From<UpstreamConfig> for UpstreamRequestDto {
    fn from(config: UpstreamConfig) -> Self {
        UpstreamRequestDto {
            enabled: config.enabled,
            alias: config.alias,
            tags: config.tags,
            server: config.server,
            protocol: config.protocol,
            auth: config.auth,
            headers: config.headers,
            plugins: config.plugins,
            rate_limit: config.rate_limit,
            cors: config.cors,
        }
    }
}

impl Default for UpstreamRequestDto {
    fn default() -> Self {
        Self {
            enabled: true,
            alias: None,
            tags: Vec::new(),
            server: Server::default(),
            protocol: Protocol::default(),
            auth: None,
            headers: HeadersConfig::default(),
            plugins: PluginBindings::default(),
            rate_limit: None,
            cors: None,
        }
    }
}

/// Response payload for one upstream.
#[derive(Debug, Clone, Serialize)]
pub struct UpstreamResponseDto {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Creation instant (epoch seconds).
    pub created_at: u64,
    /// Routing alias.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Whether the upstream accepts traffic.
    pub enabled: bool,
    /// Flat categorisation tags.
    pub tags: Vec<String>,
    /// Upstream endpoints.
    pub server: Server,
    /// Upstream protocol.
    pub protocol: Protocol,
    /// Outbound authentication plugin, with secrets redacted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    pub headers: HeadersConfig,
    /// Plugin chain.
    pub plugins: PluginBindings,
    /// Rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

impl From<Upstream> for UpstreamResponseDto {
    fn from(upstream: Upstream) -> Self {
        Self {
            id: upstream.id,
            tenant_id: upstream.tenant_id,
            created_at: upstream.created_at,
            alias: upstream.config.alias,
            enabled: upstream.config.enabled,
            tags: upstream.config.tags,
            server: upstream.config.server,
            protocol: upstream.config.protocol,
            auth: upstream.config.auth.map(|mut auth| {
                redact(&mut auth.config);
                auth
            }),
            headers: upstream.config.headers,
            plugins: upstream.config.plugins,
            rate_limit: upstream.config.rate_limit,
            cors: upstream.config.cors,
        }
    }
}

/// Request payload for `POST /routes` and `PUT /routes/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RouteRequestDto {
    /// Owning upstream (immutable after create).
    pub upstream_id: Uuid,
    /// Whether this route participates in matching. Defaults to `true`.
    #[serde(default = "crate::domain::dto::default_true")]
    pub enabled: bool,
    /// Flat categorisation tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Matching rules.
    #[serde(rename = "match")]
    pub matcher: crate::domain::dto::MatchRules,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: PluginBindings,
    /// Rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
}

impl Default for RouteRequestDto {
    fn default() -> Self {
        Self {
            upstream_id: Uuid::nil(),
            enabled: true,
            tags: Vec::new(),
            matcher: crate::domain::dto::MatchRules::default(),
            plugins: PluginBindings::default(),
            rate_limit: None,
        }
    }
}

impl From<RouteRequestDto> for RouteConfig {
    fn from(dto: RouteRequestDto) -> Self {
        RouteConfig {
            upstream_id: dto.upstream_id,
            enabled: dto.enabled,
            tags: dto.tags,
            matcher: dto.matcher,
            plugins: dto.plugins,
            rate_limit: dto.rate_limit,
        }
    }
}

impl From<RouteConfig> for RouteRequestDto {
    fn from(config: RouteConfig) -> Self {
        RouteRequestDto {
            upstream_id: config.upstream_id,
            enabled: config.enabled,
            tags: config.tags,
            matcher: config.matcher,
            plugins: config.plugins,
            rate_limit: config.rate_limit,
        }
    }
}

/// Response payload for one route.
#[derive(Debug, Clone, Serialize)]
pub struct RouteResponseDto {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Owning upstream.
    pub upstream_id: Uuid,
    /// Creation instant (epoch seconds).
    pub created_at: u64,
    /// Whether the route participates in matching.
    pub enabled: bool,
    /// Flat categorisation tags.
    pub tags: Vec<String>,
    /// Matching rules.
    #[serde(rename = "match")]
    pub matcher: crate::domain::dto::MatchRules,
    /// Plugin chain.
    pub plugins: PluginBindings,
    /// Rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
}

impl From<Route> for RouteResponseDto {
    fn from(route: Route) -> Self {
        Self {
            id: route.id,
            tenant_id: route.tenant_id,
            upstream_id: route.upstream_id,
            created_at: route.created_at,
            enabled: route.config.enabled,
            tags: route.config.tags,
            matcher: route.config.matcher,
            plugins: route.config.plugins,
            rate_limit: route.config.rate_limit,
        }
    }
}

/// Request payload for the plugin resource.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginRequestDto {
    /// Auth / guard / transform.
    pub plugin_type: crate::domain::dto::PluginKind,
    /// Human-readable name.
    pub name: String,
    /// Sandboxed source text.
    #[serde(default)]
    pub source: String,
    /// Configuration the plugin is executed with.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub config: serde_json::Value,
}

/// Response payload for the plugin resource.
#[derive(Debug, Clone, Serialize)]
pub struct PluginResponseDto {
    /// System-generated identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Creation instant (epoch seconds).
    pub created_at: u64,
    /// Auth / guard / transform.
    pub plugin_type: crate::domain::dto::PluginKind,
    /// Human-readable name.
    pub name: String,
    /// Sandboxed source text.
    pub source: String,
    /// Configuration the plugin is executed with.
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub config: serde_json::Value,
}

impl From<crate::domain::dto::Plugin> for PluginResponseDto {
    fn from(plugin: crate::domain::dto::Plugin) -> Self {
        Self {
            id: plugin.id,
            tenant_id: plugin.tenant_id,
            created_at: plugin.created_at,
            plugin_type: plugin.plugin_type,
            name: plugin.name,
            source: plugin.source,
            config: plugin.config,
        }
    }
}

/// Stored source text of a custom plugin, served by
/// `GET /plugins/{id}/source`.
#[derive(Debug, Clone, Serialize)]
pub struct PluginSourceDto {
    /// System-generated identifier.
    pub id: Uuid,
    /// Auth / guard / transform.
    pub plugin_type: crate::domain::dto::PluginKind,
    /// Human-readable name.
    pub name: String,
    /// Sandboxed source text.
    pub source: String,
}

/// Envelope for list endpoints: OData v4 `value` plus the repo-style `items`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListEnvelope<T> {
    /// OData v4 collection name.
    pub value: Vec<T>,
    /// Repo-conventional alias of [`ListEnvelope::value`].
    pub items: Vec<T>,
    /// Total number of matching rows.
    pub count: usize,
}

impl<T> ListEnvelope<T> {
    /// Wrap a page.
    #[must_use]
    pub fn from_page(page: crate::domain::repo::Page<T>) -> Self
    where
        T: Clone,
    {
        Self::from_vec(page.items, page.total)
    }

    /// Wrap already-projected items with their total count.
    #[must_use]
    pub fn from_vec(items: Vec<T>, count: usize) -> Self
    where
        T: Clone,
    {
        ListEnvelope {
            value: items.clone(),
            items,
            count,
        }
    }
}

/// Catalog of built-in plugins, advertised by `GET /plugins/catalog`.
///
/// The `auth` / `guard` / `transform` arrays list every documented plugin id
/// (built-in *and* catalog-only) as fully-qualified GTS identifiers;
/// `*_names` repeats them as the short registry names. The catalog-only ids
/// are additionally listed under `reserved` so a client can tell them apart —
/// binding one fails with a validation error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCatalog {
    /// Built-in auth plugin ids (GTS ids, built-in first, then catalog-only).
    pub auth: Vec<String>,
    /// Built-in guard plugin ids.
    pub guard: Vec<String>,
    /// Built-in transform plugin ids.
    pub transform: Vec<String>,
    /// Short names of [`PluginCatalog::auth`], in the same order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub auth_names: Vec<String>,
    /// Short names of [`PluginCatalog::guard`], in the same order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub guard_names: Vec<String>,
    /// Short names of [`PluginCatalog::transform`], in the same order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub transform_names: Vec<String>,
    /// Catalogued-but-unimplemented plugin ids, by family.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserved: Option<PluginCatalogReserved>,
}

/// The catalog-only half of [`PluginCatalog`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCatalogReserved {
    /// Reserved auth plugin ids.
    pub auth: Vec<String>,
    /// Reserved guard plugin ids.
    pub guard: Vec<String>,
    /// Reserved transform plugin ids.
    pub transform: Vec<String>,
}

/// Redact secret-looking keys in a plugin configuration object.
pub fn redact(config: &mut serde_json::Value) {
    let Some(map) = config.as_object_mut() else {
        return;
    };
    for (key, value) in map.iter_mut() {
        let lower = key.to_ascii_lowercase();
        if REDACTED_KEYS.contains(&lower.as_str()) {
            let is_reference = lower.ends_with("_ref");
            if !is_reference {
                if value.is_string() {
                    *value = serde_json::Value::String(String::from(REDACTED));
                }
                continue;
            }
        }
        if value.is_object() {
            redact(value);
        }
    }
}

/// The endpoint scheme the wire accepts, including `http`.
#[must_use]
pub fn parse_scheme(raw: &str) -> Option<EndpointScheme> {
    serde_json::from_value(serde_json::Value::String(raw.to_owned())).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_hides_values_but_keeps_references() {
        let mut config = serde_json::json!({
            "value": "sk-plain",
            "client_secret_ref": "openai",
            "nested": { "token": "abc" }
        });
        redact(&mut config);
        assert_eq!(config["value"], REDACTED);
        assert_eq!(config["client_secret_ref"], "openai");
        assert_eq!(config["nested"]["token"], REDACTED);
    }

    #[test]
    fn list_envelope_exposes_both_keys() {
        let envelope = ListEnvelope::from_page(crate::domain::repo::Page {
            items: vec![1u32, 2, 3],
            total: 3,
        });
        assert_eq!(envelope.value.len(), 3);
        assert_eq!(envelope.items.len(), 3);
        assert_eq!(envelope.count, 3);
    }

    #[test]
    fn upstream_dto_round_trips_through_the_domain_config() {
        let dto = UpstreamRequestDto {
            enabled: true,
            server: Server {
                endpoints: vec![crate::domain::dto::Endpoint {
                    scheme: EndpointScheme::Https,
                    host: String::from("api.openai.com"),
                    port: None,
                }],
            },
            ..UpstreamRequestDto::default()
        };
        let config: UpstreamConfig = dto.clone().into();
        let back: UpstreamRequestDto = config.into();
        assert_eq!(dto.server, back.server);
        assert!(back.enabled);
    }
}
