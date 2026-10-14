//! The OAGW knowledge retriever against the mock gateway: the request `POST
//! /{alias}/openai/vector_stores/{vector_store_id}/search?api-version=...` under the gear's S2S
//! security context with the `query` and `max_num_results`, no credential, the chunk texts of the
//! result page, the tenant override alias, and the failure classification (non-2xx status,
//! transport failure, malformed page) without provider text.
//!
//! Slice S2-deletion-cleanup-tools of `cpt-cf-mini-chat-feature-attachments-and-file-search`:
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop` (`inst-kl-retrieve`,
//! `inst-kl-success`, `inst-kl-failure`); constraint `cpt-cf-mini-chat-constraint-no-credentials`;
//! definition of done `cpt-cf-mini-chat-dod-attachments-and-file-search-knowledge-loop`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use serde_json::{Value, json};

use super::OagwKnowledgeRetriever;
use crate::config::{KnowledgeSearchConfig, MiniChatConfig};
use crate::domain::attachments::knowledge::{KnowledgeChunk, KnowledgeRetriever, RetrievalFailure};
use crate::domain::provider_registry::ProviderRegistry;
use crate::infra::provider_task::S2sContext;
use crate::test_support::provider::GatewayFailure;
use crate::test_support::send::provider_s2s;
use crate::test_support::storage_mock::{Answer, MockStorage, Route};
use crate::test_support::tool_fakes::{
    KB_ALIAS, KB_API_VERSION, KB_VECTOR_STORE, kb_registry, knowledge_config,
};
use crate::test_support::{TENANT_A, TENANT_B};

fn retriever(storage: &Arc<MockStorage>) -> OagwKnowledgeRetriever {
    retriever_with(
        storage,
        kb_registry("openai_responses", Some(KB_API_VERSION)),
    )
}

fn retriever_with(
    storage: &Arc<MockStorage>,
    registry: Arc<ProviderRegistry>,
) -> OagwKnowledgeRetriever {
    OagwKnowledgeRetriever::new(
        storage.clone(),
        S2sContext::bound(provider_s2s()),
        registry,
        knowledge_config(),
    )
}

fn page(texts: &[&str]) -> Value {
    json!({
        "object": "vector_store.search_results.page",
        "search_query": "q",
        "data": texts.iter().enumerate().map(|(n, text)| json!({
            "file_id": format!("file-{n}"),
            "filename": format!("doc-{n}.pdf"),
            "score": 0.9,
            "attributes": {},
            "content": [{ "type": "text", "text": text }]
        })).collect::<Vec<_>>(),
        "has_more": false,
        "next_page": null
    })
}

#[tokio::test]
async fn the_search_is_a_post_to_the_knowledge_vector_store_with_the_api_version() {
    // inst-kl-retrieve: `POST /{alias}/openai/vector_stores/{id}/search` with `api-version`
    let storage = MockStorage::new();
    storage.queue(Route::VsSearch, Answer::ok(&page(&["a"])));
    retriever(&storage)
        .search(TENANT_A, "vacation policy", 5)
        .await
        .unwrap();
    let calls = storage.calls_of(Route::VsSearch);
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].method, "POST");
    assert_eq!(
        calls[0].uri,
        format!(
            "/{KB_ALIAS}/openai/vector_stores/{KB_VECTOR_STORE}/search?api-version={KB_API_VERSION}"
        )
    );
    assert_eq!(storage.call_count(), 1, "no other provider call");
}

#[tokio::test]
async fn the_body_carries_the_query_and_the_capped_top_k_only() {
    // inst-kl-retrieve: the model's `query` and `top_k` (as `max_num_results`)
    let storage = MockStorage::new();
    storage.queue(Route::VsSearch, Answer::ok(&page(&[])));
    retriever(&storage)
        .search(TENANT_A, "the query", 3)
        .await
        .unwrap();
    let call = storage.calls_of(Route::VsSearch).remove(0);
    let body = call.json();
    assert_eq!(body["query"], "the query");
    assert_eq!(body["max_num_results"], 3);
    let keys: std::collections::BTreeSet<_> = body.as_object().unwrap().keys().cloned().collect();
    assert_eq!(
        keys,
        std::collections::BTreeSet::from(["query".to_owned(), "max_num_results".to_owned()]),
        "no filter and no tenant, user or chat identifier: {body}"
    );
    assert_eq!(call.header("content-type"), Some("application/json"));
}

#[tokio::test]
async fn the_call_runs_under_the_s2s_context_without_a_credential() {
    // constraint no-credentials: OAGW injects the credentials
    let storage = MockStorage::new();
    storage.queue(Route::VsSearch, Answer::ok(&page(&[])));
    retriever(&storage).search(TENANT_B, "q", 5).await.unwrap();
    let call = storage.calls_of(Route::VsSearch).remove(0);
    assert_eq!(call.subject, provider_s2s().subject_id());
    assert_eq!(call.tenant, provider_s2s().subject_tenant_id());
    for name in ["authorization", "api-key", "x-api-key"] {
        assert_eq!(call.header(name), None, "{name}");
    }
    assert!(!call.uri.contains(&TENANT_B.to_string()));
}

#[tokio::test]
async fn the_chunks_are_the_text_of_each_result() {
    // inst-kl-success
    let storage = MockStorage::new();
    storage.queue(
        Route::VsSearch,
        Answer::ok(&page(&["vacation: 25 days", "carry over: 5"])),
    );
    let chunks = retriever(&storage).search(TENANT_A, "q", 5).await.unwrap();
    assert_eq!(
        chunks,
        vec![
            KnowledgeChunk {
                text: "vacation: 25 days".to_owned()
            },
            KnowledgeChunk {
                text: "carry over: 5".to_owned()
            },
        ]
    );
}

#[tokio::test]
async fn an_empty_result_page_is_a_success_with_no_chunks() {
    let storage = MockStorage::new();
    storage.queue(Route::VsSearch, Answer::ok(&page(&[])));
    assert!(
        retriever(&storage)
            .search(TENANT_A, "q", 5)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn the_tenant_override_alias_is_used_for_that_tenant() {
    // implementation note 5: the alias is resolved for the caller's tenant
    let config = MiniChatConfig::from_value(json!({
        "client_credentials": { "client_id": "mini-chat", "client_secret": "dev-secret" },
        "providers": { "kb": {
            "kind": "openai_responses", "host": KB_ALIAS, "storage_kind": "azure",
            "api_version": KB_API_VERSION,
            "tenant_overrides": {
                TENANT_B.hyphenated().to_string(): { "host": "kb-b.example.com" }
            }
        } }
    }))
    .unwrap();
    let registry = Arc::new(ProviderRegistry::build(&config.providers));
    let storage = MockStorage::new();
    storage.set_default(Route::VsSearch, Answer::ok(&page(&["x"])));
    let retriever = retriever_with(&storage, registry);
    retriever.search(TENANT_A, "q", 5).await.unwrap();
    retriever.search(TENANT_B, "q", 5).await.unwrap();
    let aliases: Vec<String> = storage
        .calls_of(Route::VsSearch)
        .into_iter()
        .map(|call| call.alias)
        .collect();
    assert_eq!(
        aliases,
        vec![KB_ALIAS.to_owned(), "kb-b.example.com".to_owned()]
    );
}

#[tokio::test]
async fn a_non_2xx_status_is_a_status_failure_without_provider_text() {
    // inst-kl-failure
    for status in [400, 404, 429, 500, 503] {
        let storage = MockStorage::new();
        storage.queue(Route::VsSearch, Answer::status(status));
        let failure = retriever(&storage)
            .search(TENANT_A, "q", 5)
            .await
            .unwrap_err();
        assert_eq!(failure, RetrievalFailure::Status(status), "{status}");
        let rendered = format!("{failure:?}");
        assert!(
            !rendered.contains("provider-secret-error-text"),
            "{rendered}"
        );
    }
}

#[tokio::test]
async fn a_gateway_failure_is_a_transport_failure() {
    for failure in [
        GatewayFailure::Timeout,
        GatewayFailure::Unavailable,
        GatewayFailure::Internal,
    ] {
        let storage = MockStorage::new();
        storage.queue(Route::VsSearch, Answer::Fail(failure));
        assert_eq!(
            retriever(&storage)
                .search(TENANT_A, "q", 5)
                .await
                .unwrap_err(),
            RetrievalFailure::Transport,
            "{failure:?}"
        );
    }
}

#[tokio::test]
async fn a_2xx_answer_that_is_not_a_result_page_is_malformed() {
    for body in [
        json!({}),
        json!({ "data": "no" }),
        json!([1, 2]),
        json!("text"),
    ] {
        let storage = MockStorage::new();
        storage.queue(Route::VsSearch, Answer::ok(&body));
        assert_eq!(
            retriever(&storage)
                .search(TENANT_A, "q", 5)
                .await
                .unwrap_err(),
            RetrievalFailure::Malformed,
            "{body}"
        );
    }
}

#[tokio::test]
async fn the_adapter_is_built_from_the_knowledge_search_configuration() {
    // inst-cfg-knowledge: `vector_store_id`, `provider_id`
    let config: KnowledgeSearchConfig = knowledge_config();
    assert_eq!(config.vector_store_id.as_deref(), Some(KB_VECTOR_STORE));
    assert_eq!(config.provider_id.as_deref(), Some("kb"));
}

#[tokio::test(start_paused = true)]
async fn a_stalled_answer_body_is_a_timeout_failure() {
    // the body read is bounded in time
    let storage = MockStorage::new();
    storage.queue(Route::VsSearch, Answer::StalledBody);
    assert_eq!(
        retriever(&storage)
            .search(TENANT_A, "q", 5)
            .await
            .unwrap_err(),
        RetrievalFailure::Timeout
    );
}
