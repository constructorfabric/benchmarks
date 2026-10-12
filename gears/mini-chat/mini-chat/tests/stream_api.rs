//! `POST /v1/chats/{id}/messages:stream`: the SSE contract of a normal turn,
//! persistence of the turn, the preflight rejections (JSON, no SSE, provider
//! untouched), authorization and the registered operations.
//!
//! Source of truth: DESIGN 3.3 "Streaming Contract" (request body,
//! `request_id` semantics, attachment validation), "SSE Event Definitions",
//! "SSE Event Ordering", the REST error table, 3.6 "Send Message with
//! Streaming Response", 3.7 `messages` / `chat_turns`, 2.2 (No Buffering,
//! Bounded Channels, Model Locked Per Chat), 5.7 (user message persistence
//! invariant, content durability) and the OpenAPI schemas `StreamMessageRequest`,
//! `StreamStartedData`, `DeltaData`, `DoneData`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

mod chat_support;
mod common;
mod stream_support;

use std::collections::BTreeSet;
use std::time::Duration;

use chat_support::{
    CHAT_PDP_TYPE, assert_field_violation, assert_not_found, assert_precondition,
    assert_violation_reason, parse_ts,
};
use serde_json::{Value, json};
use stream_support::{
    ChatSeed, FakePdp, Opts, PdpMode, ProviderReply, StreamHarness, StreamOptions, TENANT_A,
    TENANT_B, USER_A, assert_400_reason, assert_problem, ctx_a, default_stream_models,
    premium_with, second_user, tenant_b_user, ts,
};
use uuid::Uuid;

fn keys(value: &Value) -> BTreeSet<String> {
    value.as_object().expect("object").keys().cloned().collect()
}

fn key_set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| (*s).to_owned()).collect()
}

// ═════════════════════════════════════════════════════════════════════════════
// the SSE contract of a normal turn
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_send_answers_200_text_event_stream_and_starts_with_stream_started() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    let raw = h
        .open(chat, &ctx_a(), &json!({"content": "Hi", "request_id": rid}))
        .await;
    assert_eq!(raw.status.as_u16(), 200);
    assert!(
        raw.content_type().starts_with("text/event-stream"),
        "content type {}",
        raw.content_type()
    );
    let sent = raw.collect().await;
    sent.assert_well_formed();
    assert_eq!(
        sent.names_no_ping(),
        vec!["stream_started", "delta", "delta", "done"]
    );

    let started = sent.started();
    assert_eq!(started["request_id"], json!(rid.to_string()));
    assert!(
        Uuid::parse_str(started["message_id"].as_str().unwrap()).is_ok(),
        "message_id is a UUID: {started}"
    );
    assert_eq!(started["is_new_turn"], json!(true));
    assert!(
        keys(started).is_subset(&key_set(&[
            "request_id",
            "message_id",
            "is_new_turn",
            "thread_summary_applied"
        ])),
        "unexpected stream_started keys: {started}"
    );
    assert!(
        started
            .get("thread_summary_applied")
            .is_none_or(Value::is_null),
        "no thread summary exists in this slice"
    );
}

#[tokio::test]
async fn the_server_generates_a_v4_request_id_when_none_is_given() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;

    let first = h.say(chat, "no id").await;
    assert_eq!(first.request_id().get_version_num(), 4);
    let turn = h.turn_of(first.request_id()).await.expect("turn row");
    assert_eq!(turn.chat_id, chat);

    let explicit_null = h
        .send(chat, &json!({"content": "null id", "request_id": null}))
        .await;
    assert_eq!(explicit_null.request_id().get_version_num(), 4);
    assert_ne!(first.request_id(), explicit_null.request_id());
}

#[tokio::test]
async fn a_client_request_id_of_any_uuid_version_is_accepted_and_echoed() {
    let h = StreamHarness::new().await;
    for rid in [
        "6ba7b810-9dad-11d1-80b4-00c04fd430c8", // v1
        "01890a5d-ac96-774b-bcce-b302099a8057", // v7
        "00000000-0000-0000-0000-000000000001", // not a versioned UUID at all
    ] {
        let chat = h.seed_chat().await;
        let rid = Uuid::parse_str(rid).unwrap();
        let sent = h.say_with(chat, "hello", rid).await;
        assert_eq!(sent.request_id(), rid);
        sent.assert_well_formed();
        assert!(h.turn_of(rid).await.is_some());
    }
}

#[tokio::test]
async fn deltas_carry_type_text_and_the_content_fragment_in_provider_order() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text(&["Al", "pha ", "beta"]));
    let sent = h.say(chat, "go").await;
    let deltas: Vec<&Value> = sent.of("delta").iter().map(|e| &e.data).collect();
    assert_eq!(
        deltas,
        vec![
            &json!({"type": "text", "content": "Al"}),
            &json!({"type": "text", "content": "pha "}),
            &json!({"type": "text", "content": "beta"}),
        ]
    );
    assert_eq!(sent.text(), "Alpha beta");
}

#[tokio::test]
async fn deltas_are_relayed_as_they_arrive_without_waiting_for_the_provider_to_finish() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text_with(
        &["first", " second"],
        &Opts::default().pause_after(1),
    ));
    let mut live = h
        .open(chat, &ctx_a(), &json!({"content": "go"}))
        .await
        .live();
    live.expect("stream_started").await;
    let first = live.next_non_ping().await.expect("first delta");
    assert_eq!(first.event, "delta");
    assert_eq!(first.data["content"], json!("first"));

    // the provider is still holding the rest back
    h.oagw.wait_paused(1).await;
    assert!(
        live.next_event_within(Duration::from_millis(300))
            .await
            .is_err(),
        "nothing else may arrive before the provider continues"
    );
    assert_eq!(h.oagw.finished_streams(), 0);

    h.oagw.release();
    let rest = live.drain().await;
    let names: Vec<&str> = rest.iter().map(|e| e.event.as_str()).collect();
    assert_eq!(names, vec!["delta", "done"]);
    assert_eq!(rest[0].data["content"], json!(" second"));
}

#[tokio::test]
async fn the_stream_closes_right_after_the_terminal_event() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let mut live = h
        .open(chat, &ctx_a(), &json!({"content": "go"}))
        .await
        .live();
    let events = live.drain().await;
    assert_eq!(events.last().unwrap().event, "done");
    assert_eq!(
        events
            .iter()
            .filter(|e| e.event == "done" || e.event == "error")
            .count(),
        1
    );
    assert!(
        matches!(
            live.next_event_within(Duration::from_millis(200)).await,
            Ok(None)
        ),
        "end of stream after the terminal event"
    );
}

#[tokio::test]
async fn done_reports_usage_models_and_the_allow_decision_without_provider_ids() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text_with(
        &["ok"],
        &Opts::default()
            .usage(42, 7)
            .response_id("resp_secretprovider0001"),
    ));
    let sent = h.say(chat, "go").await;
    let done = sent.done();
    assert_eq!(
        done["usage"],
        json!({"input_tokens": 42, "output_tokens": 7})
    );
    assert_eq!(done["effective_model"], json!("gpt-premium"));
    assert_eq!(done["selected_model"], json!("gpt-premium"));
    assert_eq!(done["quota_decision"], json!("allow"));
    assert!(done.get("downgrade_from").is_none_or(Value::is_null));
    assert!(done.get("downgrade_reason").is_none_or(Value::is_null));
    assert!(
        keys(done).is_subset(&key_set(&[
            "usage",
            "effective_model",
            "selected_model",
            "quota_decision",
            "downgrade_from",
            "downgrade_reason",
            "quota_warnings"
        ])),
        "unexpected done keys: {done}"
    );
    assert!(
        !sent.blob().contains("resp_secretprovider0001"),
        "the provider response id never reaches the client"
    );
    // the catalog id, never the provider-side model name
    assert!(!sent.blob().contains("provider-model-"));
}

#[tokio::test]
async fn done_names_the_chat_model_for_a_standard_chat() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat_on("gpt-standard").await;
    let sent = h.say(chat, "go").await;
    assert_eq!(sent.done()["selected_model"], json!("gpt-standard"));
    assert_eq!(sent.done()["effective_model"], json!("gpt-standard"));
    assert_eq!(
        h.oagw.last_request().model(),
        "provider-model-gpt-standard",
        "the provider gets the catalog provider_model_id"
    );
}

#[tokio::test]
async fn an_incomplete_provider_answer_is_a_completed_turn_without_citations() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text_with(
        &["trunc", "ated"],
        &Opts::default()
            .incomplete()
            .usage(9, 2)
            .annotated(vec![stream_support::web_annotation(
                "https://e.example/",
                "t",
                0,
                3,
            )]),
    ));
    let sent = h.say(chat, "go").await;
    sent.assert_well_formed();
    assert_eq!(sent.terminal().event, "done");
    assert_eq!(
        sent.done()["usage"],
        json!({"input_tokens": 9, "output_tokens": 2})
    );
    assert!(
        sent.of("citations").is_empty(),
        "an incomplete response sends no citations"
    );
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert_eq!(turn.state, "completed");
    assert_eq!(
        turn.error_code, None,
        "the incomplete reason is never an error code"
    );
}

#[tokio::test]
async fn a_completion_without_text_is_a_completed_turn_with_an_empty_assistant_message() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text(&[]));
    let sent = h.say(chat, "silent please").await;
    sent.assert_well_formed();
    assert_eq!(sent.names_no_ping(), vec!["stream_started", "done"]);
    let rows = h.messages_in(chat).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].role, "assistant");
    assert_eq!(rows[1].content, "");
    assert_eq!(
        h.turn_of(sent.request_id()).await.unwrap().state,
        "completed"
    );
}

#[tokio::test]
async fn a_slow_reader_applies_backpressure_and_still_receives_every_delta_in_order() {
    let h = StreamHarness::with(StreamOptions {
        config: json!({"streaming": {"sse_channel_capacity": 16}}),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let chunks: Vec<String> = (0..400).map(|i| format!("w{i} ")).collect();
    let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
    h.oagw.push(ProviderReply::text(&refs));

    // open the stream but do not read the body
    let raw = h.open(chat, &ctx_a(), &json!({"content": "flood"})).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        h.oagw.pulled_frames() < 100,
        "the provider was drained {} frames ahead of a reader that reads nothing: the relay \
         channel must be bounded (16..=64)",
        h.oagw.pulled_frames()
    );

    let sent = raw.collect().await;
    sent.assert_well_formed();
    assert_eq!(
        sent.text(),
        chunks.concat(),
        "no delta is lost or reordered"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// persistence
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_completed_turn_persists_user_message_assistant_message_and_turn() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::text_with(
        &["Hello", " world"],
        &Opts::default()
            .usage(42, 7)
            .response_id("resp_persisted000001"),
    ));
    let sent = h.say_with(chat, "Hello there", rid).await;

    let rows = h.messages_in(chat).await;
    assert_eq!(rows.len(), 2, "one user and one assistant message");
    let (user, assistant) = (&rows[0], &rows[1]);

    assert_eq!(user.role, "user");
    assert_eq!(user.content, "Hello there");
    assert_eq!(user.request_id, Some(rid));
    assert_eq!(user.tenant_id, TENANT_A);
    assert_eq!(user.model, None, "user messages carry no model");
    assert!(user.deleted_at.is_none());

    assert_eq!(assistant.role, "assistant");
    assert_eq!(assistant.content, "Hello world");
    assert_eq!(
        assistant.request_id,
        Some(rid),
        "same request_id as the user message"
    );
    assert_eq!(
        assistant.id,
        sent.message_id(),
        "stream_started announced this id"
    );
    assert_eq!(
        assistant.model.as_deref(),
        Some("gpt-premium"),
        "effective model (catalog id)"
    );
    assert_eq!(assistant.input_tokens, 42);
    assert_eq!(assistant.output_tokens, 7);
    assert_eq!(
        assistant.provider_response_id.as_deref(),
        Some("resp_persisted000001"),
        "provider ids stay in the database"
    );
    assert_eq!(assistant.content_type, "text");
    assert_eq!(assistant.request_kind, "chat");
    assert!(
        assistant.created_at > user.created_at,
        "the answer is strictly newer than the question (chronological order)"
    );

    let turn = h.turn_of(rid).await.expect("chat_turns row");
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.chat_id, chat);
    assert_eq!(turn.tenant_id, TENANT_A);
    assert_eq!(turn.requester_type, "user");
    assert_eq!(turn.requester_user_id, Some(USER_A));
    assert_eq!(turn.assistant_message_id, Some(assistant.id));
    assert_eq!(turn.effective_model.as_deref(), Some("gpt-premium"));
    assert_eq!(
        turn.provider_response_id.as_deref(),
        Some("resp_persisted000001")
    );
    assert_eq!(turn.error_code, None);
    assert!(turn.deleted_at.is_none());
    assert!(!turn.web_search_enabled);
    let completed_at = turn
        .completed_at
        .expect("completed_at is set on a terminal turn");
    assert!(completed_at >= turn.started_at);
    assert_eq!(h.all_turns().await.len(), 1);
}

#[tokio::test]
async fn sending_bumps_chat_updated_at_and_counts_two_messages_per_turn() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let before = h.chat.chat_row(chat).await.unwrap();
    assert_eq!(chat_support::from_db(before.updated_at), ts(0));

    h.say(chat, "one").await;
    let after = h.chat.chat_row(chat).await.unwrap();
    assert!(
        after.updated_at > before.updated_at,
        "updated_at is bumped by a send"
    );
    assert_eq!(after.model, "gpt-premium", "the chat model never changes");
    assert_eq!(after.title.as_deref(), Some(""));
    assert!(after.deleted_at.is_none());

    let detail = h.chat.get(&h.chat.chat_uri(chat), &ctx_a()).await;
    assert_eq!(detail.json()["message_count"], json!(2));
    assert!(parse_ts(&detail.json()["updated_at"]) > ts(0));

    h.say(chat, "two").await;
    let detail = h.chat.get(&h.chat.chat_uri(chat), &ctx_a()).await;
    assert_eq!(
        detail.json()["message_count"],
        json!(4),
        "two messages per completed turn"
    );
}

#[tokio::test]
async fn the_message_list_shows_the_turn_in_chronological_order() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text_with(
        &["answer"],
        &Opts::default().usage(11, 3),
    ));
    let sent = h.say(chat, "question").await;

    let items = h.listed_messages(chat).await;
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["role"], json!("user"));
    assert_eq!(items[0]["content"], json!("question"));
    assert_eq!(items[1]["role"], json!("assistant"));
    assert_eq!(items[1]["content"], json!("answer"));
    assert_eq!(items[0]["request_id"], items[1]["request_id"]);
    assert_eq!(items[0]["request_id"], json!(sent.request_id().to_string()));
    assert_eq!(items[1]["id"], json!(sent.message_id().to_string()));
    assert_eq!(items[1]["model"], json!("gpt-premium"));
    assert_eq!(items[1]["input_tokens"], json!(11));
    assert_eq!(items[1]["output_tokens"], json!(3));
    assert!(
        items[0].get("model").is_none(),
        "user messages have no model"
    );
    assert!(items[1]["my_reaction"].is_null());
    assert!(parse_ts(&items[0]["created_at"]) < parse_ts(&items[1]["created_at"]));
}

#[tokio::test]
async fn the_user_message_and_the_running_turn_are_durable_before_the_provider_answers() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text_with(
        &["partial"],
        &Opts::default().pause_after(0),
    ));
    let rid = Uuid::new_v4();
    let mut live = h
        .open(
            chat,
            &ctx_a(),
            &json!({"content": "durable?", "request_id": rid}),
        )
        .await
        .live();
    live.expect("stream_started").await;
    h.oagw.wait_paused(1).await;

    let rows = h.messages_in(chat).await;
    assert_eq!(
        rows.len(),
        1,
        "only the user message exists while the turn runs"
    );
    assert_eq!(rows[0].role, "user");
    assert_eq!(rows[0].content, "durable?");
    let turn = h.turn_of(rid).await.unwrap();
    assert_eq!(turn.state, "running");
    assert!(turn.completed_at.is_none());
    assert!(
        turn.last_progress_at.is_some(),
        "a running turn has a progress timestamp"
    );
    assert!(turn.assistant_message_id.is_none());

    h.oagw.release();
    live.drain().await;
}

#[tokio::test]
async fn a_valid_attachment_is_linked_to_the_user_message() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let doc = h.seed_document(chat, "report.pdf").await;
    let sent = h
        .send(
            chat,
            &json!({"content": "see the doc", "attachment_ids": [doc]}),
        )
        .await;
    sent.assert_well_formed();
    assert_eq!(sent.terminal().event, "done");

    let links = h.message_attachment_links().await;
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].attachment_id, doc);
    assert_eq!(links[0].chat_id, chat);
    let rows = h.messages_in(chat).await;
    assert_eq!(
        links[0].message_id, rows[0].id,
        "linked to the user message"
    );

    let items = h.listed_messages(chat).await;
    let attachments = items[0]["attachments"].as_array().unwrap();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0]["attachment_id"], json!(doc.to_string()));
    assert_eq!(attachments[0]["filename"], json!("report.pdf"));
    assert_eq!(attachments[0]["kind"], json!("document"));
    assert_eq!(attachments[0]["status"], json!("ready"));
    assert!(items[1]["attachments"].as_array().unwrap().is_empty());
}

// ═════════════════════════════════════════════════════════════════════════════
// preflight rejections: JSON problem, no SSE, provider untouched, nothing stored
// ═════════════════════════════════════════════════════════════════════════════

/// Row counts and the provider call counter must not move across `f`.
async fn untouched<F, Fut>(h: &StreamHarness, f: F) -> stream_support::Resp
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = stream_support::Resp>,
{
    let before = h.row_counts().await;
    let resp = f().await;
    assert_eq!(
        resp.content_type().split(';').next().unwrap(),
        "application/problem+json",
        "preflight failures are JSON problems, never SSE: {}",
        resp.raw
    );
    h.oagw.assert_no_provider_call();
    assert_eq!(
        h.row_counts().await,
        before,
        "a rejected request leaves no rows behind"
    );
    resp
}

#[tokio::test]
async fn empty_or_whitespace_content_is_400_empty_content() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    for content in ["", " ", "   \n\t  \r\n", "\u{a0}\u{2003}"] {
        let resp = untouched(&h, || h.reject(chat, json!({"content": content}))).await;
        let problem = assert_400_reason(&resp, "invalid_argument", "EMPTY_CONTENT");
        assert_field_violation(&problem, "content", "EMPTY_CONTENT");
    }
}

#[tokio::test]
async fn bodies_the_json_extractor_refuses_are_422_400_or_415() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let uri = h.stream_uri(chat);

    for payload in [
        "{}",
        r#"{"content": 5}"#,
        r#"{"content": null}"#,
        r#"{"content": "x", "request_id": "not-a-uuid"}"#,
        r#"{"content": "x", "attachment_ids": ["not-a-uuid"]}"#,
        r#"{"content": "x", "attachment_ids": "nope"}"#,
        r#"{"content": "x", "web_search": {"enabled": "yes"}}"#,
        r#"{"content": "x", "web_search": {}}"#,
        "[]",
        "5",
    ] {
        let resp = untouched(&h, || async {
            h.open_raw(&uri, Some(&ctx_a()), Some("application/json"), payload)
                .await
                .into_resp()
                .await
        })
        .await;
        let problem = assert_problem(&resp, 422, "invalid_argument");
        assert_field_violation(&problem, "body", "invalid_json_body");
    }

    let resp = untouched(&h, || async {
        h.open_raw(
            &uri,
            Some(&ctx_a()),
            Some("application/json"),
            r#"{"content": "#,
        )
        .await
        .into_resp()
        .await
    })
    .await;
    let problem = assert_problem(&resp, 400, "invalid_argument");
    assert_field_violation(&problem, "body", "json_syntax_error");

    for content_type in [None, Some("text/plain")] {
        let resp = untouched(&h, || async {
            h.open_raw(&uri, Some(&ctx_a()), content_type, r#"{"content": "x"}"#)
                .await
                .into_resp()
                .await
        })
        .await;
        let problem = assert_problem(&resp, 415, "invalid_argument");
        assert_field_violation(&problem, "body", "missing_json_content_type");
    }
}

#[tokio::test]
async fn an_unknown_foreign_or_deleted_chat_is_404() {
    let h = StreamHarness::new().await;
    let body = json!({"content": "hello"});

    let unknown = untouched(&h, || h.reject(Uuid::new_v4(), body.clone())).await;
    assert_not_found(&unknown, "chat");

    let other_user = h
        .chat
        .seed_chat(ChatSeed::new(TENANT_A, chat_support::USER_A2).at(0))
        .await;
    let resp = untouched(&h, || h.reject(other_user, body.clone())).await;
    assert_not_found(&resp, "chat");

    let other_tenant = h
        .chat
        .seed_chat(ChatSeed::new(TENANT_B, chat_support::USER_B).at(0))
        .await;
    let resp = untouched(&h, || h.reject(other_tenant, body.clone())).await;
    assert_not_found(&resp, "chat");

    let deleted = h.chat.seed_chat(ChatSeed::of_a().deleted()).await;
    let resp = untouched(&h, || h.reject(deleted, body.clone())).await;
    assert_not_found(&resp, "chat");
}

#[tokio::test]
async fn the_chat_of_another_user_stays_untouched_when_the_owner_is_somebody_else() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let raw = h
        .open(chat, &second_user(), &json!({"content": "intruder"}))
        .await;
    let resp = raw.into_resp().await;
    assert_not_found(&resp, "chat");
    let raw = h
        .open(chat, &tenant_b_user(), &json!({"content": "intruder"}))
        .await;
    assert_not_found(&raw.into_resp().await, "chat");
    h.oagw.assert_no_provider_call();
    assert!(h.messages_in(chat).await.is_empty());
}

#[tokio::test]
async fn a_non_uuid_chat_id_is_400_invalid_path_params() {
    let h = StreamHarness::new().await;
    let uri = format!("{}/v1/chats/not-a-uuid/messages:stream", h.chat.prefix);
    let resp = h
        .open_raw(
            &uri,
            Some(&ctx_a()),
            Some("application/json"),
            r#"{"content":"x"}"#,
        )
        .await
        .into_resp()
        .await;
    let problem = assert_problem(&resp, 400, "invalid_argument");
    assert_violation_reason(&problem, "invalid_path_params");
    h.oagw.assert_no_provider_call();
}

#[tokio::test]
async fn a_chat_model_that_left_the_catalog_is_400_invalid_model() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat_on("gpt-standard").await;
    // the policy plugin no longer serves the chat's model
    h.policy.set_catalog(&premium_with(json!({})));
    let resp = untouched(&h, || h.reject(chat, json!({"content": "hello"}))).await;
    let problem = assert_400_reason(&resp, "invalid_argument", "INVALID_MODEL");
    assert_field_violation(&problem, "model", "INVALID_MODEL");
}

#[tokio::test]
async fn invalid_duplicate_foreign_or_not_ready_attachments_are_400_invalid_attachment() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let other_chat = h.seed_chat().await;

    let ready = h.seed_document(chat, "ready.pdf").await;
    let foreign = h.seed_document(other_chat, "other-chat.pdf").await;
    let pending = h
        .chat
        .seed_attachment(chat_support::AttachmentSeed::document(chat, "p.pdf").status("pending"))
        .await;
    let uploaded = h
        .chat
        .seed_attachment(chat_support::AttachmentSeed::document(chat, "u.pdf").status("uploaded"))
        .await;
    let failed = h
        .chat
        .seed_attachment(chat_support::AttachmentSeed::document(chat, "f.pdf").status("failed"))
        .await;
    let deleted = h
        .chat
        .seed_attachment(chat_support::AttachmentSeed::document(chat, "d.pdf").deleted())
        .await;

    let cases: Vec<(&str, Vec<Uuid>)> = vec![
        ("unknown id", vec![Uuid::new_v4()]),
        ("duplicate ids", vec![ready, ready]),
        ("attachment of another chat", vec![foreign]),
        ("pending", vec![pending]),
        ("uploaded (not ready)", vec![uploaded]),
        ("failed", vec![failed]),
        ("soft-deleted", vec![deleted]),
        ("one bad id among good ones", vec![ready, Uuid::new_v4()]),
    ];
    for (name, ids) in cases {
        let resp = untouched(&h, || {
            h.reject(
                chat,
                json!({"content": "with files", "attachment_ids": ids}),
            )
        })
        .await;
        let problem = assert_problem(&resp, 400, "invalid_argument");
        assert_field_violation(&problem, "attachment", "invalid_attachment");
        assert!(
            !resp.raw.contains("provider"),
            "{name}: no provider detail in the problem"
        );
    }
}

#[tokio::test]
async fn an_oversized_message_is_400_input_too_long() {
    let h = StreamHarness::with(StreamOptions {
        models: premium_with(json!({"max_input_tokens": 400})),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let resp = untouched(&h, || h.reject(chat, json!({"content": "x".repeat(4000)}))).await;
    assert_400_reason(&resp, "out_of_range", "INPUT_TOO_LONG");

    // a message below the limit is fine: the input budget is
    // `max_input_tokens - fixed_overhead` = 300 tokens (DESIGN 2.2), and the system
    // prompt (120) plus a 200-byte message (165) fit it
    let sent = h.say(chat, &"y".repeat(200)).await;
    assert_eq!(sent.terminal().event, "done");
}

#[tokio::test]
async fn mandatory_context_that_does_not_fit_is_400_context_budget_exceeded() {
    // the applied output cap alone reaches the context window
    let h = StreamHarness::with(StreamOptions {
        models: premium_with(json!({"context_window": 1000, "max_output_tokens": 4096,
                                    "max_input_tokens": 0})),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let resp = untouched(&h, || h.reject(chat, json!({"content": "hi"}))).await;
    assert_400_reason(&resp, "out_of_range", "CONTEXT_BUDGET_EXCEEDED");

    // the system prompt does not fit the input budget (budget 600 - 100 - 100 = 400 tokens)
    let h = StreamHarness::with(StreamOptions {
        models: premium_with(json!({"context_window": 600, "max_output_tokens": 100,
                                    "max_input_tokens": 0,
                                    "system_prompt": "p".repeat(4000)})),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let resp = untouched(&h, || h.reject(chat, json!({"content": "hi"}))).await;
    assert_400_reason(&resp, "out_of_range", "CONTEXT_BUDGET_EXCEEDED");
}

#[tokio::test]
async fn the_web_search_kill_switch_rejects_a_request_that_enables_web_search() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    h.policy
        .set_kill_switches(json!({"disable_web_search": true}));

    let resp = untouched(&h, || {
        h.reject(
            chat,
            json!({"content": "search", "web_search": {"enabled": true}}),
        )
    })
    .await;
    let problem = assert_problem(&resp, 400, "failed_precondition");
    assert_precondition(&problem, "web_search", "FEATURE_DISABLED");

    // without the flag the turn runs
    for body in [
        json!({"content": "plain"}),
        json!({"content": "explicit off", "web_search": {"enabled": false}}),
        json!({"content": "null", "web_search": null}),
    ] {
        let sent = h.send(chat, &body).await;
        assert_eq!(sent.terminal().event, "done", "{body}");
    }
    assert!(
        h.oagw
            .requests()
            .iter()
            .all(|r| r.tool("web_search").is_none()),
        "the switch never lets web_search reach the provider"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// authorization
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_send_asks_the_pdp_for_send_message_on_the_chat() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    h.say(chat, "hello").await;
    let sent: Vec<_> = h
        .pdp
        .recorded()
        .into_iter()
        .filter(|r| r.action.name == "send_message")
        .collect();
    assert!(!sent.is_empty(), "the PDP was not asked for send_message");
    for req in sent {
        assert_eq!(req.resource.resource_type, CHAT_PDP_TYPE);
        assert_eq!(req.resource.id, Some(chat));
        assert!(req.context.require_constraints);
    }
}

#[tokio::test]
async fn a_pdp_denial_is_403_and_the_provider_is_never_called() {
    let h = StreamHarness::with(StreamOptions {
        pdp: FakePdp::deny("internal-policy-rule-42"),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let resp = untouched(&h, || h.reject(chat, json!({"content": "hello"}))).await;
    let problem = assert_problem(&resp, 403, "permission_denied");
    assert_eq!(problem["context"]["reason"], json!("AUTHZ_DENIED"));
    assert!(!resp.raw.contains("internal-policy-rule-42"));
}

#[tokio::test]
async fn an_unreachable_pdp_is_503_with_retry_after_and_nothing_happens() {
    let h = StreamHarness::with(StreamOptions {
        pdp: FakePdp::unreachable("pdp-transport-secret"),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let resp = untouched(&h, || h.reject(chat, json!({"content": "hello"}))).await;
    assert_problem(&resp, 503, "service_unavailable");
    assert_eq!(
        resp.headers
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("5")
    );
    assert!(!resp.raw.contains("pdp-transport-secret"));
}

#[tokio::test]
async fn a_permit_without_constraints_is_403() {
    let h = StreamHarness::with(StreamOptions {
        pdp: FakePdp::new(PdpMode::AllowWithoutConstraints),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let resp = untouched(&h, || h.reject(chat, json!({"content": "hello"}))).await;
    assert_problem(&resp, 403, "permission_denied");
}

// ═════════════════════════════════════════════════════════════════════════════
// model immutability (2.2 "Model Locked Per Chat")
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn every_turn_uses_the_model_of_the_chat_and_the_chat_model_never_changes() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat_on("gpt-standard").await;

    for i in 0..3 {
        // whatever the catalog default is, and whatever the request says
        let mut models = default_stream_models();
        if i == 1 {
            models[0]["preference"]["is_default"] = json!(false);
            models[1]["preference"]["is_default"] = json!(true);
        }
        h.policy.set_catalog(&models);
        let sent = h
            .send(
                chat,
                &json!({"content": format!("turn {i}"), "model": "gpt-premium"}),
            )
            .await;
        assert_eq!(
            sent.done()["selected_model"],
            json!("gpt-standard"),
            "turn {i}"
        );
        assert_eq!(
            sent.done()["effective_model"],
            json!("gpt-standard"),
            "turn {i}"
        );
        assert_eq!(h.oagw.last_request().model(), "provider-model-gpt-standard");
        assert_eq!(
            h.chat.chat_row(chat).await.unwrap().model,
            "gpt-standard",
            "chats.model is immutable"
        );
    }
    let detail = h.chat.get(&h.chat.chat_uri(chat), &ctx_a()).await;
    assert_eq!(detail.json()["model"], json!("gpt-standard"));
}

// ═════════════════════════════════════════════════════════════════════════════
// registered operations
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_stream_and_turn_operations_are_registered_with_stable_ids_auth_and_license() {
    let h = StreamHarness::new().await;
    for (key, operation_id) in [
        (
            "POST:/mini-chat/v1/chats/{id}/messages:stream",
            "mini_chat.stream_message",
        ),
        (
            "GET:/mini-chat/v1/chats/{id}/turns/{request_id}",
            "mini_chat.get_turn",
        ),
    ] {
        let spec = h
            .chat
            .openapi
            .operation_specs
            .get(key)
            .unwrap_or_else(|| panic!("operation `{key}` is not registered"));
        assert_eq!(spec.operation_id.as_deref(), Some(operation_id), "{key}");
        assert!(spec.authenticated, "{key} requires authentication");
        let license = spec
            .license_requirement
            .as_ref()
            .unwrap_or_else(|| panic!("{key} must require a license feature"));
        assert_eq!(
            license.license_names,
            vec!["gts.cf.core.lic.feat.v1~cf.core.global.base.v1".to_owned()],
            "{key}"
        );
    }
}

#[tokio::test]
async fn the_stream_operation_declares_sse_on_200_and_the_problem_statuses() {
    let h = StreamHarness::new().await;
    let spec = h
        .chat
        .openapi
        .operation_specs
        .get("POST:/mini-chat/v1/chats/{id}/messages:stream")
        .expect("registered");
    let ok = spec
        .responses
        .iter()
        .find(|r| r.status == 200)
        .expect("200 is declared");
    assert_eq!(ok.content_type, "text/event-stream");
    let statuses: Vec<u16> = spec.responses.iter().map(|r| r.status).collect();
    for status in [400, 401, 403, 404, 409, 422, 429, 500, 503] {
        assert!(
            statuses.contains(&status),
            "declares {status}: {statuses:?}"
        );
    }
    assert!(
        spec.request_body.is_some(),
        "the JSON request body is declared"
    );

    let turn = h
        .chat
        .openapi
        .operation_specs
        .get("GET:/mini-chat/v1/chats/{id}/turns/{request_id}")
        .expect("registered");
    let statuses: Vec<u16> = turn.responses.iter().map(|r| r.status).collect();
    for status in [200, 400, 401, 403, 404, 500, 503] {
        assert!(
            statuses.contains(&status),
            "turn status declares {status}: {statuses:?}"
        );
    }
}
