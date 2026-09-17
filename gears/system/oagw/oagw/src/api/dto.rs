//! REST DTOs for the OAGW management API.
//!
//! Resources are addressed by anonymous GTS identifiers on the wire
//! (`gts.cf.core.oagw.{kind}.v1~{uuid}` — DESIGN §3.3); the DTOs below map
//! stored entities into that form while reusing the domain configuration
//! models for `config`-shaped bodies.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::ids;
use crate::domain::model::{
    RouteConfig, StoredPlugin, StoredRoute, StoredUpstream, UpstreamConfig,
};

/// Upstream create/replace body — the upstream configuration schema
/// (`upstream.v1.schema.json`).
pub type UpstreamRequest = UpstreamConfig;

/// Upstream representation.
#[derive(Debug, Clone, Serialize)]
pub struct UpstreamDto {
    pub id: String,
    pub tenant_id: Uuid,
    pub alias: String,
    pub alias_derived: bool,
    pub config: UpstreamConfig,
    pub created_at: u64,
    pub updated_at: u64,
}

impl UpstreamDto {
    #[must_use]
    pub fn from_stored(value: &StoredUpstream) -> Self {
        Self {
            id: ids::upstream_gts_id(value.id),
            tenant_id: value.tenant_id,
            alias: value.alias.clone(),
            alias_derived: value.alias_derived,
            config: value.config.clone(),
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

/// Route create/replace body — the route configuration schema
/// (`route.v1.schema.json`).
pub type RouteRequest = RouteConfig;

/// Route representation.
#[derive(Debug, Clone, Serialize)]
pub struct RouteDto {
    pub id: String,
    pub tenant_id: Uuid,
    pub upstream_id: String,
    pub config: RouteConfig,
    pub created_at: u64,
    pub updated_at: u64,
}

impl RouteDto {
    #[must_use]
    pub fn from_stored(value: &StoredRoute) -> Self {
        Self {
            id: ids::route_gts_id(value.id),
            tenant_id: value.tenant_id,
            upstream_id: ids::upstream_gts_id(value.upstream_id),
            config: value.config.clone(),
            created_at: value.created_at,
            updated_at: value.updated_at,
        }
    }
}

/// Plugin create body.
#[derive(Debug, Clone, Deserialize)]
pub struct CreatePluginRequest {
    /// Optional human-readable name.
    #[serde(default)]
    pub name: Option<String>,
    /// Plugin GTS identifier whose kind classifies the plugin
    /// (`gts.cf.core.oagw.{auth,guard,transform}_plugin.v1~...`).
    #[serde(rename = "type")]
    pub plugin_type: String,
    /// Starlark source; `source_code` is accepted as an alias of `source`.
    #[serde(default, alias = "source")]
    pub source_code: String,
    /// Optional JSON schema describing accepted config.
    #[serde(default)]
    pub config_schema: serde_json::Value,
}

/// Plugin representation.
#[derive(Debug, Clone, Serialize)]
pub struct PluginDto {
    pub id: String,
    pub tenant_id: Uuid,
    pub name: String,
    pub kind: &'static str,
    pub gts_id: String,
    pub source_code: String,
    pub config_schema: serde_json::Value,
    pub created_at: u64,
}

impl PluginDto {
    #[must_use]
    pub fn from_stored(value: &StoredPlugin) -> Self {
        Self {
            id: value.gts_id.clone(),
            tenant_id: value.tenant_id,
            name: value.name.clone(),
            kind: value.kind.as_str(),
            gts_id: value.gts_id.clone(),
            source_code: value.source_code.clone(),
            config_schema: value.config_schema.clone(),
            created_at: value.created_at,
        }
    }
}

/// Starlark source for a plugin (`GET /plugins/{id}/source`).
#[derive(Debug, Clone, Serialize)]
pub struct PluginSourceDto {
    pub id: String,
    pub source_code: String,
}

/// Generic paged/list envelope used by all list endpoints.
#[derive(Debug, Clone, Serialize)]
pub struct ListResponse<T>
where
    T: Serialize,
{
    pub items: Vec<T>,
}

impl<T: Serialize> ListResponse<T> {
    #[must_use]
    pub fn new(items: Vec<T>) -> Self {
        Self { items }
    }
}
