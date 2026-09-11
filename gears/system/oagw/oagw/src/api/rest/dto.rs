//! REST representations.
//!
//! The response shapes here are exactly the ones in `docs/schemas/`: the
//! upstream schema is `additionalProperties: false`, so a `GET` returns those
//! members and nothing else — no internal bookkeeping leaks into a document a
//! caller may validate or round-trip through `PUT`.

use serde_json::{Map, Value, json};

use crate::domain::gts_helpers::anonymous_id;
use crate::domain::model::{PluginRecord, Route, Upstream};

/// Insert `key` only when `value` is present, so an unset optional block is
/// absent rather than `null`.
fn insert_opt<T: serde::Serialize>(map: &mut Map<String, Value>, key: &str, value: Option<&T>) {
    if let Some(value) = value
        && let Ok(value) = serde_json::to_value(value)
    {
        map.insert(key.to_owned(), value);
    }
}

/// Render an upstream as its schema-conformant JSON view.
#[must_use]
pub fn upstream_view(upstream: &Upstream) -> Value {
    let mut map = Map::new();
    map.insert("id".to_owned(), json!(upstream.id));
    map.insert("enabled".to_owned(), json!(upstream.enabled));
    map.insert("alias".to_owned(), json!(upstream.alias));
    map.insert("tags".to_owned(), json!(upstream.tags));
    map.insert(
        "server".to_owned(),
        serde_json::to_value(&upstream.server).unwrap_or(Value::Null),
    );
    map.insert("protocol".to_owned(), json!(upstream.protocol.as_str()));
    insert_opt(&mut map, "auth", upstream.auth.as_ref());
    insert_opt(&mut map, "headers", upstream.headers.as_ref());
    insert_opt(&mut map, "plugins", upstream.plugins.as_ref());
    insert_opt(&mut map, "rate_limit", upstream.rate_limit.as_ref());
    insert_opt(&mut map, "cors", upstream.cors.as_ref());
    Value::Object(map)
}

/// Render a route as its schema-conformant JSON view.
#[must_use]
pub fn route_view(route: &Route) -> Value {
    let mut map = Map::new();
    map.insert("id".to_owned(), json!(route.id));
    map.insert("upstream_id".to_owned(), json!(route.upstream_id));
    map.insert("enabled".to_owned(), json!(route.enabled));
    map.insert("priority".to_owned(), json!(route.priority));
    map.insert("match_type".to_owned(), json!(route.match_type()));
    map.insert("tags".to_owned(), json!(route.tags));
    map.insert(
        "match".to_owned(),
        serde_json::to_value(&route.match_config).unwrap_or(Value::Null),
    );
    insert_opt(&mut map, "plugins", route.plugins.as_ref());
    insert_opt(&mut map, "rate_limit", route.rate_limit.as_ref());
    insert_opt(&mut map, "cors", route.cors.as_ref());
    Value::Object(map)
}

/// Render a custom plugin.
///
/// The identifier is the anonymous GTS form (`…{type}_plugin.v1~{uuid}`),
/// which is what a `plugins.items[].plugin_ref` binding has to name.
#[must_use]
pub fn plugin_view(plugin: &PluginRecord) -> Value {
    let mut map = Map::new();
    map.insert(
        "id".to_owned(),
        json!(anonymous_id(plugin.plugin_type.base_type(), plugin.id)),
    );
    map.insert("uuid".to_owned(), json!(plugin.id));
    map.insert("tenant_id".to_owned(), json!(plugin.tenant_id));
    map.insert("name".to_owned(), json!(plugin.name));
    map.insert("plugin_type".to_owned(), json!(plugin.plugin_type.as_str()));
    map.insert("phases".to_owned(), json!(plugin.phases));
    if let Some(description) = &plugin.description {
        map.insert("description".to_owned(), json!(description));
    }
    if let Some(schema) = &plugin.config_schema {
        map.insert("config_schema".to_owned(), schema.clone());
    }
    map.insert("source_code".to_owned(), json!(plugin.source_code));
    Value::Object(map)
}

/// Wrap a page of items in the platform's list envelope.
#[must_use]
pub fn list_envelope(items: Vec<Value>, limit: usize) -> Value {
    json!({
        "items": items.clone(),
        "total": items.len(),
        "page_info": {
            "next_cursor": Value::Null,
            "prev_cursor": Value::Null,
            "limit": limit,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::gts_helpers::PluginKind;
    use crate::domain::model::{
        AuthConfig, Endpoint, HttpMatch, MatchConfig, PathSuffixMode, PluginConfig, Protocol,
        Scheme, ServerConfig, SharingMode,
    };
    use uuid::Uuid;

    fn upstream() -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            alias: "api.openai.com".to_owned(),
            enabled: true,
            protocol: Protocol::Http,
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Https,
                    host: "api.openai.com".to_owned(),
                    port: Some(443),
                }],
            },
            auth: Some(AuthConfig {
                plugin_type: crate::domain::gts_helpers::NOOP_AUTH_PLUGIN_ID.to_owned(),
                sharing: SharingMode::Private,
                config: PluginConfig::new(),
            }),
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: vec!["llm".to_owned()],
            seq: 0,
        }
    }

    #[test]
    fn upstream_view_holds_only_schema_members() {
        let upstream = upstream();
        let view = upstream_view(&upstream);
        let object = view.as_object().expect("object");
        let allowed = [
            "id",
            "enabled",
            "alias",
            "tags",
            "server",
            "protocol",
            "auth",
            "headers",
            "plugins",
            "rate_limit",
            "cors",
        ];
        for key in object.keys() {
            assert!(
                allowed.contains(&key.as_str()),
                "{key} is not a member of upstream.v1.schema.json"
            );
        }
        // `id` is a bare UUID per the schema's `format: uuid`.
        assert_eq!(view["id"], json!(upstream.id));
        assert_eq!(view["protocol"], json!(upstream.protocol.as_str()));
        assert!(
            object.get("headers").is_none(),
            "an unset optional block is absent, not null"
        );
        assert!(object.get("tenant_id").is_none());
    }

    #[test]
    fn upstream_view_round_trips_through_the_write_model() {
        let view = upstream_view(&upstream());
        let spec: crate::domain::dto::UpstreamSpec =
            serde_json::from_value(view).expect("a GET response is a valid PUT body");
        assert_eq!(spec.alias.as_deref(), Some("api.openai.com"));
        assert_eq!(spec.enabled, Some(true));
    }

    #[test]
    fn route_view_round_trips_through_the_write_model() {
        let route = Route {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            upstream_id: Uuid::new_v4(),
            enabled: true,
            priority: 5,
            match_config: MatchConfig {
                http: Some(HttpMatch {
                    methods: vec!["GET".to_owned()],
                    path: "/v1/chat".to_owned(),
                    query_allowlist: vec!["model".to_owned()],
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            rate_limit: None,
            cors: None,
            plugins: None,
            tags: vec![],
            seq: 0,
        };
        let view = route_view(&route);
        assert_eq!(view["match"]["http"]["path"], "/v1/chat");
        assert_eq!(view["match_type"], "http");
        assert_eq!(view["priority"], 5);
        assert!(view["match"]["grpc"].is_null());

        let spec: crate::domain::dto::RouteSpec =
            serde_json::from_value(view).expect("a GET response is a valid PUT body");
        assert_eq!(spec.priority, Some(5));
        assert_eq!(
            spec.match_type.as_deref(),
            Some("http"),
            "the derived member is accepted and ignored on write"
        );
    }

    #[test]
    fn plugin_view_names_the_gts_identifier() {
        let id = Uuid::new_v4();
        let plugin = PluginRecord {
            id,
            tenant_id: Uuid::new_v4(),
            plugin_type: PluginKind::Guard,
            name: "request_validator".to_owned(),
            description: Some("Validates headers".to_owned()),
            phases: vec!["on_request".to_owned()],
            config_schema: Some(json!({"type": "object"})),
            source_code: "def on_request(ctx):\n    return ctx.next()\n".to_owned(),
            gc_eligible_at: None,
            seq: 0,
        };
        let view = plugin_view(&plugin);
        assert_eq!(
            view["id"],
            json!(format!("gts.cf.core.oagw.guard_plugin.v1~{id}"))
        );
        assert_eq!(view["plugin_type"], "guard");
        assert_eq!(view["phases"][0], "on_request");
        assert!(view["source_code"].as_str().unwrap().contains("on_request"));
    }

    #[test]
    fn list_envelope_reports_the_page() {
        let envelope = list_envelope(vec![json!({"id": 1}), json!({"id": 2})], 50);
        assert_eq!(envelope["items"].as_array().unwrap().len(), 2);
        assert_eq!(envelope["total"], 2);
        assert_eq!(envelope["page_info"]["limit"], 50);
    }
}
