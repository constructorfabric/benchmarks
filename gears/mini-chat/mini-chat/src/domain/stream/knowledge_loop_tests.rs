#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Knowledge-search agentic loop tests (DESIGN section 4 "Knowledge Search",
//! "Streaming error codes"): the stream service with a scripted fake LLM
//! provider and a fake knowledge retriever.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use sea_orm::ActiveValue::Set;
use serde_json::json;
use toolkit_db::secure::secure_insert;
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::tests::{
    Fixture, Script, completed, completion, drain, names, request, standard_credits,
};
use super::*;
use crate::config::KnowledgeSearchConfig;
use crate::domain::knowledge::{
    KnowledgeChunk, KnowledgeRetriever, KnowledgeSearchError, LIMIT_REACHED_MESSAGE,
    SEARCH_FAILED_MESSAGE, SEARCH_KNOWLEDGE_TOOL,
};
use crate::domain::llm::{LlmEvent, LlmFunctionCall};
use crate::domain::stream::events::{
    AGENTIC_ITERATIONS_EXCEEDED, StreamError, UNEXPECTED_TOOL_USE,
};
use crate::infra::db::entities::chat_vector_stores;
use crate::infra::db::repo::tests::{AttachmentSeed, TENANT_A, at, message_rows, seed_attachment};

const GUARD: &str = "KB-GUARD: consult search_knowledge first.";
const TOP_K: usize = 3;
const MAX_CHUNK_CHARS: usize = 12;

// ── Fake retriever ───────────────────────────────────────────────────────

/// Retriever answering scripted results (default: five long chunks) and
/// recording the searches; `hang` makes every search wait forever.
#[derive(Default)]
struct FakeRetriever {
    results: Mutex<VecDeque<Result<Vec<KnowledgeChunk>, KnowledgeSearchError>>>,
    searches: Mutex<Vec<(Uuid, String, usize)>>,
    check_error: Option<String>,
    hang: AtomicBool,
}

impl FakeRetriever {
    fn searches(&self) -> Vec<(Uuid, String, usize)> {
        self.searches.lock().unwrap().clone()
    }

    fn fail_next(&self) {
        self.results
            .lock()
            .unwrap()
            .push_back(Err(KnowledgeSearchError("HTTP 500".to_owned())));
    }
}

fn chunk(index: usize) -> KnowledgeChunk {
    KnowledgeChunk {
        text: format!("CHUNK-{index}-abcTAILCUT-and-more"),
        filename: Some(format!("handbook-{index}.md")),
        score: Some(0.9),
    }
}

#[async_trait]
impl KnowledgeRetriever for FakeRetriever {
    fn check(&self, _tenant_id: Uuid) -> Result<(), String> {
        self.check_error.clone().map_or(Ok(()), Err)
    }

    async fn search(
        &self,
        tenant_id: Uuid,
        query: &str,
        max_num_results: usize,
    ) -> Result<Vec<KnowledgeChunk>, KnowledgeSearchError> {
        self.searches
            .lock()
            .unwrap()
            .push((tenant_id, query.to_owned(), max_num_results));
        if self.hang.load(Ordering::SeqCst) {
            futures::future::pending::<()>().await;
        }
        let scripted = self.results.lock().unwrap().pop_front();
        scripted.unwrap_or_else(|| Ok((1..=5).map(chunk).collect()))
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

async fn kb_fixture(retriever: &Arc<FakeRetriever>, max_calls: u32) -> Fixture {
    let mut fx = Fixture::with_settings(|settings| {
        settings.progress_interval = Duration::ZERO;
        settings.knowledge_search = KnowledgeSearchConfig {
            enabled: true,
            vector_store_id: Some("vs_kb".to_owned()),
            provider_id: Some("kb".to_owned()),
            max_calls_per_message: max_calls,
            top_k: TOP_K,
            max_chunk_chars: MAX_CHUNK_CHARS,
            guard: GUARD.to_owned(),
        };
    })
    .await;
    fx.service = fx
        .service
        .with_knowledge_retriever(Arc::clone(retriever) as Arc<dyn KnowledgeRetriever>);
    fx
}

fn call(call_id: &str, name: &str, arguments: &serde_json::Value) -> LlmEvent {
    LlmEvent::FunctionCall(LlmFunctionCall {
        call_id: call_id.to_owned(),
        name: name.to_owned(),
        arguments: arguments.to_string(),
    })
}

/// A provider response that ends with one `search_knowledge` call.
fn search_response(call_id: &str, query: &str, usage: (i64, i64)) -> Script {
    Script::Events(vec![
        call(call_id, SEARCH_KNOWLEDGE_TOOL, &json!({ "query": query })),
        completion(Some(usage)),
    ])
}

fn error_of(events: &[StreamEvent]) -> &StreamError {
    match events.last() {
        Some(StreamEvent::Error(error)) => error,
        other => panic!("expected error last, got {other:?}"),
    }
}

fn outputs(request: &LlmRequest) -> Vec<(String, String)> {
    request
        .input
        .iter()
        .filter_map(|item| match item {
            LlmInputItem::FunctionCallOutput { call_id, output } => {
                Some((call_id.clone(), output.clone()))
            }
            _ => None,
        })
        .collect()
}

fn function_tools(request: &LlmRequest) -> Vec<&LlmTool> {
    request
        .tools
        .iter()
        .filter(|tool| matches!(tool, LlmTool::Function(_)))
        .collect()
}

async fn seed_ready_document(fx: &Fixture, chat_id: Uuid) {
    seed_attachment(
        &fx.store,
        chat_id,
        TENANT_A,
        AttachmentSeed::new(
            AttachmentKind::Document,
            AttachmentStatus::Ready,
            "policy.pdf",
        ),
    )
    .await;
    let store = chat_vector_stores::ActiveModel {
        id: Set(Uuid::new_v4()),
        tenant_id: Set(TENANT_A),
        chat_id: Set(chat_id),
        vector_store_id: Set(Some("vs_chat".to_owned())),
        provider: Set("openai".to_owned()),
        file_count: Set(0),
        created_at: Set(fx.store.stamp(at(-500))),
    };
    secure_insert::<chat_vector_stores::Entity>(store, &AccessScope::allow_all(), &fx.store.conn())
        .await
        .unwrap();
}

// ── Tool and guard inclusion ─────────────────────────────────────────────

#[tokio::test]
async fn search_knowledge_tool_and_guard_are_sent_when_enabled() {
    let retriever = Arc::new(FakeRetriever::default());
    let fx = kb_fixture(&retriever, 2).await;
    let chat_id = fx.chat("standard").await;

    let events = drain(fx.live(chat_id, request("travel policy?")).await).await;

    assert_eq!(names(&events).last(), Some(&"done"));
    let sent = &fx.provider.requests()[0];
    assert_eq!(
        sent.tools,
        vec![LlmTool::Function(
            crate::domain::knowledge::search_knowledge_tool(TOP_K)
        )]
    );
    assert!(sent.instructions.contains(GUARD), "{}", sent.instructions);
    assert_eq!(sent.metadata.feature, "none");
    assert!(retriever.searches().is_empty());
}

#[tokio::test]
async fn ready_documents_make_file_search_win_over_knowledge_search() {
    let retriever = Arc::new(FakeRetriever::default());
    let fx = kb_fixture(&retriever, 2).await;
    let chat_id = fx.chat("standard").await;
    seed_ready_document(&fx, chat_id).await;

    drain(fx.live(chat_id, request("what does the policy say?")).await).await;

    let sent = &fx.provider.requests()[0];
    assert!(
        sent.tools
            .iter()
            .any(|tool| matches!(tool, LlmTool::FileSearch { .. })),
        "{:?}",
        sent.tools
    );
    assert!(function_tools(sent).is_empty(), "{:?}", sent.tools);
    assert!(!sent.instructions.contains(GUARD), "{}", sent.instructions);
}

#[tokio::test]
async fn knowledge_is_off_when_its_parameters_cannot_be_built() {
    let retriever = Arc::new(FakeRetriever {
        check_error: Some("provider 'kb' has no api_version".to_owned()),
        ..FakeRetriever::default()
    });
    let fx = kb_fixture(&retriever, 2).await;
    let chat_id = fx.chat("standard").await;
    fx.provider.enqueue(search_response("call_1", "q", (10, 2)));

    let events = drain(fx.live(chat_id, request("look it up")).await).await;

    let sent = &fx.provider.requests()[0];
    assert!(function_tools(sent).is_empty(), "{:?}", sent.tools);
    assert!(!sent.instructions.contains(GUARD));
    assert_eq!(error_of(&events).code, UNEXPECTED_TOOL_USE);
    assert!(retriever.searches().is_empty());
    assert_eq!(fx.provider.requests().len(), 1);
}

// ── Agentic loop ─────────────────────────────────────────────────────────

#[tokio::test]
async fn search_call_runs_the_retrieval_and_a_follow_up_request() {
    let retriever = Arc::new(FakeRetriever::default());
    let fx = kb_fixture(&retriever, 2).await;
    let chat_id = fx.chat("standard").await;
    fx.provider.enqueue(Script::Events(vec![
        LlmEvent::TextDelta("Let me check. ".to_owned()),
        call(
            "call_1",
            SEARCH_KNOWLEDGE_TOOL,
            &json!({ "query": "revenue", "top_k": 50 }),
        ),
        completion(Some((100, 10))),
    ]));
    fx.provider.enqueue(Script::Events(completed(
        &["Revenue grew."],
        Some((200, 20)),
    )));

    let live = fx.live(chat_id, request("how did revenue develop?")).await;
    let message_id = live.first.message_id;
    let events = drain(live).await;

    assert_eq!(names(&events), ["stream_started", "delta", "delta", "done"]);
    let StreamEvent::Done(done) = events.last().unwrap() else {
        panic!("done expected");
    };
    assert_eq!(
        (done.usage.input_tokens, done.usage.output_tokens),
        (200, 20),
        "only the final iteration's usage is reported"
    );
    assert_eq!(
        retriever.searches(),
        [(TENANT_A, "revenue".to_owned(), TOP_K)],
        "top_k is capped at knowledge_search.top_k"
    );

    let requests = fx.provider.requests();
    assert_eq!(requests.len(), 2);
    let follow_up = &requests[1];
    assert_eq!(follow_up.tools, requests[0].tools);
    assert_eq!(follow_up.instructions, requests[0].instructions);
    let tail = &follow_up.input[follow_up.input.len() - 3..];
    assert_eq!(
        tail[0],
        LlmInputItem::Assistant {
            text: "Let me check. ".to_owned()
        }
    );
    assert_eq!(
        tail[1],
        LlmInputItem::FunctionCall(LlmFunctionCall {
            call_id: "call_1".to_owned(),
            name: SEARCH_KNOWLEDGE_TOOL.to_owned(),
            arguments: json!({ "query": "revenue", "top_k": 50 }).to_string(),
        })
    );
    let LlmInputItem::FunctionCallOutput { call_id, output } = &tail[2] else {
        panic!("function_call_output expected, got {:?}", tail[2]);
    };
    assert_eq!(call_id, "call_1");
    let parsed: serde_json::Value = serde_json::from_str(output).unwrap();
    let results = parsed["results"].as_array().unwrap();
    assert_eq!(results.len(), TOP_K);
    assert_eq!(results[0]["text"], "CHUNK-1-abcT");
    assert_eq!(results[0]["source"], "handbook-1.md");
    assert!(!output.contains("TAILCUT"), "{output}");

    let turn = fx.turn(chat_id).await;
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.file_search_completed_count, 1);
    let assistant = message_rows(&fx.store, chat_id)
        .await
        .into_iter()
        .find(|m| m.id == message_id)
        .unwrap();
    assert_eq!(assistant.content, "Let me check. Revenue grew.");
    assert_eq!((assistant.input_tokens, assistant.output_tokens), (200, 20));
    let usage = fx.store.wait_delivered(&fx.queues.usage, 1).await;
    assert_eq!(usage[0].payload["file_search_calls"], 1);
    assert_eq!(
        usage[0].payload["actual_credits_micro"],
        standard_credits(200, 20)
    );
    let audit = fx.store.wait_delivered(&fx.queues.audit, 1).await;
    assert_eq!(audit[0].payload["tool_calls"]["file_search_calls"], 1);
}

#[tokio::test]
async fn completed_count_is_persisted_while_the_loop_runs() {
    let retriever = Arc::new(FakeRetriever::default());
    let fx = kb_fixture(&retriever, 2).await;
    let chat_id = fx.chat("standard").await;
    fx.provider.enqueue(search_response("call_1", "q", (10, 1)));
    let feed = fx.provider.channel();
    let live = fx.live(chat_id, request("search once")).await;

    let mut running = fx.turn(chat_id).await;
    for _ in 0..200 {
        if running.file_search_completed_count == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        running = fx.turn(chat_id).await;
    }
    assert_eq!(running.state, "running");
    assert_eq!(running.file_search_completed_count, 1);

    feed.send(LlmEvent::TextDelta("Done.".to_owned())).unwrap();
    feed.send(completion(Some((20, 2)))).unwrap();
    let events = drain(live).await;
    assert_eq!(names(&events).last(), Some(&"done"));
}

#[tokio::test]
async fn calls_beyond_max_calls_per_message_get_a_search_limit_output() {
    let retriever = Arc::new(FakeRetriever::default());
    let fx = kb_fixture(&retriever, 1).await;
    let chat_id = fx.chat("standard").await;
    fx.provider
        .enqueue(search_response("call_a", "first", (10, 1)));
    fx.provider
        .enqueue(search_response("call_b", "second", (20, 2)));
    fx.provider.enqueue(Script::Events(completed(
        &["From what I have."],
        Some((30, 3)),
    )));

    let events = drain(fx.live(chat_id, request("search a lot")).await).await;

    assert_eq!(names(&events).last(), Some(&"done"));
    assert_eq!(
        retriever.searches().len(),
        1,
        "at most max_calls retrievals"
    );
    let requests = fx.provider.requests();
    assert_eq!(requests.len(), 3);
    let last = outputs(&requests[2]);
    assert_eq!(last.len(), 2, "earlier calls and outputs stay in the input");
    assert_eq!(last[0].0, "call_a");
    assert_eq!(last[1].0, "call_b");
    assert!(last[1].1.contains(LIMIT_REACHED_MESSAGE), "{}", last[1].1);
    assert!(last[1].1.to_lowercase().contains("search limit reached"));
    let turn = fx.turn(chat_id).await;
    assert_eq!(
        (turn.state.as_str(), turn.file_search_completed_count),
        ("completed", 1)
    );
}

#[tokio::test]
async fn the_iteration_cap_fails_the_turn_with_agentic_iterations_exceeded() {
    let retriever = Arc::new(FakeRetriever::default());
    let fx = kb_fixture(&retriever, 1).await;
    let chat_id = fx.chat("standard").await;
    // max_calls_per_message + 2 = 3 provider requests, each asking again.
    for (index, usage) in [(10, 1), (20, 2), (30, 3)].into_iter().enumerate() {
        fx.provider
            .enqueue(search_response(&format!("call_{index}"), "again", usage));
    }

    let events = drain(fx.live(chat_id, request("never stop")).await).await;

    let error = error_of(&events);
    assert_eq!(error.code, AGENTIC_ITERATIONS_EXCEEDED);
    assert_eq!(fx.provider.requests().len(), 3);
    assert_eq!(retriever.searches().len(), 1);
    let turn = fx.turn(chat_id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some(AGENTIC_ITERATIONS_EXCEEDED)
    );
    let usage = fx.store.wait_delivered(&fx.queues.usage, 1).await;
    assert_eq!(usage[0].payload["settlement_method"], "actual");
    assert_eq!(
        usage[0].payload["actual_credits_micro"],
        standard_credits(30, 3),
        "the last iteration's usage is settled"
    );
}

#[tokio::test]
async fn an_unknown_function_with_knowledge_on_is_unexpected_tool_use() {
    let retriever = Arc::new(FakeRetriever::default());
    let fx = kb_fixture(&retriever, 2).await;
    let chat_id = fx.chat("standard").await;
    fx.provider.enqueue(Script::Events(vec![
        call("call_1", SEARCH_KNOWLEDGE_TOOL, &json!({ "query": "q" })),
        call("call_2", "drop_all_tables", &json!({})),
        completion(Some((10, 2))),
    ]));

    let events = drain(fx.live(chat_id, request("do something odd")).await).await;

    assert_eq!(error_of(&events).code, UNEXPECTED_TOOL_USE);
    assert!(retriever.searches().is_empty());
    assert_eq!(fx.provider.requests().len(), 1);
    let turn = fx.turn(chat_id).await;
    assert_eq!(turn.error_code.as_deref(), Some(UNEXPECTED_TOOL_USE));
}

#[tokio::test]
async fn a_failed_retrieval_is_reported_to_the_model_and_counted_as_a_call() {
    let retriever = Arc::new(FakeRetriever::default());
    retriever.fail_next();
    let fx = kb_fixture(&retriever, 2).await;
    let chat_id = fx.chat("standard").await;
    fx.provider.enqueue(search_response("call_1", "q", (10, 1)));
    fx.provider
        .enqueue(Script::Events(completed(&["No KB."], Some((20, 2)))));

    let events = drain(fx.live(chat_id, request("look it up")).await).await;

    assert_eq!(names(&events).last(), Some(&"done"));
    let follow_up = outputs(&fx.provider.requests()[1]);
    assert!(
        follow_up[0].1.contains(SEARCH_FAILED_MESSAGE),
        "{follow_up:?}"
    );
    assert!(!follow_up[0].1.contains("HTTP 500"), "{follow_up:?}");
    let turn = fx.turn(chat_id).await;
    assert_eq!(turn.file_search_completed_count, 0);
    let usage = fx.store.wait_delivered(&fx.queues.usage, 1).await;
    assert_eq!(usage[0].payload["file_search_calls"], 1);
}

#[tokio::test]
async fn unusable_arguments_get_an_error_output_without_a_retrieval() {
    let retriever = Arc::new(FakeRetriever::default());
    let fx = kb_fixture(&retriever, 2).await;
    let chat_id = fx.chat("standard").await;
    fx.provider.enqueue(Script::Events(vec![
        call("call_1", SEARCH_KNOWLEDGE_TOOL, &json!({ "top_k": 2 })),
        completion(Some((10, 1))),
    ]));

    let events = drain(fx.live(chat_id, request("look it up")).await).await;

    assert_eq!(names(&events).last(), Some(&"done"));
    assert!(retriever.searches().is_empty());
    let follow_up = outputs(&fx.provider.requests()[1]);
    assert!(
        follow_up[0].1.contains("invalid arguments"),
        "{follow_up:?}"
    );
}

#[tokio::test]
async fn a_disconnect_during_the_retrieval_cancels_the_turn() {
    let retriever = Arc::new(FakeRetriever::default());
    retriever.hang.store(true, Ordering::SeqCst);
    let fx = kb_fixture(&retriever, 2).await;
    let chat_id = fx.chat("standard").await;
    fx.provider.enqueue(Script::Events(vec![
        LlmEvent::TextDelta("Partial ".to_owned()),
        call("call_1", SEARCH_KNOWLEDGE_TOOL, &json!({ "query": "q" })),
        completion(Some((10, 1))),
    ]));
    let mut live = fx.live(chat_id, request("look it up")).await;
    let event = tokio::time::timeout(Duration::from_secs(5), live.rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.name(), "delta");
    for _ in 0..200 {
        if !retriever.searches().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(retriever.searches().len(), 1);

    live.cancel.cancel();
    drop(live);
    let turn = fx.wait_terminal(chat_id).await;

    assert_eq!(turn.state, "cancelled");
    assert_eq!(fx.provider.requests().len(), 1, "no follow-up request");
    let assistant = message_rows(&fx.store, chat_id)
        .await
        .into_iter()
        .find(|m| m.role == "assistant")
        .unwrap();
    assert_eq!(assistant.content, "Partial ");
}

#[tokio::test]
async fn a_not_sent_failure_after_a_sent_iteration_is_a_provider_error() {
    let retriever = Arc::new(FakeRetriever::default());
    let fx = kb_fixture(&retriever, 2).await;
    let chat_id = fx.chat("standard").await;
    fx.provider
        .enqueue(search_response("call_1", "first", (10, 2)));
    fx.provider
        .enqueue(Script::Fail(crate::domain::llm::LlmFailure::not_sent(
            "S2S context unavailable",
        )));

    let events = drain(fx.live(chat_id, request("look it up")).await).await;

    assert_eq!(error_of(&events).code, "provider_error");
    assert_eq!(fx.provider.requests().len(), 2);
    assert_eq!(retriever.searches().len(), 1);
    let turn = fx.turn(chat_id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("provider_error"),
        "the first iteration reached the provider: not turn_setup_failed"
    );
    let usage = fx.store.wait_delivered(&fx.queues.usage, 1).await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].payload["terminal_state"], "failed");
    assert_eq!(
        usage[0].payload["settlement_method"], "estimated",
        "a sent request without reported usage is settled estimated, not released"
    );
    let audit = fx.store.wait_delivered(&fx.queues.audit, 1).await;
    assert_eq!(audit[0].payload["error_code"], "provider_error");
}
