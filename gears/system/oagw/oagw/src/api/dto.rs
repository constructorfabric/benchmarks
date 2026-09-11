//! Request and response bodies of the management surface.
//!
//! The DTOs are deliberately close to the domain entities: the control plane is a thin
//! CRUD surface over them, and the wire shape is what the design tabulates.

/// The body of `POST /upstreams` and `PUT /upstreams/{id}`.
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct UpstreamDto {
    /// The system-generated identifier. The schema marks it read-only, so a value
    /// carried in a request body is ignored.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Derived or explicit alias; omitted means derive it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Whether the upstream accepts proxy requests.
    #[serde(default = "crate::domain::default_enabled")]
    pub enabled: bool,
    /// Endpoint pool and protocol.
    #[serde(default)]
    pub server: crate::domain::upstream::Server,
    /// Protocol identifier.
    #[serde(default)]
    pub protocol: String,
    /// Authentication configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<crate::domain::upstream::AuthConfig>,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: crate::domain::upstream::HeadersConfig,
    /// Rate-limit policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<crate::domain::upstream::RateLimit>,
    /// CORS policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<crate::domain::upstream::CorsConfig>,
    /// Ordered plugin bindings.
    #[serde(default)]
    pub plugins: Vec<crate::domain::plugin::PluginBinding>,
    /// Tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl UpstreamDto {
    /// Projects a stored upstream into its wire shape.
    #[must_use]
    pub fn from_entity(entity: &crate::domain::upstream::Upstream) -> Self {
        Self {
            id: entity.id.clone(),
            alias: Some(entity.alias.clone()),
            enabled: entity.enabled,
            server: entity.server.clone(),
            protocol: entity.protocol.clone(),
            auth: entity.auth.clone(),
            headers: entity.headers.clone(),
            rate_limit: entity.rate_limit.clone(),
            cors: entity.cors.clone(),
            plugins: entity.plugins.clone(),
            tags: entity.tags.clone(),
        }
    }

    /// Builds an entity from the wire shape, keeping the stored identity fields.
    #[must_use]
    pub fn into_entity(
        self,
        id: String,
        tenant_id: uuid::Uuid,
    ) -> crate::domain::upstream::Upstream {
        crate::domain::upstream::Upstream {
            id,
            tenant_id,
            alias: self.alias.unwrap_or_default(),
            enabled: self.enabled,
            server: self.server,
            protocol: self.protocol,
            auth: self.auth,
            headers: self.headers,
            rate_limit: self.rate_limit,
            cors: self.cors,
            plugins: self.plugins,
            tags: self.tags,
        }
    }
}

/// The body of `POST /routes` and `PUT /routes/{id}`.
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct RouteDto {
    /// The system-generated identifier. The schema marks it read-only, so a value
    /// carried in a request body is ignored.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// The upstream this route belongs to.
    pub upstream_id: String,
    /// The match rule.
    #[serde(default)]
    pub r#match: Option<crate::domain::route::RouteMatch>,
    /// Ordering key participating in the conflict rule.
    #[serde(default)]
    pub priority: i64,
    /// Whether the route participates in matching.
    #[serde(default = "crate::domain::default_enabled")]
    pub enabled: bool,
    /// Rate-limit override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<crate::domain::upstream::RateLimit>,
    /// Ordered plugin bindings.
    #[serde(default)]
    pub plugins: Vec<crate::domain::plugin::PluginBinding>,
    /// Tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl RouteDto {
    /// Projects a stored route into its wire shape.
    #[must_use]
    pub fn from_entity(entity: &crate::domain::route::Route) -> Self {
        Self {
            id: entity.id.clone(),
            upstream_id: entity.upstream_id.clone(),
            r#match: entity.r#match.clone(),
            priority: entity.priority,
            enabled: entity.enabled,
            rate_limit: entity.rate_limit.clone(),
            plugins: entity.plugins.clone(),
            tags: entity.tags.clone(),
        }
    }

    /// Builds an entity from the wire shape, keeping the stored identity fields.
    #[must_use]
    pub fn into_entity(
        self,
        id: String,
        tenant_id: uuid::Uuid,
        upstream_id: String,
    ) -> crate::domain::route::Route {
        crate::domain::route::Route {
            id,
            tenant_id,
            upstream_id,
            r#match: self.r#match,
            priority: self.priority,
            enabled: self.enabled,
            rate_limit: self.rate_limit,
            plugins: self.plugins,
            tags: self.tags,
        }
    }
}

/// The body of `POST /plugins`.
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct PluginDto {
    /// The plugin's GTS instance identifier.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Human-readable name.
    #[serde(default)]
    pub name: String,
    /// What the plugin does. The wire name is the contract's `plugin_type`.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "plugin_type")]
    pub kind: Option<crate::domain::plugin::PluginKind>,
    /// Optional configuration schema.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Source content of a scripted plugin.
    #[serde(default)]
    pub source: String,
}

impl PluginDto {
    /// Projects a stored plugin into its wire shape.
    #[must_use]
    pub fn from_entity(entity: &crate::domain::plugin::Plugin) -> Self {
        Self {
            id: entity.id.clone(),
            name: entity.name.clone(),
            kind: entity.kind,
            config_schema: entity.config_schema.clone(),
            source: entity.source.clone(),
        }
    }

    /// Builds an entity from the wire shape.
    #[must_use]
    pub fn into_entity(self, id: String, tenant_id: uuid::Uuid) -> crate::domain::plugin::Plugin {
        crate::domain::plugin::Plugin {
            id,
            tenant_id,
            name: self.name,
            kind: self.kind,
            config_schema: self.config_schema,
            source: self.source,
        }
    }
}

/// The envelope `GET /upstreams` answers with.
#[toolkit_macros::api_dto(response)]
pub struct UpstreamList {
    /// The matching upstreams, after `$filter`, `$orderby`, `$skip` and `$top`.
    ///
    /// The items are JSON because `$select` projects an arbitrary subset of fields.
    pub items: Vec<serde_json::Value>,
    /// The number of upstreams matched before paging was applied.
    pub total: u64,
}

/// The envelope `GET /routes` answers with.
#[toolkit_macros::api_dto(response)]
pub struct RouteList {
    /// The matching routes, after `$filter`, `$orderby`, `$skip` and `$top`.
    ///
    /// The items are JSON because `$select` projects an arbitrary subset of fields.
    pub items: Vec<serde_json::Value>,
    /// The number of routes matched before paging was applied.
    pub total: u64,
}

/// The envelope `GET /plugins` answers with.
#[toolkit_macros::api_dto(response)]
pub struct PluginList {
    /// The matching plugins, after `$filter`, `$orderby`, `$skip` and `$top`.
    ///
    /// The items are JSON because `$select` projects an arbitrary subset of fields.
    pub items: Vec<serde_json::Value>,
    /// The number of plugins matched before paging was applied.
    pub total: u64,
}
