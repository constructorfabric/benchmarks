#![allow(clippy::unwrap_used)]

use oagw_sdk::HttpMethod;
use serde_json::json;

use super::{routes_for, upstream_specs};
use crate::config::MiniChatConfig;

fn cfg(providers: &serde_json::Value) -> MiniChatConfig {
    MiniChatConfig::from_value(Some(&json!({
        "client_credentials": {"client_id": "c", "client_secret": "s"},
        "providers": providers
    })))
    .unwrap()
}

#[test]
fn openai_routes_cover_chat_and_rag() {
    let c = cfg(&json!({"p": {"kind": "openai_responses", "host": "127.0.0.1", "port": 9000, "use_http": true, "storage_kind": "openai"}}));
    let r = routes_for(&c.providers["p"]);
    assert!(r.iter().any(|x| x.method == HttpMethod::Post && x.path == "/v1/responses"));
    assert!(r.iter().any(|x| x.method == HttpMethod::Post && x.path == "/v1/files"));
    assert!(r.iter().any(|x| x.method == HttpMethod::Delete && x.path == "/v1/files"));
    assert!(r.iter().any(|x| x.method == HttpMethod::Get && x.path == "/v1/vector_stores"));
    let specs = upstream_specs(&c.providers);
    assert_eq!(specs.len(), 1);
    assert_eq!((specs[0].alias.as_str(), specs[0].port, specs[0].use_http), ("127.0.0.1", 9000, true));
}

#[test]
fn azure_routes_allow_api_version_and_model_placeholder() {
    let c = cfg(&json!({"az": {"kind": "openai_responses", "host": "a.example.com", "storage_kind": "azure",
        "api_version": "2025-03-01-preview", "api_path": "/openai/deployments/{model}/chat?api-version=2025",
        "tenant_overrides": {"t1": {"host": "b.example.com"}}}}));
    let r = routes_for(&c.providers["az"]);
    assert_eq!(r[0].path, "/openai/deployments");
    assert_eq!(r[0].query_allowlist, vec!["api-version".to_owned()]);
    assert!(r.iter().any(|x| x.path == "/openai/files" && x.query_allowlist == vec!["api-version".to_owned()]));
    let specs = upstream_specs(&c.providers);
    assert_eq!(specs.len(), 2);
    assert_eq!(specs[1].alias, "b.example.com");
}
