//! REST representations that are not simply the domain aggregate.

use serde::Serialize;
use uuid::Uuid;

use crate::domain::model::{PluginDef, PluginKind, PluginPhase};

/// Envelope returned by every list endpoint.
///
/// `total` is the number of matches *before* `$top`/`$skip`, so a client can
/// page without a second request.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct ListResponse {
    #[schema(value_type = Vec<Object>)]
    pub items: Vec<serde_json::Value>,
    pub total: usize,
}

impl ListResponse {
    #[must_use]
    pub fn new(items: Vec<serde_json::Value>, total: usize) -> Self {
        Self { items, total }
    }
}

/// A custom plugin definition, without its source.
///
/// The source is served separately by `GET /oagw/v1/plugins/{id}/source` so a
/// listing never carries potentially large script bodies.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PluginResponse {
    pub id: Uuid,
    pub tenant_id: Uuid,
    /// Anonymous GTS identifier, e.g. `gts.cf.core.oagw.guard_plugin.v1~{uuid}`.
    pub gts_id: String,
    pub plugin_type: PluginKind,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub phases: Vec<PluginPhase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub config_schema: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gc_eligible_at: Option<u64>,
}

impl From<&PluginDef> for PluginResponse {
    fn from(plugin: &PluginDef) -> Self {
        Self {
            id: plugin.id,
            tenant_id: plugin.tenant_id,
            gts_id: plugin.gts_id(),
            plugin_type: plugin.plugin_type,
            name: plugin.name.clone(),
            description: plugin.description.clone(),
            phases: plugin.phases.clone(),
            config_schema: plugin.config_schema.clone(),
            last_used_at: plugin.last_used_at,
            gc_eligible_at: plugin.gc_eligible_at,
        }
    }
}

/// Body of `GET /oagw/v1/plugins/{id}/source`.
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct PluginSourceResponse {
    pub id: Uuid,
    pub gts_id: String,
    pub name: String,
    pub source_code: String,
}

impl From<&PluginDef> for PluginSourceResponse {
    fn from(plugin: &PluginDef) -> Self {
        Self {
            id: plugin.id,
            gts_id: plugin.gts_id(),
            name: plugin.name.clone(),
            source_code: plugin.source_code.clone(),
        }
    }
}

impl toolkit::api::api_dto::ResponseApiDto for ListResponse {}
impl toolkit::api::api_dto::ResponseApiDto for PluginResponse {}
impl toolkit::api::api_dto::ResponseApiDto for PluginSourceResponse {}
