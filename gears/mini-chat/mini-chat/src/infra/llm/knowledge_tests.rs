#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers build scripted gateway results"
)]

//! Knowledge retriever tests over the fake OAGW gateway: parameter checks,
//! the Azure search URI and body, result parsing and failures.

use std::collections::BTreeMap;
use std::sync::Arc;

use http::Method;
use serde_json::json;

use super::*;
use crate::infra::llm::fake_gateway::{
    FakeGateway, StaticSystemContext, deadline_exceeded, json_response, lock,
};

const TENANT: Uuid = Uuid::from_u128(0x7e7a);
const OVERRIDE_TENANT: Uuid = Uuid::from_u128(0x0bad);

fn entry(value: serde_json::Value) -> ProviderEntry {
    serde_json::from_value(value).unwrap()
}

fn providers() -> BTreeMap<String, ProviderEntry> {
    BTreeMap::from([
        (
            "kb".to_owned(),
            entry(json!({
                "kind": "openai_responses",
                "host": "kb.openai.azure.com",
                "upstream_alias": "kb-alias",
                "storage_kind": "azure",
                "api_version": "2025-04-01-preview",
                "tenant_overrides": {
                    OVERRIDE_TENANT.to_string(): { "upstream_alias": "kb-tenant" }
                },
            })),
        ),
        (
            "claude".to_owned(),
            entry(json!({
                "kind": "anthropic_messages",
                "host": "api.anthropic.com",
                "upstream_alias": "api.anthropic.com",
                "storage_kind": "openai",
                "api_version": "2025-04-01-preview",
            })),
        ),
        (
            "openai".to_owned(),
            entry(json!({
                "kind": "openai_responses",
                "host": "api.openai.com",
                "upstream_alias": "api.openai.com",
                "storage_kind": "openai",
            })),
        ),
        (
            "chat".to_owned(),
            entry(json!({
                "kind": "openai_chat_completions",
                "host": "chat.example",
                "upstream_alias": "chat.example",
                "storage_kind": "azure",
                "api_version": "2025-04-01-preview",
            })),
        ),
    ])
}

fn config(provider_id: &str) -> KnowledgeSearchConfig {
    KnowledgeSearchConfig {
        enabled: true,
        vector_store_id: Some("vs_Knowledge/1".to_owned()),
        provider_id: Some(provider_id.to_owned()),
        ..KnowledgeSearchConfig::default()
    }
}

fn retriever(provider_id: &str) -> (Arc<FakeGateway>, AzureKnowledgeRetriever) {
    let gateway = Arc::new(FakeGateway::default());
    let retriever = AzureKnowledgeRetriever::from_config(
        gateway.clone(),
        Arc::new(StaticSystemContext::default()),
        &config(provider_id),
        &providers(),
        "mini_chat_test",
    )
    .unwrap();
    (gateway, retriever)
}

fn page() -> serde_json::Value {
    json!({
        "object": "vector_store.search_results.page",
        "data": [
            {
                "file_id": "file-1",
                "filename": "handbook.md",
                "score": 0.91,
                "attributes": {},
                "content": [
                    { "type": "text", "text": "first part" },
                    { "type": "text", "text": "second part" },
                ],
            },
            { "file_id": "file-2", "filename": "", "score": 0.5, "content": [{ "type": "text", "text": "other" }] },
            { "file_id": "file-3", "content": [] },
        ],
        "has_more": false,
    })
}

#[test]
fn disabled_knowledge_search_has_no_retriever() {
    let disabled = KnowledgeSearchConfig::default();
    assert!(
        AzureKnowledgeRetriever::from_config(
            Arc::new(FakeGateway::default()),
            Arc::new(StaticSystemContext::default()),
            &disabled,
            &providers(),
            "mini_chat_test",
        )
        .is_none()
    );
}

#[test]
fn search_uri_uses_the_azure_route_and_the_tenant_alias() {
    let (_, kb) = retriever("kb");

    assert_eq!(
        kb.search_uri(TENANT).unwrap(),
        "/kb-alias/openai/vector_stores/vs_Knowledge%2F1/search?api-version=2025-04-01-preview"
    );
    assert_eq!(
        kb.search_uri(OVERRIDE_TENANT).unwrap(),
        "/kb-tenant/openai/vector_stores/vs_Knowledge%2F1/search?api-version=2025-04-01-preview"
    );
    assert!(kb.check(TENANT).is_ok());
    let (_, claude) = retriever("claude");
    assert!(
        claude.check(TENANT).is_ok(),
        "anthropic_messages is allowed"
    );
}

#[test]
fn unservable_provider_entries_are_reported() {
    for (provider_id, expected) in [
        ("missing", "not a configured provider"),
        ("chat", "openai_responses or anthropic_messages"),
        ("openai", "api_version"),
    ] {
        let (_, kb) = retriever(provider_id);
        let reason = kb.check(TENANT).unwrap_err();
        assert!(reason.contains(expected), "{provider_id}: {reason}");
    }
}

#[tokio::test]
async fn search_posts_query_and_limit_and_parses_the_chunks() {
    let (gateway, kb) = retriever("kb");
    gateway.push_proxy(Ok(json_response(200, &page(), &[], None)));

    let chunks = kb.search(TENANT, "travel policy", 2).await.unwrap();

    assert_eq!(
        chunks,
        vec![
            KnowledgeChunk {
                text: "first part\nsecond part".to_owned(),
                filename: Some("handbook.md".to_owned()),
                score: Some(0.91),
            },
            KnowledgeChunk {
                text: "other".to_owned(),
                filename: None,
                score: Some(0.5),
            },
        ]
    );
    let proxied = lock(&gateway.proxied).clone();
    assert_eq!(proxied.len(), 1);
    assert_eq!(proxied[0].method, Method::POST);
    assert_eq!(
        proxied[0].uri,
        "/kb-alias/openai/vector_stores/vs_Knowledge%2F1/search?api-version=2025-04-01-preview"
    );
    assert_eq!(
        proxied[0].body,
        json!({ "query": "travel policy", "max_num_results": 2 })
    );
}

#[tokio::test]
async fn results_above_the_limit_are_dropped() {
    let (gateway, kb) = retriever("kb");
    gateway.push_proxy(Ok(json_response(200, &page(), &[], None)));

    let chunks = kb.search(TENANT, "q", 1).await.unwrap();

    assert_eq!(chunks.len(), 1);
}

#[tokio::test]
async fn failures_are_search_errors() {
    let (gateway, kb) = retriever("kb");
    gateway.push_proxy(Ok(json_response(
        500,
        &json!({ "error": { "message": "boom" } }),
        &[],
        None,
    )));
    gateway.push_proxy(Err(deadline_exceeded()));
    gateway.push_proxy(Ok(json_response(200, &json!({ "object": "x" }), &[], None)));

    for _ in 0..3 {
        assert!(kb.search(TENANT, "q", 3).await.is_err());
    }
    let (gateway, unservable) = retriever("openai");
    assert!(unservable.search(TENANT, "q", 3).await.is_err());
    assert!(lock(&gateway.proxied).is_empty(), "nothing is sent");
}
