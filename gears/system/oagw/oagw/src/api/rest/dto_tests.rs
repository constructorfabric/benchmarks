#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use super::{
    ListEnvelopeDto, PageMetaDto, PluginRequestDto, RouteRequestDto, UpstreamRequestDto,
    build_list_query, parse_plugin_id, parse_route_id, parse_upstream_id, proxy_context,
};
use crate::domain::error::DomainError;
use crate::domain::model::{
    Endpoint, EndpointScheme, GUARD_PLUGIN_TYPE, Protocol, ROUTE_TYPE, ServerConfig, UPSTREAM_TYPE,
};
use axum::http::{HeaderMap, Method};

const ROW: &str = "3f2c1b2a-1b1c-2d3e-4f50-61728394a5b6";

fn upstream_payload(alias: Option<&str>) -> UpstreamRequestDto {
    UpstreamRequestDto {
        alias: alias.map(str::to_owned),
        protocol: Protocol::Http,
        enabled: true,
        server: ServerConfig {
            endpoints: vec![Endpoint {
                scheme: EndpointScheme::Https,
                host: "api.openai.com".to_owned(),
                port: 443,
            }],
        },
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec!["primary".to_owned()],
    }
}

// ── upstream request ───────────────────────────────────────────────────────

#[test]
fn upstream_request_projects_onto_the_domain_command() {
    let command = upstream_payload(Some("api.openai.com")).into_command();
    assert_eq!(command.alias.as_deref(), Some("api.openai.com"));
    assert_eq!(command.protocol, Protocol::Http);
    assert!(command.enabled);
    assert_eq!(command.server.endpoints.len(), 1);
    assert!(command.auth.is_none());
    assert_eq!(command.tags, ["primary"]);
}

#[test]
fn upstream_request_deserializes_without_an_alias() {
    let body = r#"{
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "host": "api.openai.com" } ] }
    }"#;
    let payload: UpstreamRequestDto = serde_json::from_str(body).unwrap();
    assert_eq!(payload.alias, None);
    assert!(payload.enabled, "enabled defaults to true");
    assert_eq!(payload.server.endpoints[0].port, 443);
    assert_eq!(payload.server.endpoints[0].scheme, EndpointScheme::Https);
}

#[test]
fn upstream_request_refuses_unknown_fields() {
    let body = r#"{
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "host": "api.openai.com" } ] },
        "surprise": true
    }"#;
    assert!(serde_json::from_str::<UpstreamRequestDto>(body).is_err());
}

// ── route request ──────────────────────────────────────────────────────────

#[test]
fn route_request_parses_the_upstream_gts_id() {
    let body = format!(
        r#"{{
        "upstream_id": "{UPSTREAM_TYPE}{ROW}",
        "match": {{ "http": {{ "methods": ["GET"], "path": "/v1/models" }} }}
    }}"#
    );
    let payload: RouteRequestDto = serde_json::from_str(&body).unwrap();
    let command = payload.into_command().unwrap();
    assert_eq!(command.upstream_id.to_string(), ROW);
    assert_eq!(command.priority, 0);
    assert!(command.enabled);
}

#[test]
fn route_request_rejects_a_non_gts_upstream_id() {
    let body = format!(
        r#"{{ "upstream_id": "{ROW}", "match": {{ "http": {{ "methods": ["GET"], "path": "/" }} }} }}"#
    );
    let payload: RouteRequestDto = serde_json::from_str(&body).unwrap();
    let error = payload.into_command().unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }));
}

// ── plugin request ─────────────────────────────────────────────────────────

#[test]
fn plugin_request_projects_onto_the_domain_command() {
    let body = r#"{
        "plugin_type": "guard",
        "name": "require-tenant",
        "source_code": "def check(ctx):\n    return ctx\n",
        "phases": ["on_request"]
    }"#;
    let payload: PluginRequestDto = serde_json::from_str(body).unwrap();
    assert_eq!(payload.plugin_type, crate::domain::model::PluginType::Guard);
    let command = payload.into_command();
    assert_eq!(command.name, "require-tenant");
    assert!(command.source_code.contains("def check"));
}

// ── list envelope ──────────────────────────────────────────────────────────

#[test]
fn list_envelope_serializes_items_and_page_meta() {
    let envelope = ListEnvelopeDto {
        items: vec![serde_json::json!({ "alias": "api.openai.com" })],
        page_info: PageMetaDto {
            limit: 50,
            skip: 10,
        },
    };
    let rendered = serde_json::to_value(&envelope).unwrap();
    assert_eq!(rendered["items"][0]["alias"], "api.openai.com");
    assert_eq!(rendered["page_info"]["limit"], 50);
    assert_eq!(rendered["page_info"]["skip"], 10);
}

// ── list query binding ─────────────────────────────────────────────────────

#[test]
fn build_list_query_applies_the_default_page_size() {
    let query = build_list_query(None, 50, 100).unwrap();
    assert_eq!(query.top, Some(50));
    assert_eq!(query.skip, 0);
    assert!(query.filter.is_none());
    assert!(query.order.is_empty());
    assert!(query.select.is_empty());
}

#[test]
fn build_list_query_binds_every_documented_system_option() {
    let query = build_list_query(
        Some("$filter=alias%20eq%20'api.openai.com'&$orderby=created_at%20desc&$select=alias,id&$top=10&$skip=5"),
        50,
        100,
    )
    .unwrap();
    assert_eq!(query.top, Some(10));
    assert_eq!(query.skip, 5);
    assert_eq!(query.select, ["alias", "id"]);
    assert_eq!(query.order.len(), 1);
    assert!(query.filter.is_some());
    assert!(!query.is_unconstrained());
}

#[test]
fn build_list_query_rejects_an_unknown_system_option() {
    let error = build_list_query(Some("$count=true"), 50, 100).unwrap_err();
    assert!(matches!(error, DomainError::InvalidArgument { .. }));
}

#[test]
fn build_list_query_rejects_a_top_outside_the_window() {
    let zero = build_list_query(Some("$top=0"), 50, 100).unwrap_err();
    assert!(matches!(zero, DomainError::InvalidArgument { .. }));

    let over = build_list_query(Some("$top=101"), 50, 100).unwrap_err();
    assert!(matches!(over, DomainError::InvalidArgument { .. }));

    // The window is taken from the configuration, not hard-coded.
    let allowed = build_list_query(Some("$top=25"), 50, 25).unwrap();
    assert_eq!(allowed.top, Some(25));
    assert!(build_list_query(Some("$top=26"), 50, 25).is_err());
}

#[test]
fn build_list_query_rejects_malformed_numbers() {
    assert!(build_list_query(Some("$top=abc"), 50, 100).is_err());
    assert!(build_list_query(Some("$skip=abc"), 50, 100).is_err());
    assert!(build_list_query(Some("$top=-1"), 50, 100).is_err());
}

#[test]
fn build_list_query_rejects_a_malformed_filter() {
    let error = build_list_query(Some("$filter=alias%20%3D%3D%20'x'"), 50, 100).unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }));
}

#[test]
fn build_list_query_tolerates_a_absent_query_string() {
    assert!(build_list_query(None, 50, 100).is_ok());
}

// ── path ids ───────────────────────────────────────────────────────────────

/// `F9` — path parameters carry the anonymous GTS id of their own type, so a
/// bare UUID (or a foreign base type) is a malformed path, not a missing row.
#[test]
fn path_ids_carry_the_typed_gts_prefix() {
    let id = parse_upstream_id(&format!("{UPSTREAM_TYPE}{ROW}")).unwrap();
    assert_eq!(id.to_string(), ROW);
    assert_eq!(
        parse_route_id(&format!("{ROUTE_TYPE}{ROW}"))
            .unwrap()
            .to_string(),
        ROW
    );
    for base in [
        GUARD_PLUGIN_TYPE,
        crate::domain::model::TRANSFORM_PLUGIN_TYPE,
    ] {
        assert_eq!(
            parse_plugin_id(&format!("{base}{ROW}"))
                .unwrap()
                .to_string(),
            ROW
        );
    }
}

#[test]
fn a_bare_uuid_path_id_is_a_validation_error() {
    for parse in [parse_upstream_id, parse_route_id, parse_plugin_id] {
        let error = parse(ROW).unwrap_err();
        assert!(matches!(error, DomainError::Validation { .. }), "{error:?}");
    }
    // A cross-type id is rejected the same way: an upstream id is not a route.
    assert!(parse_route_id(&format!("{UPSTREAM_TYPE}{ROW}")).is_err());
    // A catalog-only id (no UUID tail) is rejected too.
    assert!(
        parse_plugin_id("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1")
            .is_err()
    );
    assert!(parse_upstream_id("not-an-id").is_err());
}

// ── proxy context ──────────────────────────────────────────────────────────

#[test]
fn proxy_context_lowercases_header_names_and_renders_the_suffix() {
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", "k".parse().unwrap());
    let identity = (uuid::Uuid::from_u128(0xC001), uuid::Uuid::from_u128(0x51));
    let context = proxy_context(
        "api.openai.com",
        Some("v1/models"),
        &Method::GET,
        Vec::new(),
        &headers,
        None,
        identity,
    );
    assert_eq!(context.alias, "api.openai.com");
    assert_eq!(context.path, "/v1/models");
    assert_eq!(context.method, "GET");
    assert_eq!(context.header("X-API-KEY"), Some("k"));

    let query = vec![("api-version".to_owned(), "2024-01".to_owned())];
    let without_suffix = proxy_context(
        "api.openai.com",
        None,
        &Method::POST,
        query,
        &headers,
        Some("trace".to_owned()),
        identity,
    );
    assert_eq!(without_suffix.path, "/");
    assert_eq!(
        without_suffix.query,
        vec![("api-version".to_owned(), "2024-01".to_owned())]
    );
    assert_eq!(without_suffix.trace_id.as_deref(), Some("trace"));
}

#[test]
fn query_of_preserves_the_wire_order_and_repeats() {
    assert_eq!(super::query_of(None), Vec::new());
    assert_eq!(
        super::query_of(Some("b=2&a=1&b=3&flag")),
        vec![
            ("b".to_owned(), "2".to_owned()),
            ("a".to_owned(), "1".to_owned()),
            ("b".to_owned(), "3".to_owned()),
            ("flag".to_owned(), String::new()),
        ]
    );
    assert_eq!(
        super::query_of(Some("q=with%20space")),
        vec![("q".to_owned(), "with space".to_owned())]
    );
}
