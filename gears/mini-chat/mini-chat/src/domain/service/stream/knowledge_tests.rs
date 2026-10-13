#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(
    clippy::redundant_clone,
    clippy::non_ascii_literal,
    clippy::too_many_lines
)]
//! Slice 7: the knowledge-search loop
//! (`cpt-cf-mini-chat-algo-streaming-turns-knowledge-search`): the handler
//! against a fake retriever and a fake provider client, the loop inside the
//! provider task (relayed events, settlement of the last iteration only,
//! counts, progress, cancellation), the knowledge metrics
//! (`inst-metric-knowledge`) and the wiring of the configuration into the tool
//! decision and the handler.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use mini_chat_sdk::TerminalState;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::KnowledgeToolUse;
use crate::config::{MiniChatConfig, ProviderKind};
use crate::domain::error::DomainError;
use crate::domain::service::stream::pipeline::StreamSettings;
use crate::domain::service::stream::ports::{
    BuiltRequest, KnowledgeRetriever, ProviderClient, ProviderStream, StreamMetrics, ToolCallOutput,
};
use crate::domain::service::stream::task::{
    KnowledgeCounters, ToolUseContext, ToolUseHandler, ToolUseOutcome, run_provider_task,
};
use crate::domain::service::stream::test_support::{
    CHAT, FakeProvider, FakeRetriever, ProviderFeed, REQUEST, RecordingStreamMetrics,
    RetrieverReply, StreamMetric, TaskWorld, World, chat, completed, ctx, failure, run_premium,
    settings, text, usage,
};
use crate::domain::service::stream::tools::{KnowledgeParams, ToolSpec};
use crate::domain::service::stream::types::{
    FunctionCall, ProviderEvent, StreamErrorCode, StreamEvent,
};
use crate::domain::service::stream::validate::SendRequest;
use crate::domain::service::stream::{finalize::TurnRun, pipeline::SendOutcome};
use crate::test_doubles::{LogCapture, PdpReply, USER_TENANT};

// ---- fixtures -----------------------------------------------------------------------------------

fn knowledge_run(max_calls: u32, top_k: usize, max_chunk_chars: usize) -> TurnRun {
    let mut run = run_premium();
    run.tools.tools = vec![ToolSpec::SearchKnowledge];
    run.tools.knowledge = Some(KnowledgeParams {
        max_calls_per_message: max_calls,
        top_k,
        max_chunk_chars,
    });
    run
}

fn search_call(id: &str, query: &str) -> FunctionCall {
    FunctionCall {
        call_id: id.to_owned(),
        name: "search_knowledge".to_owned(),
        arguments: json!({ "query": query }).to_string(),
    }
}

fn call_with(id: &str, name: &str, arguments: &str) -> FunctionCall {
    FunctionCall {
        call_id: id.to_owned(),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    }
}

fn tool_use(calls: Vec<FunctionCall>, input: u64, output: u64) -> ProviderEvent {
    ProviderEvent::ToolUse {
        calls,
        usage: Some(usage(input, output)),
    }
}

fn results(texts: &[&str]) -> String {
    let items: Vec<Value> = texts.iter().map(|text| json!({ "text": text })).collect();
    json!({ "results": items }).to_string()
}

const LIMIT_OUTPUT: &str = "{\"error\":\"search limit reached\"}";
const FAILURE_OUTPUT: &str = "{\"error\":\"search failed\"}";
const INVALID_OUTPUT: &str = "{\"error\":\"invalid arguments\"}";

struct Fixture {
    provider: Arc<FakeProvider>,
    retriever: Arc<FakeRetriever>,
    metrics: Arc<RecordingStreamMetrics>,
    handler: Arc<KnowledgeToolUse>,
}

fn fixture() -> Fixture {
    let provider = FakeProvider::new();
    let retriever = FakeRetriever::new();
    let metrics = RecordingStreamMetrics::new();
    let handler = Arc::new(KnowledgeToolUse::new(
        Arc::clone(&provider) as Arc<dyn ProviderClient>,
        Arc::clone(&retriever) as Arc<dyn KnowledgeRetriever>,
        Arc::clone(&metrics) as Arc<dyn StreamMetrics>,
    ));
    Fixture {
        provider,
        retriever,
        metrics,
        handler,
    }
}

fn marker_request() -> BuiltRequest {
    BuiltRequest {
        kind: ProviderKind::OpenaiResponses,
        uri: "/api.openai.test/v1/responses".to_owned(),
        headers: Vec::new(),
        body: json!({ "marker": "previous" }),
    }
}

async fn handle_with(
    fixture: &Fixture,
    run: &TurnRun,
    iteration: u32,
    counters: &KnowledgeCounters,
    calls: &[FunctionCall],
) -> ToolUseOutcome {
    let token = CancellationToken::new();
    let request = marker_request();
    let context = ToolUseContext {
        run,
        iteration,
        cancel: &token,
        request: &request,
        knowledge: counters,
    };
    tokio::time::timeout(
        Duration::from_secs(60),
        fixture.handler.handle(&context, calls, Some(usage(10, 1))),
    )
    .await
    .expect("the handler did not end")
}

async fn handle(fixture: &Fixture, run: &TurnRun, calls: &[FunctionCall]) -> ToolUseOutcome {
    handle_with(fixture, run, 1, &KnowledgeCounters::default(), calls).await
}

fn continued(outcome: ToolUseOutcome) -> (Box<dyn ProviderStream>, BuiltRequest) {
    match outcome {
        ToolUseOutcome::Continue { stream, request } => (stream, request),
        ToolUseOutcome::Fail(failure) => panic!("the loop was expected to continue: {failure:?}"),
    }
}

fn failed(outcome: ToolUseOutcome) -> crate::domain::service::stream::types::ProviderFailure {
    match outcome {
        ToolUseOutcome::Fail(failure) => failure,
        ToolUseOutcome::Continue { .. } => panic!("the loop was expected to fail"),
    }
}

fn outputs_of(fixture: &Fixture, round: usize) -> Vec<ToolCallOutput> {
    fixture.provider.continuations()[round].1.clone()
}

fn knowledge_metrics(metrics: &RecordingStreamMetrics) -> Vec<StreamMetric> {
    metrics
        .events()
        .into_iter()
        .filter(|metric| {
            matches!(
                metric,
                StreamMetric::KnowledgeSearch(_)
                    | StreamMetric::KnowledgeLatency
                    | StreamMetric::KnowledgeChunks(_)
            )
        })
        .collect()
}

// ==== the handler: refusals ======================================================================

#[tokio::test]
async fn knowledge_search_off_for_the_turn_is_unexpected_tool_use() {
    let fixture = fixture();
    let run = run_premium();
    assert!(run.tools.knowledge.is_none());
    let counters = KnowledgeCounters::default();
    let outcome = handle_with(&fixture, &run, 1, &counters, &[search_call("c1", "q")]).await;
    assert_eq!(failed(outcome).code, StreamErrorCode::UnexpectedToolUse);
    assert!(fixture.retriever.searches().is_empty());
    assert_eq!(fixture.provider.open_count(), 0);
    assert!(fixture.provider.continuations().is_empty());
    assert_eq!(counters.calls(), 0);
}

#[tokio::test]
async fn a_call_of_another_function_fails_the_whole_iteration_before_any_retrieval() {
    let fixture = fixture();
    let run = knowledge_run(3, 5, 2000);
    let counters = KnowledgeCounters::default();
    let calls = [
        search_call("c1", "q"),
        call_with("c2", "load_files", "{}"),
        search_call("c3", "q2"),
    ];
    let outcome = handle_with(&fixture, &run, 1, &counters, &calls).await;
    let failure = failed(outcome);
    assert_eq!(failure.code, StreamErrorCode::UnexpectedToolUse);
    assert_eq!(
        failure.message,
        StreamErrorCode::UnexpectedToolUse.generic_message()
    );
    assert_eq!(
        failure.usage, None,
        "the task attaches the usage of the request"
    );
    assert!(fixture.retriever.searches().is_empty());
    assert_eq!(counters.calls(), 0);
}

#[tokio::test]
async fn the_unexpected_check_comes_before_the_iteration_limit() {
    let fixture = fixture();
    let run = knowledge_run(3, 5, 2000);
    let outcome = handle_with(
        &fixture,
        &run,
        5,
        &KnowledgeCounters::default(),
        &[call_with("c1", "get_weather", "{}")],
    )
    .await;
    assert_eq!(failed(outcome).code, StreamErrorCode::UnexpectedToolUse);
}

#[tokio::test]
async fn the_loop_is_capped_at_max_calls_plus_two_provider_iterations() {
    for (max_calls, last_allowed) in [(3_u32, 4_u32), (1, 2), (5, 6)] {
        let fixture = fixture();
        let run = knowledge_run(max_calls, 5, 2000);
        // The iteration that ends with a tool use may issue the next request
        // only while that request is iteration `max_calls + 2` or lower.
        fixture.provider.expect_stream();
        let outcome = handle_with(
            &fixture,
            &run,
            last_allowed,
            &KnowledgeCounters::default(),
            &[search_call("c1", "q")],
        )
        .await;
        let _ = continued(outcome);

        let counters = KnowledgeCounters::default();
        let outcome = handle_with(
            &fixture,
            &run,
            last_allowed + 1,
            &counters,
            &[search_call("c1", "q")],
        )
        .await;
        let failure = failed(outcome);
        assert_eq!(
            failure.code,
            StreamErrorCode::AgenticIterationsExceeded,
            "max {max_calls}"
        );
        assert_eq!(
            counters.calls(),
            0,
            "no call is answered once the cap is hit"
        );
        assert_eq!(
            fixture.retriever.searches().len(),
            1,
            "only the allowed iteration retrieved (max {max_calls})"
        );
    }
}

// ==== the handler: one call ======================================================================

#[tokio::test]
async fn a_call_is_retrieved_answered_and_the_next_request_is_issued() {
    let fixture = fixture();
    let run = knowledge_run(3, 5, 2000);
    let feed = fixture.provider.expect_stream();
    fixture.retriever.push_chunks(&["chunk one", "chunk two"]);
    let counters = KnowledgeCounters::default();
    let outcome = handle_with(
        &fixture,
        &run,
        1,
        &counters,
        &[search_call("c1", "refund policy")],
    )
    .await;
    let (mut stream, request) = continued(outcome);

    // The retrieval: the tenant of the chat, the query, the configured top_k.
    let searches = fixture.retriever.searches();
    assert_eq!(searches.len(), 1);
    assert_eq!(searches[0].tenant_id, run.chat.tenant_id);
    assert_eq!(searches[0].query, "refund policy");
    assert_eq!(searches[0].top_k, 5);

    // The next request: built from the request of the iteration, with the
    // call and its output.
    let continuations = fixture.provider.continuations();
    assert_eq!(continuations.len(), 1);
    assert_eq!(continuations[0].0, marker_request());
    assert_eq!(
        continuations[0].1,
        vec![ToolCallOutput {
            call: search_call("c1", "refund policy"),
            output: results(&["chunk one", "chunk two"]),
        }]
    );
    assert_eq!(request.body["marker"], "previous");
    assert_eq!(
        request.body["continued"],
        json!([{ "call_id": "c1", "name": "search_knowledge",
                 "output": results(&["chunk one", "chunk two"]) }])
    );

    // The request was opened with the context of the turn, and its stream is
    // the one the task reads next.
    let opened = fixture.provider.opened();
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0], request);
    assert_eq!(
        fixture.provider.contexts()[0].subject_id(),
        run.ctx.subject_id()
    );
    feed.push(text("answer"));
    assert_eq!(stream.next_event().await, Some(Ok(text("answer"))));

    assert_eq!(
        (
            counters.calls(),
            counters.retrievals(),
            counters.completed()
        ),
        (1, 1, 1)
    );
}

#[tokio::test]
async fn every_chunk_is_trimmed_to_max_chunk_chars_by_characters() {
    let fixture = fixture();
    let run = knowledge_run(3, 5, 4);
    fixture.provider.expect_stream();
    fixture
        .retriever
        .push_chunks(&["ééééééééé", "ab", "abcd", "abcde"]);
    let _ = continued(handle(&fixture, &run, &[search_call("c1", "q")]).await);
    assert_eq!(
        outputs_of(&fixture, 0)[0].output,
        results(&["éééé", "ab", "abcd", "abcd"]),
        "cut at 4 characters, no marker, never inside a character"
    );
}

#[tokio::test]
async fn an_empty_result_is_an_empty_list_and_still_a_successful_retrieval() {
    let fixture = fixture();
    let run = knowledge_run(3, 5, 2000);
    fixture.provider.expect_stream();
    fixture.retriever.push_chunks(&[]);
    let counters = KnowledgeCounters::default();
    let _ = continued(handle_with(&fixture, &run, 1, &counters, &[search_call("c1", "q")]).await);
    assert_eq!(outputs_of(&fixture, 0)[0].output, "{\"results\":[]}");
    assert_eq!(counters.completed(), 1);
    assert_eq!(
        knowledge_metrics(&fixture.metrics),
        vec![
            StreamMetric::KnowledgeSearch("ok".to_owned()),
            StreamMetric::KnowledgeLatency,
            StreamMetric::KnowledgeChunks(0),
        ]
    );
}

#[tokio::test]
async fn top_k_of_the_model_is_capped_at_the_configuration_and_defaults_to_it() {
    for (arguments, expected) in [
        (r#"{"query":"q","top_k":3}"#, 3_usize),
        (r#"{"query":"q","top_k":50}"#, 5),
        (r#"{"query":"q","top_k":5}"#, 5),
        (r#"{"query":"q","top_k":0}"#, 1),
        (r#"{"query":"q"}"#, 5),
        (r#"{"query":"q","top_k":"many"}"#, 5),
        (r#"{"query":"q","top_k":-2}"#, 5),
        (r#"{"query":"q","top_k":null}"#, 5),
    ] {
        let fixture = fixture();
        let run = knowledge_run(3, 5, 2000);
        fixture.provider.expect_stream();
        fixture.retriever.push_chunks(&["x"]);
        let _ = continued(
            handle(
                &fixture,
                &run,
                &[call_with("c1", "search_knowledge", arguments)],
            )
            .await,
        );
        assert_eq!(
            fixture.retriever.searches()[0].top_k,
            expected,
            "{arguments}"
        );
    }
}

#[tokio::test]
async fn the_query_is_passed_as_the_model_wrote_it() {
    let fixture = fixture();
    let run = knowledge_run(3, 5, 2000);
    fixture.provider.expect_stream();
    fixture.retriever.push_chunks(&["x"]);
    let _ = continued(
        handle(
            &fixture,
            &run,
            &[search_call("c1", "  Wie läuft die Rückgabe? ")],
        )
        .await,
    );
    assert_eq!(
        fixture.retriever.searches()[0].query,
        "  Wie läuft die Rückgabe? "
    );
}

#[tokio::test]
async fn arguments_that_are_not_a_query_get_a_failure_output_without_a_retrieval() {
    for arguments in [
        "not json",
        "{}",
        r#"{"query":""}"#,
        r#"{"query":"   "}"#,
        r#"{"query":5}"#,
        "[1]",
    ] {
        let fixture = fixture();
        let run = knowledge_run(3, 5, 2000);
        fixture.provider.expect_stream();
        let counters = KnowledgeCounters::default();
        let outcome = handle_with(
            &fixture,
            &run,
            1,
            &counters,
            &[call_with("c1", "search_knowledge", arguments)],
        )
        .await;
        let _ = continued(outcome);
        assert!(fixture.retriever.searches().is_empty(), "{arguments}");
        assert_eq!(
            outputs_of(&fixture, 0)[0].output,
            INVALID_OUTPUT,
            "{arguments}"
        );
        assert_eq!(counters.calls(), 1, "the call is counted: {arguments}");
        assert_eq!(counters.retrievals(), 0, "{arguments}");
        assert!(
            knowledge_metrics(&fixture.metrics).is_empty(),
            "{arguments}"
        );
    }
}

// ==== the handler: failed retrievals and the limit ===================================================

#[tokio::test]
async fn a_failed_retrieval_is_answered_with_a_failure_output_and_the_loop_continues() {
    let fixture = fixture();
    let run = knowledge_run(3, 5, 2000);
    fixture.provider.expect_stream();
    fixture.retriever.push(RetrieverReply::Fail);
    let counters = KnowledgeCounters::default();
    let outcome = handle_with(&fixture, &run, 1, &counters, &[search_call("c1", "q")]).await;
    let _ = continued(outcome);
    assert_eq!(outputs_of(&fixture, 0)[0].output, FAILURE_OUTPUT);
    assert_eq!(
        (
            counters.calls(),
            counters.retrievals(),
            counters.completed()
        ),
        (1, 1, 0),
        "counted as a call and a retrieval, not as a completed one"
    );
    assert_eq!(
        knowledge_metrics(&fixture.metrics),
        vec![StreamMetric::KnowledgeSearch("error".to_owned())],
        "an error records its result only"
    );
}

#[tokio::test]
async fn the_failure_output_names_no_provider_detail() {
    let fixture = fixture();
    let run = knowledge_run(3, 5, 2000);
    fixture.provider.expect_stream();
    fixture.retriever.push(RetrieverReply::Fail);
    let _ = continued(handle(&fixture, &run, &[search_call("c1", "q")]).await);
    let output = &outputs_of(&fixture, 0)[0].output;
    assert!(!output.contains("the retrieval failed"), "{output}");
}

#[tokio::test]
async fn once_the_retrieval_count_reached_the_limit_a_call_gets_the_limit_output() {
    let fixture = fixture();
    let run = knowledge_run(2, 5, 2000);
    fixture.provider.expect_stream();
    let counters = KnowledgeCounters::default();
    counters.record_retrieval();
    counters.record_retrieval();
    let outcome = handle_with(&fixture, &run, 3, &counters, &[search_call("c1", "q")]).await;
    let _ = continued(outcome);
    assert!(fixture.retriever.searches().is_empty(), "no retrieval");
    assert_eq!(outputs_of(&fixture, 0)[0].output, LIMIT_OUTPUT);
    assert_eq!(
        (
            counters.calls(),
            counters.retrievals(),
            counters.completed()
        ),
        (1, 2, 0),
        "a limited call is a call but not a retrieval"
    );
    assert!(knowledge_metrics(&fixture.metrics).is_empty());
}

#[tokio::test]
async fn within_one_iteration_the_calls_up_to_the_limit_retrieve_and_the_rest_get_the_limit_output()
{
    let fixture = fixture();
    let run = knowledge_run(2, 5, 2000);
    fixture.provider.expect_stream();
    fixture.retriever.push_chunks(&["a"]);
    let counters = KnowledgeCounters::default();
    counters.record_retrieval(); // one retrieval ran in an earlier iteration
    let calls = [
        search_call("c1", "first"),
        search_call("c2", "second"),
        search_call("c3", "third"),
    ];
    let _ = continued(handle_with(&fixture, &run, 2, &counters, &calls).await);
    let searches = fixture.retriever.searches();
    assert_eq!(searches.len(), 1);
    assert_eq!(searches[0].query, "first");
    let outputs = outputs_of(&fixture, 0);
    assert_eq!(
        outputs
            .iter()
            .map(|o| (o.call.call_id.as_str(), o.output.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("c1", results(&["a"]).as_str()),
            ("c2", LIMIT_OUTPUT),
            ("c3", LIMIT_OUTPUT),
        ],
        "outputs in the order of the calls"
    );
    assert_eq!(
        (
            counters.calls(),
            counters.retrievals(),
            counters.completed()
        ),
        (3, 2, 1)
    );
}

#[tokio::test]
async fn failed_retrievals_count_against_the_limit() {
    let fixture = fixture();
    let run = knowledge_run(2, 5, 2000);
    fixture.provider.expect_stream();
    fixture.retriever.push(RetrieverReply::Fail);
    fixture.retriever.push(RetrieverReply::Fail);
    let calls = [
        search_call("c1", "a"),
        search_call("c2", "b"),
        search_call("c3", "c"),
    ];
    let _ = continued(handle(&fixture, &run, &calls).await);
    assert_eq!(fixture.retriever.searches().len(), 2);
    let outputs = outputs_of(&fixture, 0);
    assert_eq!(outputs[0].output, FAILURE_OUTPUT);
    assert_eq!(outputs[1].output, FAILURE_OUTPUT);
    assert_eq!(outputs[2].output, LIMIT_OUTPUT);
}

// ==== the handler: counting, metrics and failures of the next request ============================

#[tokio::test]
async fn the_call_is_counted_before_the_retrieval_runs() {
    let fixture = fixture();
    let run = knowledge_run(3, 5, 2000);
    fixture.retriever.push(RetrieverReply::Hang);
    let counters = KnowledgeCounters::default();
    let token = CancellationToken::new();
    let request = marker_request();
    let context = ToolUseContext {
        run: &run,
        iteration: 1,
        cancel: &token,
        request: &request,
        knowledge: &counters,
    };
    let calls = [search_call("c1", "q")];
    let pending = fixture.handler.handle(&context, &calls, None);
    let outcome = tokio::time::timeout(Duration::from_millis(100), pending).await;
    assert!(outcome.is_err(), "the retrieval is still running");
    assert_eq!(counters.calls(), 1);
    assert_eq!(counters.retrievals(), 1);
    assert_eq!(counters.completed(), 0);
    assert_eq!(
        fixture.retriever.dropped_searches(),
        1,
        "dropped with the handler"
    );
}

#[tokio::test(start_paused = true)]
async fn a_successful_retrieval_records_its_result_latency_and_chunk_count() {
    let fixture = fixture();
    let run = knowledge_run(3, 5, 2000);
    fixture.provider.expect_stream();
    fixture.retriever.push(RetrieverReply::After(
        Duration::from_millis(40),
        vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
    ));
    let _ = continued(handle(&fixture, &run, &[search_call("c1", "q")]).await);
    assert_eq!(
        knowledge_metrics(&fixture.metrics),
        vec![
            StreamMetric::KnowledgeSearch("ok".to_owned()),
            StreamMetric::KnowledgeLatency,
            StreamMetric::KnowledgeChunks(3),
        ]
    );
    let latency = fixture.metrics.values("knowledge_search_latency_ms");
    assert_eq!(latency.len(), 1);
    assert!((40.0..50.0).contains(&latency[0]), "latency {latency:?}");
}

#[tokio::test]
async fn each_retrieval_is_recorded_once() {
    let fixture = fixture();
    let run = knowledge_run(3, 5, 2000);
    fixture.provider.expect_stream();
    fixture.retriever.push_chunks(&["a"]);
    fixture.retriever.push(RetrieverReply::Fail);
    fixture.retriever.push_chunks(&["a", "b"]);
    let calls = [
        search_call("c1", "1"),
        search_call("c2", "2"),
        search_call("c3", "3"),
    ];
    let _ = continued(handle(&fixture, &run, &calls).await);
    assert_eq!(
        knowledge_metrics(&fixture.metrics),
        vec![
            StreamMetric::KnowledgeSearch("ok".to_owned()),
            StreamMetric::KnowledgeLatency,
            StreamMetric::KnowledgeChunks(1),
            StreamMetric::KnowledgeSearch("error".to_owned()),
            StreamMetric::KnowledgeSearch("ok".to_owned()),
            StreamMetric::KnowledgeLatency,
            StreamMetric::KnowledgeChunks(2),
        ]
    );
}

#[tokio::test]
async fn a_next_request_that_cannot_be_opened_fails_the_turn_with_the_classified_failure() {
    let fixture = fixture();
    let run = knowledge_run(3, 5, 2000);
    fixture
        .provider
        .fail_open(failure(StreamErrorCode::ProviderTimeout, "timed out"));
    fixture.retriever.push_chunks(&["x"]);
    let counters = KnowledgeCounters::default();
    let outcome = handle_with(&fixture, &run, 1, &counters, &[search_call("c1", "q")]).await;
    let failure = failed(outcome);
    assert_eq!(failure.code, StreamErrorCode::ProviderTimeout);
    assert_eq!(
        (counters.calls(), counters.completed()),
        (1, 1),
        "the counts of the answered calls survive"
    );
}

#[tokio::test]
async fn a_continuation_that_cannot_be_built_fails_the_turn_with_a_provider_error() {
    let fixture = fixture();
    let run = knowledge_run(3, 5, 2000);
    fixture.provider.fail_continue("no input array");
    fixture.retriever.push_chunks(&["x"]);
    let failure = failed(handle(&fixture, &run, &[search_call("c1", "q")]).await);
    assert_eq!(failure.code, StreamErrorCode::ProviderError);
    assert!(!failure.message.contains("no input array"), "{failure:?}");
    assert_eq!(fixture.provider.open_count(), 0);
}

// ==== the loop inside the provider task ==============================================================

struct Loop {
    world: TaskWorld,
    retriever: Arc<FakeRetriever>,
    token: CancellationToken,
    rx: mpsc::Receiver<StreamEvent>,
    task: JoinHandle<()>,
}

fn start_loop(run: TurnRun, retriever: Arc<FakeRetriever>, world: TaskWorld) -> Loop {
    let handler = Arc::new(KnowledgeToolUse::new(
        Arc::clone(&world.provider) as Arc<dyn ProviderClient>,
        Arc::clone(&retriever) as Arc<dyn KnowledgeRetriever>,
        Arc::clone(&world.metrics) as Arc<dyn StreamMetrics>,
    ));
    let world = world.with_tool_use(handler as Arc<dyn ToolUseHandler>);
    let (tx, rx) = mpsc::channel(64);
    let token = CancellationToken::new();
    let (go_tx, go_rx) = oneshot::channel();
    let task = tokio::spawn(run_provider_task(
        Arc::clone(&world.deps),
        run,
        tx,
        token.clone(),
        go_rx,
    ));
    go_tx.send(()).unwrap();
    Loop {
        world,
        retriever,
        token,
        rx,
        task,
    }
}

impl Loop {
    async fn events(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Some(event) = tokio::time::timeout(Duration::from_secs(600), self.rx.recv())
            .await
            .expect("timed out waiting for the provider task")
        {
            out.push(event);
        }
        out
    }

    async fn join(self) -> TaskWorld {
        tokio::time::timeout(Duration::from_secs(600), self.task)
            .await
            .expect("timed out")
            .expect("the task did not panic");
        self.world
    }
}

fn names(events: &[StreamEvent]) -> Vec<&'static str> {
    events.iter().map(StreamEvent::name).collect()
}

fn error_code(event: &StreamEvent) -> StreamErrorCode {
    match event {
        StreamEvent::Error(error) => error.code,
        other => panic!("error expected, got {other:?}"),
    }
}

fn last_finalization(
    world: &TaskWorld,
) -> (
    crate::domain::service::quota::finalize::FinalizeRequest,
    crate::domain::service::stream::ports::FinalizeWrites,
) {
    world.turns.finalizations().pop().expect("a finalization")
}

#[tokio::test(start_paused = true)]
async fn a_function_call_runs_the_retrieval_and_the_answer_streams_from_the_next_request() {
    let world = TaskWorld::new();
    let first = world.provider.expect_stream();
    let second = world.provider.expect_stream();
    let retriever = FakeRetriever::new();
    retriever.push_chunks(&["Refunds are paid within 5 days."]);
    first.push_all([
        text("Let me check. "),
        tool_use(vec![search_call("call_1", "refund policy")], 10, 1),
    ]);
    second.push_all([text("Refunds take 5 days."), completed(50, 9)]);
    let mut run = knowledge_run(3, 5, 2000);
    run.request.body = json!({ "model": "provider-p" });
    let mut rig = start_loop(run, retriever, world);

    let events = rig.events().await;
    assert_eq!(names(&events), ["delta", "delta", "done"]);
    assert_eq!(
        events[1],
        StreamEvent::Delta {
            kind: crate::domain::service::stream::types::DeltaKind::Text,
            content: "Refunds take 5 days.".to_owned()
        }
    );
    assert_eq!(rig.retriever.searches().len(), 1);
    let world = rig.join().await;

    let opened = world.provider.opened();
    assert_eq!(opened.len(), 2, "one request per provider iteration");
    assert_eq!(opened[0].body, json!({ "model": "provider-p" }));
    assert_eq!(
        opened[1].body["continued"],
        json!([{ "call_id": "call_1", "name": "search_knowledge",
                 "output": results(&["Refunds are paid within 5 days."]) }]),
        "the next request carries the function output"
    );
    assert!(first.is_dropped(), "the first stream is released");

    let (request, writes) = last_finalization(&world);
    assert_eq!(request.columns.state, TerminalState::Completed);
    assert_eq!(
        request.usage,
        Some(usage(50, 9)),
        "only the usage of the last iteration is settled"
    );
    assert_eq!(request.tool_calls.file_search, 1);
    assert_eq!(
        writes.assistant_message.unwrap().content,
        "Let me check. Refunds take 5 days.",
        "the text of every iteration is the answer"
    );
    assert_eq!(writes.audit.tool_calls.file_search, 1);
}

#[tokio::test(start_paused = true)]
async fn knowledge_calls_count_failed_retrievals_and_the_persisted_count_only_the_successful_ones()
{
    let logs = LogCapture::start();
    let world = TaskWorld::new();
    let streams = [
        world.provider.expect_stream(),
        world.provider.expect_stream(),
        world.provider.expect_stream(),
    ];
    let retriever = FakeRetriever::new();
    retriever.push(RetrieverReply::Fail);
    retriever.push_chunks(&["found"]);
    streams[0].push(tool_use(vec![search_call("c1", "first")], 10, 1));
    streams[1].push(tool_use(vec![search_call("c2", "second")], 20, 2));
    streams[2].push_all([text("done"), completed(30, 3)]);
    let mut rig = start_loop(knowledge_run(3, 5, 2000), retriever, world);
    let events = rig.events().await;
    assert_eq!(names(&events), ["delta", "done"]);
    let world = rig.join().await;

    let (request, writes) = last_finalization(&world);
    assert_eq!(
        request.tool_calls.file_search, 2,
        "file_search_calls of the usage event"
    );
    assert_eq!(
        writes.audit.tool_calls.file_search, 2,
        "tool_calls.file_search of the audit event"
    );
    let records = logs.at_containing(tracing::Level::INFO, "turn finalized");
    assert_eq!(records.len(), 1);
    assert!(
        records[0].text().contains("file_search_calls=2"),
        "{}",
        records[0].text()
    );
    let persisted = world
        .turns
        .progress()
        .iter()
        .filter_map(|update| update.file_search_done)
        .collect::<Vec<_>>();
    assert_eq!(
        persisted.last(),
        Some(&1),
        "chat_turns.file_search_completed_count is the number of successful retrievals: {persisted:?}"
    );
    assert!(persisted.iter().all(|count| *count <= 1));
    assert_eq!(
        knowledge_metrics(&world.metrics),
        vec![
            StreamMetric::KnowledgeSearch("error".to_owned()),
            StreamMetric::KnowledgeSearch("ok".to_owned()),
            StreamMetric::KnowledgeLatency,
            StreamMetric::KnowledgeChunks(1),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_successful_retrieval_refreshes_the_progress_of_the_turn() {
    let world = TaskWorld::new();
    let first = world.provider.expect_stream();
    let second = world.provider.expect_stream();
    let retriever = FakeRetriever::new();
    retriever.push_chunks(&["x"]);
    first.push(tool_use(vec![search_call("c1", "q")], 1, 1));
    second.push_all([completed(2, 2)]);
    let mut rig = start_loop(knowledge_run(3, 5, 2000), retriever, world);
    rig.events().await;
    let world = rig.join().await;
    let updates = world.turns.progress();
    let with_file = updates
        .iter()
        .find(|update| update.file_search_done == Some(1))
        .expect("the completed count is persisted");
    assert!(with_file.at.is_some(), "together with last_progress_at");
}

#[tokio::test(start_paused = true)]
async fn the_iteration_limit_ends_the_turn_with_the_usage_of_the_last_request() {
    let world = TaskWorld::new();
    let streams = [
        world.provider.expect_stream(),
        world.provider.expect_stream(),
        world.provider.expect_stream(),
    ];
    let retriever = FakeRetriever::new();
    retriever.push_chunks(&["x"]);
    streams[0].push(tool_use(vec![search_call("c1", "a")], 10, 1));
    streams[1].push(tool_use(vec![search_call("c2", "b")], 20, 2));
    streams[2].push(tool_use(vec![search_call("c3", "c")], 33, 3));
    // max 1 retrieval: three provider iterations are allowed (1 + 2).
    let mut rig = start_loop(knowledge_run(1, 5, 2000), retriever, world);
    let events = rig.events().await;
    assert_eq!(names(&events), ["error"]);
    assert_eq!(
        error_code(&events[0]),
        StreamErrorCode::AgenticIterationsExceeded
    );
    assert_eq!(
        rig.retriever.searches().len(),
        1,
        "the second call got the limit output"
    );
    let world = rig.join().await;
    assert_eq!(world.provider.open_count(), 3);
    let (request, _) = last_finalization(&world);
    assert_eq!(request.columns.state, TerminalState::Failed);
    assert_eq!(
        request.columns.error_code.as_deref(),
        Some("agentic_iterations_exceeded")
    );
    assert_eq!(request.usage, Some(usage(33, 3)));
    assert_eq!(
        request.tool_calls.file_search, 2,
        "two calls were answered (one retrieved, one limited); the third was not"
    );
    let outputs = world.provider.continuations();
    assert_eq!(outputs.len(), 2);
}

#[tokio::test(start_paused = true)]
async fn the_limit_output_lets_the_model_answer_from_what_it_has() {
    let world = TaskWorld::new();
    let streams = [
        world.provider.expect_stream(),
        world.provider.expect_stream(),
        world.provider.expect_stream(),
    ];
    let retriever = FakeRetriever::new();
    retriever.push_chunks(&["x"]);
    streams[0].push(tool_use(vec![search_call("c1", "a")], 10, 1));
    streams[1].push(tool_use(vec![search_call("c2", "b")], 20, 2));
    streams[2].push_all([text("best effort"), completed(30, 4)]);
    let mut rig = start_loop(knowledge_run(1, 5, 2000), retriever, world);
    let events = rig.events().await;
    assert_eq!(names(&events), ["delta", "done"]);
    let world = rig.join().await;
    let rounds = world.provider.continuations();
    assert_eq!(rounds[1].1[0].output, LIMIT_OUTPUT);
    assert_eq!(
        rounds[1].0.body["continued"].as_array().map(Vec::len),
        Some(1),
        "the second continuation is built from the request of the first one"
    );
    let (request, _) = last_finalization(&world);
    assert_eq!(request.usage, Some(usage(30, 4)));
    assert_eq!(request.tool_calls.file_search, 2);
}

#[tokio::test(start_paused = true)]
async fn another_function_ends_the_turn_as_unexpected_tool_use_with_the_usage_of_that_request() {
    let world = TaskWorld::new();
    let first = world.provider.expect_stream();
    first.push(tool_use(vec![call_with("c1", "load_files", "{}")], 30, 4));
    let mut rig = start_loop(knowledge_run(3, 5, 2000), FakeRetriever::new(), world);
    let events = rig.events().await;
    assert_eq!(names(&events), ["error"]);
    assert_eq!(error_code(&events[0]), StreamErrorCode::UnexpectedToolUse);
    let world = rig.join().await;
    assert_eq!(world.provider.open_count(), 1, "no further request");
    let (request, _) = last_finalization(&world);
    assert_eq!(
        request.columns.error_code.as_deref(),
        Some("unexpected_tool_use")
    );
    assert_eq!(request.usage, Some(usage(30, 4)));
    assert_eq!(request.tool_calls.file_search, 0);
}

#[tokio::test(start_paused = true)]
async fn a_search_call_on_a_turn_without_knowledge_search_is_unexpected_tool_use() {
    let world = TaskWorld::new();
    let first = world.provider.expect_stream();
    first.push(tool_use(vec![search_call("c1", "q")], 7, 1));
    let retriever = FakeRetriever::new();
    let mut rig = start_loop(run_premium(), retriever, world);
    let events = rig.events().await;
    assert_eq!(
        error_code(events.last().unwrap()),
        StreamErrorCode::UnexpectedToolUse
    );
    assert!(rig.retriever.searches().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_next_request_that_fails_to_open_ends_the_turn_with_its_code_and_the_last_usage() {
    let world = TaskWorld::new();
    let first = world.provider.expect_stream();
    world
        .provider
        .fail_open(failure(StreamErrorCode::RateLimited, "slow down"));
    let retriever = FakeRetriever::new();
    retriever.push_chunks(&["x"]);
    first.push(tool_use(vec![search_call("c1", "q")], 10, 1));
    let mut rig = start_loop(knowledge_run(3, 5, 2000), retriever, world);
    let events = rig.events().await;
    assert_eq!(
        error_code(events.last().unwrap()),
        StreamErrorCode::RateLimited
    );
    let world = rig.join().await;
    let (request, _) = last_finalization(&world);
    assert_eq!(request.columns.error_code.as_deref(), Some("rate_limited"));
    assert_eq!(request.usage, Some(usage(10, 1)));
    assert_eq!(
        request.tool_calls.file_search, 1,
        "the answered call is counted"
    );
}

#[tokio::test(start_paused = true)]
async fn a_provider_error_in_a_later_iteration_fails_the_turn_like_any_other() {
    let world = TaskWorld::new();
    let first = world.provider.expect_stream();
    let second = world.provider.expect_stream();
    let retriever = FakeRetriever::new();
    retriever.push_chunks(&["x"]);
    first.push(tool_use(vec![search_call("c1", "q")], 10, 1));
    second.push(ProviderEvent::ProviderError {
        code: Some("server_error".to_owned()),
        message: "boom".to_owned(),
        usage: Some(usage(12, 0)),
    });
    let mut rig = start_loop(knowledge_run(3, 5, 2000), retriever, world);
    let events = rig.events().await;
    assert_eq!(
        error_code(events.last().unwrap()),
        StreamErrorCode::ProviderError
    );
    let world = rig.join().await;
    let (request, _) = last_finalization(&world);
    assert_eq!(
        request.usage,
        Some(usage(12, 0)),
        "the usage of the failing request"
    );
    assert_eq!(request.tool_calls.file_search, 1);
}

#[tokio::test(start_paused = true)]
async fn a_disconnect_during_a_retrieval_drops_it_and_ends_the_turn_cancelled() {
    let world = TaskWorld::new();
    let first = world.provider.expect_stream();
    let retriever = FakeRetriever::new();
    retriever.push(RetrieverReply::Hang);
    first.push(tool_use(vec![search_call("c1", "q")], 10, 1));
    let mut rig = start_loop(knowledge_run(3, 5, 2000), retriever, world);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        rig.retriever.searches().len(),
        1,
        "the retrieval is running"
    );
    rig.token.cancel();
    let events = rig.events().await;
    assert!(
        events.is_empty(),
        "nothing is sent to a client that left: {events:?}"
    );
    assert_eq!(
        rig.retriever.dropped_searches(),
        1,
        "the retrieval is dropped"
    );
    let world = rig.join().await;
    assert_eq!(
        world.provider.open_count(),
        1,
        "no next request after a cancellation"
    );
    let (request, _) = last_finalization(&world);
    assert_eq!(request.columns.state, TerminalState::Cancelled);
    assert_eq!(
        request.usage, None,
        "no usage is passed for a cancelled turn"
    );
    assert_eq!(
        request.tool_calls.file_search, 1,
        "the call counted before its retrieval ran"
    );
    assert!(first.is_dropped());
}

#[tokio::test(start_paused = true)]
async fn a_disconnect_while_the_next_request_is_opening_ends_the_turn_cancelled() {
    let world = TaskWorld::new();
    let first = world.provider.expect_stream();
    let retriever = FakeRetriever::new();
    retriever.push_chunks(&["x"]);
    first.push(tool_use(vec![search_call("c1", "q")], 10, 1));
    // The second `open` never answers.
    world.provider.hang_open_after(1);
    let mut rig = start_loop(knowledge_run(3, 5, 2000), retriever, world);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        rig.world.provider.open_count(),
        2,
        "the next request is pending"
    );
    rig.token.cancel();
    let events = rig.events().await;
    assert!(events.is_empty());
    assert_eq!(
        rig.world.provider.open_calls_dropped(),
        1,
        "the pending call is dropped"
    );
    let world = rig.join().await;
    let (request, _) = last_finalization(&world);
    assert_eq!(request.columns.state, TerminalState::Cancelled);
    assert_eq!(request.usage, None);
    assert_eq!(request.tool_calls.file_search, 1);
}

#[tokio::test(start_paused = true)]
async fn function_call_tool_events_are_relayed_and_are_not_native_file_search() {
    let world = TaskWorld::new();
    let first = world.provider.expect_stream();
    let second = world.provider.expect_stream();
    let retriever = FakeRetriever::new();
    retriever.push_chunks(&["x"]);
    first.push_all([
        ProviderEvent::ToolStart {
            name: crate::domain::service::stream::types::ToolName::FunctionCall,
            details: json!({ "index": 0, "call_id": "c1", "name": "search_knowledge" }),
        },
        ProviderEvent::ToolDone {
            name: crate::domain::service::stream::types::ToolName::FunctionCall,
            details: json!({ "call_id": "c1", "name": "search_knowledge", "arguments": "{}" }),
        },
        tool_use(vec![search_call("c1", "q")], 10, 1),
    ]);
    second.push_all([completed(20, 2)]);
    let mut rig = start_loop(knowledge_run(3, 5, 2000), retriever, world);
    let events = rig.events().await;
    assert_eq!(names(&events), ["tool", "tool", "done"]);
    let world = rig.join().await;
    let (request, _) = last_finalization(&world);
    assert_eq!(
        request.tool_calls.file_search, 1,
        "function_call tool events are not native file_search"
    );
}

// ==== the wiring of the configuration ================================================================

fn knowledge_config() -> MiniChatConfig {
    let mut config = MiniChatConfig::default();
    config.knowledge_search.enabled = true;
    config.knowledge_search.provider_id = Some("kp".to_owned());
    config.knowledge_search.vector_store_id = Some("vs_kb".to_owned());
    config
}

fn knowledge_settings() -> StreamSettings {
    let mut settings = settings();
    settings.knowledge.enabled = true;
    settings.knowledge_provider_id = Some("kp".to_owned());
    settings
}

fn wired_world(settings: StreamSettings, retriever: Option<Arc<FakeRetriever>>) -> World {
    let world = World::build_with(
        Some(chat("p")),
        PdpReply::AllowScoped,
        settings,
        retriever.map(|r| r as Arc<dyn KnowledgeRetriever>),
    );
    world.directory.target.lock().unwrap().api_version = Some("2025-01-01-preview".to_owned());
    world
}

fn req(content: &str) -> SendRequest {
    SendRequest {
        content: content.to_owned(),
        request_id: Some(REQUEST),
        ..SendRequest::default()
    }
}

async fn offered(world: &World) -> (Vec<ToolSpec>, String) {
    let _feed = world.provider.expect_stream();
    let result = world.service.send(ctx(), CHAT, req("hi")).await;
    assert!(result.is_ok(), "the send must be accepted");
    let builds = world.provider.builds();
    assert_eq!(builds.len(), 1);
    (
        builds[0].tools.tools.clone(),
        builds[0].plan.instructions.clone(),
    )
}

fn has_knowledge(tools: &[ToolSpec]) -> bool {
    tools
        .iter()
        .any(|tool| matches!(tool, ToolSpec::SearchKnowledge))
}

#[test]
fn the_settings_take_the_knowledge_provider_id_from_the_configuration() {
    let settings = StreamSettings::from_config(&knowledge_config());
    assert_eq!(settings.knowledge_provider_id.as_deref(), Some("kp"));
    assert!(settings.knowledge.enabled);
    assert_eq!(
        settings.knowledge.provider, None,
        "the provider entry is resolved per turn"
    );
    assert!(
        !settings.knowledge.retriever_configured,
        "so is the retriever"
    );
    assert_eq!(settings.knowledge.max_calls_per_message, 3);
    assert_eq!(settings.knowledge.top_k, 5);
    assert_eq!(settings.knowledge.max_chunk_chars, 2000);

    let default = StreamSettings::from_config(&MiniChatConfig::default());
    assert_eq!(default.knowledge_provider_id, None);
    assert!(!default.knowledge.enabled);
}

#[tokio::test]
async fn with_knowledge_search_enabled_and_a_provider_configured_search_knowledge_is_offered() {
    let world = wired_world(knowledge_settings(), Some(FakeRetriever::new()));
    let (tools, instructions) = offered(&world).await;
    assert!(has_knowledge(&tools), "{tools:?}");
    assert!(
        instructions.contains("KS-GUARD"),
        "the guard is appended: {instructions}"
    );
    assert!(
        world
            .directory
            .resolved
            .lock()
            .unwrap()
            .contains(&("kp".to_owned(), USER_TENANT)),
        "the entry of knowledge_search.provider_id is resolved for the tenant"
    );
}

#[tokio::test]
async fn the_tool_and_the_guard_are_the_last_ones_of_the_request() {
    let world = wired_world(knowledge_settings(), Some(FakeRetriever::new()));
    let (tools, instructions) = offered(&world).await;
    assert_eq!(tools.last(), Some(&ToolSpec::SearchKnowledge));
    assert!(instructions.ends_with("KS-GUARD"));
}

#[tokio::test]
async fn disabled_knowledge_search_offers_nothing_and_resolves_nothing() {
    let mut settings = knowledge_settings();
    settings.knowledge.enabled = false;
    let world = wired_world(settings, Some(FakeRetriever::new()));
    let (tools, instructions) = offered(&world).await;
    assert!(!has_knowledge(&tools));
    assert!(!instructions.contains("KS-GUARD"));
    assert!(
        !world
            .directory
            .resolved
            .lock()
            .unwrap()
            .iter()
            .any(|(provider, _)| provider == "kp")
    );
}

#[tokio::test]
async fn without_a_retriever_knowledge_search_is_off_for_the_turn() {
    let logs = LogCapture::start();
    let world = wired_world(knowledge_settings(), None);
    let (tools, instructions) = offered(&world).await;
    assert!(!has_knowledge(&tools));
    assert!(!instructions.contains("KS-GUARD"));
    assert!(
        !logs
            .at_containing(tracing::Level::WARN, "knowledge search is off")
            .is_empty(),
        "a warning names the gap"
    );
}

#[tokio::test]
async fn a_provider_entry_without_an_api_version_turns_knowledge_search_off() {
    let world = wired_world(knowledge_settings(), Some(FakeRetriever::new()));
    world.directory.target.lock().unwrap().api_version = None;
    let (tools, _) = offered(&world).await;
    assert!(!has_knowledge(&tools));
}

#[tokio::test]
async fn an_empty_api_version_or_alias_turns_knowledge_search_off() {
    let world = wired_world(knowledge_settings(), Some(FakeRetriever::new()));
    world.directory.target.lock().unwrap().api_version = Some(String::new());
    assert!(!has_knowledge(&offered(&world).await.0));

    let world = wired_world(knowledge_settings(), Some(FakeRetriever::new()));
    world.directory.target.lock().unwrap().alias = String::new();
    assert!(!has_knowledge(&offered(&world).await.0));
}

#[tokio::test]
async fn a_provider_kind_other_than_responses_or_anthropic_turns_knowledge_search_off() {
    let world = wired_world(knowledge_settings(), Some(FakeRetriever::new()));
    world.directory.target.lock().unwrap().kind = ProviderKind::VllmResponses;
    assert!(!has_knowledge(&offered(&world).await.0));
}

#[tokio::test]
async fn an_anthropic_knowledge_provider_is_accepted() {
    let world = wired_world(knowledge_settings(), Some(FakeRetriever::new()));
    world.directory.target.lock().unwrap().kind = ProviderKind::AnthropicMessages;
    assert!(has_knowledge(&offered(&world).await.0));
}

#[tokio::test]
async fn a_knowledge_provider_that_does_not_resolve_turns_knowledge_search_off_without_failing_the_send()
 {
    let world = wired_world(knowledge_settings(), Some(FakeRetriever::new()));
    world
        .directory
        .fail_ids
        .lock()
        .unwrap()
        .push("kp".to_owned());
    let (tools, _) = offered(&world).await;
    assert!(
        !has_knowledge(&tools),
        "the chat model's provider still resolves"
    );
}

#[tokio::test]
async fn enabled_knowledge_search_without_a_provider_id_is_off() {
    let mut settings = knowledge_settings();
    settings.knowledge_provider_id = None;
    let world = wired_world(settings, Some(FakeRetriever::new()));
    assert!(!has_knowledge(&offered(&world).await.0));
}

fn opened(
    result: Result<SendOutcome, DomainError>,
) -> (
    crate::domain::service::stream::types::StreamStarted,
    crate::domain::service::stream::relay::RelayHandle,
) {
    match result {
        Ok(SendOutcome::Stream { started, relay }) => (started, relay),
        Ok(SendOutcome::Replay(_)) => panic!("a new stream expected"),
        Err(error) => panic!("a new stream expected, got {error:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn a_turn_with_knowledge_search_runs_the_loop_through_the_service() {
    let retriever = FakeRetriever::new();
    retriever.push_chunks(&["Refunds are paid within 5 days."]);
    let world = wired_world(knowledge_settings(), Some(Arc::clone(&retriever)));
    let first: ProviderFeed = world.provider.expect_stream();
    let second = world.provider.expect_stream();
    first.push(tool_use(
        vec![search_call("call_1", "refund policy")],
        10,
        1,
    ));
    second.push_all([text("Five days."), completed(50, 9)]);
    let (started, relay) = opened(
        world
            .service
            .send(ctx(), CHAT, req("how long do refunds take?"))
            .await,
    );
    let mut stream = relay.into_stream(started);
    let mut names = Vec::new();
    while let Some(event) = stream.next().await {
        names.push(event.name());
    }
    assert_eq!(names, ["stream_started", "delta", "done"]);
    assert_eq!(world.provider.open_count(), 2);
    let searches = retriever.searches();
    assert_eq!(searches.len(), 1);
    assert_eq!(searches[0].tenant_id, USER_TENANT);
    assert_eq!(searches[0].query, "refund policy");
    assert_eq!(world.provider.continuations().len(), 1);
    let (request, _) = world.turns.finalizations().remove(0);
    assert_eq!(request.tool_calls.file_search, 1);
    assert_eq!(request.usage, Some(usage(50, 9)));
}

#[tokio::test(start_paused = true)]
async fn a_tool_use_on_a_turn_without_a_retriever_is_unexpected_tool_use() {
    let world = wired_world(knowledge_settings(), None);
    let first = world.provider.expect_stream();
    first.push(tool_use(vec![search_call("call_1", "q")], 10, 1));
    let (started, relay) = opened(world.service.send(ctx(), CHAT, req("hi")).await);
    let mut stream = relay.into_stream(started);
    let mut last = None;
    while let Some(event) = stream.next().await {
        last = Some(event);
    }
    assert_eq!(
        error_code(&last.unwrap()),
        StreamErrorCode::UnexpectedToolUse
    );
}
