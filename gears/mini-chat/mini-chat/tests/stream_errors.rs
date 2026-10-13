//! Failures after the stream opened: the terminal `error` event, the failed
//! turn, error-message sanitization and the provider-identifier non-exposure
//! invariant.
//!
//! Source of truth: DESIGN 3.3 "Provider Event Translation" (`response.failed`,
//! `error`, provider HTTP error / disconnect, provider 429), "Streaming error
//! codes", "SSE Event Definitions" (`event: error` payload `{code, message}`),
//! "Provider identifier non-exposure invariant" (sanitization markers),
//! 5.7 "Terminal SSE Event Emission Guard" (terminal event only after the
//! finalization committed), 5.9 / 5.8 error-code taxonomy and the Turn Status
//! API (`error` state with `error_code`).

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

use serde_json::json;
use stream_support::{
    Opts, Piece, ProviderReply, Sent, StreamHarness, contains_provider_identifier, ctx_a, ev,
    failed_stream,
};
use uuid::Uuid;

/// Send one message whose provider answer is `reply`; return what the client saw.
async fn failing_turn(h: &StreamHarness, chat: Uuid, reply: ProviderReply) -> Sent {
    h.oagw.push(reply);
    let sent = h
        .send(
            chat,
            &json!({"content": "please fail", "request_id": Uuid::new_v4()}),
        )
        .await;
    sent.assert_well_formed();
    sent
}

fn assert_error_event(sent: &Sent, code: &str) {
    assert_eq!(sent.terminal().event, "error", "{:?}", sent.names());
    let error = sent.error();
    assert_eq!(error["code"], json!(code), "{error}");
    assert!(
        error["message"].as_str().is_some_and(|m| !m.is_empty()),
        "the error carries a human readable message: {error}"
    );
    let mut keys: Vec<&String> = error.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["code", "message"],
        "the payload is exactly {{code, message}}"
    );
    assert!(sent.of("done").is_empty(), "exactly one terminal event");
}

async fn assert_failed_turn(h: &StreamHarness, chat: Uuid, sent: &Sent, code: &str) {
    let turn = h.turn_of(sent.request_id()).await.expect("turn row");
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some(code));
    assert!(
        turn.completed_at.is_some(),
        "a terminal turn has completed_at"
    );
    assert!(
        turn.assistant_message_id.is_none(),
        "no assistant message on a failed turn"
    );

    let rows = h.messages_in(chat).await;
    assert_eq!(
        rows.iter().map(|m| m.role.as_str()).collect::<Vec<_>>(),
        vec!["user"],
        "the user message stays, no assistant message is written"
    );

    let status = h.turn_status(chat, sent.request_id(), &ctx_a()).await;
    assert_eq!(status.status.as_u16(), 200, "{}", status.raw);
    assert_eq!(status.json()["state"], json!("error"));
    assert_eq!(status.json()["error_code"], json!(code));
    assert!(status.json().get("assistant_message_id").is_none());
}

// ── provider / gateway failures ──────────────────────────────────────────────

#[tokio::test]
async fn a_provider_error_status_ends_the_stream_with_provider_error() {
    for status in [400, 401, 403, 404, 500, 502, 503] {
        let h = StreamHarness::new().await;
        let chat = h.seed_chat().await;
        let sent = failing_turn(
            &h,
            chat,
            ProviderReply::http_error(status, "upstream said no"),
        )
        .await;
        assert_eq!(
            sent.names_no_ping(),
            vec!["stream_started", "error"],
            "status {status}"
        );
        assert_error_event(&sent, "provider_error");
        assert_failed_turn(&h, chat, &sent, "provider_error").await;
        assert_eq!(
            h.oagw.request_count(),
            1,
            "the gear does not retry a stream"
        );
    }
}

#[tokio::test]
async fn a_provider_504_with_its_own_body_is_provider_error_not_a_timeout() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let sent = failing_turn(
        &h,
        chat,
        ProviderReply::http_error(504, "gateway timeout upstream"),
    )
    .await;
    assert_error_event(&sent, "provider_error");
    assert_failed_turn(&h, chat, &sent, "provider_error").await;
}

#[tokio::test]
async fn a_gateway_deadline_is_provider_timeout() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let sent = failing_turn(&h, chat, ProviderReply::gateway_timeout()).await;
    assert_error_event(&sent, "provider_timeout");
    assert_failed_turn(&h, chat, &sent, "provider_timeout").await;
}

#[tokio::test]
async fn any_other_gateway_failure_is_provider_error() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let sent = failing_turn(&h, chat, ProviderReply::gateway_unavailable()).await;
    assert_error_event(&sent, "provider_error");
    assert_failed_turn(&h, chat, &sent, "provider_error").await;
}

#[tokio::test]
async fn a_gateway_problem_document_never_reaches_the_client() {
    // OAGW answers 502 with its own RFC 9457 problem (upstream host in
    // `instance`, GTS type URIs): the client gets a fixed generic text.
    for status in [500, 502, 503] {
        let h = StreamHarness::new().await;
        let chat = h.seed_chat().await;
        let sent = failing_turn(&h, chat, ProviderReply::gateway_problem(status)).await;
        assert_error_event(&sent, "provider_error");
        assert_eq!(
            sent.error()["message"],
            json!("The provider is currently unavailable"),
            "status {status}"
        );
        let message = sent.error()["message"].as_str().unwrap().to_owned();
        for raw in [
            "{",
            "gts",
            "openai-internal",
            "/v1/responses",
            "retry_after",
            "Service temporarily",
        ] {
            assert!(!message.contains(raw), "`{raw}` leaked: {message}");
        }
        assert_failed_turn(&h, chat, &sent, "provider_error").await;
        let status_view = h.turn_status(chat, sent.request_id(), &ctx_a()).await;
        assert!(!status_view.raw.contains("gts"), "{}", status_view.raw);
        let list = h.chat.list_messages(chat, "", &ctx_a()).await;
        assert!(!list.raw.contains("openai-internal"), "{}", list.raw);
    }
}

#[tokio::test]
async fn a_gateway_429_stays_rate_limited_and_a_gateway_504_stays_a_timeout() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let sent = failing_turn(&h, chat, ProviderReply::gateway_problem(429)).await;
    assert_error_event(&sent, "rate_limited");
    assert!(!sent.blob().contains("gts"), "{}", sent.blob());

    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let sent = failing_turn(&h, chat, ProviderReply::gateway_problem(504)).await;
    assert_error_event(&sent, "provider_timeout");
    assert!(!sent.blob().contains("gts"), "{}", sent.blob());
}

#[tokio::test]
async fn a_provider_body_without_a_message_is_never_forwarded_raw() {
    let bodies = [
        json!({"type": "gts://internal~x~", "instance": "/host/v1/responses", "status": 500}),
        json!({"error": {"type": "server_error", "code": "gts-internal"}}),
        json!({"error": {"message": ""}, "detail": "raw detail body"}),
        json!([1, 2, 3]),
    ];
    for body in bodies {
        let h = StreamHarness::new().await;
        let chat = h.seed_chat().await;
        let sent = failing_turn(&h, chat, ProviderReply::http_body(500, body.clone())).await;
        assert_error_event(&sent, "provider_error");
        assert_eq!(
            sent.error()["message"],
            json!("The provider returned HTTP 500"),
            "body {body}"
        );
        assert_failed_turn(&h, chat, &sent, "provider_error").await;
    }
}

#[tokio::test]
async fn a_provider_message_is_still_forwarded_sanitized() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let sent = failing_turn(
        &h,
        chat,
        ProviderReply::http_body(400, json!({"message": "bad input, try again"})),
    )
    .await;
    assert_eq!(sent.error()["message"], json!("bad input, try again"));
}

#[tokio::test]
async fn a_provider_429_is_rate_limited() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let sent = failing_turn(&h, chat, ProviderReply::http_error(429, "slow down")).await;
    assert_eq!(sent.names_no_ping(), vec!["stream_started", "error"]);
    assert_error_event(&sent, "rate_limited");
    assert_failed_turn(&h, chat, &sent, "rate_limited").await;
    assert_eq!(
        h.oagw.request_count(),
        1,
        "OAGW and the gear do not retry a 429"
    );
}

#[tokio::test]
async fn a_numeric_retry_after_of_a_429_is_part_of_the_message() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let sent = failing_turn(
        &h,
        chat,
        ProviderReply::http_error_with(429, "slow down", vec![("retry-after", "37".to_owned())]),
    )
    .await;
    assert_error_event(&sent, "rate_limited");
    assert!(
        sent.error()["message"].as_str().unwrap().contains("37"),
        "message: {}",
        sent.error()
    );
}

// ── failures inside the provider stream ──────────────────────────────────────

#[tokio::test]
async fn response_failed_is_a_provider_error_and_partial_text_is_not_stored_as_an_answer() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let sent = failing_turn(
        &h,
        chat,
        ProviderReply::sse(failed_stream(
            "server_error",
            "The model failed",
            &["par", "tial"],
            None,
        )),
    )
    .await;
    assert_eq!(
        sent.names_no_ping(),
        vec!["stream_started", "delta", "delta", "error"],
        "the client saw the deltas before the failure"
    );
    assert_error_event(&sent, "provider_error");
    assert!(
        sent.error()["message"]
            .as_str()
            .unwrap()
            .contains("The model failed")
    );
    assert_failed_turn(&h, chat, &sent, "provider_error").await;
}

#[tokio::test]
async fn response_failed_with_usage_is_still_a_failed_turn() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let sent = failing_turn(
        &h,
        chat,
        ProviderReply::sse(failed_stream(
            "server_error",
            "overloaded",
            &[],
            Some((30, 4)),
        )),
    )
    .await;
    assert_error_event(&sent, "provider_error");
    assert!(
        !sent.blob().contains("\"usage\""),
        "no usage on a failed stream"
    );
    assert_failed_turn(&h, chat, &sent, "provider_error").await;
}

#[tokio::test]
async fn an_error_event_is_a_provider_error_in_every_shape() {
    for (name, piece) in [
        (
            "nested",
            ev(
                "error",
                json!({"type": "error", "error": {"code": "x", "message": "nested boom"}}),
            ),
        ),
        (
            "flat",
            ev(
                "error",
                json!({"type": "error", "code": "x", "message": "flat boom"}),
            ),
        ),
        (
            "unparseable",
            Piece::Raw("event: error\ndata: plain text boom\n\n".to_owned()),
        ),
        (
            "nameless",
            Piece::Raw("data: {\"type\":\"error\",\"message\":\"typed boom\"}\n\n".to_owned()),
        ),
    ] {
        let h = StreamHarness::new().await;
        let chat = h.seed_chat().await;
        let sent = failing_turn(&h, chat, ProviderReply::sse(vec![piece])).await;
        assert_error_event(&sent, "provider_error");
        assert!(
            sent.error()["message"].as_str().unwrap().contains("boom"),
            "{name}: {}",
            sent.error()
        );
        assert_failed_turn(&h, chat, &sent, "provider_error").await;
    }
}

#[tokio::test]
async fn a_stream_that_ends_without_a_terminal_event_is_a_provider_error() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let sent = failing_turn(
        &h,
        chat,
        ProviderReply::text_with(&["half", " an answer"], &Opts::default().no_terminal()),
    )
    .await;
    assert_eq!(
        sent.names_no_ping(),
        vec!["stream_started", "delta", "delta", "error"]
    );
    assert_error_event(&sent, "provider_error");
    assert_failed_turn(&h, chat, &sent, "provider_error").await;
}

#[tokio::test]
async fn a_dropped_provider_connection_is_a_provider_error() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let mut pieces = stream_support::responses_stream(&["he"], &Opts::default().no_terminal());
    pieces.push(Piece::Fail("connection reset by peer".to_owned()));
    let sent = failing_turn(&h, chat, ProviderReply::sse(pieces)).await;
    assert_error_event(&sent, "provider_error");
    assert_failed_turn(&h, chat, &sent, "provider_error").await;
}

// ── sanitization / provider id non-exposure ──────────────────────────────────

const LEAKY: &str = "Request resp_abcdef123456 failed: file-abcdef123456 and vs_abcdef123456 are \
broken, see https://platform.openai.com/docs/errors for details; key sk-abcdefghij1234567 was \
rejected, header Bearer eyJhbGciOi.abc.def";

fn assert_sanitized(text: &str) {
    for raw in [
        "resp_abcdef123456",
        "file-abcdef123456",
        "vs_abcdef123456",
        "platform.openai.com",
        "sk-abcdefghij",
        "eyJhbGciOi",
    ] {
        assert!(!text.contains(raw), "`{raw}` leaked: {text}");
    }
    assert!(
        !contains_provider_identifier(text),
        "identifier pattern leaked: {text}"
    );
}

#[tokio::test]
async fn provider_messages_are_sanitized_in_every_error_path() {
    // (name, reply, the client message is the provider text)
    let replies: Vec<(&str, ProviderReply, bool)> = vec![
        (
            "http error body",
            ProviderReply::http_error(500, LEAKY),
            true,
        ),
        (
            "response.failed",
            ProviderReply::sse(failed_stream("server_error", LEAKY, &[], None)),
            true,
        ),
        (
            "error event",
            ProviderReply::sse(vec![ev(
                "error",
                json!({"type": "error", "message": LEAKY}),
            )]),
            true,
        ),
        (
            "unparseable error event",
            ProviderReply::sse(vec![Piece::Raw(format!("event: error\ndata: {LEAKY}\n\n"))]),
            true,
        ),
        ("429 body", ProviderReply::http_error(429, LEAKY), false),
    ];
    for (name, reply, carries_text) in replies {
        let h = StreamHarness::new().await;
        let chat = h.seed_chat().await;
        let sent = failing_turn(&h, chat, reply).await;
        let message = sent.error()["message"].as_str().unwrap().to_owned();
        assert_sanitized(&message);
        assert_sanitized(&sent.blob());
        if carries_text {
            assert!(message.contains("[provider_id]"), "{name}: {message}");
            assert!(message.contains("[url]"), "{name}: {message}");
            assert!(message.contains("[credential]"), "{name}: {message}");
        }

        // nothing the REST API shows afterwards carries the raw text either
        let status = h.turn_status(chat, sent.request_id(), &ctx_a()).await;
        assert_sanitized(&status.raw);
        let list = h.chat.list_messages(chat, "", &ctx_a()).await;
        assert_sanitized(&list.raw);
    }
}

#[tokio::test]
async fn text_around_the_identifiers_survives_sanitization() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let sent = failing_turn(
        &h,
        chat,
        ProviderReply::http_error(
            500,
            "The server had an error while processing resp_abcdef123456. Try later.",
        ),
    )
    .await;
    let message = sent.error()["message"].as_str().unwrap().to_owned();
    assert!(
        message.contains("The server had an error while processing"),
        "{message}"
    );
    assert!(message.contains("Try later"), "{message}");
    assert!(message.contains("[provider_id]"), "{message}");
}

#[tokio::test]
async fn provider_ids_of_a_successful_turn_never_reach_the_client() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let seeded_doc = h.seed_document(chat, "contract.pdf").await;
    let file_id = h
        .chat
        .attachment_row(seeded_doc)
        .await
        .provider_file_id
        .expect("provider file id");
    h.seed_vector_store(chat, "vs_seededstore0099").await;
    h.oagw.push(ProviderReply::text_with(
        &["answer"],
        &Opts::default()
            .response_id("resp_secretrunid000001")
            .annotated(vec![stream_support::file_annotation(
                &file_id,
                "provider-side-name.pdf",
            )]),
    ));
    let sent = h.say(chat, "question").await;
    assert_eq!(sent.terminal().event, "done");
    for text in [
        sent.blob(),
        h.chat.list_messages(chat, "", &ctx_a()).await.raw,
        h.turn_status(chat, sent.request_id(), &ctx_a()).await.raw,
        h.chat.get(&h.chat.chat_uri(chat), &ctx_a()).await.raw,
    ] {
        for raw in [
            "resp_secretrunid000001",
            file_id.as_str(),
            "vs_seededstore0099",
            "provider-model-",
        ] {
            assert!(!text.contains(raw), "`{raw}` leaked: {text}");
        }
    }
}

// ── terminal event gating and recovery ───────────────────────────────────────

#[tokio::test]
async fn the_terminal_event_is_sent_only_after_the_turn_is_finalized() {
    // done: the completed turn and the assistant message are already committed
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let mut live = h
        .open(
            chat,
            &ctx_a(),
            &json!({"content": "go", "request_id": Uuid::new_v4()}),
        )
        .await
        .live();
    let started = live.expect("stream_started").await;
    let rid = Uuid::parse_str(started.data["request_id"].as_str().unwrap()).unwrap();
    loop {
        let event = live.next_event().await.expect("terminal event");
        if event.event == "done" {
            break;
        }
    }
    let turn = h.turn_of(rid).await.unwrap();
    assert_eq!(turn.state, "completed", "done is emitted after the commit");
    assert!(turn.completed_at.is_some());
    assert_eq!(h.messages_in(chat).await.len(), 2);

    // error: the failed turn is already committed
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::http_error(500, "nope"));
    let mut live = h
        .open(
            chat,
            &ctx_a(),
            &json!({"content": "go", "request_id": Uuid::new_v4()}),
        )
        .await
        .live();
    let started = live.expect("stream_started").await;
    let rid = Uuid::parse_str(started.data["request_id"].as_str().unwrap()).unwrap();
    live.expect("error").await;
    let turn = h.turn_of(rid).await.unwrap();
    assert_eq!(turn.state, "failed", "error is emitted after the commit");
    assert_eq!(turn.error_code.as_deref(), Some("provider_error"));
}

#[tokio::test]
async fn a_failed_turn_releases_the_chat_for_the_next_message() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let first = failing_turn(&h, chat, ProviderReply::http_error(500, "nope")).await;
    assert_eq!(first.terminal().event, "error");

    let second = h.say(chat, "try again").await;
    second.assert_well_formed();
    assert_eq!(second.terminal().event, "done");
    assert_ne!(first.request_id(), second.request_id());
    assert_eq!(h.all_turns().await.len(), 2);
}
