//! The streaming metrics of the provider task, the finalizer and the SSE relay over the in-memory
//! OTLP pipeline: stream started, completed, incomplete, failed and disconnected, the active
//! streams gauge, the three latency histograms, the cancellation series, the image inputs and the
//! thread-summary trigger, their low-cardinality labels, and the series that stay unrecorded.
//!
//! Slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithm
//! `streaming-metrics`; flow `admin-observability`; definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-observability`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde_json::json;
use uuid::Uuid;

use crate::domain::streaming::{RelayEvent, StreamOutcome, StreamTerminal};
use crate::test_support::provider::{
    Gate, GatewayFailure, MockProvider, Reply, Step, completed, delta, happy_steps, incomplete,
    web_done, web_start,
};
use crate::test_support::quota::{PREMIUM, STANDARD};
use crate::test_support::send::{
    AttachmentSeed, SendRig, SendRigOptions, seed_att, seed_completed_turn, send_rig_with, t,
};

const PROVIDER: &str = "azure_openai";

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

async fn metered(
    upstream: Arc<MockProvider>,
) -> (SendRig, crate::infra::storage::repo_impl::ChatRecord) {
    let rig = send_rig_with(SendRigOptions::new().with_upstream(upstream).with_metrics()).await;
    let chat = rig.std_chat().await;
    (rig, chat)
}

fn counts(rig: &SendRig, name: &str) -> Vec<(BTreeMap<String, String>, i64)> {
    rig.metrics.as_ref().unwrap().series(name)
}

fn total(rig: &SendRig, name: &str) -> i64 {
    rig.metrics.as_ref().unwrap().total(name)
}

async fn run_to_end(
    rig: &SendRig,
    chat: &crate::infra::storage::repo_impl::ChatRecord,
) -> Vec<RelayEvent> {
    let mut turn = rig.start_turn(chat, &json!({ "content": "hi" })).await;
    let events = turn.drain().await;
    rig.wait_terminal(chat.id).await;
    events
}

// ── completed, incomplete and failed ─────────────────────────────────────────

#[tokio::test]
async fn a_completed_stream_records_started_completed_the_latencies_and_the_finalization() {
    let (rig, chat) = metered(MockProvider::answering(Reply::Sse(happy_steps(
        &["a", "b"],
        (10, 5),
    ))))
    .await;
    run_to_end(&rig, &chat).await;
    let expected = vec![(labels(&[("provider", PROVIDER), ("model", STANDARD)]), 1)];
    assert_eq!(counts(&rig, "stream_started"), expected);
    assert_eq!(counts(&rig, "stream_completed"), expected);
    for absent in ["stream_failed", "stream_incomplete", "stream_disconnected"] {
        assert_eq!(total(&rig, absent), 0, "{absent}");
    }
    // one sample of each latency histogram, labelled by provider and model
    for histogram in [
        "ttft_provider_ms",
        "ttft_overhead_ms",
        "stream_total_latency_ms",
    ] {
        assert_eq!(counts(&rig, histogram), expected, "{histogram}");
    }
    assert_eq!(total(&rig, "finalization_latency_ms"), 1);
    // no image, no summary trigger evaluated, no cancellation
    for absent in [
        "image_inputs_per_turn",
        "cancel_requested",
        "cancel_effective",
        "streams_aborted",
        "time_to_abort_ms",
    ] {
        assert_eq!(total(&rig, absent), 0, "{absent}");
    }
}

#[tokio::test]
async fn an_incomplete_stream_records_incomplete_with_the_reason_and_completed() {
    let steps = vec![delta("cut"), incomplete("max_output_tokens", (10, 500))];
    let (rig, chat) = metered(MockProvider::answering(Reply::Sse(steps))).await;
    run_to_end(&rig, &chat).await;
    assert_eq!(
        counts(&rig, "stream_incomplete"),
        vec![(
            labels(&[
                ("provider", PROVIDER),
                ("model", STANDARD),
                ("reason", "max_output_tokens")
            ]),
            1
        )]
    );
    assert_eq!(total(&rig, "stream_completed"), 1);
    assert_eq!(total(&rig, "stream_failed"), 0);
}

#[tokio::test]
async fn failed_streams_record_the_streaming_error_code() {
    for (reply, code) in [
        (
            Reply::http(500, &json!({ "error": { "message": "boom" } })),
            "provider_error",
        ),
        (
            Reply::http_with(429, &[("retry-after", "3")], &json!({})),
            "rate_limited",
        ),
        (Reply::Fail(GatewayFailure::Timeout), "provider_timeout"),
    ] {
        let (rig, chat) = metered(MockProvider::answering(reply)).await;
        run_to_end(&rig, &chat).await;
        assert_eq!(
            counts(&rig, "stream_failed"),
            vec![(
                labels(&[
                    ("provider", PROVIDER),
                    ("model", STANDARD),
                    ("error_code", code)
                ]),
                1
            )],
            "{code}"
        );
        assert_eq!(total(&rig, "stream_started"), 1, "{code}");
        assert_eq!(total(&rig, "stream_completed"), 0, "{code}");
    }
}

#[tokio::test]
async fn a_tool_limit_breach_records_failed_with_its_code() {
    let steps = vec![
        web_start(),
        web_done(),
        web_start(),
        web_done(),
        web_start(),
        completed("x", (1, 1)),
    ];
    let (rig, chat) = metered(MockProvider::answering(Reply::Sse(steps))).await;
    let mut turn = rig
        .start_turn(
            &chat,
            &json!({ "content": "s", "web_search": { "enabled": true } }),
        )
        .await;
    turn.drain().await;
    rig.wait_terminal(chat.id).await;
    assert_eq!(
        counts(&rig, "stream_failed"),
        vec![(
            labels(&[
                ("provider", PROVIDER),
                ("model", STANDARD),
                ("error_code", "web_search_calls_exceeded")
            ]),
            1
        )]
    );
}

// ── active streams ───────────────────────────────────────────────────────────

#[tokio::test]
async fn the_active_streams_gauge_is_one_while_the_stream_is_open_and_zero_after() {
    let gate = Gate::new();
    let steps = vec![
        delta("a"),
        Step::Wait(gate.clone()),
        delta("b"),
        completed("ab", (1, 1)),
    ];
    let (rig, chat) = metered(MockProvider::answering(Reply::Sse(steps))).await;
    let mut turn = rig.start_turn(&chat, &json!({ "content": "hi" })).await;
    assert!(matches!(turn.next().await, Some(RelayEvent::Delta(_))));
    assert_eq!(total(&rig, "active_streams"), 1);
    gate.open();
    turn.drain().await;
    rig.wait_terminal(chat.id).await;
    assert_eq!(total(&rig, "active_streams"), 0);
}

// ── disconnects and cancellation ─────────────────────────────────────────────

#[tokio::test]
async fn a_mid_stream_disconnect_records_the_cancellation_series() {
    let gate = Gate::new();
    let steps = vec![delta("part"), Step::Wait(gate), completed("part", (1, 1))];
    let upstream = MockProvider::answering(Reply::Sse(steps));
    let (rig, chat) = metered(upstream.clone()).await;
    let mut turn = rig.start_turn(&chat, &json!({ "content": "hi" })).await;
    assert!(matches!(turn.next().await, Some(RelayEvent::Delta(_))));
    turn.cancel.cancel();
    turn.drain().await;
    upstream.wait_for_aborted(1).await;
    rig.wait_terminal(chat.id).await;

    assert_eq!(
        counts(&rig, "stream_disconnected"),
        vec![(labels(&[("stage", "mid_stream")]), 1)]
    );
    assert_eq!(
        counts(&rig, "cancel_requested"),
        vec![(labels(&[("trigger", "disconnect")]), 1)]
    );
    assert_eq!(
        counts(&rig, "cancel_effective"),
        vec![(labels(&[("trigger", "disconnect")]), 1)]
    );
    assert_eq!(total(&rig, "time_to_abort_ms"), 1);
    assert_eq!(
        counts(&rig, "streams_aborted"),
        vec![(labels(&[("trigger", "client_disconnect")]), 1)]
    );
    // a cancelled stream is neither completed nor failed
    assert_eq!(total(&rig, "stream_completed"), 0);
    assert_eq!(total(&rig, "stream_failed"), 0);
    assert_eq!(total(&rig, "active_streams"), 0);
}

#[tokio::test]
async fn a_disconnect_before_the_first_delta_is_stage_before_first_token() {
    let gate = Gate::new();
    let steps = vec![Step::Wait(gate), delta("never"), completed("never", (1, 1))];
    let upstream = MockProvider::answering(Reply::Sse(steps));
    let (rig, chat) = metered(upstream.clone()).await;
    let turn = rig.start_turn(&chat, &json!({ "content": "hi" })).await;
    upstream.wait_for_calls(1).await;
    turn.cancel.cancel();
    drop(turn.events);
    upstream.wait_for_aborted(1).await;
    rig.wait_terminal(chat.id).await;
    assert_eq!(
        counts(&rig, "stream_disconnected"),
        vec![(labels(&[("stage", "before_first_token")]), 1)]
    );
}

#[tokio::test]
async fn a_disconnect_seen_only_as_a_failed_send_does_not_count_as_cancel_requested() {
    let gate = Gate::new();
    let steps = vec![
        delta("first"),
        Step::Wait(gate.clone()),
        delta("second"),
        delta("third"),
        completed("x", (1, 1)),
    ];
    let upstream = MockProvider::answering(Reply::Sse(steps));
    let (rig, chat) = metered(upstream.clone()).await;
    let mut turn = rig.start_turn(&chat, &json!({ "content": "hi" })).await;
    assert!(matches!(turn.next().await, Some(RelayEvent::Delta(_))));
    let crate::test_support::send::RunningTurn { events, .. } = turn;
    drop(events);
    gate.open();
    rig.wait_terminal(chat.id).await;
    assert_eq!(total(&rig, "stream_disconnected"), 1);
    assert_eq!(total(&rig, "cancel_requested"), 0);
    assert_eq!(total(&rig, "streams_aborted"), 1);
}

// ── images and the summary trigger ───────────────────────────────────────────

#[tokio::test]
async fn a_request_with_images_records_image_inputs_once() {
    let upstream = MockProvider::answering(Reply::Sse(happy_steps(&["ok"], (1, 1))));
    let rig = send_rig_with(SendRigOptions::new().with_upstream(upstream).with_metrics()).await;
    let chat = rig.prem_chat().await;
    let one = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
    let two = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
    let mut turn = rig
        .start_turn(
            &chat,
            &json!({ "content": "look", "attachment_ids": [one.to_string(), two.to_string()] }),
        )
        .await;
    turn.drain().await;
    rig.wait_terminal(chat.id).await;
    assert_eq!(total(&rig, "image_inputs_per_turn"), 1);
    assert_eq!(turn.plan.decision.effective_model, PREMIUM);
}

#[tokio::test]
async fn the_summary_trigger_is_recorded_after_the_commit_with_its_result() {
    let rig = send_rig_with(SendRigOptions::new().with_metrics()).await;
    let chat = rig.std_chat().await;
    seed_completed_turn(&rig.quota.db, &chat, "q", "a", t(10)).await;
    let mut plan = rig.plan_for(&chat, "new").await;
    plan.context.assembled_context_tokens = 900;
    plan.context.effective_budget = 1000;
    let terminal = StreamTerminal {
        outcome: StreamOutcome::Completed,
        usage: None,
        provider_response_id: None,
        text: "answer".to_owned(),
        tool_counts: crate::domain::quota::ToolCallCounts::default(),
        incomplete_reason: None,
    };
    rig.finalizer
        .finalize_stream(&plan, &terminal)
        .await
        .unwrap();
    assert_eq!(
        counts(&rig, "thread_summary_trigger"),
        vec![(labels(&[("result", "scheduled")]), 1)]
    );
    assert_eq!(total(&rig, "finalization_latency_ms"), 1);
}

#[tokio::test]
async fn an_evaluated_trigger_with_nothing_to_schedule_is_recorded_not_needed_and_no_trigger_nothing()
 {
    // the first turn of an empty chat: the trigger fires but there is no message to summarize
    let rig = send_rig_with(SendRigOptions::new().with_metrics()).await;
    let chat = rig.std_chat().await;
    let mut plan = rig.plan_for(&chat, "first").await;
    plan.context.assembled_context_tokens = 990;
    plan.context.effective_budget = 1000;
    let terminal = StreamTerminal {
        outcome: StreamOutcome::Completed,
        usage: None,
        provider_response_id: None,
        text: "answer".to_owned(),
        tool_counts: crate::domain::quota::ToolCallCounts::default(),
        incomplete_reason: None,
    };
    rig.finalizer
        .finalize_stream(&plan, &terminal)
        .await
        .unwrap();
    assert_eq!(
        counts(&rig, "thread_summary_trigger"),
        vec![(labels(&[("result", "not_needed")]), 1)]
    );

    // below the threshold the trigger is not evaluated: nothing is recorded for the turn
    let rig = send_rig_with(SendRigOptions::new().with_metrics()).await;
    let chat = rig.std_chat().await;
    let mut plan = rig.plan_for(&chat, "first").await;
    plan.context.assembled_context_tokens = 10;
    plan.context.effective_budget = 1000;
    rig.finalizer
        .finalize_stream(&plan, &terminal)
        .await
        .unwrap();
    assert_eq!(total(&rig, "thread_summary_trigger"), 0);
}

// ── replay, unrecorded series and label cardinality ──────────────────────────

#[tokio::test]
async fn a_replay_records_nothing() {
    let upstream = MockProvider::answering(Reply::Sse(happy_steps(&["once"], (1, 1))));
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream.clone())
            .with_metrics(),
    )
    .await;
    let chat = rig.std_chat().await;
    let request_id = Uuid::new_v4();
    let body = json!({ "content": "hi", "request_id": request_id.to_string() });
    let first = rig.stream(chat.id, &body).await;
    assert_eq!(first.status, axum::http::StatusCode::OK, "{}", first.text);
    rig.wait_terminal(chat.id).await;
    let before = rig.metrics.as_ref().unwrap().all_points();
    let again = rig.stream(chat.id, &body).await;
    assert_eq!(again.status, axum::http::StatusCode::OK, "{}", again.text);
    assert_eq!(rig.metrics.as_ref().unwrap().all_points(), before);
    assert_eq!(upstream.call_count(), 1);
    assert_eq!(total(&rig, "stream_replay"), 0);
}

#[tokio::test]
async fn the_declared_but_unrecorded_series_stay_unrecorded_and_labels_stay_low_cardinality() {
    let gate = Gate::new();
    let scripts: Vec<Vec<Step>> = vec![
        // tools, citations and a normal completion
        vec![web_start(), web_done(), delta("a"), completed("a", (1, 1))],
        // an incomplete stream
        vec![delta("b"), incomplete("max_output_tokens", (1, 2))],
        // a failure after a delta
        vec![delta("c"), Step::Break],
        // a disconnect
        vec![delta("d"), Step::Wait(gate), completed("d", (1, 1))],
    ];
    let upstream = MockProvider::new();
    for script in scripts {
        upstream.queue(Reply::Sse(script));
    }
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream.clone())
            .with_metrics(),
    )
    .await;
    let chat = rig.std_chat().await;
    for index in 0..3 {
        let mut turn = rig
            .start_turn(
                &chat,
                &json!({ "content": format!("q{index}"), "web_search": { "enabled": index == 0 } }),
            )
            .await;
        turn.drain().await;
        rig.wait_terminal(chat.id).await;
        // free the chat for the next turn: terminal turns never block it
    }
    let mut turn = rig.start_turn(&chat, &json!({ "content": "last" })).await;
    assert!(matches!(turn.next().await, Some(RelayEvent::Delta(_))));
    turn.cancel.cancel();
    turn.drain().await;
    upstream.wait_for_aborted(1).await;
    rig.wait_terminal(chat.id).await;

    let metrics = rig.metrics.as_ref().unwrap();
    let recorded = metrics.recorded();
    for never in [
        "stream_replay",
        "tool_calls",
        "tool_call_limited",
        "web_search_latency_ms",
        "citations_count",
        "citations_by_source",
        "context_truncation",
        "provider_requests",
        "provider_errors",
        "provider_latency_ms",
        "tokens_after_cancel",
        "time_from_ui_disconnect_to_cancel_ms",
        "cancel_orphan",
    ] {
        assert!(
            !recorded.contains(never),
            "`{never}` was recorded: {recorded:?}"
        );
    }
    let allowed: BTreeSet<&str> = [
        "provider",
        "model",
        "error_code",
        "reason",
        "stage",
        "trigger",
        "decision",
        "tier",
        "period",
        "result",
    ]
    .into_iter()
    .collect();
    for (name, attributes, _) in metrics.all_points() {
        for (key, value) in &attributes {
            assert!(allowed.contains(key.as_str()), "label `{key}` on {name}");
            assert!(
                Uuid::parse_str(value).is_err(),
                "an identifier as the value of `{key}` on {name}"
            );
            assert!(!value.contains("resp_"), "{name}: {value}");
        }
    }
    // only the trigger values of the specification are ever emitted
    for (attributes, _) in metrics
        .series("cancel_requested")
        .into_iter()
        .chain(metrics.series("streams_aborted"))
    {
        let trigger = attributes.get("trigger").unwrap();
        assert!(
            ["disconnect", "client_disconnect", "orphan_timeout"].contains(&trigger.as_str()),
            "{trigger}"
        );
    }
}
