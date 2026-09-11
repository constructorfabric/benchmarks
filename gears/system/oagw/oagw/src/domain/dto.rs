//! Internal write models — the deserialization targets for the management
//! API, shaped exactly like `docs/schemas/*.schema.json`.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, MatchConfig, PluginKind, PluginsConfig, RateLimitConfig,
    ServerConfigInput,
};

/// `POST` / `PUT` body for `/oagw/v1/upstreams`.
///
/// Server-managed members (`id`, `tenant_id`, `created_at`, `updated_at`) are
/// accepted and ignored so a `GET` body can be edited and `PUT` back verbatim,
/// while every other unknown member is rejected per the schema's
/// `additionalProperties: false`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamWriteInput {
    // -- read-only members, accepted and ignored ---------------------------
    #[serde(default, rename = "id")]
    pub _id: Option<Value>,
    #[serde(default, rename = "tenant_id")]
    pub _tenant_id: Option<Value>,
    #[serde(default, rename = "created_at")]
    pub _created_at: Option<Value>,
    #[serde(default, rename = "updated_at")]
    pub _updated_at: Option<Value>,

    // -- writable members --------------------------------------------------
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    pub server: ServerConfigInput,
    pub protocol: String,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub headers: Option<HeadersConfig>,
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

/// `POST` / `PUT` body for `/oagw/v1/routes`.
///
/// `upstream_id` is immutable, so it is ignored on replace.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteWriteInput {
    #[serde(default, rename = "id")]
    pub _id: Option<Value>,
    #[serde(default, rename = "tenant_id")]
    pub _tenant_id: Option<Value>,
    #[serde(default, rename = "match_type")]
    pub _match_type: Option<Value>,
    #[serde(default, rename = "created_at")]
    pub _created_at: Option<Value>,
    #[serde(default, rename = "updated_at")]
    pub _updated_at: Option<Value>,

    #[serde(default)]
    pub upstream_id: Option<String>,
    #[serde(rename = "match")]
    pub match_config: MatchConfig,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub priority: Option<i32>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

/// `POST` body for `/oagw/v1/plugins` (custom Starlark plugins).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginWriteInput {
    #[serde(default, rename = "id")]
    pub _id: Option<Value>,
    #[serde(default, rename = "tenant_id")]
    pub _tenant_id: Option<Value>,
    #[serde(default, rename = "created_at")]
    pub _created_at: Option<Value>,

    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub plugin_type: PluginKind,
    #[serde(default)]
    pub phases: Option<Vec<String>>,
    #[serde(default)]
    pub config_schema: Option<Map<String, Value>>,
    pub source_code: String,
}

/// Parsed OData-ish list query (`$filter`, `$select`, `$orderby`, `$top`, `$skip`).
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    pub filter: Option<String>,
    pub select: Option<Vec<String>>,
    pub orderby: Option<Vec<(String, bool)>>,
    pub top: Option<usize>,
    pub skip: Option<usize>,
}

/// A resolved plugin chain entry: binding config plus the resolved reference.
#[derive(Debug, Clone)]
pub struct ResolvedBinding {
    pub plugin_ref: String,
    pub plugin_uuid: Option<Uuid>,
    pub config: BTreeMap<String, Value>,
}

/// The merged, ready-to-execute view of an upstream + matched route.
#[derive(Debug, Clone)]
pub struct EffectiveConfig {
    pub auth: Option<AuthConfig>,
    pub headers: HeadersConfig,
    pub rate_limit: Option<RateLimitConfig>,
    pub cors: Option<CorsConfig>,
    pub plugins: Vec<ResolvedBinding>,
    pub tags: Vec<String>,
}

/// Extra rate-limit ceilings contributed by ancestors with `sharing: enforce`.
pub type EnforcedRateLimits = Vec<RateLimitConfig>;

impl Default for EffectiveConfig {
    fn default() -> Self {
        Self {
            auth: None,
            headers: HeadersConfig::default(),
            rate_limit: None,
            cors: None,
            plugins: Vec::new(),
            tags: Vec::new(),
        }
    }
}

/// Convenience: the plugin binding list of a [`PluginsConfig`], resolved.
#[must_use]
pub fn bindings_of(cfg: &PluginsConfig) -> Vec<ResolvedBinding> {
    cfg.items
        .iter()
        .map(|b| ResolvedBinding {
            plugin_ref: b.plugin_ref.clone(),
            plugin_uuid: b.plugin_uuid,
            config: b.config.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_input_accepts_read_only_echo() {
        let json = r#"{
            "id": "7c9e6679-7425-40de-944b-e07fc1f90ae7",
            "tenant_id": "00000000-0000-0000-0000-000000000001",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z",
            "server": {"endpoints": [{"scheme": "https", "host": "api.openai.com", "port": 443}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        }"#;
        let input: UpstreamWriteInput = serde_json::from_str(json).expect("round-trippable body");
        assert_eq!(input.server.endpoints.len(), 1);
    }

    #[test]
    fn upstream_input_rejects_unknown_members() {
        let json = r#"{
            "server": {"endpoints": [{"scheme": "https", "host": "a.example.com"}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "bogus": 1
        }"#;
        assert!(serde_json::from_str::<UpstreamWriteInput>(json).is_err());
    }

    #[test]
    fn route_input_parses_the_http_match() {
        let json = r#"{
            "upstream_id": "7c9e6679-7425-40de-944b-e07fc1f90ae7",
            "match": {"http": {"methods": ["GET"], "path": "/v1/models"}}
        }"#;
        let input: RouteWriteInput = serde_json::from_str(json).expect("route body");
        let http = input.match_config.http.expect("http match");
        assert_eq!(http.path, "/v1/models");
        assert_eq!(http.query_allowlist.len(), 0);
    }
}
