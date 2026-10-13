//! Knowledge-search agentic loop (DESIGN section 4 "Knowledge Search",
//! section 3.3 streaming error codes `agentic_iterations_exceeded` and
//! `unexpected_tool_use`).

use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;

use super::*;
use crate::infra::llm::knowledge::{
    KnowledgeChunk, KnowledgeRetriever, KnowledgeTarget, SEARCH_LIMIT_REACHED,
};

/// Records searches; returns one chunk per call (or an error when `fail`).
#[derive(Default)]
struct FakeRetriever {
    searches: Mutex<Vec<(KnowledgeTarget, String, usize)>>,
    fail: bool,
}

impl FakeRetriever {
    fn searches(&self) -> Vec<(KnowledgeTarget, String, usize)> {
        self.searches.lock().unwrap().clone()
    }
}

#[async_trait]
impl KnowledgeRetriever for FakeRetriever {
    async fn search(
        &self,
        target: &KnowledgeTarget,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<KnowledgeChunk>, ProviderError> {
        self.searches
            .lock()
            .unwrap()
            .push((target.clone(), query.to_owned(), top_k));
        if self.fail {
            return Err(ProviderError::provider("search down"));
        }
        Ok(vec![KnowledgeChunk {
            filename: "hr-policy.pdf".to_owned(),
            score: Some(0.9),
            text: format!("Policy text for {query}"),
        }])
    }
}

fn knowledge_opts(retriever: &Arc<FakeRetriever>, max_calls: u32) -> Opts {
    let mut o = Opts::default();
    let k = &mut o.cfg.knowledge_search;
    k.enabled = true;
    k.vector_store_id = Some("vs_knowledgebase0001".to_owned());
    k.provider_id = Some(KB_PROVIDER.to_owned());
    k.max_calls_per_message = max_calls;
    let r: Arc<dyn KnowledgeRetriever> = retriever.clone();
    o.knowledge = Some(r);
    o
}

fn function_call(call_id: &str, name: &str, arguments: &str) -> Step {
    Step::Ev(LlmEvent::FunctionCall {
        call_id: call_id.to_owned(),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    })
}

fn has_knowledge_tool(r: &LlmRequest) -> bool {
    r.tools
        .iter()
        .any(|t| matches!(t, ToolSpec::Function { name, .. } if name == "search_knowledge"))
}

#[tokio::test]
async fn function_call_without_knowledge_search_is_unexpected_tool_use() {
    let mut fx = fx().await;
    fx.llm.push(vec![
        function_call("call_1", "search_knowledge", r#"{"query":"x"}"#),
        Step::Ev(completed(9, 2)),
    ]);
    let req = fx.req("hi");
    let rid = req.request_id.unwrap();
    let evs = collect(live(fx.send(req).await)).await;
    assert_eq!(error_of(&evs).0, "unexpected_tool_use");
    assert!(!has_knowledge_tool(&fx.llm.last_request()));
    assert_eq!(fx.llm.calls(), 1);
    let turn = fx.turn(rid).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("unexpected_tool_use"));
    let usage = fx.usage_event().await;
    assert_eq!(usage["billing_outcome"], "failed");
    assert_eq!(usage["settlement_method"], "actual");
}

#[tokio::test]
async fn one_retrieval_then_answer() {
    let retriever = Arc::new(FakeRetriever::default());
    let mut fx = fx_with(knowledge_opts(&retriever, 3)).await;
    fx.llm.push(vec![
        function_call(
            "call_1",
            "search_knowledge",
            r#"{"query":"vacation days","top_k":9}"#,
        ),
        Step::Ev(completed(10, 2)),
    ]);
    fx.llm
        .push(vec![delta("You get 25 days."), Step::Ev(completed(30, 5))]);
    let req = fx.req("how many vacation days?");
    let rid = req.request_id.unwrap();
    let evs = collect(live(fx.send(req).await)).await;

    assert_eq!(names(&evs), ["stream_started", "delta", "done"]);
    assert_eq!(evs[1], text_delta("You get 25 days."));
    assert_eq!(
        done(&evs).usage,
        UsageCounts {
            input_tokens: 30,
            output_tokens: 5
        },
        "the final iteration's usage"
    );
    assert_eq!(fx.llm.calls(), 2);

    let requests = fx.llm.requests.lock().unwrap().clone();
    assert!(has_knowledge_tool(&requests[0]));
    let guard = MiniChatConfig::default().knowledge_search.guard;
    assert!(requests[0].instructions.contains(&guard));
    let tail: Vec<InputItem> = requests[1]
        .input
        .iter()
        .rev()
        .take(2)
        .rev()
        .cloned()
        .collect();
    assert_eq!(
        tail[0],
        InputItem::FunctionCall {
            call_id: "call_1".to_owned(),
            name: "search_knowledge".to_owned(),
            arguments: r#"{"query":"vacation days","top_k":9}"#.to_owned(),
        }
    );
    let InputItem::FunctionCallOutput { call_id, output } = &tail[1] else {
        panic!("expected a function call output, got {:?}", tail[1]);
    };
    assert_eq!(call_id, "call_1");
    assert!(output.contains("Policy text for vacation days"), "{output}");

    let searches = retriever.searches();
    assert_eq!(searches.len(), 1);
    let (target, query, top_k) = &searches[0];
    assert_eq!(query, "vacation days");
    assert_eq!(*top_k, 5, "capped at knowledge_search.top_k");
    assert_eq!(target.alias, "kb.openai.azure.com");
    assert_eq!(target.api_version, "2025-04-01-preview");
    assert_eq!(target.vector_store_id, "vs_knowledgebase0001");

    let turn = fx.turn(rid).await;
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.file_search_completed_count, 1);
    let messages = fx.messages().await;
    assert_eq!(messages.last().unwrap().content, "You get 25 days.");
    let usage = fx.usage_event().await;
    assert_eq!(usage["file_search_calls"], 1);
}

#[tokio::test]
async fn iteration_cap_exceeded_fails_turn() {
    let retriever = Arc::new(FakeRetriever::default());
    let fx = fx_with(knowledge_opts(&retriever, 1)).await;
    for i in 0..3 {
        fx.llm.push(vec![
            function_call(&format!("call_{i}"), "search_knowledge", r#"{"query":"q"}"#),
            Step::Ev(completed(10, 2)),
        ]);
    }
    let req = fx.req("loop forever");
    let rid = req.request_id.unwrap();
    let evs = collect(live(fx.send(req).await)).await;
    assert_eq!(error_of(&evs).0, "agentic_iterations_exceeded");
    assert_eq!(fx.llm.calls(), 3, "max_calls_per_message + 2 iterations");
    assert_eq!(retriever.searches().len(), 1, "one retrieval allowed");
    let third = fx.llm.last_request();
    assert!(third.input.iter().any(|i| matches!(
        i,
        InputItem::FunctionCallOutput { call_id, output } if call_id == "call_1" && output == SEARCH_LIMIT_REACHED
    )));
    let turn = fx.turn(rid).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("agentic_iterations_exceeded")
    );
}

#[tokio::test]
async fn other_function_name_is_unexpected_tool_use() {
    let retriever = Arc::new(FakeRetriever::default());
    let fx = fx_with(knowledge_opts(&retriever, 3)).await;
    fx.llm.push(vec![
        function_call("call_1", "load_files", "{}"),
        Step::Ev(completed(10, 2)),
    ]);
    let evs = collect(live(fx.send(fx.req("hi")).await)).await;
    assert_eq!(error_of(&evs).0, "unexpected_tool_use");
    assert!(retriever.searches().is_empty());
}

#[tokio::test]
async fn failed_retrieval_is_reported_to_the_model_and_counted() {
    let retriever = Arc::new(FakeRetriever {
        fail: true,
        ..FakeRetriever::default()
    });
    let mut fx = fx_with(knowledge_opts(&retriever, 3)).await;
    fx.llm.push(vec![
        function_call("call_1", "search_knowledge", r#"{"query":"x"}"#),
        Step::Ev(completed(10, 2)),
    ]);
    fx.llm
        .push(vec![delta("Sorry."), Step::Ev(completed(12, 3))]);
    let req = fx.req("hi");
    let rid = req.request_id.unwrap();
    let evs = collect(live(fx.send(req).await)).await;
    assert_eq!(names(&evs), ["stream_started", "delta", "done"]);
    assert_eq!(fx.turn(rid).await.file_search_completed_count, 0);
    let usage = fx.usage_event().await;
    assert_eq!(
        usage["file_search_calls"], 1,
        "failed retrievals are counted"
    );
}

#[tokio::test]
async fn file_search_excludes_knowledge_search() {
    let retriever = Arc::new(FakeRetriever::default());
    let fx = fx_with(knowledge_opts(&retriever, 3)).await;
    fx.attachment("document", "ready", "file-AAAAAAAAAAAAAAAA")
        .await;
    fx.vector_store("vs_secretsecretsecret").await;
    let evs = collect(live(fx.send(fx.req("doc?")).await)).await;
    assert_eq!(names(&evs).last(), Some(&"done"));
    let r = fx.llm.last_request();
    assert!(
        r.tools
            .iter()
            .any(|t| matches!(t, ToolSpec::FileSearch { .. }))
    );
    assert!(!has_knowledge_tool(&r));
    let guard = MiniChatConfig::default().knowledge_search.guard;
    assert!(!r.instructions.contains(&guard));
}

#[tokio::test]
async fn enabled_without_retriever_offers_no_tool() {
    let retriever = Arc::new(FakeRetriever::default());
    let mut o = knowledge_opts(&retriever, 3);
    o.knowledge = None;
    let fx = fx_with(o).await;
    let evs = collect(live(fx.send(fx.req("hi")).await)).await;
    assert_eq!(names(&evs).last(), Some(&"done"));
    assert!(!has_knowledge_tool(&fx.llm.last_request()));
}
