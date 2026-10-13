#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::unused_async)]
//! Slices 2, 5 and 6, end to end: tools and citations, the assembled context,
//! pings, no buffering and backpressure, cancellation on disconnect (also
//! after the reserve and while the call is pending), the orphan watchdog
//! race, message ordering across turns, the request metrics and the route
//! registration.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use mini_chat_sdk::{
    BillingOutcome, KillSwitches, ModelTier, SettlementMethod, TerminalState, TurnAuditEventType,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::chat_test_env::{blob, ts};
use crate::config::{DEFAULT_FILE_SEARCH_GUARD, DEFAULT_WEB_SEARCH_GUARD};
use crate::domain::service::quota::test_support::{NO_SWITCHES, design_catalog, model};
use crate::domain::service::stream::ports::DisconnectStage;
use crate::domain::service::stream::test_support::StreamMetric;
use crate::stream_test_env::{Options, StreamEnv, body, oa, qblob, rid};
use crate::test_doubles::{ProxyReply, USER, USER_TENANT};

fn tools_of(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|t| t["type"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

async fn sent_body(env: &StreamEnv) -> Value {
    env.oagw
        .proxied()
        .last()
        .expect("a provider request")
        .json()
}

// ==== tools and citations ==========================================================================

#[tokio::test]
async fn tool_activity_and_citations_are_reported_cited_and_counted() {
    let env = StreamEnv::new().await;
    let document = env
        .seed_attachment(env.chat, 1, |row| row.set("filename", "report.pdf"))
        .await;
    env.seed_vector_store(env.chat, "vs_chat").await;
    let web_annotation = json!({
        "type": "url_citation", "url": "https://example.org/a", "title": "Example",
        "start_index": 0, "end_index": 6
    });
    env.script(&[
        oa::file_start(),
        oa::file_done(3),
        oa::web_start(),
        oa::web_done(),
        oa::delta("Answer and more"),
        oa::annotation(web_annotation),
        oa::annotation(
            json!({ "type": "file_citation", "file_id": "file-prov1", "filename": "x" }),
        ),
        oa::annotation(json!({ "type": "file_citation", "file_id": "file-unknown" })),
        oa::completed(50, 20),
    ]);
    let request = rid(1);
    let streamed = env
        .stream(
            env.chat,
            &json!({ "content": "what does the report say?", "request_id": request, "web_search": { "enabled": true } }),
        )
        .await;
    assert_eq!(
        streamed.names(),
        [
            "stream_started",
            "tool",
            "tool",
            "tool",
            "tool",
            "delta",
            "citations",
            "done"
        ]
    );
    assert_eq!(
        streamed.frames[1].data,
        json!({ "phase": "start", "name": "file_search", "details": {} })
    );
    assert_eq!(
        streamed.frames[2].data,
        json!({ "phase": "done", "name": "file_search", "details": { "files_searched": 3 } })
    );
    assert_eq!(
        streamed.frames[3].data,
        json!({ "phase": "start", "name": "web_search", "details": {} })
    );
    assert_eq!(
        streamed.frames[4].data,
        json!({ "phase": "done", "name": "web_search", "details": {} })
    );
    assert_eq!(
        streamed.of("citations")[0].data,
        json!({ "items": [
            { "source": "web", "title": "Example", "snippet": "Answer", "url": "https://example.org/a", "span": { "start": 0, "end": 6 } },
            { "source": "file", "title": "report.pdf", "snippet": "", "attachment_id": document.to_string() }
        ]}),
        "the unknown file id is omitted"
    );
    let text = &streamed.reply.text;
    assert!(
        !text.contains("file-prov1") && !text.contains("score"),
        "no provider id, no score"
    );

    // Counted in the turn, the usage event and the audit event.
    assert_eq!(
        env.turn_text(request, "web_search_completed_count")
            .await
            .as_deref(),
        Some("1")
    );
    assert_eq!(
        env.turn_text(request, "file_search_completed_count")
            .await
            .as_deref(),
        Some("1")
    );
    let usage = env.usage_events().await;
    assert_eq!(
        (usage[0].web_search_calls, usage[0].file_search_calls),
        (1, 1)
    );
    let audit = env.audit_events().await;
    assert_eq!(
        (
            audit[0].tool_calls.web_search,
            audit[0].tool_calls.file_search
        ),
        (1, 1)
    );
}

#[tokio::test]
async fn the_request_carries_the_tools_the_guards_and_the_label_of_the_turn() {
    let env = StreamEnv::new().await;
    env.seed_attachment(env.chat, 1, |row| row).await;
    env.seed_vector_store(env.chat, "vs_chat").await;
    env.script(&[oa::completed(1, 1)]);
    env.stream(
        env.chat,
        &json!({ "content": "q", "web_search": { "enabled": true } }),
    )
    .await;
    let request = sent_body(&env).await;
    let entry = &design_catalog()[0];
    assert_eq!(
        request["tools"],
        json!([
            { "type": "file_search", "vector_store_ids": ["vs_chat"], "max_num_results": entry.max_num_results },
            { "type": "web_search", "search_context_size": "low" }
        ])
    );
    assert_eq!(request["max_tool_calls"], entry.max_tool_calls);
    assert_eq!(
        request["instructions"],
        format!(
            "SECRET-SYSTEM-PROMPT\n\n{DEFAULT_FILE_SEARCH_GUARD}\n\n{DEFAULT_WEB_SEARCH_GUARD}"
        )
    );
    assert_eq!(request["metadata"]["feature"], "file_search+web_search");
    assert!(request.get("include").is_none());
}

#[tokio::test]
async fn without_a_ready_document_or_a_vector_store_there_is_no_file_search() {
    let env = StreamEnv::new().await;
    env.script(&[oa::completed(1, 1)]);
    env.stream(env.chat, &body("q", rid(1))).await;
    assert!(tools_of(&sent_body(&env).await).is_empty());
    assert_eq!(sent_body(&env).await["metadata"]["feature"], "none");

    // A ready document but no vector store row.
    env.seed_attachment(env.chat, 1, |row| row).await;
    env.script(&[oa::completed(1, 1)]);
    env.stream(env.chat, &body("q", rid(2))).await;
    assert!(tools_of(&sent_body(&env).await).is_empty());
}

#[tokio::test]
async fn file_search_is_omitted_under_its_kill_switch_without_rejecting_the_send() {
    let env = StreamEnv::with(Options {
        switches: KillSwitches {
            disable_file_search: true,
            ..NO_SWITCHES
        },
        ..Options::default()
    })
    .await;
    env.seed_attachment(env.chat, 1, |row| row).await;
    env.seed_vector_store(env.chat, "vs_chat").await;
    env.script(&[oa::completed(1, 1)]);
    let streamed = env.stream(env.chat, &body("q", rid(1))).await;
    assert_eq!(streamed.names(), ["stream_started", "done"]);
    assert!(tools_of(&sent_body(&env).await).is_empty());
}

#[tokio::test]
async fn web_search_on_a_model_without_support_adds_neither_tool_nor_guard() {
    let env = StreamEnv::with(Options {
        catalog: vec![
            model("p", ModelTier::Premium)
                .tools(false, true, true)
                .build(),
            model("s", ModelTier::Standard).build(),
        ],
        ..Options::default()
    })
    .await;
    env.script(&[oa::completed(1, 1)]);
    let streamed = env
        .stream(
            env.chat,
            &json!({ "content": "q", "web_search": { "enabled": true } }),
        )
        .await;
    assert_eq!(streamed.names(), ["stream_started", "done"], "not rejected");
    let request = sent_body(&env).await;
    assert!(tools_of(&request).is_empty());
    assert_eq!(request["instructions"], "SECRET-SYSTEM-PROMPT");
}

#[tokio::test]
async fn a_ready_spreadsheet_adds_the_code_interpreter_with_its_file_and_the_output_cap() {
    let env = StreamEnv::new().await;
    env.seed_attachment(env.chat, 1, |row| {
        row.set("for_file_search", false)
            .set("for_code_interpreter", true)
            .set("provider_file_id", "file-sheet1")
            .set("filename", "data.xlsx")
    })
    .await;
    env.script(&[
        oa::code_start(),
        oa::code_done(&"a".repeat(9000)),
        oa::delta("done"),
        oa::completed(1, 1),
    ]);
    let streamed = env.stream(env.chat, &body("analyse", rid(1))).await;
    assert_eq!(
        streamed.frames[1].data,
        json!({ "phase": "start", "name": "code_interpreter", "details": {} })
    );
    assert_eq!(
        streamed.frames[2].data["details"]["output"],
        format!("{}...[truncated]", "a".repeat(8192))
    );
    let request = sent_body(&env).await;
    assert_eq!(
        request["tools"],
        json!([{ "type": "code_interpreter", "container": { "type": "auto", "file_ids": ["file-sheet1"] } }])
    );
    assert_eq!(request["include"], json!(["code_interpreter_call.outputs"]));
    assert_eq!(request["metadata"]["feature"], "code_interpreter");
    assert_eq!(
        request["instructions"], "SECRET-SYSTEM-PROMPT",
        "the code interpreter has no guard"
    );
    assert_eq!(env.usage_events().await[0].code_interpreter_calls, 1);
}

#[tokio::test]
async fn the_code_interpreter_is_silently_omitted_under_its_kill_switch() {
    let env = StreamEnv::with(Options {
        switches: KillSwitches {
            disable_code_interpreter: true,
            ..NO_SWITCHES
        },
        ..Options::default()
    })
    .await;
    env.seed_attachment(env.chat, 1, |row| {
        row.set("for_file_search", false)
            .set("for_code_interpreter", true)
            .set("provider_file_id", "file-sheet1")
    })
    .await;
    env.script(&[oa::completed(1, 1)]);
    let streamed = env.stream(env.chat, &body("analyse", rid(1))).await;
    assert_eq!(streamed.names(), ["stream_started", "done"]);
    assert!(tools_of(&sent_body(&env).await).is_empty());
}

#[tokio::test]
async fn an_mcp_tool_is_never_sent_whatever_the_catalog_says() {
    let mut mcp = model("p", ModelTier::Premium).build();
    mcp.general_config.tool_support.mcp = true;
    let env = StreamEnv::with(Options {
        catalog: vec![mcp, model("s", ModelTier::Standard).build()],
        ..Options::default()
    })
    .await;
    env.script(&[oa::completed(1, 1)]);
    env.stream(
        env.chat,
        &json!({ "content": "q", "web_search": { "enabled": true } }),
    )
    .await;
    let text = sent_body(&env).await.to_string();
    for forbidden in ["mcp", "previous_response_id", "tool_choice"] {
        assert!(!text.contains(forbidden), "{forbidden} in {text}");
    }
}

#[tokio::test]
async fn an_image_is_sent_as_an_input_image_with_the_provider_file_id() {
    let env = StreamEnv::new().await;
    env.seed_image(env.chat, 1).await;
    let image = env.seed_image(env.chat, 2).await;
    env.script(&[oa::completed(1, 1)]);
    env.stream(
        env.chat,
        &json!({ "content": "look", "attachment_ids": [image] }),
    )
    .await;
    assert_eq!(
        sent_body(&env).await["input"][0]["content"],
        json!([
            { "type": "input_text", "text": "look" },
            { "type": "input_image", "file_id": "file-prov2" }
        ])
    );
    assert_eq!(env.metrics.count(|m| *m == StreamMetric::ImageInputs(1)), 1);
}

// ==== context ======================================================================================

async fn seed_pair(env: &StreamEnv, n: u128, question: &str, answer: &str) -> (Uuid, Uuid) {
    let q = env
        .seed_message(
            env.chat,
            n * 2 - 1,
            "user",
            question,
            i64::try_from(n * 10).unwrap(),
        )
        .await;
    let a = env
        .seed_message(
            env.chat,
            n * 2,
            "assistant",
            answer,
            i64::try_from(n * 10 + 1).unwrap(),
        )
        .await;
    (q, a)
}

#[tokio::test]
async fn the_summary_the_recent_history_and_the_current_message_reach_the_provider_in_order() {
    let env = StreamEnv::new().await;
    let (_, old_answer) = seed_pair(&env, 1, "old question", "old answer").await;
    seed_pair(&env, 2, "recent question", "recent answer").await;
    env.seed_summary(env.chat, "the summary text", old_answer, 11, 42)
        .await;
    env.script(&[oa::completed(1, 1)]);
    let streamed = env.stream(env.chat, &body("next question", rid(1))).await;
    assert_eq!(
        streamed.frames[0].data["thread_summary_applied"],
        json!({ "token_estimate": 42 })
    );
    let input = sent_body(&env).await["input"].clone();
    assert_eq!(
        input,
        json!([
            { "role": "user", "content": [ { "type": "input_text", "text": format!("{}\n\nthe summary text", crate::domain::service::stream::context::SUMMARY_PREAMBLE) } ] },
            { "role": "user", "content": [ { "type": "input_text", "text": "recent question" } ] },
            { "role": "assistant", "content": [ { "type": "output_text", "text": "recent answer" } ] },
            { "role": "user", "content": [ { "type": "input_text", "text": "next question" } ] }
        ])
    );
}

#[tokio::test]
async fn without_a_summary_there_is_no_marker_and_the_whole_history_is_sent() {
    let env = StreamEnv::new().await;
    seed_pair(&env, 1, "q1", "a1").await;
    seed_pair(&env, 2, "q2", "a2").await;
    env.script(&[oa::completed(1, 1)]);
    let streamed = env.stream(env.chat, &body("q3", rid(1))).await;
    assert!(
        streamed.frames[0]
            .data
            .get("thread_summary_applied")
            .is_none()
    );
    let texts: Vec<_> = sent_body(&env).await["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["content"][0]["text"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(texts, ["q1", "a1", "q2", "a2", "q3"]);
}

#[tokio::test]
async fn at_most_the_recent_messages_limit_is_sent_and_excluded_rows_never_are() {
    let env = StreamEnv::new().await;
    for n in 1..=7 {
        seed_pair(&env, n, &format!("q{n}"), &format!("a{n}")).await;
    }
    // Rows that never belong to the history.
    env.quota
        .db
        .insert(
            "messages",
            crate::infra::db::test_support::message_row(
                Uuid::from_u128(0xD1FF_0001),
                USER_TENANT,
                env.chat,
            )
            .set("role", "user")
            .set("content", "compressed")
            .set("request_id", Uuid::from_u128(0xE1FF_0001))
            .set("is_compressed", true)
            .set("created_at", ts(75)),
        )
        .await
        .unwrap();
    let deleted = env.seed_message(env.chat, 99, "user", "deleted", 76).await;
    env.quota.db.soft_delete("messages", deleted).await;
    env.script(&[oa::completed(1, 1)]);
    env.stream(env.chat, &body("now", rid(1))).await;
    let texts: Vec<_> = sent_body(&env).await["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["content"][0]["text"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        texts,
        [
            "q3", "a3", "q4", "a4", "q5", "a5", "q6", "a6", "q7", "a7", "now"
        ],
        "the newest ten, chronologically, then the current message"
    );
}

#[tokio::test]
async fn a_history_over_the_budget_sends_only_the_newest_whole_turns() {
    let mut small = model("p", ModelTier::Premium).build();
    small.context_window = 700; // 500 for the output: 200 for the input
    small.system_prompt = "S".repeat(20);
    let env = StreamEnv::with(Options {
        catalog: vec![small, model("s", ModelTier::Standard).build()],
        ..Options::default()
    })
    .await;
    // 60 bytes each; 200 - 20 - 5 leaves 175: two messages fit, three do not.
    for (n, role) in [(1, "user"), (2, "assistant"), (3, "user"), (4, "assistant")] {
        env.seed_message(
            env.chat,
            n,
            role,
            &"x".repeat(60),
            i64::try_from(n).unwrap(),
        )
        .await;
    }
    env.script(&[oa::completed(1, 1)]);
    env.stream(env.chat, &body("hello", rid(1))).await;
    let input = sent_body(&env).await["input"].clone();
    let roles: Vec<_> = input
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["role"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        roles,
        ["user", "assistant", "user"],
        "the newest whole turn and the current message"
    );
}

#[tokio::test]
async fn identical_state_gives_byte_identical_input_arrays() {
    let mut inputs = Vec::new();
    for _ in 0..2 {
        let env = StreamEnv::new().await;
        seed_pair(&env, 1, "q1", "a1").await;
        env.seed_summary(env.chat, "s", Uuid::from_u128(0xD100_0001), 5, 7)
            .await;
        env.script(&[oa::completed(1, 1)]);
        env.stream(env.chat, &body("same", rid(1))).await;
        inputs.push(serde_json::to_vec(&sent_body(&env).await["input"]).unwrap());
    }
    assert_eq!(inputs[0], inputs[1]);
}

// ==== pings, buffering and backpressure ============================================================

#[tokio::test]
async fn pings_run_only_before_the_first_content_event() {
    let env = StreamEnv::with(Options {
        ping: Duration::from_millis(40),
        ..Options::default()
    })
    .await;
    let feed = env.gate();
    let mut stream = env.open(env.chat, &body("Hi", rid(1))).await;
    assert_eq!(stream.next_frame().await.unwrap().name, "stream_started");
    let mut pings = 0;
    while pings < 2 {
        let frame = stream.next_frame().await.unwrap();
        assert_eq!(frame.name, "ping");
        assert_eq!(frame.data, json!({}));
        pings += 1;
    }
    feed.send_event(oa::delta("first").0, &oa::delta("first").1);
    // Frames that were already queued as pings may precede the delta.
    let seen = stream.until("delta").await;
    assert!(seen.iter().all(|f| f.name == "ping" || f.name == "delta"));
    // The provider pauses for several intervals: nothing is written.
    tokio::time::sleep(Duration::from_millis(200)).await;
    feed.send_event(oa::completed(1, 1).0, &oa::completed(1, 1).1);
    let rest = stream.rest().await;
    let names: Vec<_> = rest.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["done"], "no ping after the first delta");
}

#[tokio::test]
async fn each_delta_reaches_the_client_before_the_next_provider_chunk_is_released() {
    let env = StreamEnv::new().await;
    let feed = env.gate();
    let mut stream = env.open(env.chat, &body("Hi", rid(1))).await;
    assert_eq!(stream.next_frame().await.unwrap().name, "stream_started");
    for n in 0..5usize {
        let payload = oa::delta(&format!("d{n}"));
        feed.send_event(payload.0, &payload.1);
        let frame = stream.next_frame().await.unwrap();
        assert_eq!(frame.data["content"], format!("d{n}"));
        assert_eq!(
            feed.pulled(),
            n + 1,
            "no chunk was read ahead of its release"
        );
    }
    feed.send_event(oa::completed(1, 1).0, &oa::completed(1, 1).1);
    assert_eq!(stream.next_frame().await.unwrap().name, "done");
}

#[tokio::test]
async fn a_thousand_deltas_are_relayed_as_a_thousand_delta_events() {
    let env = StreamEnv::new().await;
    let mut events: Vec<(&str, Value)> = (0..1000).map(|n| oa::delta(&format!("{n},"))).collect();
    events.push(oa::completed(1, 1));
    env.script(&events);
    let request = rid(1);
    let streamed = env.stream(env.chat, &body("Hi", request)).await;
    assert_eq!(streamed.of("delta").len(), 1000);
    assert_eq!(streamed.names().last(), Some(&"done"));
    let stored = env.messages(env.chat).await[1].1.clone();
    assert_eq!(stored, streamed.text());
    assert!(stored.starts_with("0,1,2,") && stored.ends_with("999,"));
}

#[tokio::test]
async fn a_client_that_stops_reading_blocks_the_provider_task_not_memory() {
    let env = StreamEnv::new().await;
    let feed = env.gate();
    let mut stream = env.open(env.chat, &body("Hi", rid(1))).await;
    assert_eq!(stream.next_frame().await.unwrap().name, "stream_started");
    for n in 0..200 {
        let payload = oa::delta(&format!("d{n}"));
        feed.send_event(payload.0, &payload.1);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let capacity = env.options.channel_capacity;
    assert!(
        feed.pulled() <= capacity + 4,
        "{} chunks were read for a channel of {capacity}",
        feed.pulled()
    );
    // The client resumes: every event arrives, in order, none lost.
    feed.send_event(oa::completed(1, 1).0, &oa::completed(1, 1).1);
    let frames = stream.rest().await;
    assert_eq!(frames.iter().filter(|f| f.name == "delta").count(), 200);
    assert_eq!(frames[0].data["content"], "d0");
    assert_eq!(frames.last().unwrap().name, "done");
}

// ==== cancellation =================================================================================

/// How long a disconnect may take to reach the provider in these real-time
/// tests. The 200 ms of the acceptance criterion is asserted exactly, on a
/// paused clock, in the provider task tests; here a generous bound still
/// separates "reacts to the disconnect" from "waits for a timer or the next
/// provider event" (the ping interval of these tests is 60 s) and stays
/// robust when the whole suite runs in parallel under load.
const PROMPT: Duration = Duration::from_secs(10);

async fn promptly(feed: &crate::test_doubles::BodyFeed) {
    tokio::time::timeout(PROMPT, feed.dropped())
        .await
        .expect("the provider stream was not closed promptly after the disconnect");
}

#[tokio::test]
async fn a_disconnect_after_two_deltas_closes_the_provider_and_cancels_with_the_partial_text() {
    let env = StreamEnv::new().await;
    let feed = env.gate();
    let request = rid(1);
    let mut stream = env.open(env.chat, &body("Hi", request)).await;
    stream.next_frame().await.unwrap();
    for text in ["par", "tial"] {
        feed.send_event(oa::delta(text).0, &oa::delta(text).1);
        assert_eq!(stream.next_frame().await.unwrap().name, "delta");
    }
    drop(stream);
    promptly(&feed).await;
    assert_eq!(env.wait_terminal(request).await, "cancelled");

    let messages = env.messages(env.chat).await;
    assert_eq!(
        messages[1],
        (
            "assistant".to_owned(),
            "partial".to_owned(),
            Some("p".to_owned())
        )
    );
    let status = env.turn_status(request).await;
    assert_eq!(status.json["state"], "cancelled");
    assert!(status.json["assistant_message_id"].as_str().is_some());
    let usage = env.usage_events().await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].terminal_state, TerminalState::Cancelled);
    assert_eq!(usage[0].billing_outcome, BillingOutcome::Aborted);
    assert_eq!(usage[0].settlement_method, SettlementMethod::Estimated);
    let audit = env.audit_events().await;
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].event_type, TurnAuditEventType::TurnFailed);

    // The task drops its gauge guard after the terminal commit, so the gauge
    // is eventually (not instantly) back at 0.
    env.wait_until("the cancel metrics and the gauge", || {
        env.metrics.count(|m| *m == StreamMetric::StreamsAborted) == 1 && env.metrics.active() == 0
    })
    .await;
    for expected in [
        StreamMetric::CancelRequested,
        StreamMetric::Disconnected(DisconnectStage::MidStream),
        StreamMetric::TimeToAbort,
        StreamMetric::CancelEffective,
        StreamMetric::StreamsAborted,
    ] {
        assert_eq!(env.metrics.count(|m| *m == expected), 1, "{expected:?}");
    }
    assert_eq!(env.metrics.active(), 0, "mini_chat_active_streams is back");
    assert!(env.metrics.values("time_to_abort_ms")[0] < 1000.0);
}

#[tokio::test]
async fn a_disconnect_before_any_delta_cancels_without_a_message_even_when_the_provider_is_silent()
{
    let env = StreamEnv::new().await;
    let feed = env.gate();
    let request = rid(1);
    let mut stream = env.open(env.chat, &body("Hi", request)).await;
    stream.next_frame().await.unwrap();
    drop(stream);
    promptly(&feed).await;
    assert_eq!(env.wait_terminal(request).await, "cancelled");
    assert_eq!(
        env.messages(env.chat).await.len(),
        1,
        "no assistant message"
    );
    let status = env.turn_status(request).await;
    assert_eq!(status.json["state"], "cancelled");
    assert!(status.json.get("assistant_message_id").is_none());
    env.wait_until("the disconnect metric", || {
        env.metrics
            .count(|m| matches!(m, StreamMetric::Disconnected(_)))
            == 1
    })
    .await;
    assert_eq!(
        env.metrics
            .count(|m| *m == StreamMetric::Disconnected(DisconnectStage::BeforeFirstToken)),
        1
    );
}

#[tokio::test]
async fn a_disconnect_while_the_provider_call_is_still_pending_drops_the_call() {
    let env = StreamEnv::new().await;
    env.oagw.push_proxy(ProxyReply::Pending);
    let request = rid(1);
    let mut stream = env.open(env.chat, &body("Hi", request)).await;
    assert_eq!(stream.next_frame().await.unwrap().name, "stream_started");
    env.wait_until("the provider call", || env.oagw.proxy_call_count() == 1)
        .await;
    drop(stream);
    tokio::time::timeout(PROMPT, async {
        while env.oagw.pending_calls_dropped() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the pending proxy_request was not dropped promptly");
    assert_eq!(env.wait_terminal(request).await, "cancelled");
    // The gauge guard drops when the task returns, after the terminal commit.
    env.wait_until("the gauge", || env.metrics.active() == 0)
        .await;
    assert_eq!(env.metrics.active(), 0);
}

#[tokio::test]
async fn a_new_turn_is_accepted_after_a_cancellation_is_committed() {
    let env = StreamEnv::new().await;
    let feed = env.gate();
    let mut stream = env.open(env.chat, &body("Hi", rid(1))).await;
    stream.next_frame().await.unwrap();
    drop(stream);
    promptly(&feed).await;
    env.wait_terminal(rid(1)).await;
    env.script(&[oa::delta("again"), oa::completed(1, 1)]);
    let streamed = env.stream(env.chat, &body("Hi again", rid(2))).await;
    assert_eq!(streamed.names().last(), Some(&"done"));
}

#[tokio::test]
async fn a_caller_gone_right_after_the_reserve_commit_still_ends_cancelled_without_a_provider_call()
{
    let env = StreamEnv::new().await;
    let committed = Arc::new(tokio::sync::Notify::new());
    let proceed = Arc::new(tokio::sync::Notify::new());
    {
        let (committed, proceed) = (Arc::clone(&committed), Arc::clone(&proceed));
        *env.turns.after_create.lock().unwrap() = Some(Arc::new(move || {
            let (committed, proceed) = (Arc::clone(&committed), Arc::clone(&proceed));
            Box::pin(async move {
                committed.notify_one();
                proceed.notified().await;
            })
        }));
    }
    let request = rid(1);
    let payload = body("Hi", request);
    let mut future = Box::pin(env.open(env.chat, &payload));
    tokio::select! {
        _ = &mut future => panic!("the response cannot be ready while the hook waits"),
        () = committed.notified() => {}
    }
    drop(future); // the handler is gone before the setup result is delivered
    proceed.notify_one();

    assert_eq!(
        env.wait_terminal(request).await,
        "cancelled",
        "never left running"
    );
    assert_eq!(
        env.messages(env.chat).await.len(),
        1,
        "no assistant message"
    );
    assert_eq!(
        env.oagw.proxy_call_count(),
        0,
        "the provider receives no request"
    );
    let usage = env.usage_events().await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].billing_outcome, BillingOutcome::Aborted);
    assert_eq!(usage[0].settlement_method, SettlementMethod::Estimated);
    let audit = env.audit_events().await;
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].event_type, TurnAuditEventType::TurnFailed);
    env.wait_until("the gauge", || env.metrics.active() == 0)
        .await;

    *env.turns.after_create.lock().unwrap() = None;
    env.script(&[oa::completed(1, 1)]);
    let next = env.stream(env.chat, &body("next", rid(2))).await;
    assert_eq!(
        next.names().last(),
        Some(&"done"),
        "the chat accepts a new turn"
    );
}

// ---- the orphan watchdog race -------------------------------------------------------------------

#[tokio::test]
async fn when_the_watchdog_wins_the_open_stream_ends_with_stream_interrupted_within_one_interval() {
    let ping = Duration::from_millis(60);
    let env = StreamEnv::with(Options {
        ping,
        ..Options::default()
    })
    .await;
    let feed = env.gate();
    let request = rid(1);
    let mut stream = env.open(env.chat, &body("Hi", request)).await;
    stream.next_frame().await.unwrap();
    assert!(matches!(
        env.orphan_finalize(request).await,
        crate::domain::service::quota::finalize::FinalizeResult::Won(_)
    ));
    let committed = tokio::time::Instant::now();
    let frames = stream.rest().await;
    let names: Vec<_> = frames.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names.last(), Some(&"error"), "{names:?}");
    assert_eq!(frames.last().unwrap().data["code"], "stream_interrupted");
    assert!(
        committed.elapsed() <= ping * 10 + Duration::from_millis(500),
        "ended after {:?} for an interval of {ping:?}",
        committed.elapsed()
    );
    promptly(&feed).await;
    assert_eq!(
        env.turn_text(request, "state").await.as_deref(),
        Some("failed")
    );
    assert_eq!(
        env.turn_text(request, "error_code").await.as_deref(),
        Some("orphan_timeout")
    );
    assert_eq!(
        env.usage_events().await.len(),
        1,
        "the watchdog's event only"
    );
    assert!(
        env.audit_events().await.is_empty(),
        "the stream path writes no second terminal state or audit"
    );
    assert_eq!(env.messages(env.chat).await.len(), 1);
}

#[tokio::test]
async fn a_completion_that_loses_the_cas_to_the_watchdog_sends_no_terminal_event_but_stream_interrupted()
 {
    let env = StreamEnv::new().await;
    let feed = env.gate();
    let request = rid(1);
    let mut stream = env.open(env.chat, &body("Hi", request)).await;
    stream.next_frame().await.unwrap();
    env.orphan_finalize(request).await;
    feed.send_event(oa::delta("late").0, &oa::delta("late").1);
    feed.send_event(oa::completed(5, 5).0, &oa::completed(5, 5).1);
    let frames = stream.rest().await;
    let names: Vec<_> = frames.iter().map(|f| f.name.as_str()).collect();
    assert!(!names.contains(&"done"), "{names:?}");
    assert_eq!(frames.last().unwrap().data["code"], "stream_interrupted");
    assert_eq!(env.usage_events().await.len(), 1);
    assert!(env.audit_events().await.is_empty());
    assert_eq!(
        env.turn_text(request, "error_code").await.as_deref(),
        Some("orphan_timeout")
    );
    assert_eq!(
        env.messages(env.chat).await.len(),
        1,
        "the late answer was not persisted"
    );
}

// ==== messages across turns ========================================================================

#[tokio::test]
async fn three_completed_turns_give_six_messages_in_order_with_their_models_and_tokens() {
    let env = StreamEnv::new().await;
    let mut requests = Vec::new();
    for n in 1..=3u128 {
        env.script(&[
            oa::delta(&format!("answer {n}")),
            oa::completed(10 * u64::try_from(n).unwrap(), 5),
        ]);
        env.stream(env.chat, &body(&format!("question {n}"), rid(n)))
            .await;
        requests.push(rid(n));
        env.advance(60);
    }
    let chat = env.get(&format!("/mini-chat/v1/chats/{}", env.chat)).await;
    assert_eq!(chat.json["message_count"], 6);
    let page = env
        .get(&format!("/mini-chat/v1/chats/{}/messages", env.chat))
        .await;
    let items = page.json["items"].as_array().unwrap();
    let roles: Vec<_> = items.iter().map(|i| i["role"].as_str().unwrap()).collect();
    assert_eq!(
        roles,
        [
            "user",
            "assistant",
            "user",
            "assistant",
            "user",
            "assistant"
        ]
    );
    for (pair, request) in items.chunks(2).zip(&requests) {
        assert_eq!(pair[0]["request_id"], request.to_string());
        assert_eq!(
            pair[1]["request_id"],
            request.to_string(),
            "each pair shares its request id"
        );
        assert_eq!(pair[1]["model"], "p", "the effective model");
        assert!(pair[1]["input_tokens"].as_i64().unwrap() > 0);
        assert_eq!(pair[1]["output_tokens"], 5);
    }
}

async fn message_count(env: &StreamEnv) -> i64 {
    env.get(&format!("/mini-chat/v1/chats/{}", env.chat))
        .await
        .json["message_count"]
        .as_i64()
        .unwrap()
}

#[tokio::test]
async fn failed_cancelled_and_rejected_turns_add_the_documented_number_of_messages() {
    let env = StreamEnv::new().await;
    // A failed turn adds only its user message.
    env.script(&[oa::failed("boom")]);
    env.stream(env.chat, &body("fails", rid(1))).await;
    assert_eq!(message_count(&env).await, 1);
    env.advance(60);
    // A cancelled turn with partial text adds two.
    let feed = env.gate();
    let mut stream = env.open(env.chat, &body("partial", rid(2))).await;
    stream.next_frame().await.unwrap();
    feed.send_event(oa::delta("some").0, &oa::delta("some").1);
    stream.next_frame().await.unwrap();
    drop(stream);
    env.wait_terminal(rid(2)).await;
    assert_eq!(message_count(&env).await, 3);
    env.advance(60);
    // One without text adds one.
    let feed = env.gate();
    let mut stream = env.open(env.chat, &body("silent", rid(3))).await;
    stream.next_frame().await.unwrap();
    drop(stream);
    promptly(&feed).await;
    env.wait_terminal(rid(3)).await;
    assert_eq!(message_count(&env).await, 4);
    // A rejected send adds none.
    let reply = env.post(env.chat, &body("   ", rid(4))).await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(message_count(&env).await, 4);
}

#[tokio::test]
async fn a_completed_send_moves_the_chat_to_the_top_of_the_chat_list() {
    let env = StreamEnv::new().await;
    let other = env.seed_chat(2, "p").await;
    // Make the other chat the most recently updated one.
    env.quota
        .db
        .raw
        .execute_unprepared_for_test(&format!(
            "UPDATE chats SET updated_at = '2026-03-09T00:00:00Z' WHERE id = {}",
            blob(other)
        ))
        .await;
    let before = env.get("/mini-chat/v1/chats").await;
    assert_eq!(before.json["items"][0]["id"], other.to_string());
    env.script(&[oa::completed(1, 1)]);
    env.stream(env.chat, &body("Hi", rid(1))).await;
    let after = env.get("/mini-chat/v1/chats").await;
    assert_eq!(after.json["items"][0]["id"], env.chat.to_string());
}

trait RawExec {
    async fn execute_unprepared_for_test(&self, sql: &str);
}

impl RawExec for sea_orm::DatabaseConnection {
    async fn execute_unprepared_for_test(&self, sql: &str) {
        use sea_orm::ConnectionTrait;
        self.execute_unprepared(sql).await.unwrap();
    }
}

// ==== the request metrics, with the real instruments ==============================================

#[tokio::test]
async fn a_completed_turn_records_its_request_metrics_with_provider_and_model_labels_only() {
    use opentelemetry::metrics::MeterProvider as _;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    let real = Arc::new(crate::infra::metrics::MiniChatMetrics::new(
        &provider.meter("mini_chat"),
        "",
    ));
    let env = StreamEnv::with(Options {
        metrics_override: Some(
            real as Arc<dyn crate::domain::service::stream::ports::StreamMetrics>,
        ),
        ..Options::default()
    })
    .await;
    env.script(&[oa::delta("Hello"), oa::completed(10, 5)]);
    env.stream(env.chat, &body("Hi", rid(1))).await;

    provider.force_flush().unwrap();
    let finished = exporter.get_finished_metrics().unwrap();
    let latest = finished.last().expect("exported metrics");
    let mut counters = std::collections::BTreeMap::new();
    let mut label_keys = BTreeSet::new();
    let mut histograms = BTreeSet::new();
    for sm in latest.scope_metrics() {
        for metric in sm.metrics() {
            match metric.data() {
                AggregatedMetrics::U64(MetricData::Sum(sum)) => {
                    for point in sum.data_points() {
                        let labels: Vec<(String, String)> = point
                            .attributes()
                            .map(|kv| (kv.key.to_string(), kv.value.as_str().into_owned()))
                            .collect();
                        label_keys.extend(labels.iter().map(|(k, _)| k.clone()));
                        counters.insert(metric.name().to_owned(), (labels, point.value()));
                    }
                }
                AggregatedMetrics::F64(MetricData::Histogram(histogram)) => {
                    for point in histogram.data_points() {
                        label_keys.extend(point.attributes().map(|kv| kv.key.to_string()));
                        histograms.insert(metric.name().to_owned());
                    }
                }
                AggregatedMetrics::I64(MetricData::Sum(sum)) => {
                    for point in sum.data_points() {
                        assert_eq!(
                            point.value(),
                            0,
                            "{}: the gauge is back to its value",
                            metric.name()
                        );
                    }
                }
                _ => {}
            }
        }
    }
    let labels = vec![
        ("model".to_owned(), "p".to_owned()),
        ("provider".to_owned(), "openai".to_owned()),
    ];
    for name in ["mini_chat_stream_started", "mini_chat_stream_completed"] {
        let (found, value) = counters
            .get(name)
            .unwrap_or_else(|| panic!("{name} was not recorded"));
        let mut found = found.clone();
        found.sort();
        assert_eq!((found, *value), (labels.clone(), 1), "{name}");
    }
    for name in [
        "mini_chat_ttft_provider_ms",
        "mini_chat_ttft_overhead_ms",
        "mini_chat_stream_total_latency_ms",
        "mini_chat_finalization_latency_ms",
        "mini_chat_image_inputs_per_turn",
    ] {
        assert!(
            histograms.contains(name),
            "{name} was not recorded: {histograms:?}"
        );
    }
    assert!(
        label_keys.is_subset(
            &["provider", "model"]
                .iter()
                .map(|k| (*k).to_owned())
                .collect()
        ),
        "{label_keys:?}"
    );
}

// ==== registration =================================================================================

const SEND: &str = "POST:/mini-chat/v1/chats/{id}/messages:stream";
const STATUS: &str = "GET:/mini-chat/v1/chats/{id}/turns/{request_id}";

fn spec(env: &StreamEnv, key: &str) -> toolkit::api::operation_builder::OperationSpec {
    env.openapi
        .operation_specs
        .get(key)
        .unwrap_or_else(|| panic!("{key} is not registered"))
        .value()
        .clone()
}

fn statuses(spec: &toolkit::api::operation_builder::OperationSpec) -> BTreeSet<u16> {
    spec.responses.iter().map(|r| r.status).collect()
}

#[tokio::test]
async fn the_two_routes_are_registered_with_their_operation_ids_tags_and_license() {
    let env = StreamEnv::new().await;
    for (key, operation, tag) in [
        (SEND, "mini_chat.stream_message", "Mini Chat Messages"),
        (STATUS, "mini_chat.get_turn", "Mini Chat Turns"),
    ] {
        let spec = spec(&env, key);
        assert_eq!(spec.operation_id.as_deref(), Some(operation), "{key}");
        assert!(spec.tags.iter().any(|t| t == tag), "{key}: {:?}", spec.tags);
        assert!(spec.authenticated, "{key}");
        let license = spec.license_requirement.clone().expect("a license feature");
        assert_eq!(
            license.license_names,
            vec![super::BASE_LICENSE_FEATURE.to_owned()],
            "{key}"
        );
    }
}

#[tokio::test]
async fn the_send_route_documents_the_sse_response_the_body_and_the_problems() {
    let env = StreamEnv::new().await;
    let spec = spec(&env, SEND);
    let ok = spec
        .responses
        .iter()
        .find(|r| r.status == 200)
        .expect("a 200 response");
    assert_eq!(ok.content_type, "text/event-stream");
    assert_eq!(
        ok.schema.as_ref().unwrap().schema_name(),
        "MiniChatSseEvent"
    );
    let body = spec.request_body.clone().expect("a request body");
    assert_eq!(body.content_type, "application/json");
    assert!(matches!(
        &body.schema,
        toolkit::api::operation_builder::RequestBodySchema::Ref { schema_name } if schema_name == "StreamMessageRequest"
    ));
    let statuses = statuses(&spec);
    for status in [200, 400, 401, 403, 404, 409, 422, 429, 500, 503] {
        assert!(statuses.contains(&status), "{status}: {statuses:?}");
    }
    let unavailable = spec.responses.iter().find(|r| r.status == 503).unwrap();
    assert!(
        unavailable
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("retry-after"))
    );
    assert!(spec.params.iter().any(|p| p.name == "id"));
}

#[tokio::test]
async fn the_status_route_documents_its_response_and_problems() {
    let env = StreamEnv::new().await;
    let spec = spec(&env, STATUS);
    let ok = spec
        .responses
        .iter()
        .find(|r| r.status == 200)
        .expect("a 200 response");
    assert_eq!(
        ok.schema.as_ref().unwrap().schema_name(),
        "TurnStatusResponse"
    );
    assert!(spec.request_body.is_none());
    let statuses = statuses(&spec);
    for status in [200, 400, 401, 403, 404, 500, 503] {
        assert!(statuses.contains(&status), "{status}: {statuses:?}");
    }
    for status in [409, 422, 429] {
        assert!(
            !statuses.contains(&status),
            "{status} is not a status-route problem"
        );
    }
    let names: BTreeSet<_> = spec.params.iter().map(|p| p.name.clone()).collect();
    assert!(
        names.contains("id") && names.contains("request_id"),
        "{names:?}"
    );
}

#[test]
fn the_stream_request_schema_has_no_model_field() {
    let schema = serde_json::to_value(
        <super::super::dto::StreamMessageRequest as utoipa::PartialSchema>::schema(),
    )
    .unwrap();
    let properties: BTreeSet<String> = schema["properties"]
        .as_object()
        .expect("an object schema")
        .keys()
        .cloned()
        .collect();
    assert_eq!(
        properties,
        ["attachment_ids", "content", "request_id", "web_search"]
            .iter()
            .map(|k| (*k).to_owned())
            .collect(),
        "no `model`: the turn runs on the chat's model"
    );
    assert_eq!(schema["required"], json!(["content"]));
}

#[allow(dead_code)]
fn unused(_: &Uuid) {
    let _ = (USER, qblob, SettlementMethod::Actual);
}
