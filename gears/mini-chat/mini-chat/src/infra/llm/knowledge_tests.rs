#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::json;
use uuid::Uuid;

use super::*;
use crate::config::{MiniChatConfig, ProviderKind};
use crate::infra::llm::{ProviderResolver, RagClient};
use crate::infra::oagw::s2s::S2sContext;
use crate::testing::providers::{anthropic_entry, azure_entry};
use crate::testing::{FakeProvider, TestUser};

const TENANT: Uuid = Uuid::from_u128(0x0000_0000_df51_5b42_9538_d2b5_6b7e_e953);

fn config(f: impl FnOnce(&mut MiniChatConfig)) -> MiniChatConfig {
    let mut cfg = MiniChatConfig::default();
    "test-client".clone_into(&mut cfg.client_credentials.client_id);
    cfg.client_credentials.client_secret = "test-secret".to_owned().into();
    cfg.providers
        .insert("kb".to_owned(), azure_entry("kb.openai.azure.com"));
    cfg.knowledge_search.enabled = true;
    cfg.knowledge_search.vector_store_id = Some("vs_kb".to_owned());
    cfg.knowledge_search.provider_id = Some("kb".to_owned());
    f(&mut cfg);
    cfg.apply_defaults();
    cfg.validate().unwrap();
    cfg
}

fn search(cfg: &MiniChatConfig) -> (Arc<FakeProvider>, KnowledgeSearch) {
    let fake = FakeProvider::new();
    let s2s = Arc::new(S2sContext::new());
    s2s.set(TestUser::S2S.security_context());
    let rag = Arc::new(RagClient::new(
        Arc::clone(&fake) as Arc<dyn oagw_sdk::ServiceGatewayClientV1>,
        s2s,
    ));
    let ks = KnowledgeSearch::new(
        &cfg.knowledge_search,
        Arc::new(ProviderResolver::new(cfg)),
        rag,
    );
    (fake, ks)
}

#[tokio::test]
async fn knowledge_retriever_posts_search_and_trims_chunks() {
    let cfg = config(|c| c.knowledge_search.max_chunk_chars = 5);
    let (fake, ks) = search(&cfg);
    fake.push_search_results(vec!["0123456789", "abc"]);
    let turn = ks.for_tenant(TENANT).expect("knowledge search on");
    assert_eq!(turn.max_calls, 3);
    assert_eq!(turn.top_k, 5);

    let chunks = turn.retriever.search("vacation policy", 2).await.unwrap();
    assert_eq!(chunks, ["01234", "abc"]);

    let req = fake.requests().pop().unwrap();
    assert_eq!(req.method, http::Method::POST);
    assert_eq!(
        req.path,
        "/kb.openai.azure.com/openai/vector_stores/vs_kb/search"
    );
    assert_eq!(req.query.as_deref(), Some("api-version=2025-04-01-preview"));
    assert_eq!(
        req.json.unwrap(),
        json!({"query": "vacation policy", "max_num_results": 2})
    );
    assert_eq!(req.subject_id, TestUser::S2S.user_id);

    // A provider failure is an error of the search.
    fake.fail_next("/openai/vector_stores", 500);
    assert!(turn.retriever.search("q", 1).await.is_err());
}

#[test]
fn knowledge_off_when_the_provider_kind_or_api_version_is_unusable() {
    // Disabled: no parameters.
    let (_, ks) = search(&config(|c| c.knowledge_search.enabled = false));
    assert!(ks.for_tenant(TENANT).is_none());

    // The kind selects the tool-result format: only Responses and Anthropic.
    let (_, ks) = search(&config(|c| {
        c.providers.get_mut("kb").unwrap().kind = ProviderKind::OpenaiChatCompletions;
    }));
    assert!(ks.for_tenant(TENANT).is_none());

    // No api_version (an OpenAI storage entry).
    let (_, ks) = search(&config(|c| {
        c.knowledge_search.provider_id = Some("openai".to_owned());
    }));
    assert!(ks.for_tenant(TENANT).is_none());

    // An anthropic_messages entry is accepted when it has an api_version.
    let (_, ks) = search(&config(|c| {
        let mut e = anthropic_entry();
        e.api_version = Some("2025-04-01-preview".to_owned());
        c.providers.insert("anthropic".to_owned(), e);
        c.knowledge_search.provider_id = Some("anthropic".to_owned());
    }));
    assert!(ks.for_tenant(TENANT).is_some());
}
