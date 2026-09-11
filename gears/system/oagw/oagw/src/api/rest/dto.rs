//! Wire DTOs for the management API.
//!
//! The domain model is the wire format: an upstream body is the upstream. The
//! DTOs here exist only for the shapes the model does not have — the list
//! envelopes, the count page and the create/replace responses.

use crate::domain::model::{Plugin, Route, Upstream};
use serde::Serialize;

/// A single-resource response body.
#[derive(Debug, Clone, Serialize)]
pub struct ResourceEnvelope<T> {
    /// The resource, at the top level of the document.
    #[serde(flatten)]
    pub resource: T,
}

impl<T> ResourceEnvelope<T> {
    /// Wraps a resource.
    #[must_use]
    pub fn new(resource: T) -> Self {
        Self { resource }
    }
}

/// A collection response.
#[derive(Debug, Clone, Serialize)]
pub struct CollectionEnvelope<T> {
    /// Number of items in this page.
    pub count: usize,
    /// Offset the page started at.
    pub offset: usize,
    /// Total number of items the filter matched, when `$count` was asked for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<usize>,
    /// The page.
    pub items: Vec<T>,
}

impl<T> CollectionEnvelope<T> {
    /// Builds a page from the full result set and the query options.
    #[must_use]
    pub fn paginate(items: Vec<T>, total: Option<usize>, offset: usize) -> Self {
        Self {
            count: items.len(),
            offset,
            total,
            items,
        }
    }
}

/// The response to a create.
#[derive(Debug, Clone, Serialize)]
pub struct CreatedResource<T> {
    /// The resource as stored.
    #[serde(flatten)]
    pub resource: T,
}

impl<T> CreatedResource<T> {
    /// Wraps a freshly created resource.
    #[must_use]
    pub fn new(resource: T) -> Self {
        Self { resource }
    }
}

/// Alias echo for a created upstream, so a client can see what was derived.
#[derive(Debug, Clone, Serialize)]
pub struct AliasEcho {
    /// The alias in force after the create.
    pub alias: String,
    /// Whether the alias was derived from the endpoints rather than supplied.
    pub derived: bool,
}

/// A management resource as it is returned on the wire.
#[derive(Debug, Clone, Serialize)]
pub struct UpstreamView {
    /// Identifier.
    pub id: String,
    /// Owning tenant.
    pub tenant_id: String,
    /// Alias.
    pub alias: String,
    /// Whether the upstream is addressable.
    pub enabled: bool,
    /// Protocol GTS identifier.
    pub protocol: String,
    /// Server endpoints.
    pub server: crate::domain::model::ServerConfig,
    /// Credential-injection configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<crate::domain::model::AuthConfig>,
    /// Header rules.
    #[serde(skip_serializing_if = "crate::domain::model::HeadersConfig::is_empty")]
    pub headers: crate::domain::model::HeadersConfig,
    /// Rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<crate::domain::model::RateLimit>,
    /// CORS policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<crate::domain::model::Cors>,
    /// Plugin chain.
    pub plugins: crate::domain::model::PluginsConfig,
    /// Tags.
    pub tags: Vec<String>,
    /// Creation timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Last modification timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl From<Upstream> for UpstreamView {
    fn from(value: Upstream) -> Self {
        Self {
            id: value.id,
            tenant_id: value.tenant_id.to_string(),
            alias: value.alias,
            enabled: value.enabled,
            protocol: value.protocol.as_gts_id().to_owned(),
            server: value.server,
            auth: value.auth,
            headers: value.headers,
            rate_limit: value.rate_limit,
            cors: value.cors,
            plugins: value.plugins,
            tags: value.tags,
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

/// A route as it is returned on the wire.
#[derive(Debug, Clone, Serialize)]
pub struct RouteView {
    /// Identifier.
    pub id: String,
    /// Owning tenant.
    pub tenant_id: String,
    /// Upstream the route belongs to.
    pub upstream_id: String,
    /// Whether the route is matchable.
    pub enabled: bool,
    /// Match priority.
    pub priority: i32,
    /// Match configuration.
    #[serde(rename = "match")]
    pub match_config: crate::domain::model::MatchConfig,
    /// Rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<crate::domain::model::RateLimit>,
    /// CORS policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<crate::domain::model::Cors>,
    /// Plugin chain.
    pub plugins: crate::domain::model::PluginsConfig,
    /// Tags.
    pub tags: Vec<String>,
    /// Creation timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Last modification timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl From<Route> for RouteView {
    fn from(value: Route) -> Self {
        Self {
            id: value.id,
            tenant_id: value.tenant_id.to_string(),
            upstream_id: value.upstream_id,
            enabled: value.enabled,
            priority: value.priority,
            match_config: value.match_config,
            rate_limit: value.rate_limit,
            cors: value.cors,
            plugins: value.plugins,
            tags: value.tags,
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

/// A plugin as it is returned on the wire.
#[derive(Debug, Clone, Serialize)]
pub struct PluginView {
    /// Identifier.
    pub id: String,
    /// Owning tenant.
    pub tenant_id: String,
    /// Kind of plugin: `auth`, `guard` or `transform`.
    #[serde(rename = "plugin_type")]
    pub plugin_type: crate::domain::model::PluginKind,
    /// Human-readable name.
    pub name: String,
    /// Optional description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema the plugin configuration must satisfy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Phases the plugin participates in.
    pub phases: Vec<crate::domain::model::PluginPhase>,
    /// Reference to the stored source, `…/source`.
    pub source_ref: String,
}

impl From<Plugin> for PluginView {
    fn from(value: Plugin) -> Self {
        let source_ref = format!("plugins/{}/source", value.id);
        Self {
            id: value.id,
            tenant_id: value.tenant_id.to_string(),
            plugin_type: value.plugin_type,
            name: value.name,
            description: value.description,
            config_schema: value.config_schema,
            phases: value.phases,
            source_ref,
        }
    }
}

/// A built-in plugin the gear implements itself.
#[derive(Debug, Clone, Serialize)]
pub struct BuiltinPlugin {
    /// GTS identifier.
    #[serde(rename = "type")]
    pub plugin_type: String,
    /// Human-readable name.
    pub name: String,
    /// Kind.
    #[serde(rename = "plugin_type")]
    pub kind: crate::domain::model::PluginKind,
    /// Whether the plugin accepts configuration.
    pub configurable: bool,
}

/// The plugin catalogue: built-ins plus the tenant's custom plugins.
#[derive(Debug, Clone, Serialize)]
pub struct PluginCatalogue {
    /// Built-in plugins.
    pub builtins: Vec<BuiltinPlugin>,
    /// Custom plugins registered by the tenant.
    pub custom: Vec<PluginView>,
}
