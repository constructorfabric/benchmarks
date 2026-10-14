//! The knowledge search loop over the real router, SQLite, the real quota service, the real
//! provider task and a mock OAGW with a scripted retriever: a `search_knowledge` call is answered
//! and the next provider request carries the call and its output, the tools stay the same,
//! successful retrievals count in `chat_turns.file_search_completed_count` and in the usage
//! event, only the final iteration is settled, the fourth call with the default limit gets the
//! limit output, the sixth provider iteration is never issued and the stream ends with SSE
//! `error{code: "agentic_iterations_exceeded"}`, and a call of another tool name ends the turn with
//! `unexpected_tool_use`; a failed retrieval is answered with a failure output.
//!
//! Slice S2-deletion-cleanup-tools of `cpt-cf-mini-chat-feature-attachments-and-file-search`:
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop` (`inst-kl-*`),
//! flow `cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files`
//! (`inst-ask-knowledge`, `inst-ask-knowledge-loop`), algorithm `-metrics` (`inst-met-knowledge`);
//! definition of done `cpt-cf-mini-chat-dod-attachments-and-file-search-knowledge-loop`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use time::Duration as TimeDuration;

use crate::config::KnowledgeSearchConfig;
use crate::domain::attachments::knowledge::{KnowledgeLoopSettings, RetrievalFailure};
use crate::domain::streaming::RelayEvent;
use crate::infra::provider_task::ProviderTaskSettings;
use crate::test_support::attach_fakes::RecordingAttachmentMetrics;
use crate::test_support::provider::{MockProvider, Reply, Step, happy_steps, tool_use_steps};
use crate::test_support::quota::{FixedClock, default_now};
use crate::test_support::send::{
    CC_MODEL, KnowledgeLoopSpec, SendRig, SendRigOptions, VLLM_MODEL, multi_catalog,
    multi_providers, names, parse_sse, rig_task_settings, send_rig_with, turn_rows,
};
use crate::test_support::tool_fakes::{ScriptedRetriever, chunk, knowledge_config, tool_settings};

struct Harness {
    rig: SendRig,
    upstream: Arc<MockProvider>,
    retriever: Arc<ScriptedRetriever>,
    metrics: Arc<RecordingAttachmentMetrics>,
}

async fn harness_with(config: KnowledgeSearchConfig) -> Harness {
    let settings = KnowledgeLoopSettings::from_config(&config);
    harness_on(config, settings, SendRigOptions::new()).await
}

async fn harness_on(
    config: KnowledgeSearchConfig,
    settings: KnowledgeLoopSettings,
    options: SendRigOptions,
) -> Harness {
    let upstream = MockProvider::new();
    let retriever = ScriptedRetriever::new();
    let metrics = RecordingAttachmentMetrics::new();
    let rig = send_rig_with(
        options
            .with_upstream(upstream.clone())
            .with_attachment_tools(tool_settings(config))
            .with_knowledge_loop(KnowledgeLoopSpec {
                retriever: retriever.clone(),
                settings,
                metrics: metrics.clone(),
            }),
    )
    .await;
    Harness {
        rig,
        upstream,
        retriever,
        metrics,
    }
}

async fn harness() -> Harness {
    harness_with(knowledge_config()).await
}

fn tool_use(call_id: &str, query: &str, usage: (u64, u64)) -> Reply {
    Reply::Sse(tool_use_steps(
        "search_knowledge",
        call_id,
        &json!({ "query": query }),
        usage,
    ))
}

fn answer(text: &str, usage: (u64, u64)) -> Reply {
    Reply::Sse(happy_steps(&[text], usage))
}

fn input_items(call: &crate::test_support::provider::RecordedRequest) -> Vec<Value> {
    call.body["input"].as_array().cloned().unwrap_or_default()
}

fn question() -> Value {
    json!({ "content": "how many vacation days do I get?" })
}

// ── one search ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_search_knowledge_call_is_answered_and_the_next_request_carries_the_call_and_its_output()
{
    // inst-kl-retrieve, inst-kl-success, inst-kl-append, inst-kl-next
    let h = harness().await;
    h.retriever
        .queue(Ok(vec![chunk("vacation: 25 days"), chunk("carry over: 5")]));
    h.upstream
        .queue(tool_use("call_1", "vacation policy", (100, 20)));
    h.upstream.queue(answer("You get 25 days.", (210, 30)));
    let chat = h.rig.std_chat().await;
    let reply = h.rig.stream(chat.id, &question()).await;
    assert_eq!(reply.status, axum::http::StatusCode::OK, "{}", reply.text);
    let frames = parse_sse(&reply.text);
    assert_eq!(names(&frames).last(), Some(&"done"), "{}", reply.text);
    assert!(reply.text.contains("You get 25 days."));

    let calls = h.upstream.calls();
    assert_eq!(
        calls.len(),
        2,
        "one more provider request after the tool call"
    );
    assert_eq!(
        calls[0].body["tools"], calls[1].body["tools"],
        "the same tools"
    );
    let items = input_items(&calls[1]);
    let output = items.last().unwrap();
    assert_eq!(output["type"], "function_call_output");
    assert_eq!(output["call_id"], "call_1");
    let text = output["output"].as_str().unwrap();
    assert!(
        text.contains("vacation: 25 days") && text.contains("carry over: 5"),
        "{text}"
    );
    let call = &items[items.len() - 2];
    assert_eq!(call["type"], "function_call");
    assert_eq!(call["name"], "search_knowledge");
    assert_eq!(call["call_id"], "call_1");
    assert_eq!(h.retriever.calls()[0].query, "vacation policy");
}

#[tokio::test]
async fn successful_retrievals_count_in_the_turn_and_only_the_final_iteration_is_settled() {
    // inst-kl-success, inst-kl-settle: the durable counter, `file_search_calls` in the usage event
    let h = harness().await;
    h.upstream.queue(tool_use("call_1", "a", (100, 20)));
    h.upstream.queue(tool_use("call_2", "b", (150, 25)));
    h.upstream.queue(answer("done", (210, 30)));
    let chat = h.rig.std_chat().await;
    let reply = h.rig.stream(chat.id, &question()).await;
    assert_eq!(
        names(&parse_sse(&reply.text)).last(),
        Some(&"done"),
        "{}",
        reply.text
    );
    let turn = h.rig.wait_terminal(chat.id).await;
    assert_eq!(turn.file_search_completed_count, 2);
    let events = h.rig.events().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].file_search_calls, 2);
    let usage = events[0].usage.expect("usage");
    assert_eq!(
        (usage.input_tokens, usage.output_tokens),
        (210, 30),
        "the final iteration only"
    );
    assert_eq!(turns_of(&h, chat.id).await, 1);
}

async fn turns_of(h: &Harness, chat_id: uuid::Uuid) -> usize {
    turn_rows(&h.rig.quota.db, chat_id).await.len()
}

#[tokio::test]
async fn the_knowledge_metrics_are_recorded_per_retrieval() {
    // inst-met-knowledge
    let h = harness().await;
    h.upstream.queue(tool_use("call_1", "a", (100, 20)));
    h.upstream.queue(answer("done", (210, 30)));
    let chat = h.rig.std_chat().await;
    h.rig.stream(chat.id, &question()).await;
    assert_eq!(h.metrics.knowledge(), vec!["ok".to_owned()]);
    assert_eq!(h.metrics.knowledge_chunks(), vec![2]);
    assert_eq!(h.metrics.knowledge_latencies(), 1);
}

// ── the limit and the cap ────────────────────────────────────────────────────

#[tokio::test]
async fn the_fourth_call_gets_the_limit_output_and_the_model_answers_from_what_it_has() {
    // inst-kl-limit-output
    let h = harness().await;
    for n in 1..=4 {
        h.upstream
            .queue(tool_use(&format!("call_{n}"), &format!("q{n}"), (100, 20)));
    }
    h.upstream.queue(answer("final answer", (210, 30)));
    let chat = h.rig.std_chat().await;
    let reply = h.rig.stream(chat.id, &question()).await;
    assert_eq!(
        names(&parse_sse(&reply.text)).last(),
        Some(&"done"),
        "{}",
        reply.text
    );
    assert_eq!(
        h.retriever.calls().len(),
        3,
        "three retrievals, the fourth call is not retrieved"
    );
    let calls = h.upstream.calls();
    assert_eq!(calls.len(), 5);
    let output = input_items(&calls[4]).last().cloned().unwrap();
    assert_eq!(output["call_id"], "call_4");
    assert!(
        output["output"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("search limit reached"),
        "{output}"
    );
    let turn = h.rig.wait_terminal(chat.id).await;
    assert_eq!(turn.file_search_completed_count, 3);
}

#[tokio::test]
async fn the_sixth_provider_iteration_is_never_issued_and_the_stream_ends_with_the_iteration_error()
{
    // inst-kl-cap, inst-kl-iterations: hard cap `max_calls_per_message` plus 2
    let h = harness().await;
    for n in 1..=6 {
        h.upstream
            .queue(tool_use(&format!("call_{n}"), &format!("q{n}"), (100, 20)));
    }
    let chat = h.rig.std_chat().await;
    let reply = h.rig.stream(chat.id, &question()).await;
    let frames = parse_sse(&reply.text);
    assert_eq!(names(&frames).last(), Some(&"error"), "{}", reply.text);
    let error = &frames.last().unwrap().data;
    assert_eq!(error["code"], "agentic_iterations_exceeded");
    assert_eq!(
        h.upstream.call_count(),
        5,
        "the sixth request is never sent"
    );
    let turn = h.rig.wait_terminal(chat.id).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("agentic_iterations_exceeded")
    );
    assert_eq!(turn.file_search_completed_count, 3);
}

#[tokio::test]
async fn a_lower_limit_moves_the_cap() {
    // `knowledge_search.max_calls_per_message` 1: cap 3 iterations
    let config = KnowledgeSearchConfig {
        max_calls_per_message: 1,
        ..knowledge_config()
    };
    let h = harness_with(config).await;
    for n in 1..=4 {
        h.upstream
            .queue(tool_use(&format!("call_{n}"), "q", (100, 20)));
    }
    let chat = h.rig.std_chat().await;
    let reply = h.rig.stream(chat.id, &question()).await;
    let frames = parse_sse(&reply.text);
    assert_eq!(
        frames.last().unwrap().data["code"],
        "agentic_iterations_exceeded"
    );
    assert_eq!(h.upstream.call_count(), 3);
    assert_eq!(h.retriever.calls().len(), 1);
}

// ── the unexpected tool ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_call_of_another_tool_name_ends_the_turn_with_unexpected_tool_use() {
    // inst-kl-unexpected
    let h = harness().await;
    h.upstream.queue(Reply::Sse(tool_use_steps(
        "get_weather",
        "call_x",
        &json!({ "city": "Oslo" }),
        (100, 20),
    )));
    let chat = h.rig.std_chat().await;
    let reply = h.rig.stream(chat.id, &question()).await;
    let frames = parse_sse(&reply.text);
    assert_eq!(names(&frames).last(), Some(&"error"), "{}", reply.text);
    assert_eq!(frames.last().unwrap().data["code"], "unexpected_tool_use");
    assert_eq!(h.upstream.call_count(), 1);
    assert!(h.retriever.calls().is_empty());
    let turn = h.rig.wait_terminal(chat.id).await;
    assert_eq!(turn.error_code.as_deref(), Some("unexpected_tool_use"));
    assert_eq!(turn.file_search_completed_count, 0);
}

#[tokio::test]
async fn an_unexpected_tool_after_a_search_ends_the_turn_too() {
    let h = harness().await;
    h.upstream.queue(tool_use("call_1", "q", (100, 20)));
    h.upstream.queue(Reply::Sse(tool_use_steps(
        "web_search",
        "call_2",
        &json!({}),
        (110, 20),
    )));
    let chat = h.rig.std_chat().await;
    let reply = h.rig.stream(chat.id, &question()).await;
    assert_eq!(
        parse_sse(&reply.text).last().unwrap().data["code"],
        "unexpected_tool_use"
    );
    assert_eq!(h.upstream.call_count(), 2);
}

// ── a failed retrieval ───────────────────────────────────────────────────────

#[tokio::test]
async fn a_failed_retrieval_is_answered_with_a_failure_output_and_the_turn_goes_on() {
    // inst-kl-failure: no counter increment, `result="error"`
    let h = harness().await;
    h.retriever.queue(Err(RetrievalFailure::Transport));
    h.upstream.queue(tool_use("call_1", "q", (100, 20)));
    h.upstream
        .queue(answer("sorry, I could not search", (210, 30)));
    let chat = h.rig.std_chat().await;
    let reply = h.rig.stream(chat.id, &question()).await;
    assert_eq!(
        names(&parse_sse(&reply.text)).last(),
        Some(&"done"),
        "{}",
        reply.text
    );
    let output = input_items(&h.upstream.calls()[1]).last().cloned().unwrap();
    assert_eq!(output["type"], "function_call_output");
    assert!(!output["output"].as_str().unwrap().is_empty());
    let turn = h.rig.wait_terminal(chat.id).await;
    assert_eq!(
        turn.file_search_completed_count, 0,
        "not a completed retrieval"
    );
    assert_eq!(h.metrics.knowledge(), vec!["error".to_owned()]);
}

// ── without the loop ─────────────────────────────────────────────────────────

#[tokio::test]
async fn without_knowledge_search_a_function_call_is_an_unexpected_tool_use() {
    // `knowledge_search.enabled` false: the tool is not offered, so any function call is unexpected
    let upstream = MockProvider::new();
    upstream.queue(tool_use("call_1", "q", (100, 20)));
    let rig = send_rig_with(SendRigOptions::new().with_upstream(upstream.clone())).await;
    let chat = rig.std_chat().await;
    let reply = rig.stream(chat.id, &question()).await;
    assert_eq!(
        parse_sse(&reply.text).last().unwrap().data["code"],
        "unexpected_tool_use"
    );
    assert_eq!(upstream.call_count(), 1);
}

// ── the usage of the last completed iteration is settled ─────────────────────

#[tokio::test]
async fn the_iteration_cap_settles_on_the_usage_of_the_last_iteration() {
    // inst-kl-cap: `failed` with `agentic_iterations_exceeded` and the usage of the last
    // iteration; the settlement table settles it `actual` when the provider reported usage
    let h = harness().await;
    for n in 1..=6 {
        h.upstream.queue(tool_use(
            &format!("call_{n}"),
            &format!("q{n}"),
            (100 + n, 20 + n),
        ));
    }
    let chat = h.rig.std_chat().await;
    let reply = h.rig.stream(chat.id, &question()).await;
    assert_eq!(
        parse_sse(&reply.text).last().unwrap().data["code"],
        "agentic_iterations_exceeded"
    );
    let turn = h.rig.wait_terminal(chat.id).await;
    assert_eq!(turn.state, "failed");
    let events = h.rig.events().await;
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.terminal_state, "failed");
    assert_eq!(event.settlement_method, "actual", "{event:?}");
    let usage = event.usage.expect("the usage of the last iteration");
    assert_eq!(
        (usage.input_tokens, usage.output_tokens),
        (105, 25),
        "the fifth (last completed) iteration"
    );
    assert_eq!(event.file_search_calls, 3);
    let audit = h.rig.audit_events().await;
    assert_eq!(audit.len(), 1);
}

#[tokio::test]
async fn an_unexpected_tool_after_a_search_settles_on_the_usage_of_that_iteration() {
    // the completed iteration ends with another tool name: `unexpected_tool_use`, `actual`
    let h = harness().await;
    h.upstream.queue(tool_use("call_1", "q", (100, 20)));
    h.upstream.queue(Reply::Sse(tool_use_steps(
        "web_search",
        "call_2",
        &json!({}),
        (110, 22),
    )));
    let chat = h.rig.std_chat().await;
    let reply = h.rig.stream(chat.id, &question()).await;
    assert_eq!(
        parse_sse(&reply.text).last().unwrap().data["code"],
        "unexpected_tool_use"
    );
    h.rig.wait_terminal(chat.id).await;
    let events = h.rig.events().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].settlement_method, "actual", "{:?}", events[0]);
    let usage = events[0].usage.expect("usage");
    assert_eq!((usage.input_tokens, usage.output_tokens), (110, 22));
}

#[tokio::test]
async fn an_unexpected_first_tool_settles_on_its_usage_too() {
    let h = harness().await;
    h.upstream.queue(Reply::Sse(tool_use_steps(
        "get_weather",
        "call_x",
        &json!({}),
        (100, 20),
    )));
    let chat = h.rig.std_chat().await;
    h.rig.stream(chat.id, &question()).await;
    h.rig.wait_terminal(chat.id).await;
    let events = h.rig.events().await;
    assert_eq!(events[0].settlement_method, "actual", "{:?}", events[0]);
    assert_eq!(events[0].usage.expect("usage").input_tokens, 100);
}

// ── the retrieval is bounded and cancellable ─────────────────────────────────

fn short_search(timeout: Duration) -> (KnowledgeSearchConfig, KnowledgeLoopSettings) {
    let config = knowledge_config();
    let settings = KnowledgeLoopSettings {
        search_timeout: timeout,
        ..KnowledgeLoopSettings::from_config(&config)
    };
    (config, settings)
}

#[tokio::test]
async fn a_cancelled_turn_ends_while_the_retriever_never_resolves() {
    // the retrieval is raced against the turn cancellation
    let (config, settings) = short_search(Duration::from_secs(300));
    let h = harness_on(config, settings, SendRigOptions::new()).await;
    h.retriever.hang_next(1);
    h.upstream.queue(tool_use("call_1", "q", (100, 20)));
    let chat = h.rig.std_chat().await;
    let mut turn = h.rig.start_turn(&chat, &question()).await;
    // the search is running
    for _ in 0..500 {
        if !h.retriever.calls().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(h.retriever.calls().len(), 1);
    turn.cancel.cancel();
    let rest = tokio::time::timeout(Duration::from_secs(5), turn.drain())
        .await
        .expect("the task ends within bounded time");
    assert!(rest.is_empty(), "{rest:?}");
    let row = h.rig.wait_terminal(chat.id).await;
    assert_eq!(row.state, "cancelled");
    assert_eq!(h.upstream.call_count(), 1, "no next provider request");
}

#[tokio::test]
async fn a_client_that_goes_away_ends_the_wait_for_the_retriever() {
    let (config, settings) = short_search(Duration::from_secs(300));
    let h = harness_on(config, settings, SendRigOptions::new()).await;
    h.retriever.hang_next(1);
    h.upstream.queue(tool_use("call_1", "q", (100, 20)));
    let chat = h.rig.std_chat().await;
    let turn = h.rig.start_turn(&chat, &question()).await;
    for _ in 0..500 {
        if !h.retriever.calls().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // the relay channel closes: no token fires
    drop(turn.events);
    let row = tokio::time::timeout(Duration::from_secs(5), h.rig.wait_terminal(chat.id))
        .await
        .expect("the turn is finalized within bounded time");
    assert_eq!(row.state, "cancelled");
    drop(turn.cancel);
}

#[tokio::test]
async fn a_retriever_slower_than_the_timeout_is_a_failed_retrieval_and_the_turn_goes_on() {
    // FEATURE inst-kl-failure: a timeout answers the call with a failure output
    let (config, settings) = short_search(Duration::from_millis(100));
    let h = harness_on(config, settings, SendRigOptions::new()).await;
    h.retriever.hang_next(1);
    h.upstream.queue(tool_use("call_1", "q", (100, 20)));
    h.upstream.queue(answer("sorry, no search", (210, 30)));
    let chat = h.rig.std_chat().await;
    let reply = h.rig.stream(chat.id, &question()).await;
    assert_eq!(
        names(&parse_sse(&reply.text)).last(),
        Some(&"done"),
        "{}",
        reply.text
    );
    let output = input_items(&h.upstream.calls()[1]).last().cloned().unwrap();
    assert_eq!(output["type"], "function_call_output");
    assert!(!output["output"].as_str().unwrap().is_empty());
    let turn = h.rig.wait_terminal(chat.id).await;
    assert_eq!(turn.state, "completed");
    assert_eq!(
        turn.file_search_completed_count, 0,
        "not a completed search"
    );
    assert_eq!(h.metrics.knowledge(), vec!["error".to_owned()]);
}

#[tokio::test]
async fn the_progress_is_refreshed_while_the_retriever_runs() {
    // the orphan watchdog must not finalize a live turn that waits for a slow search
    let clock = FixedClock::at(default_now());
    let (config, settings) = short_search(Duration::from_secs(300));
    let options = SendRigOptions::new()
        .with_clock(clock.clone())
        .with_task_settings(ProviderTaskSettings {
            progress_interval: Duration::from_millis(20),
            ..rig_task_settings()
        });
    let h = harness_on(config, settings, options).await;
    h.retriever.hang_next(1);
    h.upstream.queue(tool_use("call_1", "q", (100, 20)));
    let chat = h.rig.std_chat().await;
    let turn = h.rig.start_turn(&chat, &question()).await;
    let at = default_now() + TimeDuration::seconds(100);
    clock.set(at);
    let mut written = false;
    for _ in 0..300 {
        let row = turn_rows(&h.rig.quota.db, chat.id).await.remove(0);
        if row.last_progress_at == Some(at) {
            written = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        written,
        "the waiting retrieval did not refresh the progress"
    );
    turn.cancel.cancel();
    h.rig.wait_terminal(chat.id).await;
}

// ── Chat Completions ─────────────────────────────────────────────────────────

fn cc_chunk(delta: &Value, finish: Option<&str>) -> Step {
    Step::unnamed(&json!({
        "id": "chatcmpl-kl", "object": "chat.completion.chunk",
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }]
    }))
}

/// A Chat Completions iteration that ends with a `search_knowledge` tool call.
fn cc_tool_call(call_id: &str, query: &str, usage: (u64, u64)) -> Reply {
    Reply::Sse(vec![
        cc_chunk(
            &json!({ "role": "assistant", "tool_calls": [{
                "index": 0, "id": call_id, "type": "function",
                "function": { "name": "search_knowledge", "arguments": "" } }] }),
            None,
        ),
        cc_chunk(
            &json!({ "tool_calls": [{ "index": 0, "function": {
                "arguments": json!({ "query": query }).to_string() } }] }),
            None,
        ),
        cc_chunk(&json!({}), Some("tool_calls")),
        Step::unnamed(&json!({ "id": "chatcmpl-kl", "choices": [],
            "usage": { "prompt_tokens": usage.0, "completion_tokens": usage.1 } })),
        Step::Raw("data: [DONE]\n\n".to_owned()),
    ])
}

fn cc_answer(text: &str, usage: (u64, u64)) -> Reply {
    Reply::Sse(vec![
        cc_chunk(&json!({ "role": "assistant", "content": text }), None),
        cc_chunk(&json!({}), Some("stop")),
        Step::unnamed(&json!({ "id": "chatcmpl-kl", "choices": [],
            "usage": { "prompt_tokens": usage.0, "completion_tokens": usage.1 } })),
        Step::Raw("data: [DONE]\n\n".to_owned()),
    ])
}

async fn multi_harness() -> Harness {
    let config = knowledge_config();
    let settings = KnowledgeLoopSettings::from_config(&config);
    harness_on(
        config,
        settings,
        SendRigOptions::new()
            .with_snapshot(multi_catalog())
            .with_providers(multi_providers()),
    )
    .await
}

#[tokio::test]
async fn a_chat_completions_tool_call_is_answered_and_the_next_request_carries_it() {
    // inst-kl-append on `openai_chat_completions`: the assistant `tool_calls` message and the
    // `tool` message
    let h = multi_harness().await;
    h.retriever.queue(Ok(vec![chunk("vacation: 25 days")]));
    h.upstream
        .queue(cc_tool_call("call_cc1", "vacation", (100, 20)));
    h.upstream.queue(cc_answer("You get 25 days.", (210, 30)));
    let chat = h.rig.quota.chat_a1(CC_MODEL).await;
    let mut turn = h.rig.start_turn(&chat, &question()).await;
    let events = turn.drain().await;
    assert!(
        matches!(events.last(), Some(RelayEvent::Done(_))),
        "{events:?}"
    );
    assert_eq!(h.retriever.calls().len(), 1);
    assert_eq!(h.retriever.calls()[0].query, "vacation");
    let calls = h.upstream.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].body["tools"], calls[1].body["tools"]);
    let messages = calls[1].body["messages"].as_array().unwrap();
    let tool = messages.last().unwrap();
    assert_eq!(tool["role"], "tool");
    assert_eq!(tool["tool_call_id"], "call_cc1");
    assert!(
        tool["content"]
            .as_str()
            .unwrap()
            .contains("vacation: 25 days")
    );
    let assistant = &messages[messages.len() - 2];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["tool_calls"][0]["id"], "call_cc1");
    assert_eq!(assistant["tool_calls"][0]["type"], "function");
    assert_eq!(
        assistant["tool_calls"][0]["function"]["name"],
        "search_knowledge"
    );
    assert_eq!(
        assistant["tool_calls"][0]["function"]["arguments"],
        json!({ "query": "vacation" }).to_string()
    );
    let row = h.rig.wait_terminal(chat.id).await;
    assert_eq!(row.state, "completed");
    assert_eq!(row.file_search_completed_count, 1);
}

#[tokio::test]
async fn vllm_responses_sends_no_tools_and_a_function_call_there_is_unexpected() {
    // 0004 `inst-kind-vllm-responses`: no tools at all, so no loop on this adapter
    let h = multi_harness().await;
    h.upstream.queue(tool_use("call_v1", "q", (100, 20)));
    let chat = h.rig.quota.chat_a1(VLLM_MODEL).await;
    let mut turn = h.rig.start_turn(&chat, &question()).await;
    let events = turn.drain().await;
    let Some(RelayEvent::Error(error)) = events.last() else {
        panic!("{events:?}");
    };
    assert_eq!(error.code.as_str(), "unexpected_tool_use");
    assert!(h.upstream.calls()[0].body.get("tools").is_none());
    assert!(h.retriever.calls().is_empty());
    assert_eq!(h.upstream.call_count(), 1);
}
