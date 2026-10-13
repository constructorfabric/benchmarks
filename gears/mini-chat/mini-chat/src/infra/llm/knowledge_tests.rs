#![allow(clippy::unwrap_used, clippy::expect_used)]

use oagw_sdk::api::ErrorSource;
use serde_json::json;

use super::{
    AzureKnowledgeRetriever, KnowledgeChunk, KnowledgeRetriever, KnowledgeTarget, SEARCH_KNOWLEDGE,
    SearchArgs, format_output, parse_args, search_knowledge_tool,
};
use crate::infra::llm::fake_gw::{FakeGw, http_error, json_ok, parts};
use crate::infra::llm::types::ToolSpec;

fn target() -> KnowledgeTarget {
    KnowledgeTarget {
        alias: "kb.openai.azure.com".to_owned(),
        api_version: "2025-04-01-preview".to_owned(),
        vector_store_id: "vs_kb000000000001".to_owned(),
    }
}

fn retriever(gw: &std::sync::Arc<FakeGw>) -> (AzureKnowledgeRetriever, uuid::Uuid) {
    let (gw, identity, ctx) = parts(gw);
    (AzureKnowledgeRetriever::new(gw, identity), ctx.subject_id())
}

#[tokio::test]
async fn search_posts_to_azure_vector_store_search() {
    let gw = FakeGw::with(json_ok(&json!({
        "object": "vector_store.search_results.page",
        "search_query": ["vacation policy"],
        "data": [
            {"file_id": "assistant-abc", "filename": "hr.pdf", "score": 0.91, "attributes": {},
             "content": [{"type": "text", "text": "Employees get "}, {"type": "text", "text": "25 days."}]},
            {"file_id": "assistant-def", "filename": "faq.md", "score": 0.5,
             "content": [{"type": "text", "text": "See HR."}]}
        ],
        "has_more": false, "next_page": null
    })));
    let (r, subject) = retriever(&gw);
    let chunks = r.search(&target(), "vacation policy", 4).await.unwrap();

    let cap = gw.last();
    assert_eq!(cap.method, http::Method::POST);
    assert_eq!(
        cap.uri,
        "/kb.openai.azure.com/openai/vector_stores/vs_kb000000000001/search?api-version=2025-04-01-preview"
    );
    assert_eq!(cap.subject_id, subject, "service identity");
    assert_eq!(
        cap.body,
        json!({"query": "vacation policy", "max_num_results": 4})
    );
    assert_eq!(
        chunks,
        vec![
            KnowledgeChunk {
                filename: "hr.pdf".to_owned(),
                score: Some(0.91),
                text: "Employees get 25 days.".to_owned(),
            },
            KnowledgeChunk {
                filename: "faq.md".to_owned(),
                score: Some(0.5),
                text: "See HR.".to_owned(),
            },
        ]
    );
}

#[tokio::test]
async fn search_http_error_is_provider_error() {
    let gw = FakeGw::with(http_error(
        404,
        ErrorSource::Upstream,
        vec![],
        &json!({"error": {"code": "not_found", "message": "Vector store vs_kb000000000001 not found"}}),
    ));
    let (r, _) = retriever(&gw);
    let err = r.search(&target(), "q", 3).await.unwrap_err();
    assert_eq!(err.code, "provider_error");
    assert_eq!(err.message, "Vector store [provider_id] not found");
}

#[test]
fn tool_is_a_function_named_search_knowledge() {
    let ToolSpec::Function {
        name, parameters, ..
    } = search_knowledge_tool()
    else {
        panic!("function tool expected");
    };
    assert_eq!(name, SEARCH_KNOWLEDGE);
    assert_eq!(parameters["required"], json!(["query"]));
    assert_eq!(parameters["properties"]["query"]["type"], "string");
    assert_eq!(parameters["properties"]["top_k"]["type"], "integer");
}

#[test]
fn args_cap_top_k_and_require_a_query() {
    assert_eq!(
        parse_args(r#"{"query":"a","top_k":50}"#, 5),
        Some(SearchArgs {
            query: "a".to_owned(),
            top_k: 5
        })
    );
    assert_eq!(
        parse_args(r#"{"query":"a","top_k":2}"#, 5).unwrap().top_k,
        2
    );
    assert_eq!(
        parse_args(r#"{"query":"a","top_k":0}"#, 5).unwrap().top_k,
        1
    );
    assert_eq!(parse_args(r#"{"query":"a"}"#, 5).unwrap().top_k, 5);
    assert_eq!(parse_args(r#"{"query":"  "}"#, 5), None);
    assert_eq!(parse_args("not json", 5), None);
}

#[test]
fn output_trims_each_chunk() {
    let chunks = vec![KnowledgeChunk {
        filename: "a.txt".to_owned(),
        score: Some(0.7),
        text: "abcd\u{e9}fgh".to_owned(),
    }];
    let out: serde_json::Value = serde_json::from_str(&format_output(&chunks, 5)).unwrap();
    assert_eq!(
        out,
        json!({"results": [{"filename": "a.txt", "score": 0.7, "text": "abcd\u{e9}"}]})
    );
    let empty: serde_json::Value = serde_json::from_str(&format_output(&[], 5)).unwrap();
    assert_eq!(empty, json!({"results": []}));
}
