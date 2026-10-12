#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use uuid::Uuid;

use super::*;

struct StubRetriever {
    check_error: Option<String>,
}

#[async_trait]
impl KnowledgeRetriever for StubRetriever {
    fn check(&self, _tenant_id: Uuid) -> Result<(), String> {
        self.check_error.clone().map_or(Ok(()), Err)
    }

    async fn search(
        &self,
        _tenant_id: Uuid,
        _query: &str,
        _max_num_results: usize,
    ) -> Result<Vec<KnowledgeChunk>, KnowledgeSearchError> {
        Ok(Vec::new())
    }
}

fn enabled() -> KnowledgeSearchConfig {
    KnowledgeSearchConfig {
        enabled: true,
        vector_store_id: Some("vs_1".to_owned()),
        provider_id: Some("kb".to_owned()),
        max_calls_per_message: 2,
        top_k: 4,
        max_chunk_chars: 10,
        ..KnowledgeSearchConfig::default()
    }
}

fn retriever(check_error: Option<&str>) -> Arc<dyn KnowledgeRetriever> {
    Arc::new(StubRetriever {
        check_error: check_error.map(str::to_owned),
    })
}

#[test]
fn tool_requires_query_and_offers_top_k() {
    let tool = search_knowledge_tool(5);

    assert_eq!(tool.name, "search_knowledge");
    assert!(!tool.description.is_empty());
    assert_eq!(tool.parameters["type"], "object");
    assert_eq!(tool.parameters["required"], json!(["query"]));
    assert_eq!(tool.parameters["properties"]["query"]["type"], "string");
    assert_eq!(tool.parameters["properties"]["top_k"]["type"], "integer");
    assert_eq!(tool.parameters["properties"]["top_k"]["maximum"], 5);
}

#[test]
fn iteration_cap_is_max_calls_plus_two() {
    let limits = KnowledgeLimits::from_config(&enabled());

    assert_eq!(
        limits,
        KnowledgeLimits {
            max_calls: 2,
            top_k: 4,
            max_chunk_chars: 10,
        }
    );
    assert_eq!(limits.max_iterations(), 4);
    let huge = KnowledgeLimits {
        max_calls: u32::MAX,
        ..limits
    };
    assert_eq!(huge.max_iterations(), u32::MAX);
}

#[test]
fn parameters_are_built_only_when_enabled_and_servable() {
    let tenant = Uuid::new_v4();
    let ok = retriever(None);

    let turn = KnowledgeTurn::for_request(&enabled(), Some(&ok), tenant).unwrap();
    assert_eq!(turn.tenant_id, tenant);
    assert_eq!(turn.limits.max_calls, 2);

    let disabled = KnowledgeSearchConfig {
        enabled: false,
        ..enabled()
    };
    assert!(KnowledgeTurn::for_request(&disabled, Some(&ok), tenant).is_none());
    assert!(KnowledgeTurn::for_request(&enabled(), None, tenant).is_none());
    let unservable = retriever(Some("provider 'kb' has no api_version"));
    assert!(KnowledgeTurn::for_request(&enabled(), Some(&unservable), tenant).is_none());
}

#[test]
fn arguments_cap_top_k_and_require_a_query() {
    let parse = |arguments: Value| parse_arguments(&arguments.to_string(), 3);

    assert_eq!(
        parse(json!({ "query": " revenue ", "top_k": 50 })),
        Some(SearchArguments {
            query: "revenue".to_owned(),
            top_k: 3,
        })
    );
    assert_eq!(parse(json!({ "query": "q", "top_k": 2 })).unwrap().top_k, 2);
    assert_eq!(
        parse(json!({ "query": "q", "top_k": "1" })).unwrap().top_k,
        1
    );
    for default in [
        json!({ "query": "q" }),
        json!({ "query": "q", "top_k": 0 }),
        json!({ "query": "q", "top_k": -4 }),
        json!({ "query": "q", "top_k": "many" }),
        json!({ "query": "q", "top_k": null }),
    ] {
        assert_eq!(parse(default.clone()).unwrap().top_k, 3, "{default}");
    }
    for invalid in [
        json!({}),
        json!({ "query": "" }),
        json!({ "query": "   " }),
        json!({ "query": 5 }),
        json!(["query"]),
    ] {
        assert_eq!(parse(invalid.clone()), None, "{invalid}");
    }
    assert_eq!(parse_arguments("not json", 3), None);
    assert_eq!(parse_arguments(r#"{"query":"q"}"#, 0).unwrap().top_k, 1);
}

#[test]
fn chunks_are_trimmed_to_characters_not_bytes() {
    assert_eq!(trim_chunk("abcdef", 3), "abc");
    assert_eq!(trim_chunk("abc", 3), "abc");
    assert_eq!(trim_chunk("ab", 5), "ab");
    assert_eq!(
        trim_chunk("\u{e4}\u{f6}\u{fc}\u{df}\u{20ac}", 3),
        "\u{e4}\u{f6}\u{fc}"
    );
    assert_eq!(trim_chunk("abc", 0), "");
}

#[test]
fn results_output_keeps_top_k_trimmed_passages_with_sources() {
    let chunks: Vec<KnowledgeChunk> = (1..=4)
        .map(|i| KnowledgeChunk {
            text: format!("passage-{i}-TAIL"),
            filename: (i != 2).then(|| format!("doc-{i}.md")),
            score: Some(0.5),
        })
        .collect();

    let output = results_output(&chunks, 3, 9);

    let parsed: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(
        parsed,
        json!({
            "results": [
                { "index": 1, "text": "passage-1", "source": "doc-1.md" },
                { "index": 2, "text": "passage-2" },
                { "index": 3, "text": "passage-3", "source": "doc-3.md" },
            ]
        })
    );
    assert!(!output.contains("TAIL"));
}

#[test]
fn empty_results_and_error_outputs_explain_themselves() {
    let empty: Value = serde_json::from_str(&results_output(&[], 3, 10)).unwrap();
    assert_eq!(empty["results"], json!([]));
    assert_eq!(empty["message"], NO_RESULTS_MESSAGE);

    assert!(
        limit_reached_output()
            .to_lowercase()
            .contains("search limit reached")
    );
    assert!(search_failed_output().contains(SEARCH_FAILED_MESSAGE));
    assert!(invalid_arguments_output().contains("invalid arguments"));
}
