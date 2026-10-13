#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 5 and 6: the provider task
//! (`cpt-cf-mini-chat-algo-streaming-turns-run-provider-task`,
//! `cpt-cf-mini-chat-flow-streaming-turns-client-disconnect`): the relayed
//! event sequence, the per-turn tool limits, the outcomes handed to the
//! finalization, cancellation on every await, the liveness check, progress
//! writes, backpressure and the stream metrics.
//!
//! Every test runs on a paused clock: time passes only when every task is
//! idle, so "within 200 ms" and "within one ping interval" are exact, and a
//! task that waits for a timer instead of reacting to an event is caught.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::TerminalState;
use serde_json::json;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::*;
use crate::domain::error::DomainError;
use crate::domain::service::quota::finalize::{FinalizeResult, TerminalClassification};
use crate::domain::service::quota::preflight::QuotaDecision;
use crate::domain::service::stream::ports::FinalizeError;
use crate::domain::service::stream::ports::{ProviderClient, ProviderStream};
use crate::domain::service::stream::test_support::{
    ASSISTANT, CHAT, FakeProvider, ProviderFeed, StreamMetric, TURN, TaskWorld, TurnsCall, at,
    completed, failure, run_downgraded, run_premium, text, usage,
};
use crate::domain::service::stream::types::{
    CitationSource, DeltaKind, ProviderEvent, ToolName, ToolPhase, TurnState,
};
use crate::test_doubles::{LogCapture, USER_TENANT};

const PING: Duration = Duration::from_secs(5);

async fn within<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(600), future)
        .await
        .expect("timed out waiting on the provider task")
}

/// Lets every ready task run (virtual time barely moves).
async fn settle() {
    tokio::time::sleep(Duration::from_millis(1)).await;
}

struct Rig {
    world: TaskWorld,
    token: CancellationToken,
    rx: mpsc::Receiver<StreamEvent>,
    go: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

fn start(world: TaskWorld, run: TurnRun, capacity: usize) -> Rig {
    let (tx, rx) = mpsc::channel(capacity);
    let token = CancellationToken::new();
    let (go_tx, go_rx) = oneshot::channel();
    let task = tokio::spawn(run_provider_task(
        Arc::clone(&world.deps),
        run,
        tx,
        token.clone(),
        go_rx,
    ));
    Rig {
        world,
        token,
        rx,
        go: Some(go_tx),
        task,
    }
}

impl Rig {
    fn go(&mut self) {
        self.go.take().expect("go fired once").send(()).unwrap();
    }

    async fn next(&mut self) -> Option<StreamEvent> {
        within(self.rx.recv()).await
    }

    /// Every event until the channel closes.
    async fn events(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        while let Some(event) = self.next().await {
            out.push(event);
        }
        out
    }

    async fn join(self) -> TaskWorld {
        within(self.task).await.expect("the task did not panic");
        self.world
    }
}

/// A rig with a provider stream, the go-signal fired, `events` queued.
fn running(events: Vec<ProviderEvent>) -> (Rig, ProviderFeed) {
    let world = TaskWorld::new();
    let feed = world.provider.expect_stream();
    feed.push_all(events);
    let mut rig = start(world, run_premium(), 64);
    rig.go();
    (rig, feed)
}

fn delta(content: &str) -> StreamEvent {
    StreamEvent::Delta {
        kind: DeltaKind::Text,
        content: content.to_owned(),
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

fn tool_start(name: ToolName) -> ProviderEvent {
    ProviderEvent::ToolStart {
        name,
        details: json!({}),
    }
}

fn tool_done(name: ToolName, details: serde_json::Value) -> ProviderEvent {
    ProviderEvent::ToolDone { name, details }
}

fn last_finalization(world: &TaskWorld) -> (FinalizeRequestView, FinalizeWrites) {
    let (request, writes) = world.turns.finalizations().pop().expect("a finalization");
    (FinalizeRequestView(request), writes)
}

/// Thin wrapper to read the interesting parts of a finalization request.
struct FinalizeRequestView(crate::domain::service::quota::finalize::FinalizeRequest);

impl FinalizeRequestView {
    fn classification(&self) -> &TerminalClassification {
        &self.0.classification
    }

    fn state(&self) -> TerminalState {
        self.0.columns.state
    }

    fn code(&self) -> Option<&str> {
        self.0.columns.error_code.as_deref()
    }
}

use crate::domain::service::stream::ports::FinalizeWrites;

// ---- the happy path -----------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn text_deltas_are_relayed_in_order_and_the_turn_completes() {
    let (mut rig, _feed) = running(vec![text("Hel"), text("lo"), completed(100, 40)]);
    let events = rig.events().await;
    assert_eq!(names(&events), ["delta", "delta", "done"]);
    assert_eq!(events[0], delta("Hel"));
    assert_eq!(events[1], delta("lo"));
    let StreamEvent::Done(done) = &events[2] else {
        panic!("done expected");
    };
    assert_eq!(done.usage, (100, 40));
    assert_eq!(done.quota_decision, QuotaDecision::Allow);
    assert!(done.quota_warnings.is_some(), "present on a completed turn");
    let world = rig.join().await;
    let (request, writes) = last_finalization(&world);
    assert_eq!(*request.classification(), TerminalClassification::Completed);
    assert_eq!(writes.assistant_message.unwrap().content, "Hello");
}

#[tokio::test(start_paused = true)]
async fn the_provider_is_called_once_with_the_built_request_and_the_request_context() {
    let (mut rig, _feed) = running(vec![completed(1, 1)]);
    rig.events().await;
    let world = rig.join().await;
    assert_eq!(world.provider.open_count(), 1);
    assert_eq!(world.provider.opened()[0], run_premium().request);
    assert_eq!(
        world.provider.contexts()[0].subject_id(),
        run_premium().ctx.subject_id()
    );
}

#[tokio::test(start_paused = true)]
async fn a_completion_without_usage_reports_zero_tokens() {
    let (mut rig, _feed) = running(vec![
        text("x"),
        ProviderEvent::Completed {
            usage: None,
            response_id: None,
            incomplete_reason: None,
        },
    ]);
    let events = rig.events().await;
    let StreamEvent::Done(done) = events.last().unwrap() else {
        panic!("done expected");
    };
    assert_eq!(done.usage, (0, 0));
}

#[tokio::test(start_paused = true)]
async fn a_downgraded_turn_reports_the_downgrade_in_done() {
    let world = TaskWorld::new();
    let feed = world.provider.expect_stream();
    feed.push_all([text("x"), completed(1, 1)]);
    let mut rig = start(world, run_downgraded(), 16);
    rig.go();
    let events = rig.events().await;
    let StreamEvent::Done(done) = events.last().unwrap() else {
        panic!("done expected");
    };
    assert_eq!(done.effective_model, "s");
    assert_eq!(done.selected_model, "p");
    assert_eq!(done.quota_decision, QuotaDecision::Downgrade);
}

// ---- the go-signal and the cancellation before the call ----------------------------------------

#[tokio::test(start_paused = true)]
async fn the_provider_is_not_called_before_the_go_signal() {
    let world = TaskWorld::new();
    let feed = world.provider.expect_stream();
    feed.push(completed(1, 1));
    let mut rig = start(world, run_premium(), 16);
    settle().await;
    settle().await;
    assert_eq!(rig.world.provider.open_count(), 0, "no go, no call");
    assert!(rig.world.turns.finalizations().is_empty());
    rig.go();
    rig.events().await;
    assert_eq!(rig.world.provider.open_count(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_token_cancelled_before_the_call_finalizes_as_cancelled_without_calling_the_provider() {
    let world = TaskWorld::new();
    let _feed = world.provider.expect_stream();
    let mut rig = start(world, run_premium(), 16);
    rig.token.cancel();
    rig.go();
    assert_eq!(
        rig.events().await,
        Vec::<StreamEvent>::new(),
        "nothing is sent"
    );
    let world = rig.join().await;
    assert_eq!(world.provider.open_count(), 0);
    let (request, writes) = last_finalization(&world);
    assert_eq!(*request.classification(), TerminalClassification::Cancelled);
    assert_eq!(request.state(), TerminalState::Cancelled);
    assert_eq!(writes.assistant_message, None, "no assistant message");
    assert_eq!(request.0.usage, None);
    assert_eq!(
        world.metrics.count(|m| *m == StreamMetric::CancelEffective),
        1
    );
    assert_eq!(
        world.metrics.count(|m| *m == StreamMetric::StreamsAborted),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_token_is_seen_even_when_the_go_signal_never_comes() {
    let world = TaskWorld::new();
    let mut rig = start(world, run_premium(), 16);
    rig.token.cancel();
    // `go` is neither fired nor dropped.
    assert!(rig.events().await.is_empty());
    let world = rig.world;
    within(rig.task).await.unwrap();
    assert_eq!(world.provider.open_count(), 0);
    assert_eq!(world.turns.finalizations().len(), 1);
    drop(rig.go);
}

#[tokio::test(start_paused = true)]
async fn a_dropped_go_sender_means_no_go_and_the_turn_is_cancelled() {
    let world = TaskWorld::new();
    let mut rig = start(world, run_premium(), 16);
    drop(rig.go.take());
    assert!(rig.events().await.is_empty());
    let world = rig.join().await;
    assert_eq!(world.provider.open_count(), 0);
    let (request, _) = last_finalization(&world);
    assert_eq!(*request.classification(), TerminalClassification::Cancelled);
}

#[tokio::test(start_paused = true)]
async fn the_active_streams_gauge_returns_to_its_value_after_a_cancellation_before_the_call() {
    let world = TaskWorld::new();
    let mut rig = start(world, run_premium(), 16);
    rig.token.cancel();
    rig.go();
    rig.events().await;
    let world = rig.join().await;
    assert_eq!(world.metrics.active(), 0);
    assert_eq!(
        world
            .metrics
            .count(|m| matches!(m, StreamMetric::Started(..))),
        1
    );
}

// ---- tool events --------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn tool_events_are_relayed_with_their_details_and_interleaved_with_text() {
    let (mut rig, _feed) = running(vec![
        tool_start(ToolName::FileSearch),
        tool_done(ToolName::FileSearch, json!({"files_searched": 3})),
        text("answer"),
        tool_start(ToolName::WebSearch),
        tool_done(ToolName::WebSearch, json!({})),
        completed(1, 2),
    ]);
    let events = rig.events().await;
    assert_eq!(
        names(&events),
        ["tool", "tool", "delta", "tool", "tool", "done"]
    );
    assert_eq!(
        events[0],
        StreamEvent::Tool {
            phase: ToolPhase::Start,
            name: ToolName::FileSearch,
            details: json!({}),
        }
    );
    assert_eq!(
        events[1],
        StreamEvent::Tool {
            phase: ToolPhase::Done,
            name: ToolName::FileSearch,
            details: json!({"files_searched": 3}),
        }
    );
}

#[tokio::test(start_paused = true)]
async fn completed_tool_calls_are_counted_persisted_and_settled() {
    let (mut rig, _feed) = running(vec![
        tool_start(ToolName::WebSearch),
        tool_done(ToolName::WebSearch, json!({})),
        tool_start(ToolName::CodeInterpreter),
        tool_done(ToolName::CodeInterpreter, json!({"output": "ok"})),
        tool_start(ToolName::FileSearch),
        tool_done(ToolName::FileSearch, json!({"files_searched": 1})),
        completed(1, 1),
    ]);
    rig.events().await;
    let world = rig.join().await;
    settle().await;
    // The writes are off the relay path and coalesce while one is in flight;
    // the persisted counts are what they add up to, always with a timestamp.
    let writes = world.turns.progress();
    assert!(!writes.is_empty());
    assert!(writes.iter().all(|p| p.at.is_some()));
    let folded = writes.iter().fold((None, None, None), |acc, p| {
        (
            p.web_search_done.or(acc.0),
            p.code_interpreter_done.or(acc.1),
            p.file_search_done.or(acc.2),
        )
    });
    assert_eq!(folded, (Some(1), Some(1), Some(1)));
    let (request, _) = last_finalization(&world);
    assert_eq!(
        (
            request.0.tool_calls.web_search,
            request.0.tool_calls.code_interpreter,
            request.0.tool_calls.file_search
        ),
        (1, 1, 1),
        "finalization uses the in-memory counts"
    );
}

#[tokio::test(start_paused = true)]
async fn other_tool_starts_are_relayed_without_a_limit() {
    let mut events = Vec::new();
    for _ in 0..20 {
        events.push(tool_start(ToolName::FileSearch));
    }
    events.push(completed(1, 1));
    let (mut rig, _feed) = running(events);
    let relayed = rig.events().await;
    assert_eq!(relayed.len(), 21);
}

#[tokio::test(start_paused = true)]
async fn the_third_web_search_start_exceeds_the_limit_and_fails_the_turn() {
    let (mut rig, feed) = running(vec![
        tool_start(ToolName::WebSearch),
        tool_done(ToolName::WebSearch, json!({})),
        tool_start(ToolName::WebSearch),
        tool_done(ToolName::WebSearch, json!({})),
        tool_start(ToolName::WebSearch),
        text("never relayed"),
        completed(9, 9),
    ]);
    let events = rig.events().await;
    assert_eq!(names(&events), ["tool", "tool", "tool", "tool", "error"]);
    assert_eq!(
        error_code(events.last().unwrap()),
        StreamErrorCode::WebSearchCallsExceeded
    );
    assert!(feed.is_dropped(), "the provider body is dropped");
    let world = rig.join().await;
    let (request, writes) = last_finalization(&world);
    assert_eq!(request.code(), Some("web_search_calls_exceeded"));
    assert_eq!(request.0.usage, None, "no usage after a breach");
    assert_eq!(writes.assistant_message, None);
    assert_eq!(request.0.tool_calls.web_search, 2);
}

#[tokio::test(start_paused = true)]
async fn the_breaching_start_event_is_not_relayed() {
    let (mut rig, _feed) = running(vec![
        tool_start(ToolName::WebSearch),
        tool_start(ToolName::WebSearch),
        tool_start(ToolName::WebSearch),
    ]);
    let events = rig.events().await;
    let starts = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                StreamEvent::Tool {
                    phase: ToolPhase::Start,
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        starts, 2,
        "the third start announces a call that never runs"
    );
}

#[tokio::test(start_paused = true)]
async fn the_eleventh_code_interpreter_start_exceeds_its_limit() {
    let mut script = Vec::new();
    for _ in 0..11 {
        script.push(tool_start(ToolName::CodeInterpreter));
    }
    let (mut rig, _feed) = running(script);
    let events = rig.events().await;
    assert_eq!(
        error_code(events.last().unwrap()),
        StreamErrorCode::CodeInterpreterCallsExceeded
    );
    let world = rig.join().await;
    assert_eq!(
        last_finalization(&world).0.code(),
        Some("code_interpreter_calls_exceeded")
    );
}

// ---- text ---------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn reasoning_deltas_are_relayed_but_not_persisted() {
    let (mut rig, _feed) = running(vec![
        text("a"),
        ProviderEvent::ReasoningDelta {
            text: "thinking".to_owned(),
        },
        text("b"),
        completed(1, 1),
    ]);
    let events = rig.events().await;
    assert_eq!(
        events[1],
        StreamEvent::Delta {
            kind: DeltaKind::Reasoning,
            content: "thinking".to_owned(),
        }
    );
    let world = rig.join().await;
    assert_eq!(
        last_finalization(&world)
            .1
            .assistant_message
            .unwrap()
            .content,
        "ab"
    );
}

// ---- citations ----------------------------------------------------------------------------------

fn annotation(value: serde_json::Value) -> ProviderEvent {
    ProviderEvent::Annotation(crate::domain::service::stream::types::Annotation {
        value,
        item_id: Some("msg_1".to_owned()),
        content_index: Some(0),
        part_text: None,
    })
}

#[tokio::test(start_paused = true)]
async fn citations_follow_the_last_delta_and_precede_done() {
    let (mut rig, _feed) = running(vec![
        text("Hello world"),
        annotation(json!({
            "type": "url_citation", "url": "https://e.org/p", "title": "Page",
            "start_index": 6, "end_index": 11
        })),
        text(" and more"),
        completed(1, 1),
    ]);
    let events = rig.events().await;
    assert_eq!(names(&events), ["delta", "delta", "citations", "done"]);
    let StreamEvent::Citations(items) = &events[2] else {
        panic!("citations expected");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].source, CitationSource::Web);
    assert_eq!(
        items[0].snippet, "world",
        "the snippet comes from the streamed text"
    );
}

#[tokio::test(start_paused = true)]
async fn file_citations_resolve_through_the_citation_map_of_the_turn() {
    let world = TaskWorld::new();
    let feed = world.provider.expect_stream();
    feed.push_all([
        text("x"),
        annotation(json!({"type": "file_citation", "file_id": "file-doc1"})),
        annotation(json!({"type": "file_citation", "file_id": "file-unknown"})),
        completed(1, 1),
    ]);
    let mut run = run_premium();
    run.tools.citation_map.insert(
        "file-doc1".to_owned(),
        crate::domain::service::stream::ports::CitationTarget {
            attachment_id: uuid::Uuid::from_u128(0x1001),
            filename: "report.pdf".to_owned(),
        },
    );
    let mut rig = start(world, run, 16);
    rig.go();
    let events = rig.events().await;
    let StreamEvent::Citations(items) = &events[1] else {
        panic!("citations expected: {events:?}");
    };
    assert_eq!(items.len(), 1, "the unknown file is omitted");
    assert_eq!(items[0].source, CitationSource::File);
    assert_eq!(items[0].title, "report.pdf");
}

#[tokio::test(start_paused = true)]
async fn no_mapped_item_means_no_citations_event() {
    let (mut rig, _feed) = running(vec![
        text("x"),
        annotation(json!({"type": "file_citation", "file_id": "file-unknown"})),
        completed(1, 1),
    ]);
    let events = rig.events().await;
    assert_eq!(names(&events), ["delta", "done"]);
}

#[tokio::test(start_paused = true)]
async fn annotations_of_the_completed_payload_are_mapped_with_their_part_text() {
    let mut carried = crate::domain::service::stream::types::Annotation {
        value: json!({
            "type": "url_citation", "url": "https://e.org", "title": "T",
            "start_index": 0, "end_index": 5
        }),
        item_id: None,
        content_index: None,
        part_text: Some("Hello there".to_owned()),
    };
    carried.item_id = Some("msg_9".to_owned());
    let (mut rig, _feed) = running(vec![
        text("Hello there"),
        ProviderEvent::Annotation(carried),
        completed(1, 1),
    ]);
    let events = rig.events().await;
    let StreamEvent::Citations(items) = &events[1] else {
        panic!("citations expected");
    };
    assert_eq!(items[0].snippet, "Hello");
}

#[tokio::test(start_paused = true)]
async fn an_incomplete_response_sends_no_citations_and_ends_with_done() {
    let (mut rig, _feed) = running(vec![
        text("cut"),
        annotation(json!({"type": "url_citation", "url": "https://e.org", "title": "T"})),
        ProviderEvent::Completed {
            usage: Some(usage(10, 20)),
            response_id: None,
            incomplete_reason: Some("max_output_tokens".to_owned()),
        },
    ]);
    let events = rig.events().await;
    assert_eq!(names(&events), ["delta", "done"]);
    let world = rig.join().await;
    let (request, _) = last_finalization(&world);
    assert_eq!(*request.classification(), TerminalClassification::Completed);
    assert_eq!(request.code(), None);
    assert_eq!(
        world.metrics.count(
            |m| matches!(m, StreamMetric::Incomplete(_, _, reason) if reason == "max_output_tokens")
        ),
        1
    );
    assert_eq!(
        world
            .metrics
            .count(|m| matches!(m, StreamMetric::Completed(..))),
        1
    );
}

// ---- failures -----------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_provider_error_event_fails_the_turn_with_a_sanitized_message() {
    let (mut rig, _feed) = running(vec![
        text("partial"),
        ProviderEvent::ProviderError {
            code: Some("invalid_request_error".to_owned()),
            message: "Invalid file file-abc123def456ghi in resp_9xY2".to_owned(),
            usage: Some(usage(5, 0)),
        },
    ]);
    let events = rig.events().await;
    assert_eq!(names(&events), ["delta", "error"]);
    let StreamEvent::Error(error) = &events[1] else {
        panic!("error expected");
    };
    assert_eq!(error.code, StreamErrorCode::ProviderError);
    assert_eq!(error.message, "Invalid file [provider_id] in [provider_id]");
    let world = rig.join().await;
    let (request, writes) = last_finalization(&world);
    assert_eq!(request.code(), Some("provider_error"));
    assert_eq!(
        request.0.columns.error_detail.as_deref(),
        Some("invalid_request_error: Invalid file [provider_id] in [provider_id]")
    );
    assert_eq!(
        request.0.usage,
        Some(usage(5, 0)),
        "the reported usage is kept"
    );
    assert_eq!(
        writes.assistant_message, None,
        "no message after a provider error"
    );
}

#[tokio::test(start_paused = true)]
async fn an_empty_provider_message_becomes_the_generic_text() {
    let (mut rig, _feed) = running(vec![ProviderEvent::ProviderError {
        code: None,
        message: String::new(),
        usage: None,
    }]);
    let events = rig.events().await;
    let StreamEvent::Error(error) = &events[0] else {
        panic!("error expected");
    };
    assert_eq!(
        error.message,
        StreamErrorCode::ProviderError.generic_message()
    );
}

#[tokio::test(start_paused = true)]
async fn a_stream_that_ends_without_a_terminal_event_is_a_provider_error() {
    let (mut rig, feed) = running(vec![text("a")]);
    feed.end();
    let events = rig.events().await;
    assert_eq!(names(&events), ["delta", "error"]);
    assert_eq!(error_code(&events[1]), StreamErrorCode::ProviderError);
    let world = rig.join().await;
    let (request, _) = last_finalization(&world);
    assert_eq!(request.code(), Some("provider_error"));
    assert_eq!(request.0.usage, None);
}

#[tokio::test(start_paused = true)]
async fn a_failed_call_fails_the_turn_with_the_classified_failure_and_reads_nothing() {
    let world = TaskWorld::new();
    world.provider.fail_open(failure(
        StreamErrorCode::RateLimited,
        "The provider rate limit was reached. Retry after 7 seconds.",
    ));
    let mut rig = start(world, run_premium(), 16);
    rig.go();
    let events = rig.events().await;
    assert_eq!(names(&events), ["error"]);
    let StreamEvent::Error(error) = &events[0] else {
        panic!("error expected");
    };
    assert_eq!(error.code, StreamErrorCode::RateLimited);
    assert!(error.message.contains("7 seconds"));
    let world = rig.join().await;
    assert_eq!(last_finalization(&world).0.code(), Some("rate_limited"));
    assert_eq!(world.metrics.active(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_failed_body_read_fails_the_turn_with_its_code() {
    let (mut rig, feed) = running(vec![text("a")]);
    feed.fail(failure(
        StreamErrorCode::ProviderTimeout,
        "The provider did not answer in time.",
    ));
    let events = rig.events().await;
    assert_eq!(
        error_code(events.last().unwrap()),
        StreamErrorCode::ProviderTimeout
    );
    let world = rig.join().await;
    assert_eq!(last_finalization(&world).0.code(), Some("provider_timeout"));
}

#[tokio::test(start_paused = true)]
async fn the_failure_detail_of_a_classified_failure_is_stored() {
    let world = TaskWorld::new();
    let mut failing = failure(StreamErrorCode::ProviderError, "Bad request [provider_id]");
    failing.detail = Some("provider_error: Bad request [provider_id]".to_owned());
    world.provider.fail_open(failing);
    let mut rig = start(world, run_premium(), 16);
    rig.go();
    rig.events().await;
    let world = rig.join().await;
    assert_eq!(
        last_finalization(&world)
            .0
            .0
            .columns
            .error_detail
            .as_deref(),
        Some("provider_error: Bad request [provider_id]")
    );
}

// ---- tool use (the extension point) -------------------------------------------------------------

fn function_call() -> crate::domain::service::stream::types::FunctionCall {
    crate::domain::service::stream::types::FunctionCall {
        call_id: "call_1".to_owned(),
        name: "search_knowledge".to_owned(),
        arguments: "{\"query\":\"q\"}".to_owned(),
    }
}

#[tokio::test(start_paused = true)]
async fn tool_use_without_a_handler_is_unexpected_tool_use_with_the_usage_of_the_request() {
    let (mut rig, _feed) = running(vec![ProviderEvent::ToolUse {
        calls: vec![function_call()],
        usage: Some(usage(30, 4)),
    }]);
    let events = rig.events().await;
    assert_eq!(
        error_code(events.last().unwrap()),
        StreamErrorCode::UnexpectedToolUse
    );
    let world = rig.join().await;
    let (request, _) = last_finalization(&world);
    assert_eq!(request.code(), Some("unexpected_tool_use"));
    assert_eq!(request.0.usage, Some(usage(30, 4)));
}

/// A handler that answers with scripted outcomes and records its calls.
struct ScriptedToolUse {
    outcomes: Mutex<VecDeque<ToolUseOutcome>>,
    calls: Mutex<Vec<(u32, usize)>>,
    /// `search_knowledge` calls the handler reports through the counters of
    /// the context on every call.
    report_calls: u32,
}

#[async_trait]
impl ToolUseHandler for ScriptedToolUse {
    async fn handle(
        &self,
        context: &ToolUseContext<'_>,
        calls: &[crate::domain::service::stream::types::FunctionCall],
        _usage: Option<mini_chat_sdk::UsageTokens>,
    ) -> ToolUseOutcome {
        self.calls
            .lock()
            .unwrap()
            .push((context.iteration, calls.len()));
        for _ in 0..self.report_calls {
            context.knowledge.record_call();
        }
        self.outcomes
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted outcome")
    }
}

fn scripted(outcomes: Vec<ToolUseOutcome>) -> Arc<ScriptedToolUse> {
    Arc::new(ScriptedToolUse {
        outcomes: Mutex::new(outcomes.into()),
        calls: Mutex::new(Vec::new()),
        report_calls: 0,
    })
}

#[tokio::test(start_paused = true)]
async fn a_handler_that_continues_makes_the_task_read_the_next_stream() {
    let world = TaskWorld::new();
    let first = world.provider.expect_stream();
    // The second request is opened by the handler through the same client.
    let second_provider = FakeProvider::new();
    let second = second_provider.expect_stream();
    let next: Box<dyn ProviderStream> = second_provider
        .open(&run_premium().ctx, &run_premium().request)
        .await
        .map_err(|_| ())
        .unwrap();
    let handler = Arc::new(ScriptedToolUse {
        outcomes: Mutex::new(
            vec![ToolUseOutcome::Continue {
                stream: next,
                request: run_premium().request,
            }]
            .into(),
        ),
        calls: Mutex::new(Vec::new()),
        report_calls: 2,
    });
    let world = world.with_tool_use(Arc::clone(&handler) as Arc<dyn ToolUseHandler>);
    first.push(ProviderEvent::ToolUse {
        calls: vec![function_call()],
        usage: Some(usage(10, 1)),
    });
    second.push_all([text("final answer"), completed(50, 9)]);
    let mut run = run_premium();
    run.tools.knowledge = Some(crate::domain::service::stream::tools::KnowledgeParams {
        max_calls_per_message: 3,
        top_k: 5,
        max_chunk_chars: 2000,
    });
    let mut rig = start(world, run, 16);
    rig.go();
    let events = rig.events().await;
    assert_eq!(names(&events), ["delta", "done"]);
    assert_eq!(handler.calls.lock().unwrap().clone(), vec![(1, 1)]);
    let world = rig.join().await;
    let (request, writes) = last_finalization(&world);
    assert_eq!(
        request.0.usage,
        Some(usage(50, 9)),
        "only the last iteration is settled"
    );
    assert_eq!(
        request.0.tool_calls.file_search, 2,
        "the knowledge call count"
    );
    assert_eq!(writes.assistant_message.unwrap().content, "final answer");
    assert!(first.is_dropped(), "the first stream is released");
}

#[tokio::test(start_paused = true)]
async fn a_handler_failure_fails_the_turn_with_its_code_and_the_usage_of_the_last_request() {
    let world = TaskWorld::new();
    let first = world.provider.expect_stream();
    let handler = scripted(vec![ToolUseOutcome::Fail(failure(
        StreamErrorCode::AgenticIterationsExceeded,
        StreamErrorCode::AgenticIterationsExceeded.generic_message(),
    ))]);
    let world = world.with_tool_use(handler as Arc<dyn ToolUseHandler>);
    first.push(ProviderEvent::ToolUse {
        calls: vec![function_call()],
        usage: Some(usage(10, 1)),
    });
    let mut rig = start(world, run_premium(), 16);
    rig.go();
    let events = rig.events().await;
    assert_eq!(
        error_code(events.last().unwrap()),
        StreamErrorCode::AgenticIterationsExceeded
    );
    let world = rig.join().await;
    let (request, _) = last_finalization(&world);
    assert_eq!(request.code(), Some("agentic_iterations_exceeded"));
    assert_eq!(request.0.usage, Some(usage(10, 1)));
}

// ---- the lost CAS and finalization failures -----------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_lost_cas_ends_the_task_without_a_terminal_event() {
    let world = TaskWorld::new();
    let feed = world.provider.expect_stream();
    feed.push_all([text("a"), completed(1, 1)]);
    world
        .turns
        .with(|t| t.finalize_results.push_back(Ok(FinalizeResult::Lost)));
    let mut rig = start(world, run_premium(), 16);
    rig.go();
    let events = rig.events().await;
    assert_eq!(names(&events), ["delta"], "no done after a lost CAS");
    let world = rig.join().await;
    assert_eq!(
        world
            .metrics
            .count(|m| matches!(m, StreamMetric::Completed(..))),
        0
    );
    assert_eq!(world.metrics.active(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_failed_finalization_sends_finalization_failed_instead_of_done() {
    let world = TaskWorld::new();
    let feed = world.provider.expect_stream();
    feed.push_all([text("a"), completed(1, 1)]);
    world.turns.with(|t| {
        t.finalize_results
            .push_back(Err(FinalizeError::Other(DomainError::Database(
                "down".to_owned(),
            ))));
    });
    let mut rig = start(world, run_premium(), 16);
    rig.go();
    let events = rig.events().await;
    assert_eq!(names(&events), ["delta", "error"]);
    assert_eq!(error_code(&events[1]), StreamErrorCode::FinalizationFailed);
}

#[tokio::test(start_paused = true)]
async fn done_is_sent_only_after_the_finalization_returned() {
    let world = TaskWorld::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    *world.turns.gates.finalize_gate.lock().unwrap() = Some(Arc::clone(&gate));
    let feed = world.provider.expect_stream();
    feed.push_all([text("a"), completed(1, 1)]);
    let mut rig = start(world, run_premium(), 16);
    rig.go();
    assert_eq!(rig.next().await, Some(delta("a")));
    settle().await;
    assert!(
        rig.rx.try_recv().is_err(),
        "done must not be queued while the transaction is still open"
    );
    gate.notify_one();
    assert_eq!(rig.next().await.unwrap().name(), "done");
}

// ---- cancellation -------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_disconnect_drops_the_provider_stream_at_once_even_when_the_provider_is_silent() {
    let (mut rig, feed) = running(vec![text("par"), text("tial")]);
    assert_eq!(rig.next().await, Some(delta("par")));
    assert_eq!(rig.next().await, Some(delta("tial")));
    settle().await;
    let before = Instant::now();
    rig.token.cancel();
    within(feed.dropped()).await;
    assert!(
        before.elapsed() < Duration::from_millis(200),
        "dropped after {:?}",
        before.elapsed()
    );
    assert!(
        rig.events().await.is_empty(),
        "nothing is written after a disconnect"
    );
    let world = rig.join().await;
    let (request, writes) = last_finalization(&world);
    assert_eq!(*request.classification(), TerminalClassification::Cancelled);
    assert_eq!(request.state(), TerminalState::Cancelled);
    assert_eq!(request.0.usage, None);
    assert_eq!(writes.assistant_message.unwrap().content, "partial");
    assert_eq!(
        world.metrics.count(|m| *m == StreamMetric::CancelEffective),
        1
    );
    assert_eq!(
        world.metrics.count(|m| *m == StreamMetric::StreamsAborted),
        1
    );
    assert_eq!(world.metrics.count(|m| *m == StreamMetric::TimeToAbort), 1);
    assert!(world.metrics.values("time_to_abort_ms")[0] < 200.0);
    assert_eq!(world.metrics.active(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_disconnect_before_any_delta_persists_no_message() {
    let (mut rig, feed) = running(vec![]);
    settle().await;
    rig.token.cancel();
    within(feed.dropped()).await;
    rig.events().await;
    let world = rig.join().await;
    let (request, writes) = last_finalization(&world);
    assert_eq!(request.0.columns.assistant_message_id, None);
    assert_eq!(writes.assistant_message, None);
}

#[tokio::test(start_paused = true)]
async fn a_disconnect_while_the_call_is_still_pending_drops_the_call() {
    let world = TaskWorld::new();
    world.provider.hang_open();
    let mut rig = start(world, run_premium(), 16);
    rig.go();
    settle().await;
    assert_eq!(rig.world.provider.open_count(), 1, "the call was made");
    let before = Instant::now();
    rig.token.cancel();
    rig.events().await;
    assert!(before.elapsed() < Duration::from_millis(200));
    let world = rig.join().await;
    assert_eq!(
        world.provider.open_calls_dropped(),
        1,
        "the pending call was dropped"
    );
    let (request, writes) = last_finalization(&world);
    assert_eq!(*request.classification(), TerminalClassification::Cancelled);
    assert_eq!(writes.assistant_message, None);
}

#[tokio::test(start_paused = true)]
async fn a_disconnect_while_a_send_waits_for_channel_capacity_ends_the_task() {
    let world = TaskWorld::new();
    let feed = world.provider.expect_stream();
    feed.push_all([text("a"), text("b"), text("c")]);
    let mut rig = start(world, run_premium(), 1);
    rig.go();
    settle().await;
    assert_eq!(
        feed.pulled(),
        2,
        "one event queued, the next one blocked on the send"
    );
    rig.token.cancel();
    within(feed.dropped()).await;
    let world = {
        let rx_events = rig.rx.recv().await;
        assert_eq!(rx_events, Some(delta("a")));
        rig.join().await
    };
    let (request, writes) = last_finalization(&world);
    assert_eq!(*request.classification(), TerminalClassification::Cancelled);
    assert_eq!(writes.assistant_message.unwrap().content, "ab");
}

#[tokio::test(start_paused = true)]
async fn a_failed_send_because_the_receiver_is_gone_is_a_disconnect() {
    let world = TaskWorld::new();
    let feed = world.provider.expect_stream();
    feed.push_all([text("a"), text("b"), text("c")]);
    let mut rig = start(world, run_premium(), 1);
    rig.go();
    settle().await;
    drop(std::mem::replace(&mut rig.rx, mpsc::channel(1).1));
    within(feed.dropped()).await;
    let world = rig.world;
    within(rig.task).await.unwrap();
    let (request, _) = last_finalization(&world);
    assert_eq!(*request.classification(), TerminalClassification::Cancelled);
    assert_eq!(
        world.metrics.count(|m| *m == StreamMetric::CancelEffective),
        1
    );
    assert_eq!(
        world.metrics.count(|m| *m == StreamMetric::CancelRequested),
        0,
        "the cancel_requested count belongs to the dropped relay, not to the failed send"
    );
}

#[tokio::test(start_paused = true)]
async fn a_provider_terminal_event_received_before_the_disconnect_is_finalized_as_it_is() {
    let world = TaskWorld::new();
    let feed = world.provider.expect_stream();
    // Capacity 1: the delta fills the channel, the citations event then blocks.
    feed.push_all([
        text("a"),
        annotation(json!({"type": "url_citation", "url": "https://e.org", "title": "T"})),
        completed(7, 3),
    ]);
    let mut rig = start(world, run_premium(), 1);
    rig.go();
    settle().await;
    assert_eq!(feed.pulled(), 3, "the completion was read");
    rig.token.cancel();
    assert_eq!(rig.rx.recv().await, Some(delta("a")));
    let world = rig.join().await;
    let (request, writes) = last_finalization(&world);
    assert_eq!(*request.classification(), TerminalClassification::Completed);
    assert_eq!(request.0.usage, Some(usage(7, 3)));
    assert_eq!(writes.assistant_message.unwrap().content, "a");
    assert_eq!(
        world.metrics.count(|m| *m == StreamMetric::CancelEffective),
        0
    );
}

#[tokio::test(start_paused = true)]
async fn the_disconnect_is_not_waited_for_on_a_provider_that_never_sends() {
    // No go-between: the token is the only thing that ends this turn.
    let (mut rig, feed) = running(vec![]);
    settle().await;
    let before = Instant::now();
    rig.token.cancel();
    within(feed.dropped()).await;
    assert_eq!(before.elapsed(), Duration::ZERO, "no timer was waited for");
    rig.events().await;
}

// ---- liveness -----------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn the_task_checks_the_turn_state_once_per_ping_interval() {
    let (mut rig, _feed) = running(vec![]);
    tokio::time::sleep(PING * 4 + Duration::from_secs(1)).await;
    assert_eq!(rig.world.turns.state_check_count(), 4);
    assert!(
        !rig.task.is_finished(),
        "the task adds no deadline of its own"
    );
    assert!(rig.rx.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn the_liveness_query_is_tenant_scoped_and_chat_bound() {
    let (mut rig, _feed) = running(vec![]);
    tokio::time::sleep(PING + Duration::from_secs(1)).await;
    assert_eq!(
        rig.world
            .turns
            .calls()
            .iter()
            .find(|c| matches!(c, TurnsCall::TurnState(..))),
        Some(&TurnsCall::TurnState(USER_TENANT, CHAT, TURN))
    );
    let _ = &mut rig;
}

#[tokio::test(start_paused = true)]
async fn a_turn_moved_by_the_watchdog_ends_the_task_without_finalizing() {
    let world = TaskWorld::new();
    let feed = world.provider.expect_stream();
    world.turns.with(|t| {
        t.turn_states.push_back(Ok(Some(TurnState::Running)));
        t.turn_states.push_back(Ok(Some(TurnState::Failed)));
    });
    let mut rig = start(world, run_premium(), 16);
    rig.go();
    let begin = Instant::now();
    assert!(
        rig.events().await.is_empty(),
        "the relay synthesizes stream_interrupted"
    );
    assert_eq!(begin.elapsed(), PING * 2, "noticed at the second check");
    assert!(feed.is_dropped(), "the provider stream is dropped");
    let world = rig.join().await;
    assert!(
        world.turns.finalizations().is_empty(),
        "no CAS attempt, nothing settled"
    );
    assert!(world.quota_metrics.events().is_empty());
    assert_eq!(world.metrics.active(), 0);
    assert_eq!(
        world.metrics.count(|m| matches!(
            m,
            StreamMetric::Completed(..) | StreamMetric::Failed(..) | StreamMetric::CancelEffective
        )),
        0
    );
}

#[tokio::test(start_paused = true)]
async fn the_watchdog_is_noticed_within_one_interval_of_its_commit() {
    let world = TaskWorld::new();
    let feed = world.provider.expect_stream();
    let mut rig = start(world, run_premium(), 16);
    rig.go();
    tokio::time::sleep(Duration::from_secs(12)).await;
    // The watchdog commits at t = 12 s.
    rig.world
        .turns
        .with(|t| t.default_turn_state = Some(TurnState::Failed));
    let committed = Instant::now();
    within(feed.dropped()).await;
    assert!(committed.elapsed() <= PING, "{:?}", committed.elapsed());
    rig.events().await;
}

#[tokio::test(start_paused = true)]
async fn a_failed_liveness_check_is_logged_and_retried_at_the_next_interval() {
    let logs = LogCapture::start();
    let world = TaskWorld::new();
    let feed = world.provider.expect_stream();
    world.turns.with(|t| {
        t.turn_states
            .push_back(Err(DomainError::Database("db down".to_owned())));
        t.turn_states.push_back(Ok(Some(TurnState::Running)));
        t.turn_states.push_back(Ok(Some(TurnState::Cancelled)));
    });
    let mut rig = start(world, run_premium(), 16);
    rig.go();
    let begin = Instant::now();
    rig.events().await;
    assert_eq!(
        begin.elapsed(),
        PING * 3,
        "the error and the Running check did not end it"
    );
    assert!(feed.is_dropped());
    assert!(
        !logs.at(tracing::Level::WARN).is_empty(),
        "the failed check is logged"
    );
}

#[tokio::test(start_paused = true)]
async fn a_pending_liveness_check_never_delays_an_event_and_never_overlaps_another() {
    let world = TaskWorld::new();
    let gate = Arc::new(tokio::sync::Notify::new());
    *world.turns.gates.state_gate.lock().unwrap() = Some(Arc::clone(&gate));
    let feed = world.provider.expect_stream();
    let mut rig = start(world, run_premium(), 16);
    rig.go();
    tokio::time::sleep(PING + Duration::from_secs(1)).await;
    assert_eq!(
        rig.world.turns.state_check_count(),
        1,
        "the check is in flight"
    );
    feed.push(text("hello"));
    assert_eq!(
        rig.next().await,
        Some(delta("hello")),
        "relayed while the check is pending"
    );
    tokio::time::sleep(PING * 3).await;
    assert_eq!(
        rig.world.turns.state_check_count(),
        1,
        "at most one check in flight"
    );
    assert_eq!(
        rig.world
            .turns
            .max_state_checks_in_flight
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    gate.notify_one();
    feed.push(completed(1, 1));
    assert_eq!(rig.next().await.unwrap().name(), "done");
}

// ---- progress writes ----------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn last_progress_is_refreshed_at_most_every_thirty_seconds_on_text() {
    let world = TaskWorld::with_ping(Duration::from_secs(60));
    let feed = world.provider.expect_stream();
    let origin = Instant::now();
    let mut rig = start(world, run_premium(), 64);
    rig.go();
    for (secs, expect_writes) in [(1, 0), (29, 0), (31, 1), (40, 1), (62, 2)] {
        tokio::time::sleep_until(origin + Duration::from_secs(secs)).await;
        feed.push(text("x"));
        rig.next().await;
        settle().await;
        assert_eq!(
            rig.world.turns.progress().len(),
            expect_writes,
            "at {secs}s"
        );
    }
    assert!(
        rig.world
            .turns
            .progress()
            .iter()
            .all(|p| p.web_search_done.is_none() && p.at.is_some())
    );
}

#[tokio::test(start_paused = true)]
async fn a_tool_start_refreshes_the_progress_under_the_same_throttle() {
    let world = TaskWorld::with_ping(Duration::from_secs(60));
    let feed = world.provider.expect_stream();
    let origin = Instant::now();
    let mut rig = start(world, run_premium(), 64);
    rig.go();
    tokio::time::sleep_until(origin + Duration::from_secs(10)).await;
    feed.push(tool_start(ToolName::FileSearch));
    rig.next().await;
    settle().await;
    assert!(
        rig.world.turns.progress().is_empty(),
        "within 30 s of the start"
    );
    tokio::time::sleep_until(origin + Duration::from_secs(35)).await;
    feed.push(tool_start(ToolName::FileSearch));
    rig.next().await;
    settle().await;
    assert_eq!(rig.world.turns.progress().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_reasoning_delta_refreshes_the_progress_under_the_same_throttle() {
    // A model that only reasons for a long time (vLLM `<think>` text) is alive:
    // its reasoning deltas keep `last_progress_at` fresh like text and tools do.
    let world = TaskWorld::with_ping(Duration::from_secs(60));
    let feed = world.provider.expect_stream();
    let origin = Instant::now();
    let mut rig = start(world, run_premium(), 64);
    rig.go();
    tokio::time::sleep_until(origin + Duration::from_secs(10)).await;
    feed.push(ProviderEvent::ReasoningDelta {
        text: "hmm".to_owned(),
    });
    rig.next().await;
    settle().await;
    assert!(
        rig.world.turns.progress().is_empty(),
        "within 30 s of the start"
    );
    tokio::time::sleep_until(origin + Duration::from_secs(35)).await;
    feed.push(ProviderEvent::ReasoningDelta {
        text: "still thinking".to_owned(),
    });
    rig.next().await;
    settle().await;
    assert_eq!(rig.world.turns.progress().len(), 1);
    assert!(rig.world.turns.progress()[0].at.is_some());
    tokio::time::sleep_until(origin + Duration::from_secs(50)).await;
    feed.push(ProviderEvent::ReasoningDelta {
        text: "again".to_owned(),
    });
    rig.next().await;
    settle().await;
    assert_eq!(
        rig.world.turns.progress().len(),
        1,
        "the throttle of 30 s applies to reasoning too"
    );
}

#[tokio::test(start_paused = true)]
async fn a_progress_write_is_dispatched_off_the_relay_path_with_one_in_flight() {
    let world = TaskWorld::with_ping(Duration::from_secs(60));
    let gate = Arc::new(tokio::sync::Notify::new());
    *world.turns.gates.progress_gate.lock().unwrap() = Some(Arc::clone(&gate));
    let feed = world.provider.expect_stream();
    let mut rig = start(world, run_premium(), 64);
    rig.go();
    tokio::time::sleep(Duration::from_secs(31)).await;
    feed.push(text("a"));
    assert_eq!(
        rig.next().await,
        Some(delta("a")),
        "not delayed by the blocked write"
    );
    tokio::time::sleep(Duration::from_secs(31)).await;
    feed.push(text("b"));
    assert_eq!(rig.next().await, Some(delta("b")));
    assert_eq!(
        rig.world
            .turns
            .count(|c| matches!(c, TurnsCall::Progress(_))),
        1,
        "one write in flight, the next waits"
    );
    gate.notify_waiters();
    settle().await;
    tokio::time::sleep(Duration::from_secs(31)).await;
    feed.push(text("c"));
    rig.next().await;
    settle().await;
    assert!(
        rig.world
            .turns
            .count(|c| matches!(c, TurnsCall::Progress(_)))
            >= 2
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_progress_write_is_logged_and_retried_with_the_next_event() {
    let logs = LogCapture::start();
    let world = TaskWorld::with_ping(Duration::from_secs(60));
    world.turns.with(|t| t.progress_failures = 1);
    let feed = world.provider.expect_stream();
    let mut rig = start(world, run_premium(), 64);
    rig.go();
    tokio::time::sleep(Duration::from_secs(31)).await;
    feed.push(text("a"));
    rig.next().await;
    settle().await;
    assert!(
        rig.world.turns.progress().is_empty(),
        "the first write failed"
    );
    assert!(!logs.at(tracing::Level::WARN).is_empty());
    feed.push(text("b"));
    rig.next().await;
    settle().await;
    assert_eq!(
        rig.world.turns.progress().len(),
        1,
        "retried with the next event"
    );
}

// ---- metrics ------------------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_completed_stream_records_started_completed_ttft_and_latency_with_low_cardinality_labels()
{
    let world = TaskWorld::new();
    let feed = world.provider.expect_stream();
    let mut rig = start(world, run_premium(), 16);
    rig.go();
    tokio::time::sleep(Duration::from_millis(120)).await;
    feed.push(text("a"));
    tokio::time::sleep(Duration::from_millis(30)).await;
    feed.push(text("b"));
    feed.push(completed(1, 1));
    rig.events().await;
    let world = rig.join().await;
    let events = world.metrics.events();
    assert!(events.contains(&StreamMetric::Started("openai".to_owned(), "p".to_owned())));
    assert!(events.contains(&StreamMetric::Completed(
        "openai".to_owned(),
        "p".to_owned()
    )));
    assert_eq!(
        world
            .metrics
            .count(|m| matches!(m, StreamMetric::TtftProvider(..))),
        1
    );
    assert_eq!(
        world
            .metrics
            .count(|m| matches!(m, StreamMetric::TtftOverhead(..))),
        1
    );
    assert_eq!(
        world
            .metrics
            .count(|m| matches!(m, StreamMetric::TotalLatency(..))),
        1
    );
    let ttft = world.metrics.values("ttft_provider_ms")[0];
    assert!((120.0..130.0).contains(&ttft), "ttft {ttft}");
    let total = world.metrics.values("stream_total_latency_ms")[0];
    assert!((150.0..170.0).contains(&total), "total {total}");
    assert!(world.metrics.values("ttft_overhead_ms")[0] < 50.0);
    assert_eq!(world.metrics.active(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_tool_event_counts_as_the_first_content_event_for_the_ttft() {
    let (mut rig, _feed) = running(vec![tool_start(ToolName::FileSearch), completed(1, 1)]);
    rig.events().await;
    let world = rig.join().await;
    assert_eq!(
        world
            .metrics
            .count(|m| matches!(m, StreamMetric::TtftProvider(..))),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_stream_records_the_error_code_and_no_completion() {
    let (mut rig, _feed) = running(vec![ProviderEvent::ProviderError {
        code: None,
        message: "boom".to_owned(),
        usage: None,
    }]);
    rig.events().await;
    let world = rig.join().await;
    assert_eq!(
        world.metrics.count(|m| *m
            == StreamMetric::Failed(
                "openai".to_owned(),
                "p".to_owned(),
                "provider_error".to_owned()
            )),
        1
    );
    assert_eq!(
        world
            .metrics
            .count(|m| matches!(m, StreamMetric::Completed(..))),
        0
    );
    assert_eq!(
        world
            .metrics
            .count(|m| matches!(m, StreamMetric::TotalLatency(..))),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_stream_records_its_total_latency_but_neither_completion_nor_failure() {
    let (mut rig, feed) = running(vec![text("a")]);
    rig.next().await;
    rig.token.cancel();
    within(feed.dropped()).await;
    rig.events().await;
    let world = rig.join().await;
    assert_eq!(
        world
            .metrics
            .count(|m| matches!(m, StreamMetric::TotalLatency(..))),
        1
    );
    assert_eq!(
        world
            .metrics
            .count(|m| matches!(m, StreamMetric::Completed(..) | StreamMetric::Failed(..))),
        0
    );
}

#[tokio::test(start_paused = true)]
async fn no_metric_of_the_label_set_carries_an_identifier() {
    let (mut rig, _feed) = running(vec![text("a"), completed(1, 1)]);
    rig.events().await;
    let world = rig.join().await;
    let debug = format!("{:?}", world.metrics.events());
    for forbidden in [
        CHAT.to_string(),
        TURN.to_string(),
        ASSISTANT.to_string(),
        USER_TENANT.to_string(),
        "resp_internal".to_owned(),
    ] {
        assert!(!debug.contains(&forbidden), "{forbidden} in {debug}");
    }
    let _ = at(0);
}

// ---- backpressure and no buffering --------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn each_delta_reaches_the_reader_before_the_next_provider_event_is_released() {
    let world = TaskWorld::new();
    let feed = world.provider.expect_stream();
    let mut rig = start(world, run_premium(), 16);
    rig.go();
    for n in 0..5 {
        feed.push(text(&format!("d{n}")));
        assert_eq!(rig.next().await, Some(delta(&format!("d{n}"))));
        assert_eq!(
            feed.pulled(),
            n + 1,
            "no event was read ahead of its release"
        );
    }
    feed.push(completed(1, 1));
    assert_eq!(rig.next().await.unwrap().name(), "done");
}

#[tokio::test(start_paused = true)]
async fn a_reader_that_stops_blocks_the_provider_task_at_the_channel_capacity() {
    let world = TaskWorld::new();
    let feed = world.provider.expect_stream();
    for n in 0..100 {
        feed.push(text(&format!("d{n}")));
    }
    let mut rig = start(world, run_premium(), 4);
    rig.go();
    settle().await;
    assert!(
        feed.pulled() <= 4 + 1,
        "{} events pulled for a channel of 4",
        feed.pulled()
    );
    // Reading resumes the flow, in order and without loss.
    for n in 0..100 {
        assert_eq!(rig.next().await, Some(delta(&format!("d{n}"))));
    }
}

#[tokio::test(start_paused = true)]
async fn a_thousand_deltas_are_relayed_as_a_thousand_delta_events() {
    let world = TaskWorld::new();
    let feed = world.provider.expect_stream();
    for n in 0..1000 {
        feed.push(text(&format!("{n},")));
    }
    feed.push(completed(1, 1));
    let mut rig = start(world, run_premium(), 32);
    rig.go();
    let events = rig.events().await;
    assert_eq!(events.len(), 1001);
    assert!(
        events[..1000]
            .iter()
            .all(|e| matches!(e, StreamEvent::Delta { .. }))
    );
    let world = rig.join().await;
    let content = last_finalization(&world)
        .1
        .assistant_message
        .unwrap()
        .content;
    assert!(content.starts_with("0,1,2,") && content.ends_with("999,"));
}

#[tokio::test(start_paused = true)]
async fn a_silent_provider_is_not_cut_off_by_the_task() {
    let (mut rig, _feed) = running(vec![text("a")]);
    assert_eq!(rig.next().await, Some(delta("a")));
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert!(
        !rig.task.is_finished(),
        "the gear adds no deadline of its own"
    );
    assert!(rig.world.turns.finalizations().is_empty());
}
