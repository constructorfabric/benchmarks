//! Wire DTOs for the OAGW management API.
//!
//! Top-level fields (identifiers, alias, protocol, `enabled`, tags) are
//! typed so the `OpenAPI` document is precise. Nested configuration blocks
//! (`auth`, `headers`, `rate_limit`, `cors`, `plugins`, `match`) cross the
//! API as free-form JSON objects: their shape is owned by
//! `docs/schemas/*.schema.json` and enforced by serde on the way in, so
//! restating it in utoipa would only let the two drift.

use serde_json::Value;
use uuid::Uuid;

use crate::api::error::uuid_from;
use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, EndpointScheme, Plugin, PluginKind, ServerConfig, Upstream};

/// Endpoint of an upstream.
#[toolkit_macros::api_dto(request, response)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointDto {
    /// Transport scheme: `http`, `https`, `wss`, `wt` or `grpc`. Defaults to
    /// `https`.
    #[serde(default = "default_scheme")]
    pub scheme: String,
    /// Upstream hostname or IP.
    pub host: String,
    /// TCP port. Defaults to 443.
    #[serde(default = "default_port")]
    pub port: u16,
}

/// Scheme used when the endpoint omits one.
fn default_scheme() -> String {
    "https".to_owned()
}

/// Port used when the endpoint omits one.
fn default_port() -> u16 {
    443
}

impl EndpointDto {
    /// Project the domain endpoint onto the wire form.
    #[must_use]
    pub fn from_endpoint(endpoint: &Endpoint) -> Self {
        Self {
            scheme: endpoint.scheme.as_str().to_owned(),
            host: endpoint.host.clone(),
            port: endpoint.port,
        }
    }

    /// Project the wire endpoint onto the domain model.
    ///
    /// # Errors
    /// Returns a validation error when the scheme is unknown.
    pub fn into_endpoint(self) -> Result<Endpoint, DomainError> {
        let scheme = EndpointScheme::parse(&self.scheme).ok_or_else(|| {
            DomainError::validation(format!("unknown endpoint scheme '{}'", self.scheme))
        })?;
        Ok(Endpoint {
            scheme,
            host: self.host,
            port: self.port,
        })
    }
}

/// Endpoint pool of an upstream.
#[toolkit_macros::api_dto(request, response)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerDto {
    /// At least one endpoint.
    #[serde(default)]
    pub endpoints: Vec<EndpointDto>,
}

/// An upstream as it is stored and served.
#[toolkit_macros::api_dto(request, response)]
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamDto {
    /// GTS identifier `gts.cf.core.oagw.upstream.v1~{uuid}`.
    #[serde(default)]
    pub id: String,
    /// Owning tenant; ignored on create, taken from the security context.
    #[serde(default)]
    pub tenant_id: Uuid,
    /// Routing identifier; unique per tenant. May be omitted to derive it.
    #[serde(default)]
    pub alias: String,
    /// Protocol identifier.
    pub protocol: String,
    /// Whether the upstream accepts traffic.
    #[serde(default = "crate::domain::model::default_enabled")]
    pub enabled: bool,
    /// Endpoint pool.
    pub server: ServerDto,
    /// Auth plugin binding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<Value>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<Value>,
    /// Token-bucket rate limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<Value>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<Value>,
    /// Plugin chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<Value>,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl UpstreamDto {
    /// Project the domain model onto the wire document.
    #[must_use]
    pub fn from_model(upstream: &Upstream) -> Self {
        Self {
            id: format!(
                "{}{}",
                crate::domain::model::gts::UPSTREAM_TYPE,
                upstream.id
            ),
            tenant_id: upstream.tenant_id,
            alias: upstream.alias.clone(),
            protocol: upstream.protocol.clone(),
            enabled: upstream.enabled,
            server: ServerDto {
                endpoints: upstream
                    .server
                    .endpoints
                    .iter()
                    .map(EndpointDto::from_endpoint)
                    .collect(),
            },
            auth: to_value_opt(upstream.auth.as_ref()),
            headers: to_value_opt(upstream.headers.as_ref()),
            rate_limit: to_value_opt(upstream.rate_limit.as_ref()),
            cors: to_value_opt(upstream.cors.as_ref()),
            plugins: to_value_opt(upstream.plugins.as_ref()),
            tags: upstream.tags.clone(),
        }
    }

    /// Project the wire document onto the domain model.
    ///
    /// # Errors
    /// Returns a validation error when an endpoint or the auth binding is
    /// malformed.
    pub fn into_model(self) -> Result<Upstream, DomainError> {
        let mut endpoints = Vec::with_capacity(self.server.endpoints.len());
        for endpoint in &self.server.endpoints {
            endpoints.push(endpoint.clone().into_endpoint()?);
        }
        Ok(Upstream {
            id: uuid_from(&self.id).unwrap_or_else(uuid::Uuid::new_v4),
            tenant_id: self.tenant_id,
            alias: self.alias.clone(),
            protocol: self.protocol.clone(),
            enabled: self.enabled,
            server: ServerConfig { endpoints },
            auth: from_value_opt(self.auth.clone(), "auth binding")?,
            headers: from_value_opt(self.headers.clone(), "headers")?,
            rate_limit: from_value_opt(self.rate_limit.clone(), "rate_limit")?,
            cors: from_value_opt(self.cors.clone(), "cors")?,
            plugins: from_value_opt(self.plugins.clone(), "plugins")?,
            tags: self.tags.clone(),
        })
    }
}

/// A route owned by an upstream.
#[toolkit_macros::api_dto(request, response)]
#[derive(Debug, Clone, PartialEq)]
pub struct RouteDto {
    /// GTS identifier `gts.cf.core.oagw.route.v1~{uuid}`.
    #[serde(default)]
    pub id: String,
    /// Owning tenant.
    #[serde(default)]
    pub tenant_id: Uuid,
    /// Owning upstream GTS identifier.
    pub upstream_id: String,
    /// Protocol-scoped match rule.
    #[serde(rename = "match")]
    pub match_rule: Value,
    /// Match priority; higher wins before prefix length.
    #[serde(default)]
    pub priority: i64,
    /// Whether the route participates in matching.
    #[serde(default = "crate::domain::model::default_enabled")]
    pub enabled: bool,
    /// Rate-limit override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<Value>,
    /// CORS override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<Value>,
    /// Plugin chain appended after the upstream's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<Value>,
    /// Discovery tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl RouteDto {
    /// Project the domain model onto the wire document.
    #[must_use]
    pub fn from_model(route: &crate::domain::model::Route) -> Self {
        Self {
            id: format!("{}{}", crate::domain::model::gts::ROUTE_TYPE, route.id),
            tenant_id: route.tenant_id,
            upstream_id: format!(
                "{}{}",
                crate::domain::model::gts::UPSTREAM_TYPE,
                route.upstream_id
            ),
            match_rule: serde_json::to_value(&route.match_rule).unwrap_or(serde_json::Value::Null),
            priority: route.priority,
            enabled: route.enabled,
            rate_limit: to_value_opt(route.rate_limit.as_ref()),
            cors: to_value_opt(route.cors.as_ref()),
            plugins: to_value_opt(route.plugins.as_ref()),
            tags: route.tags.clone(),
        }
    }

    /// Project the wire document onto the domain model.
    ///
    /// # Errors
    /// Returns a validation error when the upstream reference or the match
    /// rule is malformed.
    pub fn into_model(self, tenant_id: Uuid) -> Result<crate::domain::model::Route, DomainError> {
        let upstream_id = crate::api::gts_id::parse_upstream_id(&self.upstream_id)
            .map_err(|err| DomainError::validation(err.to_string()))?;
        Ok(crate::domain::model::Route {
            id: uuid_from(&self.id).unwrap_or_else(uuid::Uuid::new_v4),
            tenant_id,
            upstream_id,
            match_rule: from_value_opt(Some(self.match_rule.clone()), "match")?.unwrap_or_default(),
            priority: self.priority,
            enabled: self.enabled,
            rate_limit: from_value_opt(self.rate_limit.clone(), "rate_limit")?,
            cors: from_value_opt(self.cors.clone(), "cors")?,
            plugins: from_value_opt(self.plugins.clone(), "plugins")?,
            tags: self.tags.clone(),
        })
    }
}

/// A tenant-defined plugin.
#[toolkit_macros::api_dto(request, response)]
#[derive(Debug, Clone, PartialEq)]
pub struct PluginDto {
    /// GTS instance identifier of the plugin.
    #[serde(default)]
    pub id: String,
    /// Owning tenant.
    #[serde(default)]
    pub tenant_id: Uuid,
    /// Plugin kind: `auth`, `guard` or `transform`.
    pub plugin_type: String,
    /// Tenant-unique name.
    pub name: String,
    /// Human-readable description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// JSON Schema the plugin config must satisfy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<Value>,
    /// Plugin source text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_code: Option<String>,
    /// Phases the plugin supports.
    #[serde(default)]
    pub phases: Vec<String>,
}

impl PluginDto {
    /// Project the domain model onto the wire document.
    #[must_use]
    pub fn from_model(plugin: &Plugin, _tenant: Uuid) -> Self {
        Self {
            id: format!("{}{}", plugin.kind.base_type(), plugin.id),
            tenant_id: plugin.tenant_id,
            plugin_type: plugin.kind.wire_name().to_owned(),
            name: plugin.name.clone(),
            description: plugin.description.clone(),
            config_schema: plugin.config_schema.clone(),
            source_code: plugin.source_code.clone(),
            phases: plugin.phases.clone(),
        }
    }

    /// Project the wire document onto the domain model.
    ///
    /// # Errors
    /// Returns a validation error when the plugin kind is unknown.
    pub fn into_model(self, tenant_id: Uuid) -> Result<Plugin, DomainError> {
        let plugin_type = PluginKind::parse(&self.plugin_type).ok_or_else(|| {
            DomainError::validation(format!("unknown plugin kind '{}'", self.plugin_type))
        })?;
        Ok(Plugin {
            id: uuid_from(&self.id).unwrap_or_else(uuid::Uuid::new_v4),
            tenant_id,
            kind: plugin_type,
            name: self.name.clone(),
            description: self.description.clone(),
            config_schema: self.config_schema.clone(),
            source_code: self.source_code.clone(),
            phases: self.phases.clone(),
        })
    }
}

/// The source text of a plugin, served by `GET /plugins/{id}/source`.
#[toolkit_macros::api_dto(response)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSourceDto {
    /// Plugin GTS instance identifier.
    pub id: String,
    /// Plugin name.
    pub name: String,
    /// Plugin kind.
    pub plugin_type: String,
    /// Stored source text.
    pub source_code: String,
}

impl PluginSourceDto {
    /// Project a stored plugin onto the source document.
    #[must_use]
    pub fn from_plugin(plugin: &Plugin, id: String) -> Self {
        Self {
            id,
            name: plugin.name.clone(),
            plugin_type: plugin.kind.wire_name().to_owned(),
            source_code: plugin.source_code.clone().unwrap_or_default(),
        }
    }
}

/// Serialize an optional domain block, mapping `None` to `None`.
fn to_value_opt<T: serde::Serialize>(value: Option<&T>) -> Option<Value> {
    value.and_then(|inner| serde_json::to_value(inner).ok())
}

/// Deserialize an optional wire block, reporting the field on failure.
///
/// # Errors
/// Returns a validation error naming `field` when the JSON does not match the
/// schema-owned shape.
fn from_value_opt<T: serde::de::DeserializeOwned>(
    value: Option<Value>,
    field: &str,
) -> Result<Option<T>, DomainError> {
    value
        .map(|inner| {
            serde_json::from_value(inner)
                .map_err(|err| DomainError::validation(format!("invalid {field}: {err}")))
        })
        .transpose()
}
