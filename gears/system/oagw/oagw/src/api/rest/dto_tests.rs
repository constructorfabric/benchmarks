//! DTO wire-shape tests.

use super::{
    PluginDto, PluginSourceDto, RouteDto, UpstreamDto, parse_plugin_id, parse_plugin_kind,
    parse_route_id, parse_upstream_id,
};
use crate::domain::gts_helpers as gts;
use crate::domain::model::{
    Endpoint, HttpMatch, MatchConfig, PathSuffixMode, Plugin, PluginKind, Route, RouteSpec,
    ServerConfig, Upstream, UpstreamSpec,
};
use crate::domain::timeutil;
use serde_json::json;
use uuid::Uuid;

fn upstream() -> Upstream {
    Upstream {
        id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        created_at: timeutil::now_rfc3339(),
        updated_at: timeutil::now_rfc3339(),
        spec: UpstreamSpec {
            enabled: true,
            alias: "api.openai.com".to_owned(),
            tags: vec!["openai".to_owned()],
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: "https".to_owned(),
                    host: "api.openai.com".to_owned(),
                    port: 443,
                }],
            },
            protocol: gts::PROTOCOL_HTTP.to_owned(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        },
    }
}

fn route(upstream_id: Uuid) -> Route {
    Route {
        id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        upstream_id,
        created_at: timeutil::now_rfc3339(),
        updated_at: timeutil::now_rfc3339(),
        spec: RouteSpec {
            enabled: true,
            priority: 7,
            tags: Vec::new(),
            match_config: MatchConfig {
                http: Some(HttpMatch {
                    methods: vec!["POST".to_owned()],
                    path: "/v1/chat/completions".to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            cors: None,
        },
    }
}

#[test]
fn upstream_response_carries_a_gts_identifier() {
    let domain = upstream();
    let dto = UpstreamDto::from_domain(&domain);
    let json = serde_json::to_value(&dto).expect("serializes");
    assert_eq!(
        json["id"],
        format!("gts.cf.core.oagw.upstream.v1~{}", domain.id)
    );
    assert_eq!(json["alias"], "api.openai.com");
    assert_eq!(json["enabled"], true);
    assert_eq!(json["protocol"], gts::PROTOCOL_HTTP);
    assert_eq!(json["server"]["endpoints"][0]["scheme"], "https");
    assert_eq!(json["tags"][0], "openai");
    assert!(json["created_at"].is_string());
}

#[test]
fn upstream_request_omits_absent_blocks() {
    let dto: UpstreamDto = serde_json::from_value(json!({
        "server": { "endpoints": [ { "scheme": "http", "host": "mock.local", "port": 80 } ] },
        "protocol": gts::PROTOCOL_HTTP
    }))
    .expect("parses");
    let input = dto.into_input();
    assert!(input.auth.is_none());
    assert!(input.cors.is_none());
    assert!(input.enabled.is_none(), "absent means 'use the default'");
    let server = input.server.expect("server");
    assert_eq!(server.endpoints[0].scheme, "http");
    assert_eq!(server.endpoints[0].port, 80);
}

#[test]
fn upstream_request_tolerates_unknown_members() {
    // Forward compatibility: an unrecognised member must not fail a request
    // that is otherwise valid.
    let dto: UpstreamDto = serde_json::from_value(json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com" } ] },
        "protocol": gts::PROTOCOL_HTTP,
        "some_future_field": 42
    }))
    .expect("parses");
    assert!(dto.server.is_some());
}

#[test]
fn endpoint_defaults_follow_the_schema() {
    let dto: UpstreamDto = serde_json::from_value(json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "api.openai.com" } ] },
        "protocol": gts::PROTOCOL_HTTP
    }))
    .expect("parses");
    let server = dto.server.expect("server");
    assert_eq!(server.endpoints[0].port, 443, "port defaults to 443");
}

#[test]
fn route_response_uses_gts_identifiers_for_both_ids() {
    let upstream_id = Uuid::new_v4();
    let domain = route(upstream_id);
    let dto = RouteDto::from_domain(&domain);
    let json = serde_json::to_value(&dto).expect("serializes");
    assert_eq!(
        json["id"],
        format!("gts.cf.core.oagw.route.v1~{}", domain.id)
    );
    assert_eq!(
        json["upstream_id"],
        format!("gts.cf.core.oagw.upstream.v1~{upstream_id}")
    );
    assert_eq!(json["match_type"], "http");
    assert_eq!(json["match"]["http"]["path"], "/v1/chat/completions");
    assert_eq!(json["priority"], 7);
}

#[test]
fn route_request_accepts_both_upstream_id_spellings() {
    let upstream_id = Uuid::new_v4();
    for raw in [
        upstream_id.to_string(),
        format!("gts.cf.core.oagw.upstream.v1~{upstream_id}"),
    ] {
        let dto: RouteDto = serde_json::from_value(json!({
            "upstream_id": raw,
            "match": { "http": { "methods": ["POST"], "path": "/v1/chat" } }
        }))
        .expect("parses");
        let input = dto.into_input().expect("converts");
        assert_eq!(input.upstream_id, Some(upstream_id));
    }
}

#[test]
fn route_request_rejects_a_nonsense_upstream_id() {
    let dto: RouteDto = serde_json::from_value(json!({
        "upstream_id": "not-an-id",
        "match": { "http": { "methods": ["POST"], "path": "/v1/chat" } }
    }))
    .expect("parses");
    assert_eq!(dto.into_input().expect_err("rejected").status, 400);
}

#[test]
fn http_match_defaults_are_applied() {
    let dto: RouteDto = serde_json::from_value(json!({
        "upstream_id": Uuid::new_v4().to_string(),
        "match": { "http": { "methods": ["GET"], "path": "/v1" } }
    }))
    .expect("parses");
    let http = dto.match_config.expect("match").http.expect("http");
    assert!(http.query_allowlist.is_empty(), "empty allows none");
    assert_eq!(http.path_suffix_mode, PathSuffixMode::Append);
}

#[test]
fn plugin_kind_parsing_accepts_short_and_full_forms() {
    assert_eq!(parse_plugin_kind("guard"), Some(PluginKind::Guard));
    assert_eq!(parse_plugin_kind("Auth"), Some(PluginKind::Auth));
    assert_eq!(parse_plugin_kind("transform"), Some(PluginKind::Transform));
    assert_eq!(
        parse_plugin_kind(gts::GUARD_PLUGIN_TYPE),
        Some(PluginKind::Guard)
    );
    assert_eq!(
        parse_plugin_kind("gts.cf.core.oagw.transform_plugin.v1"),
        Some(PluginKind::Transform)
    );
    assert_eq!(parse_plugin_kind("nonsense"), None);
}

#[test]
fn plugin_response_never_echoes_the_source() {
    let plugin = Plugin {
        id: Uuid::new_v4(),
        tenant_id: Uuid::new_v4(),
        kind: PluginKind::Guard,
        name: "request_validator".to_owned(),
        description: Some("Validates request headers".to_owned()),
        phases: Vec::new(),
        config_schema: Some(json!({ "type": "object" })),
        source_code: "def on_request(ctx):\n    return ctx.next()\n".to_owned(),
        created_at: timeutil::now_rfc3339(),
        last_used_at: None,
        gc_eligible_at: None,
    };
    let json = serde_json::to_value(PluginDto::from_domain(&plugin)).expect("serializes");
    assert_eq!(
        json["id"],
        format!("gts.cf.core.oagw.guard_plugin.v1~{}", plugin.id)
    );
    assert_eq!(json["plugin_type"], "guard");
    assert_eq!(json["name"], "request_validator");
    assert!(
        json.get("source_code").is_none(),
        "source is only served by /plugins/{{id}}/source"
    );

    let source = serde_json::to_value(PluginSourceDto::from_domain(&plugin)).expect("serializes");
    assert!(
        source["source_code"]
            .as_str()
            .is_some_and(|s| s.contains("on_request"))
    );
}

#[test]
fn path_parameter_parsing_accepts_both_spellings_and_rejects_the_wrong_type() {
    let id = Uuid::new_v4();
    assert_eq!(
        parse_upstream_id(&format!("gts.cf.core.oagw.upstream.v1~{id}")).expect("parses"),
        id
    );
    assert_eq!(parse_upstream_id(&id.to_string()).expect("parses"), id);
    assert_eq!(
        parse_upstream_id(&format!("gts.cf.core.oagw.route.v1~{id}"))
            .expect_err("wrong type")
            .status,
        404
    );

    assert_eq!(
        parse_route_id(&format!("gts.cf.core.oagw.route.v1~{id}")).expect("parses"),
        id
    );
    for kind in ["auth", "guard", "transform"] {
        let raw = format!("gts.cf.core.oagw.{kind}_plugin.v1~{id}");
        assert_eq!(parse_plugin_id(&raw).expect("parses"), id);
    }
    assert_eq!(parse_plugin_id("garbage").expect_err("bad").status, 404);
}
