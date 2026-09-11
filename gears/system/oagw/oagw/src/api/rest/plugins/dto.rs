//! Request/response DTOs for the Plugin Management REST API
//! (`cpt-cf-oagw-feature-plugin-management`, 2.4).

use serde::Deserialize;
use uuid::Uuid;

use crate::model::plugin::{Plugin, PluginType};

/// `POST /oagw/v1/plugins` request body shape, for `OpenAPI` schema
/// registration only -- the handler parses the raw JSON body itself
/// (`api::rest::plugins::handlers::parse_create_request`) so that a
/// malformed body renders the gear's own RFC 9457 `ValidationError` rather
/// than axum's default JSON-extraction rejection. Its fields are therefore
/// never read directly by this crate (only by the generated `Deserialize`
/// impl, for round-trip tests and `OpenAPI` tooling), hence the dead-code
/// waiver.
#[derive(Debug, Clone)]
#[allow(dead_code)]
#[toolkit_macros::api_dto(request)]
pub struct CreatePluginRequest {
    pub plugin_type: PluginType,
    pub name: String,
    pub description: Option<String>,
    pub config_schema: Option<serde_json::Value>,
    pub source_code: String,
}

/// `POST`/`GET` plugin resource body. `id` is always the bare UUID
/// (`inst-plugin-create-issue-gts`); `source_code` is deliberately excluded
/// (see `cpt-cf-oagw-flow-plugin-get-source`).
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PluginResponse {
    pub id: Uuid,
    pub plugin_type: PluginType,
    pub name: String,
    pub description: Option<String>,
    pub config_schema: Option<serde_json::Value>,
    pub gc_eligible_at: Option<String>,
    pub last_used_at: Option<String>,
}

impl From<&Plugin> for PluginResponse {
    fn from(plugin: &Plugin) -> Self {
        Self {
            id: plugin.id.unwrap_or_default(),
            plugin_type: plugin.plugin_type,
            name: plugin.name.clone(),
            description: plugin.description.clone(),
            config_schema: plugin.config_schema.clone(),
            gc_eligible_at: plugin.gc_eligible_at.clone(),
            last_used_at: plugin.last_used_at.clone(),
        }
    }
}

/// `GET /oagw/v1/plugins` OData-lite query parameters
/// (`inst-plugin-list-odata`): `$orderby` is not offered, matching
/// `DESIGN.md`'s Plugin List Query Parameters table.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PluginListQuery {
    #[serde(rename = "$filter", default)]
    pub filter: Option<String>,
    #[serde(rename = "$select", default)]
    pub select: Option<String>,
    #[serde(rename = "$top", default)]
    pub top: Option<String>,
    #[serde(rename = "$skip", default)]
    pub skip: Option<String>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn create_plugin_request_deserializes_from_a_well_formed_body() {
        let json = serde_json::json!({
            "plugin_type": "guard",
            "name": "my-guard",
            "source_code": "def guard(req): return req",
        });
        let request: CreatePluginRequest = serde_json::from_value(json).unwrap();
        assert_eq!(request.plugin_type, PluginType::Guard);
        assert_eq!(request.name, "my-guard");
    }

    #[test]
    fn plugin_response_from_plugin_excludes_source_code() {
        let plugin = Plugin {
            id: Some(Uuid::new_v4()),
            tenant_id: Some(Uuid::new_v4()),
            plugin_type: PluginType::Transform,
            name: "req-id".to_owned(),
            description: Some("adds a request id".to_owned()),
            config_schema: None,
            source_code: "def transform_request(req): return req".to_owned(),
            last_used_at: None,
            gc_eligible_at: Some("2026-01-01T00:00:00Z".to_owned()),
        };
        let response = PluginResponse::from(&plugin);
        assert_eq!(response.id, plugin.id.unwrap());
        assert_eq!(response.name, "req-id");
        let serialized = serde_json::to_value(&response).unwrap();
        assert!(serialized.get("source_code").is_none());
    }
}
