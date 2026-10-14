//! `POST /v1/chats/{id}/messages:stream` end to end over the real router, SQLite, the real quota
//! service, the real provider task and a mock OAGW: the SSE response and its event contract
//! (headers, `stream_started`, `delta`, `tool`, `citations`, `done`, `error`, the event order),
//! ping before the first content only, no buffering, a bounded channel, `stream_interrupted`
//! when the watchdog wins, every disconnect case, replay and recovery after a real stream, and the
//! persisted rows seen through the messages list, the turn status and `message_count`.
//!
//! Slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`: flows `send-message`,
//! `disconnect`, `web-search-turn`; algorithms `sse-relay`, `terminal-emission`,
//! `event-translation`, `citations`, `message-order` (assistant parts); states `sse-stream` and
//! `turn-lifecycle`; definitions of done
//! `cpt-cf-mini-chat-dod-message-streaming-sse-contract`,
//! `cpt-cf-mini-chat-dod-message-streaming-cancellation`,
//! `cpt-cf-mini-chat-dod-message-streaming-finalization`,
//! `cpt-cf-mini-chat-dod-message-streaming-turn-lifecycle` and
//! `cpt-cf-mini-chat-dod-message-streaming-tools-web-search`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};
use time::Duration as TimeDuration;
use uuid::Uuid;

use crate::domain::streaming::StreamSettings;
use crate::infra::provider_task::{ProviderTaskSettings, S2sContext};
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::chats::{a1, assert_problem, chat_uri, keys, message_rows, parse_ts};
use crate::test_support::provider::{
    Gate, MockProvider, Reply, Step, completed, completed_parts, delta, happy_steps,
    numbered_deltas, numbered_text, output_part, url_annotation, web_done, web_start,
};
use crate::test_support::quota::{FixedClock, PREMIUM, STANDARD, default_now};
use crate::test_support::send::{
    ReserveGate, SendRig, SendRigOptions, delta_text, footprint, messages_uri, names, rig_settings,
    rig_task_settings, seed_summary, send_rig_with, stream_rig, t, turn_rows, turn_uri,
};

fn question(text: &str) -> Value {
    json!({ "content": text })
}

async fn streamed(reply: Reply) -> (SendRig, ChatRecord, Arc<MockProvider>) {
    let upstream = MockProvider::answering(reply);
    let rig = stream_rig(upstream.clone()).await;
    let chat = rig.std_chat().await;
    (rig, chat, upstream)
}

/// The order grammar of the specification: `stream_started ping* (delta | tool)* citations?
/// (done | error)`.
fn assert_grammar(frames: &[crate::test_support::send::SseFrame]) {
    let names = names(frames);
    assert_eq!(names.first(), Some(&"stream_started"), "{names:?}");
    let last = *names.last().unwrap();
    assert!(last == "done" || last == "error", "{names:?}");
    let body = &names[1..names.len() - 1];
    let mut content_seen = false;
    let mut citations_seen = false;
    for name in body {
        match *name {
            "ping" => assert!(!content_seen, "a ping after content: {names:?}"),
            "delta" | "tool" => {
                assert!(!citations_seen, "content after citations: {names:?}");
                content_seen = true;
            }
            "citations" => {
                assert!(!citations_seen, "two citations events: {names:?}");
                citations_seen = true;
            }
            other => panic!("unexpected event `{other}` in {names:?}"),
        }
    }
    assert_eq!(
        names
            .iter()
            .filter(|n| **n == "done" || **n == "error")
            .count(),
        1,
        "exactly one terminal event: {names:?}"
    );
}

// ── the SSE response and its event contract ──────────────────────────────────

#[tokio::test]
async fn a_streamed_answer_is_a_well_formed_sse_response_with_the_specified_events() {
    let (rig, chat, _upstream) = streamed(Reply::Sse(happy_steps(&["Hel", "lo"], (120, 30)))).await;
    let request_id = Uuid::new_v4();
    let reply = rig
        .stream(
            chat.id,
            &json!({ "content": "what is rust?", "request_id": request_id.to_string() }),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert!(
        reply.content_type.starts_with("text/event-stream"),
        "{}",
        reply.content_type
    );

    let frames = crate::test_support::send::parse_sse(&reply.text);
    assert_eq!(
        names(&frames),
        vec!["stream_started", "delta", "delta", "done"],
        "{}",
        reply.text
    );
    assert_grammar(&frames);

    let started = &frames[0].data;
    assert_eq!(
        keys(started),
        BTreeSet::from(["request_id", "message_id", "is_new_turn"])
    );
    assert_eq!(started["request_id"], request_id.to_string());
    assert_eq!(started["is_new_turn"], true);

    assert_eq!(frames[1].data, json!({ "type": "text", "content": "Hel" }));
    assert_eq!(frames[2].data, json!({ "type": "text", "content": "lo" }));

    let done = &frames[3].data;
    assert_eq!(
        done["usage"],
        json!({ "input_tokens": 120, "output_tokens": 30 })
    );
    assert_eq!(done["effective_model"], STANDARD);
    assert_eq!(done["selected_model"], STANDARD);
    assert_eq!(done["quota_decision"], "allow");
    for hidden in [
        "request_id",
        "message_id",
        "turn_id",
        "provider_response_id",
        "reserved_credits_micro",
        "billing_outcome",
        "downgrade_from",
        "downgrade_reason",
    ] {
        assert!(done.get(hidden).is_none(), "`{hidden}` in {done}");
    }
    assert!(
        !reply.text.contains("resp_"),
        "a provider id reached the client: {}",
        reply.text
    );

    // `message_id` is the persisted assistant message and the status API's id
    let message_id = started["message_id"].as_str().unwrap().to_owned();
    let messages = message_rows(&rig.quota.db, chat.id).await;
    let assistant = messages.iter().find(|m| m.role == "assistant").unwrap();
    assert_eq!(assistant.id.to_string(), message_id);
    assert_eq!(assistant.content, "Hello");
    let status = rig.get(&turn_uri(chat.id, request_id), &a1()).await.json();
    assert_eq!(status["state"], "done");
    assert_eq!(status["assistant_message_id"], message_id);
}

#[tokio::test]
async fn the_response_has_the_stream_headers_and_no_compression() {
    let (rig, chat, _upstream) = streamed(Reply::Sse(happy_steps(&["x"], (1, 1)))).await;
    let mut open = rig.open_stream(chat.id, &question("hi")).await;
    assert_eq!(open.status, StatusCode::OK);
    assert!(
        open.content_type.starts_with("text/event-stream"),
        "{}",
        open.content_type
    );
    assert_eq!(open.headers.get("cache-control").unwrap(), "no-cache");
    assert!(open.headers.get("content-encoding").is_none());
    // no `Connection: keep-alive` header on the response
    assert!(
        open.headers
            .get("connection")
            .is_none_or(|v| !v.to_str().unwrap().eq_ignore_ascii_case("keep-alive")),
        "{:?}",
        open.headers
    );
    open.rest().await;
}

#[tokio::test]
async fn a_server_generated_request_id_is_a_uuid_v4() {
    let (rig, chat, _upstream) = streamed(Reply::Sse(happy_steps(&["x"], (1, 1)))).await;
    let reply = rig.stream(chat.id, &question("hi")).await;
    let started = crate::test_support::send::parse_sse(&reply.text)
        .remove(0)
        .data;
    let request_id = Uuid::parse_str(started["request_id"].as_str().unwrap()).unwrap();
    assert_eq!(request_id.get_version_num(), 4);
}

#[tokio::test]
async fn a_thread_summary_in_the_context_is_announced_in_stream_started() {
    let (rig, chat, upstream) = streamed(Reply::Sse(happy_steps(&["x"], (1, 1)))).await;
    seed_summary(
        &rig.quota.db,
        &chat,
        "the earlier talk",
        42,
        (t(1), Uuid::new_v4()),
    )
    .await;
    let reply = rig.stream(chat.id, &question("hi")).await;
    let started = crate::test_support::send::parse_sse(&reply.text)
        .remove(0)
        .data;
    assert_eq!(
        started["thread_summary_applied"],
        json!({ "token_estimate": 42 })
    );
    // the summary reached the provider as the first input item
    let input = upstream.only_call().body["input"].clone();
    assert!(
        input[0]["content"]
            .as_str()
            .unwrap()
            .contains("the earlier talk"),
        "{input}"
    );
}

#[tokio::test]
async fn tool_events_and_citations_arrive_in_order_with_the_specified_payloads() {
    let text = "Growth was strong in 2025 according to analysts.";
    let steps = vec![
        delta("Growth "),
        web_start(),
        web_done(),
        delta("was strong in 2025 according to analysts."),
        completed_parts(
            &[output_part(
                text,
                &[url_annotation(
                    "https://example.com/m",
                    "Market Analysis",
                    7,
                    18,
                )],
            )],
            (200, 50),
        ),
    ];
    let (rig, chat, _upstream) = streamed(Reply::Sse(steps)).await;
    let reply = rig
        .stream(
            chat.id,
            &json!({ "content": "market?", "web_search": { "enabled": true } }),
        )
        .await;
    let frames = crate::test_support::send::parse_sse(&reply.text);
    assert_eq!(
        names(&frames),
        vec![
            "stream_started",
            "delta",
            "tool",
            "tool",
            "delta",
            "citations",
            "done"
        ],
        "{}",
        reply.text
    );
    assert_grammar(&frames);
    assert_eq!(
        frames[2].data,
        json!({ "phase": "start", "name": "web_search", "details": {} })
    );
    assert_eq!(
        frames[3].data,
        json!({ "phase": "done", "name": "web_search", "details": {} })
    );
    let items = frames[5].data["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    let item = &items[0];
    assert_eq!(item["source"], "web");
    assert_eq!(item["title"], "Market Analysis");
    assert_eq!(item["url"], "https://example.com/m");
    assert_eq!(item["snippet"], "was strong ");
    assert_eq!(item["span"], json!({ "start": 7, "end": 18 }));
    assert!(item.get("score").is_none(), "{item}");
    assert!(!reply.text.contains("resp_"));
}

#[tokio::test]
async fn a_downgrade_is_reported_in_done() {
    let upstream = MockProvider::answering(Reply::Sse(happy_steps(&["x"], (7, 8))));
    let rig = stream_rig(upstream).await;
    rig.gateway.set_limits((60_000_000, 600_000_000), (1, 1));
    let chat = rig.prem_chat().await;
    let reply = rig.stream(chat.id, &question("hi")).await;
    let frames = crate::test_support::send::parse_sse(&reply.text);
    let done = &frames.last().unwrap().data;
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(done["selected_model"], PREMIUM);
    assert_eq!(done["effective_model"], STANDARD);
    assert_eq!(done["downgrade_from"], PREMIUM);
    assert_eq!(done["downgrade_reason"], "premium_quota_exhausted");
}

// ── errors after the stream opened ───────────────────────────────────────────

#[tokio::test]
async fn a_provider_failure_ends_the_stream_with_one_sanitized_error_event() {
    let body = json!({ "error": { "message":
        "The server had an error (request ID req_abc123). See https://help.example.com and \
         sk-proj-****abcd and resp_0aB1c2D3e4F5g6H7." } });
    let (rig, chat, _upstream) = streamed(Reply::http(500, &body)).await;
    let reply = rig.stream(chat.id, &question("hi")).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let frames = crate::test_support::send::parse_sse(&reply.text);
    assert_eq!(
        names(&frames),
        vec!["stream_started", "error"],
        "{}",
        reply.text
    );
    assert_grammar(&frames);
    let error = &frames[1].data;
    assert_eq!(keys(error), BTreeSet::from(["code", "message"]));
    assert_eq!(error["code"], "provider_error");
    let message = error["message"].as_str().unwrap();
    for leaked in ["req_abc123", "https://", "sk-proj", "abcd", "resp_0aB1"] {
        assert!(!message.contains(leaked), "`{leaked}` in {message}");
    }
    // the turn agrees with the terminal event
    let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("provider_error"));
    assert_eq!(turn.error_detail.as_deref(), Some(message));
    let status = rig
        .get(
            &turn_uri(
                chat.id,
                Uuid::parse_str(frames[0].data["request_id"].as_str().unwrap()).unwrap(),
            ),
            &a1(),
        )
        .await
        .json();
    assert_eq!(status["state"], "error");
    assert_eq!(status["error_code"], "provider_error");
    assert!(!status.to_string().contains("req_abc123"));
}

/// The error of a provider failure text: the SSE `message` and `chat_turns.error_detail`.
async fn failure_texts(text: &str) -> (String, Option<String>) {
    let body = json!({ "error": { "message": text } });
    let (rig, chat, _upstream) = streamed(Reply::http(500, &body)).await;
    let reply = rig.stream(chat.id, &question("hi")).await;
    let frames = crate::test_support::send::parse_sse(&reply.text);
    assert_eq!(
        names(&frames),
        vec!["stream_started", "error"],
        "{}",
        reply.text
    );
    let message = frames[1].data["message"].as_str().unwrap().to_owned();
    let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
    (message, turn.error_detail)
}

#[tokio::test]
async fn the_sse_error_message_and_the_stored_detail_keep_ordinary_words() {
    let (message, detail) = failure_texts("The file-based store failed").await;
    assert!(message.contains("file-based"), "{message}");
    assert_eq!(detail.as_deref(), Some(message.as_str()));
}

#[tokio::test]
async fn the_sse_error_message_and_the_stored_detail_mask_a_response_id() {
    let (message, detail) = failure_texts("Request failed: resp_abc123def456ghi").await;
    assert!(!message.contains("resp_abc123def456ghi"), "{message}");
    assert!(message.contains("[provider_id]"), "{message}");
    assert_eq!(detail.as_deref(), Some(message.as_str()));
}

#[tokio::test]
async fn a_failure_after_deltas_discards_the_partial_text_and_ends_with_error() {
    let steps = vec![delta("partial"), Step::Break];
    let (rig, chat, _upstream) = streamed(Reply::Sse(steps)).await;
    let reply = rig.stream(chat.id, &question("hi")).await;
    let frames = crate::test_support::send::parse_sse(&reply.text);
    assert_eq!(
        names(&frames),
        vec!["stream_started", "delta", "error"],
        "{}",
        reply.text
    );
    assert_eq!(frames[2].data["code"], "provider_error");
    assert_eq!(message_rows(&rig.quota.db, chat.id).await.len(), 1);
}

#[tokio::test]
async fn the_web_search_breach_ends_the_stream_with_its_code_never_a_429() {
    let steps = vec![
        web_start(),
        web_done(),
        web_start(),
        web_done(),
        web_start(),
        completed("x", (1, 1)),
    ];
    let (rig, chat, _upstream) = streamed(Reply::Sse(steps)).await;
    let reply = rig
        .stream(
            chat.id,
            &json!({ "content": "s", "web_search": { "enabled": true } }),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let frames = crate::test_support::send::parse_sse(&reply.text);
    assert_eq!(frames.last().unwrap().event, "error");
    assert_eq!(
        frames.last().unwrap().data["code"],
        "web_search_calls_exceeded"
    );
    assert_eq!(frames.iter().filter(|f| f.event == "tool").count(), 4);
}

#[tokio::test]
async fn a_provider_task_without_the_s2s_context_is_a_json_500_and_no_stream() {
    let upstream = MockProvider::new();
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream.clone())
            .with_s2s(S2sContext::unbound()),
    )
    .await;
    let chat = rig.std_chat().await;
    let reply = rig.stream(chat.id, &question("hi")).await;
    assert_problem(&reply, 500, "internal");
    assert_eq!(upstream.call_count(), 0);
    // the committed turn never stays running with its reserve
    let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("provider_error"));
    assert_eq!(footprint(&rig.quota.db, &chat).await.reserved_credits, 0);
}

// ── ping, no buffering and backpressure ──────────────────────────────────────

fn fast_ping(rig_settings: StreamSettings) -> StreamSettings {
    StreamSettings {
        sse_ping_interval: Duration::from_millis(50),
        ..rig_settings
    }
}

#[tokio::test]
async fn ping_is_sent_while_waiting_for_the_first_content_and_never_after() {
    let first = Gate::new();
    let second = Gate::new();
    let steps = vec![
        Step::Wait(first.clone()),
        delta("a"),
        Step::Wait(second.clone()),
        delta("b"),
        completed("ab", (1, 1)),
    ];
    let upstream = MockProvider::answering(Reply::Sse(steps));
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream)
            .with_settings(fast_ping(rig_settings())),
    )
    .await;
    let chat = rig.std_chat().await;
    let mut open = rig.open_stream(chat.id, &question("hi")).await;
    assert_eq!(open.next_frame().await.unwrap().event, "stream_started");
    // idle before the first content: pings arrive, with an empty object as data
    for _ in 0..2 {
        let ping = open.next_frame().await.unwrap();
        assert_eq!(ping.event, "ping");
        assert_eq!(ping.data, json!({}));
    }
    first.open();
    // pings may still be in flight before the first delta; the first delta ends the ping phase
    loop {
        let frame = open.next_frame().await.unwrap();
        if frame.event == "delta" {
            assert_eq!(frame.data["content"], "a");
            break;
        }
        assert_eq!(frame.event, "ping");
    }
    // several intervals of silence after content started: no ping, no event at all
    assert!(
        open.next_frame_within(Duration::from_millis(400))
            .await
            .is_err()
    );
    second.open();
    let rest = open.rest().await;
    assert_eq!(names(&rest), vec!["delta", "done"]);
}

#[tokio::test]
async fn a_tool_event_also_ends_the_ping_phase() {
    let gate = Gate::new();
    let steps = vec![
        web_start(),
        Step::Wait(gate.clone()),
        web_done(),
        completed("x", (1, 1)),
    ];
    let upstream = MockProvider::answering(Reply::Sse(steps));
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream)
            .with_settings(fast_ping(rig_settings())),
    )
    .await;
    let chat = rig.std_chat().await;
    let mut open = rig
        .open_stream(
            chat.id,
            &json!({ "content": "s", "web_search": { "enabled": true } }),
        )
        .await;
    assert_eq!(open.next_frame().await.unwrap().event, "stream_started");
    loop {
        let frame = open.next_frame().await.unwrap();
        if frame.event == "tool" {
            break;
        }
        assert_eq!(frame.event, "ping");
    }
    assert!(
        open.next_frame_within(Duration::from_millis(300))
            .await
            .is_err()
    );
    gate.open();
    let rest = open.rest().await;
    assert_eq!(names(&rest), vec!["tool", "done"]);
}

#[tokio::test]
async fn nothing_follows_the_terminal_event_and_the_connection_closes() {
    let (rig, chat, _upstream) = streamed(Reply::Sse(happy_steps(&["x"], (1, 1)))).await;
    let mut open = rig.open_stream(chat.id, &question("hi")).await;
    let frames = open.until("done").await;
    assert_grammar(&frames);
    // the body ends right after `done`: no ping, no keep-alive event, nothing
    assert!(open.next_frame().await.is_none());
}

#[tokio::test]
async fn a_delta_reaches_the_client_before_the_provider_sends_the_next_event() {
    let gate = Gate::new();
    let steps = vec![
        delta("first"),
        Step::Wait(gate.clone()),
        delta("second"),
        completed("firstsecond", (1, 1)),
    ];
    let (rig, chat, _upstream) = streamed(Reply::Sse(steps)).await;
    let mut open = rig.open_stream(chat.id, &question("hi")).await;
    assert_eq!(open.next_frame().await.unwrap().event, "stream_started");
    let first = open.next_frame().await.unwrap();
    assert_eq!(first.event, "delta");
    assert_eq!(first.data["content"], "first");
    // the provider is still paused: nothing was collected before relaying
    assert_eq!(turn_rows(&rig.quota.db, chat.id).await[0].state, "running");
    gate.open();
    let rest = open.rest().await;
    assert_eq!(delta_text(&rest), "second");
    assert_eq!(rest.last().unwrap().event, "done");
}

#[tokio::test]
async fn a_slow_client_blocks_the_producer_through_the_bounded_channel() {
    let mut steps = numbered_deltas(60);
    steps.push(completed("x", (1, 1)));
    let upstream = MockProvider::answering(Reply::Sse(steps));
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream.clone())
            .with_task_settings(ProviderTaskSettings {
                channel_capacity: 2,
                ..rig_task_settings()
            }),
    )
    .await;
    let chat = rig.std_chat().await;
    let mut open = rig.open_stream(chat.id, &question("hi")).await;
    // the client reads nothing: the provider is pulled only as far as the channel allows
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        upstream.delivered_events() < 30,
        "{}",
        upstream.delivered_events()
    );
    let frames = open.rest().await;
    assert_eq!(frames.last().unwrap().event, "done");
    assert_eq!(delta_text(&frames), numbered_text(60));
}

// ── disconnects ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_disconnect_mid_stream_cancels_the_turn_and_persists_the_partial_text() {
    let gate = Gate::new();
    let steps = vec![
        delta("partial"),
        Step::Wait(gate),
        completed("partial", (1, 1)),
    ];
    let (rig, chat, upstream) = streamed(Reply::Sse(steps)).await;
    let mut open = rig.open_stream(chat.id, &question("hi")).await;
    assert_eq!(open.next_frame().await.unwrap().event, "stream_started");
    assert_eq!(open.next_frame().await.unwrap().event, "delta");
    // the client goes away
    open.disconnect();
    upstream.wait_for_aborted(1).await;
    let turn = rig.wait_terminal(chat.id).await;
    assert_eq!(turn.state, "cancelled");
    assert_eq!(turn.error_code, None);
    let messages = message_rows(&rig.quota.db, chat.id).await;
    let assistant = messages
        .iter()
        .find(|m| m.role == "assistant")
        .expect("the partial message");
    assert_eq!(assistant.content, "partial");
    assert_eq!(turn.assistant_message_id, Some(assistant.id));
    // estimated settlement, the aborted usage event and the turn_failed audit event
    assert_eq!(footprint(&rig.quota.db, &chat).await.reserved_credits, 0);
    let events = rig.events().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].billing_outcome, "aborted");
    assert_eq!(events[0].settlement_method, "estimated");
    assert_eq!(rig.audit_events().await.len(), 1);
    // the status API reports the cancelled turn with its partial message
    let status = rig
        .get(&turn_uri(chat.id, turn.request_id), &a1())
        .await
        .json();
    assert_eq!(status["state"], "cancelled");
    assert_eq!(status["assistant_message_id"], assistant.id.to_string());
    // the chat accepts a new request id at once
    upstream.set_default(Reply::Sse(happy_steps(&["again"], (1, 1))));
    let next = rig.stream(chat.id, &question("again")).await;
    assert_eq!(next.status, StatusCode::OK, "{}", next.text);
    assert_eq!(
        crate::test_support::send::parse_sse(&next.text)
            .last()
            .unwrap()
            .event,
        "done"
    );
}

#[tokio::test]
async fn a_disconnect_before_any_text_persists_no_message() {
    let gate = Gate::new();
    let steps = vec![Step::Wait(gate), delta("never"), completed("never", (1, 1))];
    let (rig, chat, upstream) = streamed(Reply::Sse(steps)).await;
    let mut open = rig.open_stream(chat.id, &question("hi")).await;
    assert_eq!(open.next_frame().await.unwrap().event, "stream_started");
    open.disconnect();
    upstream.wait_for_aborted(1).await;
    let turn = rig.wait_terminal(chat.id).await;
    assert_eq!(turn.state, "cancelled");
    assert_eq!(turn.assistant_message_id, None);
    assert_eq!(message_rows(&rig.quota.db, chat.id).await.len(), 1);
    let status = rig
        .get(&turn_uri(chat.id, turn.request_id), &a1())
        .await
        .json();
    assert_eq!(status["state"], "cancelled");
    assert!(status.get("assistant_message_id").is_none());
}

#[tokio::test]
async fn a_response_dropped_after_the_provider_request_started_cancels_the_provider_request() {
    let gate = Gate::new();
    let steps = vec![Step::Wait(gate), completed("never", (1, 1))];
    let (rig, chat, upstream) = streamed(Reply::Sse(steps)).await;
    // the handler answered, the provider request started, the client never read a byte
    let open = rig.open_stream(chat.id, &question("hi")).await;
    upstream.wait_for_calls(1).await;
    drop(open);
    upstream.wait_for_aborted(1).await;
    let turn = rig.wait_terminal(chat.id).await;
    // never left `running` until the watchdog
    assert_eq!(turn.state, "cancelled");
    assert_eq!(footprint(&rig.quota.db, &chat).await.reserved_credits, 0);
    assert_eq!(rig.events().await.len(), 1);
}

#[tokio::test]
async fn a_disconnect_after_done_changes_nothing() {
    let (rig, chat, _upstream) = streamed(Reply::Sse(happy_steps(&["done"], (1, 1)))).await;
    let mut open = rig.open_stream(chat.id, &question("hi")).await;
    let frames = open.until("done").await;
    assert_eq!(frames.last().unwrap().event, "done");
    open.disconnect();
    tokio::time::sleep(Duration::from_millis(150)).await;
    let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
    assert_eq!(turn.state, "completed");
    assert!(turn.assistant_message_id.is_some());
    assert_eq!(rig.events().await.len(), 1);
    assert_eq!(rig.audit_events().await.len(), 1);
}

#[tokio::test]
async fn a_disconnect_between_the_reserve_and_the_provider_call_sends_nothing_and_cancels() {
    let gate = Arc::new(ReserveGate::default());
    let upstream = MockProvider::new();
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream.clone())
            .with_gate(Arc::clone(&gate)),
    )
    .await;
    let chat = rig.std_chat().await;
    let body = question("hi");
    let request = rig.stream(chat.id, &body);
    tokio::select! {
        _ = request => panic!("the setup must be parked after the reserve commit"),
        () = gate.committed.notified() => {}
    }
    gate.release.notify_one();
    let turn = rig.wait_terminal(chat.id).await;
    assert_eq!(upstream.call_count(), 0, "a provider request was sent");
    assert_eq!(turn.state, "cancelled");
    assert_eq!(turn.assistant_message_id, None);
    let events = rig.events().await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].billing_outcome, "aborted");
    assert_eq!(events[0].settlement_method, "estimated");
    // the pre-provider cancellation enqueues the turn_failed audit event as well
    let audits = rig.audit_events().await;
    assert_eq!(audits.len(), 1, "{audits:?}");
    gate.release.notify_one();
    upstream.set_default(Reply::Sse(happy_steps(&["x"], (1, 1))));
    let next = rig.stream(chat.id, &question("again")).await;
    assert_eq!(next.status, StatusCode::OK, "{}", next.text);
}

#[tokio::test]
async fn a_provider_that_never_answers_still_gets_started_and_pings_and_a_disconnect_cancels_it() {
    let upstream = MockProvider::answering(Reply::Pending);
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream.clone())
            .with_settings(fast_ping(rig_settings())),
    )
    .await;
    let chat = rig.std_chat().await;
    let mut open = rig.open_stream(chat.id, &question("hi")).await;
    // the stream opens at once: the provider request is in flight and the client is kept alive
    assert_eq!(open.next_frame().await.unwrap().event, "stream_started");
    assert_eq!(open.next_frame().await.unwrap().event, "ping");
    upstream.wait_for_calls(1).await;
    open.disconnect();
    // the in-flight request is hard-cancelled and the turn ends cancelled
    upstream.wait_for_aborted(1).await;
    let turn = rig.wait_terminal(chat.id).await;
    assert_eq!(turn.state, "cancelled");
    assert_eq!(turn.assistant_message_id, None);
    assert_eq!(footprint(&rig.quota.db, &chat).await.reserved_credits, 0);
}

#[tokio::test]
async fn a_replay_of_a_cited_downgraded_turn_sends_no_citations_and_no_reason() {
    let text = "Growth was strong.";
    let steps = vec![
        delta(text),
        completed_parts(
            &[output_part(
                text,
                &[url_annotation("https://example.com/m", "M", 0, 6)],
            )],
            (5, 5),
        ),
    ];
    let upstream = MockProvider::answering(Reply::Sse(steps));
    let rig = stream_rig(upstream).await;
    rig.gateway.set_limits((60_000_000, 600_000_000), (1, 1));
    let chat = rig.prem_chat().await;
    let request_id = Uuid::new_v4();
    let body = json!({ "content": "q", "request_id": request_id.to_string() });
    let first = rig.stream(chat.id, &body).await;
    let first_frames = crate::test_support::send::parse_sse(&first.text);
    assert!(
        names(&first_frames).contains(&"citations"),
        "{}",
        first.text
    );
    assert_eq!(
        first_frames.last().unwrap().data["downgrade_reason"],
        "premium_quota_exhausted"
    );

    let again = rig.stream(chat.id, &body).await;
    let frames = crate::test_support::send::parse_sse(&again.text);
    assert_eq!(
        names(&frames),
        vec!["stream_started", "delta", "done"],
        "{}",
        again.text
    );
    let done = &frames[2].data;
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(done["downgrade_from"], PREMIUM);
    assert!(done.get("downgrade_reason").is_none());
    assert!(done.get("quota_warnings").is_none());
}

// ── first terminal wins ──────────────────────────────────────────────────────

#[tokio::test]
async fn when_the_watchdog_wins_the_client_gets_stream_interrupted_and_the_state_stays_failed() {
    let clock = FixedClock::at(default_now());
    let gate = Gate::new();
    let steps = vec![
        delta("part"),
        Step::Wait(gate.clone()),
        completed("part", (9, 9)),
    ];
    let upstream = MockProvider::answering(Reply::Sse(steps));
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream)
            .with_clock(clock.clone()),
    )
    .await;
    let chat = rig.std_chat().await;
    let mut open = rig.open_stream(chat.id, &question("hi")).await;
    let started = open.next_frame().await.unwrap();
    assert_eq!(started.event, "stream_started");
    assert_eq!(open.next_frame().await.unwrap().event, "delta");

    // the watchdog finalizes the stale turn while the provider is still silent
    clock.set(default_now() + TimeDuration::seconds(301));
    let report = rig
        .watchdog(Duration::from_secs(300))
        .scan_once()
        .await
        .unwrap();
    assert_eq!(report.finalized, 1, "{report:?}");
    gate.open();

    // the provider terminal lost the CAS: the relay synthesizes `stream_interrupted`
    let rest = open.rest().await;
    assert_eq!(names(&rest), vec!["error"], "{rest:?}");
    assert_eq!(rest[0].data["code"], "stream_interrupted");
    assert_eq!(keys(&rest[0].data), BTreeSet::from(["code", "message"]));

    let turn = turn_rows(&rig.quota.db, chat.id).await.remove(0);
    assert_eq!(turn.state, "failed");
    // the SSE-only code is never stored
    assert_eq!(turn.error_code.as_deref(), Some("orphan_timeout"));
    assert_eq!(turn.assistant_message_id, None);
    assert_eq!(message_rows(&rig.quota.db, chat.id).await.len(), 1);
    assert_eq!(rig.events().await.len(), 1);
    assert_eq!(rig.audit_payloads().await.len(), 1);
    let status = rig
        .get(&turn_uri(chat.id, turn.request_id), &a1())
        .await
        .json();
    assert_eq!(status["state"], "error");
    assert_eq!(status["error_code"], "orphan_timeout");
}

// ── replay and recovery after a real stream ──────────────────────────────────

#[tokio::test]
async fn resending_the_request_id_of_a_streamed_turn_replays_it_without_side_effects() {
    let (rig, chat, upstream) = streamed(Reply::Sse(happy_steps(&["Hel", "lo"], (120, 30)))).await;
    let request_id = Uuid::new_v4();
    let body = json!({ "content": "hi", "request_id": request_id.to_string() });
    let first = rig.stream(chat.id, &body).await;
    let first_frames = crate::test_support::send::parse_sse(&first.text);
    let message_id = first_frames[0].data["message_id"].clone();
    let rows_before = footprint(&rig.quota.db, &chat).await;

    let again = rig.stream(chat.id, &body).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.text);
    let frames = crate::test_support::send::parse_sse(&again.text);
    assert_eq!(
        names(&frames),
        vec!["stream_started", "delta", "done"],
        "{}",
        again.text
    );
    assert_eq!(frames[0].data["is_new_turn"], false);
    assert_eq!(frames[0].data["message_id"], message_id);
    assert_eq!(frames[1].data["content"], "Hello");
    assert_eq!(
        frames[2].data["usage"],
        json!({ "input_tokens": 120, "output_tokens": 30 })
    );
    // no provider request, no new row, no new outbox message
    assert_eq!(upstream.call_count(), 1);
    assert_eq!(footprint(&rig.quota.db, &chat).await, rows_before);
}

#[tokio::test]
async fn a_streamed_turn_conflicts_while_running_and_a_new_turn_follows_every_terminal_state() {
    let gate = Gate::new();
    let upstream = MockProvider::new();
    upstream.queue(Reply::Sse(vec![
        delta("a"),
        Step::Wait(gate.clone()),
        completed("a", (1, 1)),
    ]));
    let rig = stream_rig(upstream.clone()).await;
    let chat = rig.std_chat().await;
    let mut open = rig.open_stream(chat.id, &question("one")).await;
    assert_eq!(open.next_frame().await.unwrap().event, "stream_started");
    // another request while the first one streams
    let second = rig.stream(chat.id, &question("two")).await;
    let problem = assert_problem(&second, 409, "aborted");
    assert_eq!(problem["context"]["reason"], "turn_already_running");
    gate.open();
    open.rest().await;

    // after completed, failed and cancelled turns a new request id is accepted
    upstream.queue(Reply::http(500, &json!({ "error": { "message": "boom" } })));
    let failed = rig.stream(chat.id, &question("three")).await;
    assert_eq!(
        crate::test_support::send::parse_sse(&failed.text)
            .last()
            .unwrap()
            .event,
        "error"
    );
    let hang = Gate::new();
    upstream.queue(Reply::Sse(vec![Step::Wait(hang)]));
    let cancelled = rig.open_stream(chat.id, &question("four")).await;
    upstream.wait_for_calls(3).await;
    drop(cancelled);
    upstream.wait_for_aborted(1).await;
    // wait for the cancellation to commit
    for _ in 0..500 {
        if turn_rows(&rig.quota.db, chat.id)
            .await
            .iter()
            .all(|t| t.state != "running")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // the turns of one fixed clock share their start instant: compare the states as a set
    let mut states: Vec<String> = turn_rows(&rig.quota.db, chat.id)
        .await
        .into_iter()
        .map(|t| t.state)
        .collect();
    states.sort();
    assert_eq!(states, vec!["cancelled", "completed", "failed"]);
    let next = rig.stream(chat.id, &question("five")).await;
    assert_eq!(next.status, StatusCode::OK, "{}", next.text);
}

// ── the persisted result seen through the read APIs ──────────────────────────

#[tokio::test]
async fn the_messages_list_and_the_count_follow_several_streamed_turns() {
    let upstream = MockProvider::new();
    upstream.queue(Reply::Sse(happy_steps(&["one"], (10, 4))));
    upstream.queue(Reply::http(500, &json!({ "error": { "message": "boom" } })));
    upstream.queue(Reply::Sse(vec![delta("partial"), Step::Wait(Gate::new())]));
    upstream.queue(Reply::Sse(happy_steps(&["four"], (30, 8))));
    let rig = stream_rig(upstream.clone()).await;
    let chat = rig.std_chat().await;
    let completed_id = Uuid::new_v4();
    rig.stream(
        chat.id,
        &json!({ "content": "q1", "request_id": completed_id.to_string() }),
    )
    .await;
    rig.stream(chat.id, &question("q2 fails")).await;
    let mut open = rig.open_stream(chat.id, &question("q3 cancelled")).await;
    open.until("delta").await;
    open.disconnect();
    rig.wait_idle(chat.id).await;
    rig.stream(chat.id, &question("q4")).await;

    // completed adds 2, failed 1, cancelled with a partial message 2, completed 2
    let detail = rig.get(&chat_uri(chat.id), &a1()).await.json();
    assert_eq!(detail["message_count"], 7);

    let list = rig.get(&messages_uri(chat.id), &a1()).await.json();
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 7, "{list}");
    let roles: Vec<&str> = items.iter().map(|i| i["role"].as_str().unwrap()).collect();
    assert_eq!(
        roles,
        vec![
            "user",
            "assistant",
            "user",
            "user",
            "assistant",
            "user",
            "assistant"
        ]
    );
    // chronological order: strictly increasing instants, the assistant after its user message
    let times: Vec<time::OffsetDateTime> =
        items.iter().map(|i| parse_ts(&i["created_at"])).collect();
    assert!(times.windows(2).all(|pair| pair[0] < pair[1]), "{times:?}");
    // the user and assistant messages of a turn share its request id
    assert_eq!(items[0]["request_id"], completed_id.to_string());
    assert_eq!(items[1]["request_id"], completed_id.to_string());
    assert_eq!(items[1]["content"], "one");
    assert_eq!(items[1]["model"], STANDARD);
    assert_eq!(items[1]["input_tokens"], 10);
    assert_eq!(items[1]["output_tokens"], 4);
    // the partial message of the cancelled turn carries no tokens
    assert_eq!(items[4]["content"], "partial");
    assert!(items[4].get("input_tokens").is_none() && items[4].get("output_tokens").is_none());
    // no provider identifier in the read model
    assert!(!list.to_string().contains("resp_"));
}
