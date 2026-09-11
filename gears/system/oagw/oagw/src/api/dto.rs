// Created: 2026-09-01 by Constructor Tech
//! REST DTOs.
//!
//! `docs/schemas/upstream.v1.schema.json` and `docs/schemas/route.v1.schema.json`
//! are the wire shapes. The domain types in `crate::domain::model` are those
//! same shapes, so the DTOs here are thin aliases rather than a second
//! mapping layer — one source of truth, and no chance for the REST surface
//! and the store to drift.

use crate::domain::model;

/// An upstream resource as it appears on the wire.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct UpstreamDto {
    /// System-generated identifier; absent on a create payload.
    #[serde(default)]
    pub id: String,
    /// Calling tenant; always assigned by the gateway.
    #[serde(default)]
    pub tenant_id: String,
    /// Routing identifier, derived or explicit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Whether the upstream accepts traffic.
    #[serde(default = "true_bool")]
    pub enabled: bool,
    /// Flat tags for discovery.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tags: Vec<String>,
    /// Endpoints.
    pub server: model::ServerConfig,
    /// Outbound protocol.
    pub protocol: String,
    /// Credential injection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<model::AuthConfig>,
    /// Header transformation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<model::HeadersConfig>,
    /// Bound plugins.
    #[serde(default)]
    pub plugins: model::PluginSet,
    /// Rate limiting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<model::RateLimit>,
    /// Cross-origin policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<model::Cors>,
}

/// `true` — the default for a boolean the schema gives a `true` default.
#[must_use]
pub fn true_bool() -> bool {
    true
}

impl UpstreamDto {
    /// A stored upstream rendered for the wire.
    #[must_use]
    pub fn from_domain(value: &model::Upstream) -> Self {
        Self {
            id: value.id.clone(),
            tenant_id: value.tenant_id.clone(),
            alias: Some(value.alias.clone()),
            enabled: value.enabled,
            tags: value.tags.clone(),
            server: value.server.clone(),
            protocol: value.protocol.clone(),
            auth: value.auth.clone(),
            headers: value.headers.clone(),
            plugins: value.plugins.clone(),
            rate_limit: value.rate_limit.clone(),
            cors: value.cors.clone(),
        }
    }

    /// The create/replace payload as the domain sees it.
    #[must_use]
    pub fn into_domain(self, id: &str, tenant_id: &str) -> model::Upstream {
        model::Upstream {
            id: id.to_owned(),
            tenant_id: tenant_id.to_owned(),
            alias: self.alias.unwrap_or_default(),
            enabled: self.enabled,
            tags: self.tags,
            server: self.server,
            protocol: self.protocol,
            auth: self.auth,
            headers: self.headers,
            plugins: self.plugins,
            rate_limit: self.rate_limit,
            cors: self.cors,
        }
    }
}

/// A route resource.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct RouteDto {
    /// System-generated identifier; absent on a create payload.
    #[serde(default)]
    pub id: String,
    /// Calling tenant; always assigned by the gateway.
    #[serde(default)]
    pub tenant_id: String,
    /// The upstream this route belongs to.
    pub upstream_id: String,
    /// Match priority; higher wins on equal path length.
    #[serde(default)]
    pub priority: i64,
    /// Whether the route participates in matching.
    #[serde(default = "true_bool")]
    pub enabled: bool,
    /// Flat tags for discovery.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Match rules.
    #[serde(rename = "match")]
    pub matcher: model::RouteMatch,
    /// Bound plugins.
    #[serde(default)]
    pub plugins: model::PluginSet,
    /// Route-level rate limit override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<model::RateLimit>,
    /// Route-level CORS override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<model::Cors>,
}

impl RouteDto {
    /// A stored route rendered for the wire.
    #[must_use]
    pub fn from_domain(value: &model::Route) -> Self {
        Self {
            id: value.id.clone(),
            tenant_id: value.tenant_id.clone(),
            upstream_id: value.upstream_id.clone(),
            priority: value.priority,
            enabled: value.enabled,
            tags: value.tags.clone(),
            matcher: value.matcher.clone(),
            plugins: value.plugins.clone(),
            rate_limit: value.rate_limit.clone(),
            cors: value.cors.clone(),
        }
    }

    /// The payload as the domain sees it.
    #[must_use]
    pub fn into_domain(self, id: &str, tenant_id: &str) -> model::Route {
        model::Route {
            id: id.to_owned(),
            tenant_id: tenant_id.to_owned(),
            upstream_id: self.upstream_id,
            priority: self.priority,
            tags: self.tags,
            matcher: self.matcher,
            plugins: self.plugins,
            rate_limit: self.rate_limit,
            enabled: self.enabled,
            cors: self.cors,
        }
    }
}

/// A custom plugin resource.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request, response)]
pub struct PluginDto {
    /// System-generated identifier; absent on a create payload.
    #[serde(default)]
    pub id: String,
    /// Calling tenant; always assigned by the gateway.
    #[serde(default)]
    pub tenant_id: String,
    /// Which of the three plugin types this is.
    pub plugin_type: String,
    /// Human-readable name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The plugin source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Plugin configuration.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub config: std::collections::BTreeMap<String, serde_json::Value>,
}

impl PluginDto {
    /// A stored plugin rendered for the wire.
    #[must_use]
    pub fn from_domain(value: &model::Plugin) -> Self {
        Self {
            id: value.id.clone(),
            tenant_id: value.tenant_id.clone(),
            plugin_type: value.plugin_type.clone(),
            name: value.name.clone(),
            source: value.source.clone(),
            config: value.config.clone(),
        }
    }

    /// The payload as the domain sees it.
    #[must_use]
    pub fn into_domain(self, id: &str, tenant_id: &str) -> model::Plugin {
        model::Plugin {
            id: id.to_owned(),
            tenant_id: tenant_id.to_owned(),
            plugin_type: self.plugin_type,
            name: self.name,
            source: self.source,
            config: self.config,
        }
    }
}

/// Which resources reference a plugin.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(response)]
pub struct PluginSource {
    /// The plugin.
    pub plugin_id: String,
    /// Whether the plugin is named (built-in) or custom.
    pub origin: String,
    /// Upstreams that bind it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upstreams: Vec<String>,
    /// Routes that bind it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<String>,
    /// The plugin source, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn upstream() -> model::Upstream {
        model::Upstream {
            id: "u1".to_owned(),
            tenant_id: "t1".to_owned(),
            alias: "api.example.com".to_owned(),
            enabled: true,
            tags: vec!["llm".to_owned()],
            server: model::ServerConfig {
                endpoints: vec![model::Endpoint {
                    scheme: "https".to_owned(),
                    host: "api.example.com".to_owned(),
                    port: 443,
                }],
            },
            protocol: model::protocol::HTTP.to_owned(),
            auth: None,
            headers: None,
            plugins: model::PluginSet::default(),
            rate_limit: None,
            cors: None,
        }
    }

    #[test]
    fn an_upstream_round_trips() {
        let stored = upstream();
        let dto = UpstreamDto::from_domain(&stored);
        let json = serde_json::to_value(&dto).expect("json");
        assert_eq!(json["id"], "u1");
        assert_eq!(json["alias"], "api.example.com");
        assert_eq!(json["server"]["endpoints"][0]["host"], "api.example.com");
        let back = serde_json::from_value::<UpstreamDto>(json).expect("dto");
        let restored = back.into_domain("u1", "t1");
        assert_eq!(restored.alias, stored.alias);
        assert_eq!(restored.tags, stored.tags);
    }

    #[test]
    fn a_route_round_trips() {
        let stored = model::Route {
            id: "r1".to_owned(),
            tenant_id: "t1".to_owned(),
            upstream_id: "u1".to_owned(),
            priority: 5,
            tags: Vec::new(),
            matcher: model::RouteMatch {
                http: Some(model::HttpMatch {
                    methods: vec!["GET".to_owned()],
                    path: "/v1/x".to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: model::PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: model::PluginSet::default(),
            rate_limit: None,
            enabled: true,
            cors: None,
        };
        let json = serde_json::to_value(RouteDto::from_domain(&stored)).expect("json");
        assert_eq!(json["match"]["http"]["path"], "/v1/x");
        assert_eq!(json["enabled"], true);
        let back = serde_json::from_value::<RouteDto>(json).expect("dto");
        assert_eq!(
            back.into_domain("r1", "t1").matcher.http.unwrap().path,
            "/v1/x"
        );
    }

    #[test]
    fn a_plugin_round_trips() {
        let stored = model::Plugin {
            id: "p1".to_owned(),
            tenant_id: "t1".to_owned(),
            plugin_type: "guard".to_owned(),
            name: Some("guard".to_owned()),
            source: Some("def run(ctx): pass".to_owned()),
            config: std::collections::BTreeMap::new(),
        };
        let json = serde_json::to_value(PluginDto::from_domain(&stored)).expect("json");
        assert_eq!(json["plugin_type"], "guard");
        let back = serde_json::from_value::<PluginDto>(json).expect("dto");
        assert_eq!(back.into_domain("p1", "t1").name.as_deref(), Some("guard"));
    }
}
