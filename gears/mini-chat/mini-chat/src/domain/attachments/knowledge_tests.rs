//! The knowledge search loop over a scripted retriever, a recording turn-progress port and the
//! recording metrics: the iteration cap (`max_calls_per_message` plus 2), the retrieval limit and
//! its limit output, `top_k` capped at the configuration, chunks trimmed to `max_chunk_chars`, the
//! failure output without a counter increment, a tool call of another name ending the turn with
//! `unexpected_tool_use`, the guarded `file_search_completed_count` increment, the knowledge
//! metrics, cancellation, the in-memory file search count and the tool-result items of the two
//! provider formats.
//!
//! Slice S2-deletion-cleanup-tools of `cpt-cf-mini-chat-feature-attachments-and-file-search`:
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop` (`inst-kl-*`),
//! flow `cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files`
//! (`inst-ask-knowledge`, `inst-ask-knowledge-loop`), algorithm
//! `cpt-cf-mini-chat-algo-attachments-and-file-search-metrics` (`inst-met-knowledge`); definition
//! of done `cpt-cf-mini-chat-dod-attachments-and-file-search-knowledge-loop`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    KnowledgeLoop, KnowledgeLoopDeps, KnowledgeLoopSettings, LoopStep, LoopStop, RetrievalFailure,
    SEARCH_LIMIT_OUTPUT, ToolCall, tool_result_item,
};
use crate::config::KnowledgeSearchConfig;
use crate::domain::attachments::contributions::{SEARCH_KNOWLEDGE, ToolResultFormat};
use crate::domain::error::StreamErrorCode;
use crate::domain::streaming::ToolKind;
use crate::test_support::TENANT_A;
use crate::test_support::attach_fakes::RecordingAttachmentMetrics;
use crate::test_support::tool_fakes::{RecordingProgress, ScriptedRetriever, chunk};

const TURN: Uuid = Uuid::from_u128(0x7A12);

struct Harness {
    knowledge: KnowledgeLoop,
    retriever: Arc<ScriptedRetriever>,
    progress: Arc<RecordingProgress>,
    metrics: Arc<RecordingAttachmentMetrics>,
    cancel: CancellationToken,
}

fn harness_with(config: &KnowledgeSearchConfig) -> Harness {
    let retriever = ScriptedRetriever::new();
    let progress = RecordingProgress::new();
    let metrics = RecordingAttachmentMetrics::new();
    let knowledge = KnowledgeLoop::new(
        KnowledgeLoopDeps {
            retriever: retriever.clone(),
            progress: progress.clone(),
            metrics: metrics.clone(),
            settings: KnowledgeLoopSettings::from_config(config),
        },
        TENANT_A,
        TURN,
    );
    Harness {
        knowledge,
        retriever,
        progress,
        metrics,
        cancel: CancellationToken::new(),
    }
}

fn harness() -> Harness {
    harness_with(&KnowledgeSearchConfig::default())
}

fn search(query: &str) -> ToolCall {
    ToolCall {
        name: SEARCH_KNOWLEDGE.to_owned(),
        call_id: "call_1".to_owned(),
        arguments: json!({ "query": query }).to_string(),
    }
}

fn search_top_k(query: &str, top_k: u64) -> ToolCall {
    ToolCall {
        name: SEARCH_KNOWLEDGE.to_owned(),
        call_id: "call_1".to_owned(),
        arguments: json!({ "query": query, "top_k": top_k }).to_string(),
    }
}

fn other(name: &str) -> ToolCall {
    ToolCall {
        name: name.to_owned(),
        call_id: "call_x".to_owned(),
        arguments: "{}".to_owned(),
    }
}

impl Harness {
    async fn answer(&mut self, call: &ToolCall) -> LoopStep {
        let cancel = self.cancel.clone();
        self.knowledge.answer(call, &cancel).await
    }
}

fn output_of(step: LoopStep) -> String {
    match step {
        LoopStep::Continue { output } => output,
        other @ LoopStep::Stop(_) => panic!("expected an answer, got {other:?}"),
    }
}

// ── the iteration cap ────────────────────────────────────────────────────────

#[test]
fn the_hard_cap_is_max_calls_per_message_plus_two() {
    // inst-kl-iterations
    for (max_calls, cap) in [(1, 3), (3, 5), (10, 12)] {
        let config = KnowledgeSearchConfig {
            max_calls_per_message: max_calls,
            ..KnowledgeSearchConfig::default()
        };
        assert_eq!(
            KnowledgeLoopSettings::from_config(&config).max_iterations(),
            cap
        );
    }
}

#[test]
fn the_settings_are_the_knowledge_search_configuration_values() {
    // inst-cfg-knowledge: `max_calls_per_message` 3, `top_k` 5, `max_chunk_chars` 2000
    let settings = KnowledgeLoopSettings::from_config(&KnowledgeSearchConfig::default());
    assert_eq!(settings.max_calls_per_message, 3);
    assert_eq!(settings.top_k, 5);
    assert_eq!(settings.max_chunk_chars, 2000);
}

#[tokio::test]
async fn the_first_provider_iteration_counts_and_every_answer_issues_the_next() {
    // inst-kl-iterations, inst-kl-next
    let mut h = harness();
    assert_eq!(h.knowledge.iterations(), 1);
    h.answer(&search("a")).await;
    assert_eq!(h.knowledge.iterations(), 2);
    h.answer(&search("b")).await;
    assert_eq!(h.knowledge.iterations(), 3);
}

#[tokio::test]
async fn the_sixth_provider_iteration_ends_the_turn_with_agentic_iterations_exceeded() {
    // inst-kl-limit-output, inst-kl-cap: three retrievals, the fourth and fifth calls get the
    // limit output, and the next iteration would be the sixth
    let mut h = harness();
    for call in 1..=3 {
        let output = output_of(h.answer(&search(&format!("q{call}"))).await);
        assert_ne!(output, SEARCH_LIMIT_OUTPUT, "call {call}");
    }
    assert_eq!(h.retriever.calls().len(), 3);
    let fourth = output_of(h.answer(&search("q4")).await);
    assert!(
        fourth.to_lowercase().contains("search limit reached"),
        "{fourth}"
    );
    assert_eq!(
        h.knowledge.iterations(),
        5,
        "the fifth iteration was issued"
    );
    let fifth = h.answer(&search("q5")).await;
    assert_eq!(
        fifth,
        LoopStep::Stop(LoopStop::Failed(StreamErrorCode::AgenticIterationsExceeded))
    );
    assert_eq!(
        h.retriever.calls().len(),
        3,
        "the limit calls never retrieve"
    );
}

#[tokio::test]
async fn a_lower_limit_moves_both_the_limit_output_and_the_cap() {
    // inst-kl-limit-output, inst-kl-cap with `max_calls_per_message` 1: the cap is 3
    let config = KnowledgeSearchConfig {
        max_calls_per_message: 1,
        ..KnowledgeSearchConfig::default()
    };
    let mut h = harness_with(&config);
    let first = output_of(h.answer(&search("a")).await);
    assert_ne!(first, SEARCH_LIMIT_OUTPUT);
    let second = output_of(h.answer(&search("b")).await);
    assert!(second.to_lowercase().contains("search limit reached"));
    assert_eq!(
        h.answer(&search("c")).await,
        LoopStep::Stop(LoopStop::Failed(StreamErrorCode::AgenticIterationsExceeded))
    );
    assert_eq!(h.retriever.calls().len(), 1);
}

// ── the unexpected tool ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_call_of_another_tool_name_ends_the_turn_with_unexpected_tool_use() {
    // inst-kl-unexpected: no retrieval, no counter, no metric
    for name in [
        "web_search",
        "get_weather",
        "",
        "SEARCH_KNOWLEDGE",
        "search_knowledge ",
    ] {
        let mut h = harness();
        assert_eq!(
            h.answer(&other(name)).await,
            LoopStep::Stop(LoopStop::Failed(StreamErrorCode::UnexpectedToolUse)),
            "{name:?}"
        );
        assert!(h.retriever.calls().is_empty(), "{name:?}");
        assert!(h.progress.increments().is_empty(), "{name:?}");
        assert!(h.metrics.knowledge().is_empty(), "{name:?}");
        assert_eq!(h.knowledge.file_search_calls(), 0);
    }
}

#[tokio::test]
async fn an_unexpected_tool_in_a_later_iteration_also_ends_the_turn() {
    // inst-kl-unexpected inside the loop
    let mut h = harness();
    h.answer(&search("a")).await;
    assert_eq!(
        h.answer(&other("web_search")).await,
        LoopStep::Stop(LoopStop::Failed(StreamErrorCode::UnexpectedToolUse))
    );
    assert_eq!(h.retriever.calls().len(), 1);
}

// ── the retrieval ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_successful_retrieval_answers_with_the_chunks_and_counts_it() {
    // inst-kl-retrieve, inst-kl-success, inst-kl-append
    let mut h = harness();
    h.retriever
        .queue(Ok(vec![chunk("vacation days: 25"), chunk("carry over: 5")]));
    let output = output_of(h.answer(&search("vacation policy")).await);
    assert!(output.contains("vacation days: 25"), "{output}");
    assert!(output.contains("carry over: 5"), "{output}");
    let calls = h.retriever.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].query, "vacation policy");
    assert_eq!(calls[0].tenant_id, TENANT_A);
    assert_eq!(h.knowledge.file_search_calls(), 1);
    // the durable counter: `search_knowledge` counts as `file_search`
    assert_eq!(
        h.progress.increments(),
        vec![(TURN, ToolKind::SearchKnowledge)]
    );
}

#[tokio::test]
async fn the_success_records_the_result_the_latency_and_the_chunk_count() {
    // inst-kl-success, inst-met-knowledge
    let mut h = harness();
    h.retriever
        .queue(Ok(vec![chunk("a"), chunk("b"), chunk("c")]));
    h.answer(&search("q")).await;
    assert_eq!(h.metrics.knowledge(), vec!["ok".to_owned()]);
    assert_eq!(h.metrics.knowledge_latencies(), 1);
    assert_eq!(h.metrics.knowledge_chunks(), vec![3]);
}

#[tokio::test]
async fn top_k_is_capped_at_the_configuration() {
    // inst-kl-retrieve: the model's `top_k` capped at `knowledge_search.top_k` (default 5)
    for (asked, sent) in [(50, 5), (6, 5), (5, 5), (2, 2), (1, 1)] {
        let mut h = harness();
        h.answer(&search_top_k("q", asked)).await;
        assert_eq!(h.retriever.calls()[0].top_k, sent, "asked {asked}");
    }
}

#[tokio::test]
async fn an_absent_top_k_uses_the_configured_one() {
    let config = KnowledgeSearchConfig {
        top_k: 4,
        ..KnowledgeSearchConfig::default()
    };
    let mut h = harness_with(&config);
    h.answer(&search("q")).await;
    assert_eq!(h.retriever.calls()[0].top_k, 4);
}

#[tokio::test]
async fn every_chunk_is_trimmed_to_max_chunk_chars() {
    // inst-kl-success: trim each chunk to `knowledge_search.max_chunk_chars`
    let config = KnowledgeSearchConfig {
        max_chunk_chars: 10,
        ..KnowledgeSearchConfig::default()
    };
    let mut h = harness_with(&config);
    h.retriever.queue(Ok(vec![
        chunk("0123456789ABCDEFGHIJ"),
        chunk("short"),
        chunk("exactly-10"),
    ]));
    let output = output_of(h.answer(&search("q")).await);
    assert!(output.contains("0123456789"), "{output}");
    assert!(
        !output.contains("0123456789A"),
        "the tail is trimmed: {output}"
    );
    assert!(output.contains("short"), "{output}");
    assert!(output.contains("exactly-10"), "{output}");
}

#[tokio::test]
async fn the_trim_counts_characters_not_bytes() {
    let config = KnowledgeSearchConfig {
        max_chunk_chars: 4,
        ..KnowledgeSearchConfig::default()
    };
    let mut h = harness_with(&config);
    h.retriever.queue(Ok(vec![chunk(&"\u{e9}".repeat(8))]));
    let output = output_of(h.answer(&search("q")).await);
    assert!(output.contains(&"\u{e9}".repeat(4)), "{output}");
    assert!(!output.contains(&"\u{e9}".repeat(5)), "{output}");
}

#[tokio::test]
async fn a_failed_retrieval_answers_with_a_short_failure_output_and_no_counter() {
    // inst-kl-failure: the failure output, `result="error"`, no `file_search_completed_count`
    for failure in [
        RetrievalFailure::Transport,
        RetrievalFailure::Status(500),
        RetrievalFailure::Status(429),
        RetrievalFailure::Malformed,
    ] {
        let mut h = harness();
        h.retriever.queue(Err(failure));
        let output = output_of(h.answer(&search("q")).await);
        assert!(!output.is_empty(), "{failure:?}");
        assert!(output.len() < 200, "a short output: {output}");
        assert!(h.progress.increments().is_empty(), "{failure:?}");
        assert_eq!(
            h.metrics.knowledge(),
            vec!["error".to_owned()],
            "{failure:?}"
        );
        assert!(h.metrics.knowledge_chunks().is_empty(), "{failure:?}");
        assert_eq!(h.knowledge.file_search_calls(), 1, "the attempt is counted");
    }
}

#[tokio::test]
async fn a_failed_retrieval_does_not_end_the_turn() {
    // the model answers from what it has: the next iteration is issued
    let mut h = harness();
    h.retriever.queue(Err(RetrievalFailure::Transport));
    assert!(matches!(
        h.answer(&search("q")).await,
        LoopStep::Continue { .. }
    ));
    assert_eq!(h.knowledge.iterations(), 2);
}

#[tokio::test]
async fn the_failure_output_leaks_no_provider_text() {
    let mut h = harness();
    h.retriever.queue(Err(RetrievalFailure::Status(500)));
    let output = output_of(h.answer(&search("q")).await);
    for needle in ["500", "provider-secret", "vs_", "azure"] {
        assert!(!output.contains(needle), "{needle} in {output}");
    }
}

#[tokio::test]
async fn only_successful_retrievals_increment_the_durable_counter() {
    // dod-knowledge-loop: successful retrievals count in `file_search_completed_count`
    let mut h = harness();
    h.retriever.queue(Ok(vec![chunk("a")]));
    h.retriever.queue(Err(RetrievalFailure::Transport));
    h.retriever.queue(Ok(vec![chunk("b")]));
    for call in 1..=3 {
        h.answer(&search(&format!("q{call}"))).await;
    }
    assert_eq!(h.progress.increments().len(), 2);
    assert_eq!(
        h.knowledge.file_search_calls(),
        3,
        "reported as file_search_calls"
    );
    assert_eq!(
        h.metrics.knowledge(),
        vec!["ok".to_owned(), "error".to_owned(), "ok".to_owned()]
    );
}

#[tokio::test]
async fn a_limit_answer_records_no_metric_and_no_counter() {
    // inst-kl-limit-output: no retrieval, so none of the retrieval series
    let mut h = harness();
    for call in 1..=3 {
        h.answer(&search(&format!("q{call}"))).await;
    }
    let before = (h.metrics.knowledge().len(), h.progress.increments().len());
    h.answer(&search("q4")).await;
    assert_eq!(
        (h.metrics.knowledge().len(), h.progress.increments().len()),
        before
    );
    assert_eq!(h.knowledge.file_search_calls(), 3);
}

// ── cancellation ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_fired_cancellation_token_stops_the_loop_without_the_next_iteration() {
    // inst-kl-cancelled: the disconnect path finalizes the turn
    let mut h = harness();
    h.retriever.cancel_during_search(&h.cancel);
    assert_eq!(
        h.answer(&search("q")).await,
        LoopStep::Stop(LoopStop::Cancelled)
    );
    assert_eq!(h.knowledge.iterations(), 1, "no next provider request");
}

#[tokio::test(start_paused = true)]
async fn a_cancellation_ends_a_retrieval_that_never_resolves() {
    // the retrieval is raced against the turn cancellation (paused time: no real wait)
    let mut h = harness();
    h.retriever.hang_next(1);
    let cancel = h.cancel.clone();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        cancel.cancel();
    });
    assert_eq!(
        h.answer(&search("q")).await,
        LoopStep::Stop(LoopStop::Cancelled)
    );
    canceller.await.unwrap();
    assert_eq!(h.knowledge.iterations(), 1);
    assert!(h.progress.increments().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_retrieval_slower_than_the_timeout_is_answered_with_a_failure_output() {
    // inst-kl-failure: no counter increment, `result="error"`, the loop goes on
    let mut h = harness();
    h.retriever.hang_next(1);
    let step = h.answer(&search("q")).await;
    let LoopStep::Continue { output } = step else {
        panic!("{step:?}");
    };
    assert!(!output.is_empty());
    assert!(h.progress.increments().is_empty());
    assert_eq!(h.metrics.knowledge(), vec!["error".to_owned()]);
    assert_eq!(h.knowledge.file_search_calls(), 1);
    assert_eq!(h.knowledge.iterations(), 2);
}

// ── the tool-result items ────────────────────────────────────────────────────

#[test]
fn the_openai_item_is_a_function_call_output() {
    // inst-ks-return: the format selected by the provider kind
    let call = search("q");
    assert_eq!(
        tool_result_item(ToolResultFormat::FunctionCallOutput, &call, "the chunks"),
        json!({ "type": "function_call_output", "call_id": "call_1", "output": "the chunks" })
    );
}

#[test]
fn the_anthropic_item_is_a_tool_result_block() {
    let call = search("q");
    assert_eq!(
        tool_result_item(ToolResultFormat::ToolResult, &call, "the chunks"),
        json!({ "type": "tool_result", "tool_use_id": "call_1", "content": "the chunks" })
    );
}
