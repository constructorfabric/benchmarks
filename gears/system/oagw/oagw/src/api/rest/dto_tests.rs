//! Tests for [`crate::api::rest::dto`].

use serde_json::json;
use uuid::Uuid;

use super::{CreatePluginRequest, CreateRouteRequest, ReplaceRouteRequest, UpstreamRequest};
use crate::domain::model::{
    CorsConfig, Endpoint, GrpcMatch, HttpMatch, HttpMethod, PathSuffixMode, Protocol, RouteMatch,
    Scheme, ServerConfig, SharingMode, Upstream,
};

fn tenant() -> Uuid {
    Uuid::from_u128(0x42)
}

// -- request DTOs keep server fields out ----------------------------------------

#[test]
fn upstream_requests_reject_server_generated_fields() {
    let body = json!({
        "id": "gts.cf.core.oagw.upstream.v1~00000000-0000-0000-0000-000000000001",
        "tenant_id": tenant(),
        "created_at": "2026-02-03T11:09:37.431Z",
        "server": {"endpoints": []}
    });
    let error = serde_json::from_value::<UpstreamRequest>(body).expect_err("rejected");
    assert!(error.to_string().contains("unknown field"), "{error}");
}

#[test]
fn route_replace_requests_reject_upstream_id() {
    let body = json!({
        "upstream_id": Uuid::from_u128(0x1),
        "match": {"http": {"methods": ["GET"], "path": "/v1"}}
    });
    let error = serde_json::from_value::<ReplaceRouteRequest>(body).expect_err("rejected");
    assert!(error.to_string().contains("upstream_id"), "{error}");
}

#[test]
fn route_create_requests_accept_upstream_id() {
    let body = json!({
        "upstream_id": Uuid::from_u128(0x1),
        "match": {"http": {"methods": ["GET"], "path": "/v1"}},
        "headers": {},
        "plugins": {}
    });
    let request = serde_json::from_value::<CreateRouteRequest>(body).expect("accepted");
    assert_eq!(request.upstream_id, Uuid::from_u128(0x1));
    assert_eq!(
        request
            .as_input()
            .r#match
            .http
            .as_ref()
            .map(|http| http.path.as_str()),
        Some("/v1")
    );
}

#[test]
fn plugin_requests_default_to_an_empty_object_config() {
    let request: CreatePluginRequest = serde_json::from_value(json!({
        "plugin_type": "gts.cf.core.oagw.guard_plugin.v1"
    }))
    .expect("accepted");
    assert_eq!(request.config, json!({}));
    assert!(request.as_input().config.is_object());
}

// -- conversions ----------------------------------------------------------------

#[test]
fn upstream_requests_convert_into_validator_inputs() {
    let request = UpstreamRequest {
        alias: Some("payments".to_owned()),
        enabled: Some(false),
        tags: vec!["team-a".to_owned()],
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: Scheme::Https,
                host: "10.0.0.1".to_owned(),
                port: 8443,
            }],
        },
        protocol: Protocol::Grpc,
        auth: None,
        headers: crate::domain::model::HeadersConfig::default(),
        plugins: crate::domain::model::PluginConfig::default(),
        rate_limit: None,
        cors: None,
    };
    let input = request.as_input();
    assert_eq!(input.alias.as_deref(), Some("payments"));
    assert!(!input.enabled.expect("explicit"));
    assert_eq!(input.server.endpoints.len(), 1);
    assert_eq!(input.protocol, Protocol::Grpc);
}

#[test]
fn route_requests_keep_their_match_block() {
    let r#match = RouteMatch {
        http: None,
        grpc: Some(GrpcMatch {
            service: "svc.v1.Svc".to_owned(),
            method: "Get".to_owned(),
        }),
    };
    let create = CreateRouteRequest {
        upstream_id: Uuid::from_u128(0x7),
        r#match: r#match.clone(),
        headers: crate::domain::model::HeadersConfig::default(),
        plugins: crate::domain::model::PluginConfig::default(),
        rate_limit: None,
        cors: None,
        enabled: None,
        priority: Some(5),
        tags: Vec::new(),
    };
    let replace = ReplaceRouteRequest {
        r#match: r#match.clone(),
        headers: crate::domain::model::HeadersConfig::default(),
        plugins: crate::domain::model::PluginConfig::default(),
        rate_limit: None,
        cors: None,
        enabled: None,
        priority: Some(5),
        tags: Vec::new(),
    };
    assert_eq!(create.as_input().r#match, r#match);
    assert_eq!(replace.as_input().r#match, r#match);
}

// -- response DTOs --------------------------------------------------------------

#[test]
fn upstream_dtos_render_timestamps_and_hide_nothing() {
    let upstream = Upstream {
        id: Uuid::from_u128(0x3),
        alias: "api.openai.com".to_owned(),
        cors: Some(CorsConfig {
            enabled: true,
            sharing: SharingMode::Private,
            allowed_origins: vec!["https://app.example.com".to_owned()],
            allowed_methods: Vec::new(),
            allow_headers: Vec::new(),
            expose_headers: Vec::new(),
            allow_credentials: false,
            max_age: None,
        }),
        ..Upstream::default()
    };
    let rendered = serde_json::to_value(super::UpstreamDto::from(&upstream)).expect("serialises");
    assert_eq!(rendered["id"], json!(Uuid::from_u128(0x3).to_string()));
    assert_eq!(rendered["alias"], json!("api.openai.com"));
    assert!(rendered["created_at"].is_string());
    assert!(rendered["cors"]["allowed_origins"].is_array());
    assert!(
        rendered["tenant_id"].is_null(),
        "tenant_id stays off the wire"
    );
}

#[test]
fn route_dtos_render_the_match_block_under_match() {
    let route = crate::domain::model::Route {
        r#match: RouteMatch {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get, HttpMethod::Post],
                path: "/v1/pay".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
        ..crate::domain::model::Route::default()
    };
    let rendered = serde_json::to_value(super::RouteDto::from(&route)).expect("serialises");
    assert_eq!(rendered["match"]["http"]["path"], json!("/v1/pay"));
    assert_eq!(rendered["match"]["http"]["methods"], json!(["GET", "POST"]));
}

#[test]
fn plugin_dtos_render_their_type_and_config() {
    let plugin = crate::domain::model::Plugin {
        plugin_type: "gts.cf.core.oagw.guard_plugin.v1".to_owned(),
        config: json!({"headers": ["x-request-id"]}),
        ..crate::domain::model::Plugin::default()
    };
    let rendered = serde_json::to_value(super::PluginDto::from(&plugin)).expect("serialises");
    assert_eq!(
        rendered["plugin_type"],
        json!("gts.cf.core.oagw.guard_plugin.v1")
    );
    assert_eq!(rendered["config"]["headers"], json!(["x-request-id"]));
}

// -- sharing modes ---------------------------------------------------------------

#[test]
fn enforced_sections_block_a_descendant_override() {
    let mut upstream = Upstream::default();
    assert!(!super::enforces_override(&upstream), "nothing configured");

    upstream.rate_limit = Some(crate::domain::model::RateLimitConfig {
        sharing: SharingMode::Enforce,
        algorithm: crate::domain::model::RateLimitAlgorithm::TokenBucket,
        sustained: crate::domain::model::SustainedRateConfig {
            rate: 1,
            window: crate::domain::model::RateLimitWindow::Second,
        },
        burst: None,
        scope: crate::domain::model::RateLimitScope::Tenant,
        strategy: crate::domain::model::RateLimitStrategy::Reject,
        response_headers: true,
        cost: 1,
    });
    assert!(super::enforces_override(&upstream));

    upstream.rate_limit.as_mut().expect("configured").sharing = SharingMode::Inherit;
    assert!(!super::enforces_override(&upstream));
}
