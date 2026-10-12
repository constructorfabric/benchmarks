#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The SSE contract of `messages:stream`: event shapes and ordering, provider event translation,
//! the `done` payload, sanitized terminal `error` events and the REST / streaming error split
//! (DESIGN sections 3.3 "SSE Event Definitions", "Provider Event Translation", "Streaming error
//! codes", ADR-0004).

mod common;

use common::{
    CHAT_RESOURCE_TYPE, PROVIDER_RESPONSE_ID, assert_failed_precondition, assert_invalid_argument,
    assert_not_found, assert_quota_exhausted, body_text, closing_frames, content_type,
    delta_frames, enqueue_chat, enqueue_text, env_two_models, env_user_chat, env_with_policy,
    expect_stream, failed_script, first, frame, incomplete_frames, merge, named, names, new_chat,
    only_turn, opening_frames, partial_frames, policy, send, send_text, send_with_id, started,
    stream_message_id, stream_request_id, streamed_text, terminal, tiny_limits, two_model_catalog,
    without_pings,
};
use mini_chat::test_support::{ScriptedResponse, SseEvent, TestUser};
use serde_json::{Value, json};
use uuid::Uuid;

const RAW_ERROR: &str = "Request resp_abc123def456 failed for file-AbCdEfGhIjKlMnOp at https://x.y/z using sk-ABCDEFGHIJKLMNOP now";
const SANITIZED_ERROR: &str =
    "Request [provider_id] failed for [provider_id] at [url] using [credential] now";

fn assert_stream_leaks_no_provider_identifier(body: &str) {
    for secret in [
        "resp_abc123def456",
        "file-AbCdEfGhIjKlMnOp",
        "https://x.y/z",
        "sk-ABCDEFGHIJKLMNOP",
        PROVIDER_RESPONSE_ID,
        "msg_0123456789abcdef",
    ] {
        assert!(!body.contains(secret), "`{secret}` leaked into: {body}");
    }
    assert!(
        !body.contains("resp_"),
        "a `resp_` identifier leaked: {body}"
    );
}

// ===========================================================================
// stream_started, delta, ordering
// ===========================================================================

#[tokio::test]
async fn stream_started_comes_first_and_carries_request_id_message_id_and_is_new_turn() {
    let (env, user, chat) = env_user_chat().await;
    let request_id = Uuid::new_v4();

    let events = expect_stream(&send_with_id(&env, &user, &chat, "hello", request_id).await);

    let first = started(&events);
    assert_eq!(first.data["request_id"], json!(request_id.to_string()));
    assert_eq!(first.data["is_new_turn"], json!(true));
    assert!(
        Uuid::parse_str(first.data["message_id"].as_str().unwrap()).is_ok(),
        "{first:?}"
    );
}

#[tokio::test]
async fn stream_started_message_id_is_the_persisted_assistant_message_id() {
    let (env, user, chat) = env_user_chat().await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let rows = common::message_rows(&env, &chat).await;
    let assistant = rows.iter().find(|row| row.role == "assistant").unwrap();
    assert_eq!(assistant.id.to_string(), stream_message_id(&events));
}

#[tokio::test]
async fn stream_started_has_no_thread_summary_when_there_is_none() {
    let (env, user, chat) = env_user_chat().await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert!(
        started(&events)
            .data
            .get("thread_summary_applied")
            .is_none(),
        "{events:?}"
    );
}

#[tokio::test]
async fn delta_events_carry_type_text_and_the_content_in_order() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_text(&env, &["alpha", " beta", " gamma"], 10, 5);

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let deltas = named(&events, "delta");
    assert_eq!(deltas.len(), 3, "{events:?}");
    for delta in &deltas {
        assert_eq!(delta.data["type"], json!("text"), "{delta:?}");
    }
    let contents: Vec<&str> = deltas
        .iter()
        .map(|delta| delta.data["content"].as_str().unwrap())
        .collect();
    assert_eq!(contents, ["alpha", " beta", " gamma"]);
}

#[tokio::test]
async fn a_well_formed_stream_is_stream_started_content_and_exactly_one_terminal_event() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_text(&env, &["a", "b"], 10, 5);

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(names(&events), ["stream_started", "delta", "delta", "done"]);
    assert_eq!(terminal(&events).event, "done");
}

#[tokio::test]
async fn ping_events_never_follow_content_or_the_terminal_event() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_text(&env, &["a", "b"], 10, 5);

    let resp = send_text(&env, &user, &chat, "hello").await;

    let raw = resp.sse_events();
    let mut content_started = false;
    for event in &raw {
        match event.event.as_str() {
            "delta" | "tool" | "citations" | "done" | "error" => content_started = true,
            "ping" => assert!(
                !content_started,
                "ping is only allowed between stream_started and the first content event: {raw:?}"
            ),
            _ => {}
        }
    }
    assert_eq!(raw.last().map(|event| event.event.as_str()), Some("done"));
}

#[tokio::test]
async fn the_event_stream_is_closed_after_the_terminal_event() {
    let (env, user, chat) = env_user_chat().await;

    let resp = send_text(&env, &user, &chat, "hello").await;

    let events = resp.sse_events();
    assert_eq!(events.last().unwrap().event, "done");
    assert_eq!(
        events.iter().filter(|e| e.event == "done").count(),
        1,
        "{events:?}"
    );
}

// ===========================================================================
// tool events
// ===========================================================================

fn tool_events(events: &[SseEvent]) -> Vec<&SseEvent> {
    named(events, "tool")
}

#[tokio::test]
async fn file_search_provider_events_become_tool_start_and_done() {
    let (env, user, chat) = env_user_chat().await;
    let mut frames = opening_frames();
    frames.push(frame(
        "response.file_search_call.searching",
        json!({ "item_id": "fs_0123456789abcdef", "output_index": 0, "sequence_number": 5 }),
    ));
    frames.push(frame(
        "response.file_search_call.completed",
        json!({ "item_id": "fs_0123456789abcdef", "output_index": 0, "sequence_number": 6 }),
    ));
    frames.extend(delta_frames(&["Done"]));
    frames.extend(closing_frames(
        "Done",
        &json!([]),
        Some((10, 5)),
        "response.completed",
    ));
    enqueue_chat(&env, ScriptedResponse::Sse(frames));

    let events = expect_stream(&send_text(&env, &user, &chat, "look it up").await);

    let tools = tool_events(&events);
    assert_eq!(tools.len(), 2, "{events:?}");
    assert_eq!(
        tools[0].data,
        json!({ "phase": "start", "name": "file_search", "details": {} })
    );
    assert_eq!(
        tools[1].data,
        json!({ "phase": "done", "name": "file_search", "details": { "files_searched": 0 } })
    );
    assert_eq!(terminal(&events).event, "done");
}

#[tokio::test]
async fn file_search_done_reports_the_number_of_results_when_the_provider_sends_them() {
    let (env, user, chat) = env_user_chat().await;
    let mut frames = opening_frames();
    frames.push(frame(
        "response.file_search_call.searching",
        json!({ "item_id": "fs_0123456789abcdef", "output_index": 0 }),
    ));
    frames.push(frame(
        "response.file_search_call.completed",
        json!({
            "item_id": "fs_0123456789abcdef", "output_index": 0,
            "results": [{ "text": "a" }, { "text": "b" }, { "text": "c" }]
        }),
    ));
    frames.extend(closing_frames(
        "",
        &json!([]),
        Some((10, 5)),
        "response.completed",
    ));
    enqueue_chat(&env, ScriptedResponse::Sse(frames));

    let events = expect_stream(&send_text(&env, &user, &chat, "look it up").await);

    let done = tool_events(&events)
        .into_iter()
        .find(|tool| tool.data["phase"] == json!("done"))
        .expect("a tool done event");
    assert_eq!(done.data["details"]["files_searched"], json!(3), "{done:?}");
}

#[tokio::test]
async fn web_search_provider_events_become_tool_start_and_done() {
    let (env, user, chat) = env_user_chat().await;
    let mut frames = opening_frames();
    frames.push(frame(
        "response.web_search_call.searching",
        json!({ "item_id": "ws_0123456789abcdef", "output_index": 0, "sequence_number": 5 }),
    ));
    frames.push(frame(
        "response.web_search_call.completed",
        json!({ "item_id": "ws_0123456789abcdef", "output_index": 0, "sequence_number": 6 }),
    ));
    frames.extend(delta_frames(&["Found"]));
    frames.extend(closing_frames(
        "Found",
        &json!([]),
        Some((10, 5)),
        "response.completed",
    ));
    enqueue_chat(&env, ScriptedResponse::Sse(frames));

    let events = expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "search", "web_search": { "enabled": true } }),
        )
        .await,
    );

    let tools = tool_events(&events);
    assert_eq!(tools.len(), 2, "{events:?}");
    assert_eq!(
        tools[0].data,
        json!({ "phase": "start", "name": "web_search", "details": {} })
    );
    assert_eq!(
        tools[1].data,
        json!({ "phase": "done", "name": "web_search", "details": {} })
    );
}

fn code_interpreter_frames(outputs: &Value) -> Vec<(String, Value)> {
    let mut frames = opening_frames();
    frames.push(frame(
        "response.code_interpreter_call.in_progress",
        json!({ "item_id": "ci_0123456789abcdef", "output_index": 1, "sequence_number": 5 }),
    ));
    frames.push(frame(
        "response.code_interpreter_call.interpreting",
        json!({ "item_id": "ci_0123456789abcdef", "output_index": 1, "sequence_number": 6 }),
    ));
    frames.push(frame(
        "response.code_interpreter_call.completed",
        json!({ "item_id": "ci_0123456789abcdef", "output_index": 1, "sequence_number": 7 }),
    ));
    frames.push(frame(
        "response.output_item.done",
        json!({
            "output_index": 1,
            "item": {
                "id": "ci_0123456789abcdef", "type": "code_interpreter_call", "status": "completed",
                "container_id": "cntr_0123456789abcdef", "code": "print(2 + 2)", "outputs": outputs
            },
            "sequence_number": 8
        }),
    ));
    frames.extend(delta_frames(&["The result"]));
    frames.extend(closing_frames(
        "The result",
        &json!([]),
        Some((10, 5)),
        "response.completed",
    ));
    frames
}

#[tokio::test]
async fn code_interpreter_provider_events_become_tool_start_and_done_with_the_output() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(code_interpreter_frames(&json!([
            { "type": "logs", "logs": "4" },
            { "type": "logs", "logs": "done" }
        ]))),
    );

    let events = expect_stream(&send_text(&env, &user, &chat, "compute").await);

    let tools = tool_events(&events);
    assert_eq!(
        tools.len(),
        2,
        "`interpreting` and `completed` produce no client event: {events:?}"
    );
    assert_eq!(
        tools[0].data,
        json!({ "phase": "start", "name": "code_interpreter", "details": {} })
    );
    assert_eq!(
        tools[1].data,
        json!({ "phase": "done", "name": "code_interpreter", "details": { "output": "4\ndone" } })
    );
}

#[tokio::test]
async fn code_interpreter_output_is_capped_at_8192_characters_with_a_truncation_suffix() {
    let (env, user, chat) = env_user_chat().await;
    let logs = "x".repeat(20_000);
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(code_interpreter_frames(&json!([
            { "type": "logs", "logs": logs }
        ]))),
    );

    let events = expect_stream(&send_text(&env, &user, &chat, "compute").await);

    let done = tool_events(&events)
        .into_iter()
        .find(|tool| tool.data["phase"] == json!("done"))
        .expect("a tool done event");
    let output = done.data["details"]["output"].as_str().unwrap();
    assert!(output.ends_with("...[truncated]"), "{output}");
    assert!(
        output.chars().count() <= 8192 + "...[truncated]".len(),
        "the output is capped: {} characters",
        output.chars().count()
    );
}

#[tokio::test]
async fn other_output_item_done_events_produce_no_tool_event() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_text(&env, &["plain"], 10, 5);

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert!(
        tool_events(&events).is_empty(),
        "the message item's output_item.done is ignored: {events:?}"
    );
}

#[tokio::test]
async fn tool_and_delta_events_may_interleave() {
    let (env, user, chat) = env_user_chat().await;
    let mut frames = opening_frames();
    frames.extend(delta_frames(&["one "]));
    frames.push(frame(
        "response.web_search_call.searching",
        json!({ "item_id": "ws_0123456789abcdef", "output_index": 1 }),
    ));
    frames.push(frame(
        "response.web_search_call.completed",
        json!({ "item_id": "ws_0123456789abcdef", "output_index": 1 }),
    ));
    frames.extend(delta_frames(&["two"]));
    frames.extend(closing_frames(
        "one two",
        &json!([]),
        Some((10, 5)),
        "response.completed",
    ));
    enqueue_chat(&env, ScriptedResponse::Sse(frames));

    let events = expect_stream(
        &send(
            &env,
            &user,
            &chat,
            json!({ "content": "search", "web_search": { "enabled": true } }),
        )
        .await,
    );

    assert_eq!(
        names(&events),
        ["stream_started", "delta", "tool", "tool", "delta", "done"]
    );
    assert_eq!(streamed_text(&events), "one two");
}

// ===========================================================================
// citations
// ===========================================================================

fn web_annotation() -> Value {
    json!({
        "type": "url_citation", "start_index": 0, "end_index": 4,
        "url": "https://example.com/rust", "title": "Rust Language"
    })
}

fn citation_frames(final_event: &str) -> Vec<(String, Value)> {
    let annotation = web_annotation();
    let mut frames = opening_frames();
    frames.push(frame(
        "response.web_search_call.searching",
        json!({ "item_id": "ws_0123456789abcdef", "output_index": 0 }),
    ));
    frames.push(frame(
        "response.web_search_call.completed",
        json!({ "item_id": "ws_0123456789abcdef", "output_index": 0 }),
    ));
    frames.extend(delta_frames(&["Rust is a ", "systems language."]));
    frames.push(frame(
        "response.output_text.annotation.added",
        json!({
            "item_id": "msg_0123456789abcdef", "output_index": 1, "content_index": 0,
            "annotation_index": 0, "annotation": annotation, "sequence_number": 20
        }),
    ));
    frames.extend(closing_frames(
        "Rust is a systems language.",
        &json!([annotation]),
        Some((10, 5)),
        final_event,
    ));
    frames
}

fn web_search_body() -> Value {
    json!({ "content": "what is rust", "web_search": { "enabled": true } })
}

#[tokio::test]
async fn web_url_citations_are_sent_once_before_done() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(citation_frames("response.completed")),
    );

    let events = expect_stream(&send(&env, &user, &chat, web_search_body()).await);

    let citations = named(&events, "citations");
    assert_eq!(
        citations.len(),
        1,
        "exactly one citations event: {events:?}"
    );
    let position = events.iter().position(|e| e.event == "citations").unwrap();
    assert_eq!(
        events[position + 1].event,
        "done",
        "citations precede done: {events:?}"
    );
    assert!(
        events[..position].iter().all(|e| e.event != "done"),
        "{events:?}"
    );
    let last_delta = events.iter().rposition(|e| e.event == "delta").unwrap();
    assert!(
        last_delta < position,
        "citations follow every delta: {events:?}"
    );
}

#[tokio::test]
async fn web_citation_items_carry_source_url_title_snippet_and_span() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(citation_frames("response.completed")),
    );

    let events = expect_stream(&send(&env, &user, &chat, web_search_body()).await);

    let items = first(&events, "citations").data["items"]
        .as_array()
        .unwrap()
        .clone();
    assert!(!items.is_empty(), "{events:?}");
    for item in &items {
        assert_eq!(item["source"], json!("web"), "{item}");
        assert_eq!(item["url"], json!("https://example.com/rust"), "{item}");
        assert_eq!(item["title"], json!("Rust Language"), "{item}");
        assert_eq!(item["span"], json!({ "start": 0, "end": 4 }), "{item}");
        assert_eq!(
            item["snippet"],
            json!("Rust"),
            "the snippet is the answer text in the annotation range: {item}"
        );
        assert!(item.get("attachment_id").is_none(), "{item}");
        assert!(item.get("score").is_none(), "{item}");
    }
}

#[tokio::test]
async fn a_stream_without_annotations_has_no_citations_event() {
    let (env, user, chat) = env_user_chat().await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert!(named(&events, "citations").is_empty(), "{events:?}");
}

#[tokio::test]
async fn an_incomplete_provider_response_sends_no_citations() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(citation_frames("response.incomplete")),
    );

    let events = expect_stream(&send(&env, &user, &chat, web_search_body()).await);

    assert!(named(&events, "citations").is_empty(), "{events:?}");
    assert_eq!(terminal(&events).event, "done");
}

#[tokio::test]
async fn a_citation_annotation_never_exposes_provider_identifiers() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(citation_frames("response.completed")),
    );

    let resp = send(&env, &user, &chat, web_search_body()).await;

    assert!(!body_text(&resp).contains("resp_"));
    assert!(!body_text(&resp).contains("msg_0123456789abcdef"));
}

// ===========================================================================
// done
// ===========================================================================

#[tokio::test]
async fn done_reports_usage_models_and_an_allow_decision() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_text(&env, &["Hello"], 123, 45);

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let done = terminal(&events);
    assert_eq!(done.event, "done");
    assert_eq!(
        done.data["usage"],
        json!({ "input_tokens": 123, "output_tokens": 45 })
    );
    assert_eq!(done.data["effective_model"], json!(common::PREMIUM_MODEL));
    assert_eq!(done.data["selected_model"], json!(common::PREMIUM_MODEL));
    assert_eq!(done.data["quota_decision"], json!("allow"));
    assert!(done.data.get("downgrade_from").is_none(), "{done:?}");
    assert!(done.data.get("downgrade_reason").is_none(), "{done:?}");
}

#[tokio::test]
async fn done_usage_carries_token_counts_only() {
    let (env, user, chat) = env_user_chat().await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let usage = terminal(&events).data["usage"].as_object().unwrap().clone();
    let mut keys: Vec<&str> = usage.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["input_tokens", "output_tokens"], "{usage:?}");
}

#[tokio::test]
async fn done_of_a_completed_turn_carries_quota_warnings_as_an_array() {
    let (env, user, chat) = env_user_chat().await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let warnings = terminal(&events).data["quota_warnings"]
        .as_array()
        .unwrap_or_else(|| panic!("quota_warnings must be an array: {events:?}"));
    for warning in warnings {
        for member in [
            "tier",
            "period",
            "remaining_percentage",
            "warning",
            "exhausted",
        ] {
            assert!(
                warning.get(member).is_some(),
                "`{member}` missing: {warning}"
            );
        }
    }
}

#[tokio::test]
async fn done_carries_only_the_documented_members() {
    let (env, user, chat) = env_user_chat().await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let done = terminal(&events).data.as_object().unwrap().clone();
    let allowed = [
        "usage",
        "effective_model",
        "selected_model",
        "quota_decision",
        "downgrade_from",
        "downgrade_reason",
        "quota_warnings",
    ];
    for key in done.keys() {
        assert!(
            allowed.contains(&key.as_str()),
            "unexpected `{key}` in done: {done:?}"
        );
    }
}

#[tokio::test]
async fn done_exposes_no_internal_identifiers() {
    let (env, user, chat) = env_user_chat().await;

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let done = terminal(&events);
    let text = done.data.to_string();
    let turn = only_turn(&env, &chat).await;
    let request_id = stream_request_id(&events);
    for forbidden in [
        turn.id.to_string(),
        turn.id.simple().to_string(),
        request_id.to_string(),
        stream_message_id(&events),
        PROVIDER_RESPONSE_ID.to_owned(),
    ] {
        assert!(
            !text.contains(&forbidden),
            "`{forbidden}` leaked into done: {text}"
        );
    }
    for key in ["request_id", "message_id", "turn_id", "response_id", "id"] {
        assert!(
            done.data.get(key).is_none(),
            "`{key}` must not be in done: {text}"
        );
    }
}

#[tokio::test]
async fn no_provider_identifier_appears_anywhere_in_the_stream() {
    let (env, user, chat) = env_user_chat().await;

    let resp = send_text(&env, &user, &chat, "hello").await;

    assert_stream_leaks_no_provider_identifier(&body_text(&resp));
}

#[tokio::test]
async fn a_provider_incomplete_response_still_ends_with_done_and_a_completed_turn() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(
        &env,
        ScriptedResponse::Sse(incomplete_frames(&["cut o", "ff"], 30, 8)),
    );

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let done = terminal(&events);
    assert_eq!(done.event, "done", "{events:?}");
    assert_eq!(
        done.data["usage"],
        json!({ "input_tokens": 30, "output_tokens": 8 })
    );
    let turn = only_turn(&env, &chat).await;
    assert_eq!(turn.state, "completed");
    assert_eq!(
        turn.error_code, None,
        "the incomplete reason is never stored as an error code"
    );
}

#[tokio::test]
async fn done_after_a_standard_model_turn_names_the_standard_model_for_both_fields() {
    let env = env_two_models().await;
    let user = TestUser::new();
    let chat = common::chat_id(
        &common::create_chat(&env, &user, json!({ "model": common::STANDARD_MODEL })).await,
    );

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let done = terminal(&events);
    assert_eq!(done.data["selected_model"], json!(common::STANDARD_MODEL));
    assert_eq!(done.data["effective_model"], json!(common::STANDARD_MODEL));
    assert_eq!(done.data["quota_decision"], json!("allow"));
}

// ===========================================================================
// error: terminal, sanitized, mapped
// ===========================================================================

#[tokio::test]
async fn response_failed_ends_the_stream_with_one_provider_error_event() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, failed_script("The model is overloaded", None));

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(names(&events), ["stream_started", "error"], "{events:?}");
    let error = terminal(&events);
    assert_eq!(error.data["code"], json!("provider_error"));
    assert_eq!(error.data["message"], json!("The model is overloaded"));
    assert!(named(&events, "done").is_empty());
}

#[tokio::test]
async fn error_payload_is_code_and_message_only() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, failed_script("boom", None));

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let mut keys: Vec<String> = terminal(&events)
        .data
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    keys.sort();
    assert_eq!(keys, ["code", "message"]);
}

#[tokio::test]
async fn response_failed_messages_are_sanitized() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, failed_script(RAW_ERROR, None));

    let resp = send_text(&env, &user, &chat, "hello").await;

    let events = expect_stream(&resp);
    let error = terminal(&events);
    assert_eq!(error.data["code"], json!("provider_error"));
    assert_eq!(error.data["message"], json!(SANITIZED_ERROR), "{error:?}");
    assert_stream_leaks_no_provider_identifier(&body_text(&resp));
}

#[tokio::test]
async fn an_sse_error_event_from_the_provider_is_a_sanitized_provider_error() {
    let (env, user, chat) = env_user_chat().await;
    let mut frames = opening_frames();
    frames.push(frame(
        "error",
        json!({ "code": "server_error", "message": RAW_ERROR, "param": null, "sequence_number": 5 }),
    ));
    enqueue_chat(&env, ScriptedResponse::Sse(frames));

    let resp = send_text(&env, &user, &chat, "hello").await;

    let events = expect_stream(&resp);
    let error = terminal(&events);
    assert_eq!(error.data["code"], json!("provider_error"), "{events:?}");
    assert_eq!(error.data["message"], json!(SANITIZED_ERROR));
    assert_stream_leaks_no_provider_identifier(&body_text(&resp));
}

#[tokio::test]
async fn response_failed_with_a_top_level_error_is_read_as_a_fallback() {
    let (env, user, chat) = env_user_chat().await;
    let mut frames = opening_frames();
    frames.push(frame(
        "response.failed",
        json!({
            "response": { "id": PROVIDER_RESPONSE_ID, "status": "failed", "output": [] },
            "error": { "message": RAW_ERROR }
        }),
    ));
    enqueue_chat(&env, ScriptedResponse::Sse(frames));

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let error = terminal(&events);
    assert_eq!(error.data["code"], json!("provider_error"), "{events:?}");
    assert_eq!(error.data["message"], json!(SANITIZED_ERROR));
}

#[tokio::test]
async fn bearer_tokens_in_provider_messages_become_a_credential_placeholder() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(
        &env,
        failed_script(
            "Rejected header Authorization: Bearer abcDEF123.xyz-token",
            None,
        ),
    );

    let resp = send_text(&env, &user, &chat, "hello").await;

    let events = expect_stream(&resp);
    let message = terminal(&events).data["message"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(message.contains("[credential]"), "{message}");
    assert!(!message.contains("abcDEF123.xyz-token"), "{message}");
}

#[tokio::test]
async fn ordinary_words_that_look_like_identifier_prefixes_are_left_intact() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(
        &env,
        failed_script(
            "file_search failed on a file-based index near response",
            None,
        ),
    );

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(
        terminal(&events).data["message"],
        json!("file_search failed on a file-based index near response")
    );
}

#[tokio::test]
async fn text_streamed_before_a_failure_is_delivered_and_the_error_is_last() {
    let (env, user, chat) = env_user_chat().await;
    let mut frames = partial_frames(&["partial answer"]);
    frames.push(frame(
        "response.failed",
        json!({
            "response": {
                "id": PROVIDER_RESPONSE_ID, "status": "failed", "output": [],
                "error": { "code": "server_error", "message": "stopped" }
            }
        }),
    ));
    enqueue_chat(&env, ScriptedResponse::Sse(frames));

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(
        names(&events),
        ["stream_started", "delta", "error"],
        "{events:?}"
    );
    assert_eq!(streamed_text(&events), "partial answer");
}

#[tokio::test]
async fn a_provider_failure_marks_the_turn_failed_with_the_streamed_error_code() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, failed_script("boom", None));

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let turn = only_turn(&env, &chat).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        terminal(&events).data["code"].as_str()
    );
    assert!(turn.has_completed_at);
    assert_eq!(turn.assistant_message_id, None);
}

#[tokio::test]
async fn provider_http_429_is_a_rate_limited_error_event_with_the_retry_delay() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(
        &env,
        ScriptedResponse::Error {
            status: 429,
            body: json!({ "error": { "message": "Rate limit reached for requests",
                                     "type": "requests", "code": "rate_limit_exceeded" } }),
            headers: vec![("retry-after".to_owned(), "7".to_owned())],
        },
    );

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(names(&events), ["stream_started", "error"], "{events:?}");
    let error = terminal(&events);
    assert_eq!(error.data["code"], json!("rate_limited"));
    assert!(
        error.data["message"].as_str().unwrap().contains('7'),
        "the message includes the Retry-After delay: {error:?}"
    );
    let turn = only_turn(&env, &chat).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("rate_limited"));
}

#[tokio::test]
async fn provider_http_429_without_retry_after_is_still_rate_limited() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(
        &env,
        ScriptedResponse::Error {
            status: 429,
            body: json!({ "error": { "message": "Too many requests" } }),
            headers: vec![],
        },
    );

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    assert_eq!(terminal(&events).data["code"], json!("rate_limited"));
}

#[tokio::test]
async fn provider_http_500_is_a_sanitized_provider_error_event() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(
        &env,
        ScriptedResponse::Error {
            status: 500,
            body: json!({ "error": { "message": RAW_ERROR, "type": "server_error" } }),
            headers: vec![],
        },
    );

    let resp = send_text(&env, &user, &chat, "hello").await;

    let events = expect_stream(&resp);
    assert_eq!(
        terminal(&events).data["code"],
        json!("provider_error"),
        "{events:?}"
    );
    assert_stream_leaks_no_provider_identifier(&body_text(&resp));
    let turn = only_turn(&env, &chat).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("provider_error"));
}

#[tokio::test]
async fn no_event_follows_the_terminal_error() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, failed_script("boom", None));

    let resp = send_text(&env, &user, &chat, "hello").await;

    let raw = resp.sse_events();
    let error_position = raw.iter().position(|e| e.event == "error").unwrap();
    assert_eq!(
        error_position,
        raw.len() - 1,
        "nothing follows error: {raw:?}"
    );
}

#[tokio::test]
async fn a_failed_turn_leaves_the_chat_usable_with_a_new_request_id() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, failed_script("boom", None));
    send_text(&env, &user, &chat, "first").await;

    let events = common::send_ok(&env, &user, &chat, "second").await;

    assert_eq!(terminal(&events).event, "done");
}

// ===========================================================================
// REST vs streaming error mapping
// ===========================================================================

#[tokio::test]
async fn pre_stream_validation_failures_are_json_problems_not_event_streams() {
    let (env, user, chat) = env_user_chat().await;

    let resp = send_text(&env, &user, &chat, "   ").await;

    assert_invalid_argument(&resp, "content", "EMPTY_CONTENT");
    assert!(
        content_type(&resp).starts_with("application/problem+json"),
        "{}",
        content_type(&resp)
    );
    assert!(
        resp.sse_events().iter().all(|e| e.event == "message"),
        "no SSE frames"
    );
}

#[tokio::test]
async fn pre_stream_authorization_failures_are_json_problems_not_event_streams() {
    let (env, _owner, chat) = env_user_chat().await;

    let resp = send_text(&env, &TestUser::new(), &chat, "hello").await;

    assert_not_found(&resp, CHAT_RESOURCE_TYPE);
    assert!(!content_type(&resp).contains("event-stream"));
}

#[tokio::test]
async fn pre_stream_kill_switch_rejections_are_json_problems_not_event_streams() {
    let env = env_with_policy(merge(
        policy(two_model_catalog()),
        json!({ "kill_switches": { "disable_web_search": true } }),
    ))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let resp = send(
        &env,
        &user,
        &chat,
        json!({ "content": "search", "web_search": { "enabled": true } }),
    )
    .await;

    assert_failed_precondition(&resp, "web_search", "FEATURE_DISABLED");
    assert!(!content_type(&resp).contains("event-stream"));
}

#[tokio::test]
async fn pre_stream_quota_rejections_are_json_problems_not_event_streams() {
    let env = env_with_policy(merge(
        policy(two_model_catalog()),
        json!({ "default_standard_limits": tiny_limits() }),
    ))
    .await;
    let user = TestUser::new();
    let chat = new_chat(&env, &user).await;

    let resp = send_text(&env, &user, &chat, "hello").await;

    assert_quota_exhausted(&resp, "tokens");
    assert!(!content_type(&resp).contains("event-stream"));
}

#[tokio::test]
async fn failures_after_the_stream_started_are_sse_error_events_with_http_200() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, failed_script("boom", None));

    let resp = send_text(&env, &user, &chat, "hello").await;

    assert_eq!(resp.status, http::StatusCode::OK);
    assert!(content_type(&resp).starts_with("text/event-stream"));
    let events = without_pings(resp.sse_events());
    assert_eq!(started(&events).event, "stream_started");
    assert_eq!(terminal(&events).event, "error");
}

#[tokio::test]
async fn sse_error_codes_come_from_the_streaming_code_list() {
    let (env, user, chat) = env_user_chat().await;
    enqueue_chat(&env, failed_script("boom", None));

    let events = expect_stream(&send_text(&env, &user, &chat, "hello").await);

    let code = terminal(&events).data["code"].as_str().unwrap().to_owned();
    let known = [
        "provider_error",
        "provider_timeout",
        "rate_limited",
        "web_search_calls_exceeded",
        "code_interpreter_calls_exceeded",
        "agentic_iterations_exceeded",
        "unexpected_tool_use",
        "message_persistence_failed",
        "finalization_failed",
        "stream_interrupted",
    ];
    assert!(
        known.contains(&code.as_str()),
        "`{code}` is not a streaming error code"
    );
}
