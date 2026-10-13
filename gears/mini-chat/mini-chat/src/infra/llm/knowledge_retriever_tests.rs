#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 7: the knowledge retriever over the fake gateway
//! (`cpt-cf-mini-chat-algo-streaming-turns-knowledge-search`, `inst-retrieve`):
//! `POST /{alias}/openai/vector_stores/{vector_store_id}/search?api-version={ver}`
//! with `query` and `max_num_results`, the parsing of the chunks and the
//! failures.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use uuid::Uuid;

use super::*;
use crate::domain::service::stream::test_support::{ALIAS, FakeDirectory};
use crate::test_doubles::{FakeOagw, Fault, ProxyReply, user_ctx};

const TENANT: Uuid = Uuid::from_u128(0x7E);

struct Rig {
    oagw: Arc<FakeOagw>,
    directory: Arc<FakeDirectory>,
    retriever: OagwKnowledgeRetriever,
}

fn rig() -> Rig {
    let oagw = FakeOagw::new();
    let directory = FakeDirectory::new();
    directory.target.lock().unwrap().api_version = Some("2025-01-01-preview".to_owned());
    let retriever = OagwKnowledgeRetriever::new(
        oagw.clone(),
        directory.clone() as Arc<dyn ProviderDirectory>,
        "kp",
        "vs_kb",
    );
    Rig {
        oagw,
        directory,
        retriever,
    }
}

async fn search(rig: &Rig, query: &str, top_k: usize) -> Result<Vec<KnowledgeChunk>, DomainError> {
    tokio::time::timeout(
        Duration::from_secs(5),
        rig.retriever.search(
            &user_ctx(),
            &KnowledgeSearchRequest {
                tenant_id: TENANT,
                query,
                top_k,
            },
        ),
    )
    .await
    .expect("the search did not end")
}

fn answer(body: &Value) -> ProxyReply {
    ProxyReply::json(200, body)
}

fn texts(chunks: &[KnowledgeChunk]) -> Vec<&str> {
    chunks.iter().map(|chunk| chunk.text.as_str()).collect()
}

// ---- the request --------------------------------------------------------------------------------

#[tokio::test]
async fn the_search_is_a_post_to_the_vector_store_search_path_with_the_api_version() {
    let rig = rig();
    rig.oagw.push_proxy(answer(&json!({ "data": [] })));
    search(&rig, "refund policy", 3).await.unwrap();
    let sent = rig.oagw.proxied();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].method, "POST");
    assert_eq!(
        sent[0].uri,
        format!("/{ALIAS}/openai/vector_stores/vs_kb/search?api-version=2025-01-01-preview")
    );
    assert_eq!(sent[0].header("content-type"), Some("application/json"));
    assert!(
        sent[0].header("authorization").is_none(),
        "OAGW injects the credential"
    );
    assert_eq!(sent[0].subject, user_ctx().subject_id());
    assert_eq!(sent[0].tenant, user_ctx().subject_tenant_id());
}

#[tokio::test]
async fn the_body_carries_the_query_and_the_capped_top_k_as_max_num_results() {
    let rig = rig();
    rig.oagw.push_proxy(answer(&json!({ "data": [] })));
    search(&rig, "refund policy", 3).await.unwrap();
    assert_eq!(
        rig.oagw.proxied()[0].json(),
        json!({ "query": "refund policy", "max_num_results": 3 })
    );
}

#[tokio::test]
async fn the_provider_is_resolved_for_the_tenant_of_the_turn() {
    let rig = rig();
    rig.oagw.push_proxy(answer(&json!({ "data": [] })));
    search(&rig, "q", 1).await.unwrap();
    assert_eq!(
        rig.directory.resolved.lock().unwrap().clone(),
        vec![("kp".to_owned(), TENANT)]
    );
}

// ---- the response -------------------------------------------------------------------------------

#[tokio::test]
async fn each_result_is_one_chunk_with_its_text_parts_joined_by_a_newline() {
    let rig = rig();
    rig.oagw.push_proxy(answer(&json!({
        "object": "vector_store.search_results.page",
        "data": [
            { "file_id": "file-1", "filename": "a.md", "score": 0.91,
              "content": [ { "type": "text", "text": "alpha" }, { "type": "text", "text": "beta" } ] },
            { "file_id": "file-2", "filename": "b.md", "score": 0.5,
              "content": [ { "type": "text", "text": "gamma" } ] }
        ]
    })));
    let chunks = search(&rig, "q", 5).await.unwrap();
    assert_eq!(texts(&chunks), ["alpha\nbeta", "gamma"]);
}

#[tokio::test]
async fn results_without_any_text_are_skipped_and_an_empty_page_is_no_chunks() {
    let rig = rig();
    rig.oagw.push_proxy(answer(&json!({
        "data": [
            { "file_id": "f1", "content": [] },
            { "file_id": "f2", "content": [ { "type": "image", "url": "x" } ] },
            { "file_id": "f3" },
            { "file_id": "f4", "content": [ { "type": "text", "text": "kept" } ] }
        ]
    })));
    assert_eq!(texts(&search(&rig, "q", 5).await.unwrap()), ["kept"]);
    rig.oagw.push_proxy(answer(&json!({ "data": [] })));
    assert!(search(&rig, "q", 5).await.unwrap().is_empty());
}

#[tokio::test]
async fn the_order_of_the_provider_is_kept() {
    let rig = rig();
    let data: Vec<Value> = (0..4)
        .map(|n| json!({ "content": [ { "type": "text", "text": format!("c{n}") } ] }))
        .collect();
    rig.oagw.push_proxy(answer(&json!({ "data": data })));
    assert_eq!(
        texts(&search(&rig, "q", 5).await.unwrap()),
        ["c0", "c1", "c2", "c3"]
    );
}

// ---- failures -----------------------------------------------------------------------------------

#[tokio::test]
async fn a_non_success_status_is_an_error() {
    for status in [400_u16, 401, 404, 429, 500, 503] {
        let rig = rig();
        rig.oagw.push_proxy(ProxyReply::json(
            status,
            &json!({ "error": { "message": "nope" } }),
        ));
        assert!(search(&rig, "q", 1).await.is_err(), "{status}");
    }
}

#[tokio::test]
async fn a_failed_call_is_an_error() {
    let rig = rig();
    rig.oagw
        .push_proxy(ProxyReply::Error(Fault::DeadlineExceeded));
    assert!(search(&rig, "q", 1).await.is_err());
}

#[tokio::test]
async fn a_body_that_is_not_the_search_page_is_an_error() {
    for body in [
        ProxyReply::json(200, &json!({ "unexpected": true })),
        ProxyReply::json(200, &json!({ "data": "no" })),
        ProxyReply::json(200, &json!([1, 2])),
        ProxyReply::sse_text("not json at all".to_owned()),
    ] {
        let rig = rig();
        rig.oagw.push_proxy(body);
        assert!(search(&rig, "q", 1).await.is_err());
    }
}

#[tokio::test]
async fn a_provider_without_an_api_version_is_an_error_and_nothing_is_sent() {
    let rig = rig();
    rig.directory.target.lock().unwrap().api_version = None;
    assert!(search(&rig, "q", 1).await.is_err());
    assert_eq!(rig.oagw.proxy_call_count(), 0);
}

#[tokio::test]
async fn an_unresolvable_provider_is_an_error_and_nothing_is_sent() {
    let rig = rig();
    rig.directory.fail_ids.lock().unwrap().push("kp".to_owned());
    let error = search(&rig, "q", 1).await.unwrap_err();
    assert!(
        matches!(error, DomainError::ProviderResolution(_)),
        "{error:?}"
    );
    assert_eq!(rig.oagw.proxy_call_count(), 0);
}

#[tokio::test]
async fn a_search_that_never_answers_is_dropped_with_its_caller() {
    let rig = rig();
    rig.oagw.push_proxy(ProxyReply::Pending);
    let ctx = user_ctx();
    let pending = rig.retriever.search(
        &ctx,
        &KnowledgeSearchRequest {
            tenant_id: TENANT,
            query: "q",
            top_k: 1,
        },
    );
    let outcome = tokio::time::timeout(Duration::from_millis(100), pending).await;
    assert!(outcome.is_err(), "the search is still pending");
    assert_eq!(
        rig.oagw.pending_calls_dropped(),
        1,
        "dropping the search drops the OAGW call"
    );
}
