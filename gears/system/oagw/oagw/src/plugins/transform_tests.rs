//! Tests for the built-in transform plugins.

use crate::plugins::token_cache::TokenCache;
use crate::plugins::{BoundPlugin, PluginContext};
use crate::security::{NoopCredentialResolver, SecurityContextHolder};

fn binding(config: serde_json::Value) -> BoundPlugin {
    let config = match config {
        serde_json::Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };
    BoundPlugin { name: "request_id".to_owned(), config }
}

struct Harness {
    holder: SecurityContextHolder,
    token_cache: TokenCache,
}

fn ctx<'a>(h: &'a Harness, request_id: &'a str) -> PluginContext<'a> {
    PluginContext {
        security: &h.holder,
        config: empty_config(),
        upstream_id: "gts.cf.core.oagw.upstream.v1~abc",
        host: "api.example.com",
        path: "/v1/items",
        request_id,
        credentials: &NoopCredentialResolver,
        token_cache: &h.token_cache,
    }
}

fn empty_config() -> &'static serde_json::Map<String, serde_json::Value> {
    static EMPTY: std::sync::OnceLock<serde_json::Map<String, serde_json::Value>> =
        std::sync::OnceLock::new();
    EMPTY.get_or_init(serde_json::Map::new)
}

fn harness() -> Harness {
    let tenant = uuid::Uuid::new_v4();
    Harness {
        holder: SecurityContextHolder::new(
            toolkit_security::SecurityContext::builder()
                .subject_id(uuid::Uuid::new_v4())
                .subject_tenant_id(tenant)
                .build()
                .expect("the context is complete"),
            vec![tenant],
        ),
        token_cache: TokenCache::new(8, std::time::Duration::from_mins(5)),
    }
}

/// FR-018, request half: a caller who sent an identifier keeps it.
#[test]
fn a_caller_supplied_identifier_is_propagated() {
    let h = harness();
    let ctx = ctx(&h, "");
    let plugin = binding(serde_json::json!({}));
    let mut headers = http::HeaderMap::new();
    headers.insert("x-request-id", http::HeaderValue::from_static("from-caller"));

    crate::plugins::transform::execute_request(&plugin, &ctx, &mut headers)
        .expect("the transform accepts a request");
    assert_eq!(headers["x-request-id"], "from-caller");
}

/// FR-018, request half: a caller who sent none is given one.
#[test]
fn a_missing_identifier_is_generated() {
    let h = harness();
    let ctx = ctx(&h, "");
    let plugin = binding(serde_json::json!({}));
    let mut headers = http::HeaderMap::new();

    crate::plugins::transform::execute_request(&plugin, &ctx, &mut headers)
        .expect("the transform generates one");
    let generated = headers["x-request-id"].to_str().expect("a plain value");
    assert!(generated.starts_with("req_"), "{generated}");
}

/// FR-018, response half: the caller is handed the identifier the request was given.
#[test]
fn the_response_carries_the_identifier_the_request_was_given() {
    let h = harness();
    let ctx = ctx(&h, "req_6f9c0c0f");
    let plugin = binding(serde_json::json!({}));
    let mut headers = http::HeaderMap::new();
    headers.insert("x-request-id", http::HeaderValue::from_static("upstreams-own"));

    crate::plugins::transform::execute_response(&plugin, &ctx, &mut headers)
        .expect("the response carries it");
    assert_eq!(headers["x-request-id"], "req_6f9c0c0f");
}

/// Nothing is invented on the response either: with no identifier to hand back the
/// gateway adds no header rather than an empty one.
#[test]
fn a_response_without_an_identifier_to_hand_back_adds_none() {
    let h = harness();
    let ctx = ctx(&h, "");
    let plugin = binding(serde_json::json!({}));
    let mut headers = http::HeaderMap::new();

    crate::plugins::transform::execute_response(&plugin, &ctx, &mut headers)
        .expect("nothing to add");
    assert!(headers.get("x-request-id").is_none());
}
