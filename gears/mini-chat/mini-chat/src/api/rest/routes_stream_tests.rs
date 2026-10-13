#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slices 1 to 5, end to end: `POST /v1/chats/{id}/messages:stream` through
//! the router over the real stores and the fake OAGW. The refusals before the
//! stream (as canonical problems, with no side effect), the stream itself with
//! its exact events, the persisted rows and outbox events, replay, the
//! idempotency and parallel-turn matrix and the failures after the stream
//! opened.

use axum::http::StatusCode;
use mini_chat_sdk::{
    BillingOutcome, KillSwitches, SettlementMethod, TerminalState, TurnAuditEventType,
};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::chat_test_env::{CHAT_TYPE, Reply, blob};
use crate::domain::service::quota::test_support::{NO_SWITCHES, design_catalog, model};
use crate::stream_test_env::{Frame, Options, StreamEnv, Streamed, body, oa, qblob, rid};
use crate::test_doubles::{
    FakePdp, LogCapture, PdpReply, USER, USER_TENANT, other_tenant_ctx, other_user_ctx, user_ctx,
};
use mini_chat_sdk::ModelTier;

// ---- assertions on problems ---------------------------------------------------------------------

fn category(reply: &Reply) -> &str {
    reply.json["type"].as_str().unwrap_or_default()
}

fn assert_out_of_range(reply: &Reply, field: &str, reason: &str) {
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert!(category(reply).contains("out_of_range"), "{}", reply.text);
    assert_eq!(
        reply.first_violation(),
        (field.to_owned(), reason.to_owned()),
        "{}",
        reply.text
    );
}

fn assert_precondition(reply: &Reply, subject: &str) {
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert!(
        category(reply).contains("failed_precondition"),
        "{}",
        reply.text
    );
    let violation = &reply.json["context"]["violations"][0];
    assert_eq!(violation["subject"], subject, "{}", reply.text);
    assert_eq!(violation["type"], "FEATURE_DISABLED", "{}", reply.text);
}

fn assert_conflict(reply: &Reply, reason: &str) {
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.text);
    assert!(category(reply).contains("aborted"), "{}", reply.text);
    assert_eq!(reply.json["context"]["reason"], reason, "{}", reply.text);
}

/// The refusal opened no stream, changed nothing and never reached the provider.
async fn assert_refused_cleanly(env: &StreamEnv, before: &str, reply: &Reply) {
    assert_ne!(reply.status, StatusCode::OK, "{}", reply.text);
    assert!(
        reply
            .header("content-type")
            .unwrap_or_default()
            .contains("application/problem+json"),
        "a problem, not a stream: {:?}",
        reply.headers
    );
    assert_eq!(
        env.snapshot().await,
        before,
        "no chat_turns, messages, links or quota change"
    );
    assert_eq!(
        env.oagw.proxy_call_count(),
        0,
        "the provider received nothing"
    );
}

async fn refused(env: &StreamEnv, request: Value) -> Reply {
    let before = env.snapshot().await;
    let reply = env.post(env.chat, &request).await;
    assert_refused_cleanly(env, &before, &reply).await;
    reply
}

// ==== the stream ===================================================================================

#[tokio::test]
async fn a_send_streams_the_answer_as_exact_events() {
    let env = StreamEnv::new().await;
    env.script(&[
        oa::delta("Hello"),
        oa::delta(" world"),
        oa::completed(120, 34),
    ]);
    let request = rid(1);
    let streamed = env.stream(env.chat, &body("Hi there", request)).await;

    assert_eq!(
        streamed.names(),
        ["stream_started", "delta", "delta", "done"]
    );
    let started = &streamed.frames[0].data;
    assert_eq!(started["request_id"], request.to_string());
    assert_eq!(started["is_new_turn"], true);
    assert!(
        started["message_id"]
            .as_str()
            .is_some_and(|id| Uuid::parse_str(id).is_ok())
    );
    assert!(started.get("thread_summary_applied").is_none());
    assert_eq!(
        streamed.frames[1].data,
        json!({ "type": "text", "content": "Hello" })
    );
    assert_eq!(
        streamed.frames[2].data,
        json!({ "type": "text", "content": " world" })
    );
    let mut done = streamed.last().data.clone();
    // The warnings are the quota view: nothing warns here, so no entry is
    // flagged and none carries `next_reset`.
    let warnings = done
        .as_object_mut()
        .unwrap()
        .remove("quota_warnings")
        .expect("present on a completed turn");
    for entry in warnings.as_array().unwrap() {
        assert_eq!(entry["warning"], false, "{entry}");
        assert_eq!(entry["exhausted"], false, "{entry}");
        assert!(entry.get("next_reset").is_none(), "{entry}");
    }
    assert_eq!(
        done,
        json!({
            "usage": { "input_tokens": 120, "output_tokens": 34 },
            "effective_model": "p",
            "selected_model": "p",
            "quota_decision": "allow"
        })
    );
}

#[tokio::test]
async fn a_premium_quota_exhaustion_downgrades_the_turn_and_the_warnings_report_the_threshold() {
    let env = StreamEnv::new().await;
    // DESIGN 5.10: premium 20M of 22M spent today, total 25M of 60M.
    env.quota.seed_design_world(0, false).await;
    env.script(&[oa::delta("x"), oa::completed(10, 5)]);
    // 1000 bytes make the premium reserve 3.75M credits: 23.75M > 22M.
    let streamed = env.stream(env.chat, &body(&"x".repeat(1000), rid(1))).await;
    let done = &streamed.last().data;
    assert_eq!(done["effective_model"], "s");
    assert_eq!(done["selected_model"], "p");
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(done["downgrade_from"], "p");
    assert_eq!(done["downgrade_reason"], "premium_quota_exhausted");
    assert_eq!(
        env.oagw.proxied()[0].json()["model"],
        "provider-s",
        "the effective model is sent"
    );
    let warnings = done["quota_warnings"].as_array().unwrap();
    let premium_daily = warnings
        .iter()
        .find(|w| w["tier"] == "premium" && w["period"] == "daily")
        .expect("the premium daily warning");
    assert_eq!(premium_daily["warning"], true);
    assert!(
        premium_daily["next_reset"].as_str().is_some(),
        "{premium_daily}"
    );
    for entry in warnings {
        assert_eq!(
            entry.get("next_reset").is_some(),
            entry["warning"] == true || entry["exhausted"] == true,
            "next_reset only when warning or exhausted: {entry}"
        );
    }
}

#[tokio::test]
async fn the_frames_are_event_and_data_lines_with_compact_json() {
    let env = StreamEnv::new().await;
    env.script(&[oa::delta("Hello"), oa::completed(1, 2)]);
    let streamed = env.stream(env.chat, &body("Hi", rid(1))).await;
    assert!(
        streamed
            .reply
            .text
            .contains("event: delta\ndata: {\"type\":\"text\",\"content\":\"Hello\"}\n\n"),
        "{}",
        streamed.reply.text
    );
    assert!(
        streamed
            .reply
            .text
            .starts_with("event: stream_started\ndata: {")
    );
    assert!(
        streamed.reply.text.ends_with("}\n\n"),
        "closed right after the terminal event"
    );
}

#[tokio::test]
async fn the_response_headers_are_event_stream_and_no_cache_without_keep_alive_or_compression() {
    let env = StreamEnv::new().await;
    env.script(&[oa::completed(1, 1)]);
    let reply = env.post(env.chat, &body("Hi", rid(1))).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply
            .header("content-type")
            .unwrap()
            .starts_with("text/event-stream")
    );
    assert_eq!(reply.header("cache-control"), Some("no-cache"));
    assert!(reply.header("connection").is_none());
    assert!(reply.header("content-encoding").is_none());
}

#[tokio::test]
async fn a_send_without_a_request_id_gets_a_server_generated_uuid_v4() {
    let env = StreamEnv::new().await;
    env.script(&[oa::completed(1, 1)]);
    let streamed = env.stream(env.chat, &json!({ "content": "Hi" })).await;
    let id = Uuid::parse_str(streamed.frames[0].data["request_id"].as_str().unwrap()).unwrap();
    assert_eq!(id.get_version_num(), 4);
    assert_eq!(env.turn_state_of(id).await, "completed");
}

#[tokio::test]
async fn a_null_request_id_attachments_and_web_search_mean_absent() {
    let env = StreamEnv::new().await;
    env.script(&[oa::completed(1, 1)]);
    let streamed = env
        .stream(
            env.chat,
            &json!({ "content": "Hi", "request_id": null, "attachment_ids": [], "web_search": null }),
        )
        .await;
    assert_eq!(streamed.names(), ["stream_started", "done"]);
}

#[tokio::test]
async fn unknown_body_fields_such_as_model_are_ignored() {
    let env = StreamEnv::new().await;
    env.script(&[oa::completed(1, 1)]);
    let streamed = env
        .stream(
            env.chat,
            &json!({ "content": "Hi", "model": "s", "bogus": [1, 2] }),
        )
        .await;
    assert_eq!(
        streamed.last().data["selected_model"],
        "p",
        "the chat's model, never the body's"
    );
}

#[tokio::test]
async fn the_started_message_id_is_the_persisted_assistant_message() {
    let env = StreamEnv::new().await;
    env.script(&[oa::delta("x"), oa::completed(1, 1)]);
    let request = rid(1);
    let streamed = env.stream(env.chat, &body("Hi", request)).await;
    let message_id =
        Uuid::parse_str(streamed.frames[0].data["message_id"].as_str().unwrap()).unwrap();
    assert_eq!(
        env.turn_col(request, "assistant_message_id").await,
        qblob(message_id)
    );
    let status = env.turn_status(request).await;
    assert_eq!(status.json["state"], "done");
    assert_eq!(status.json["assistant_message_id"], message_id.to_string());
}

// ---- what the provider receives -----------------------------------------------------------------

#[tokio::test]
async fn the_provider_receives_exactly_the_documented_request() {
    let env = StreamEnv::new().await;
    env.script(&[oa::completed(1, 1)]);
    env.stream(env.chat, &body("Hello", rid(1))).await;
    let sent = env.oagw.proxied();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].method, "POST");
    assert_eq!(sent[0].uri, "/api.openai.test/v1/responses");
    assert_eq!(sent[0].header("content-type"), Some("application/json"));
    assert_eq!(sent[0].header("accept"), Some("text/event-stream"));
    assert!(
        sent[0].header("authorization").is_none(),
        "OAGW injects the credential"
    );
    assert_eq!(sent[0].subject, USER, "the request's own security context");
    assert_eq!(sent[0].tenant, USER_TENANT);
    assert_eq!(
        sent[0].json(),
        json!({
            "model": "provider-p",
            "instructions": "SECRET-SYSTEM-PROMPT",
            "input": [ { "role": "user", "content": [ { "type": "input_text", "text": "Hello" } ] } ],
            "max_output_tokens": 500,
            "stream": true,
            "store": false,
            "user": format!("{}{}", USER_TENANT.simple(), USER.simple()),
            "metadata": {
                "tenant_id": USER_TENANT.to_string(),
                "user_id": USER.to_string(),
                "chat_id": env.chat.to_string(),
                "request_type": "chat",
                "feature": "none"
            }
        })
    );
}

#[tokio::test]
async fn the_content_is_sent_to_the_provider_and_stored_exactly_as_given() {
    let env = StreamEnv::new().await;
    env.script(&[oa::completed(1, 1)]);
    env.stream(env.chat, &body("  padded \n", rid(1))).await;
    assert_eq!(
        env.oagw.proxied()[0].json()["input"][0]["content"][0]["text"],
        "  padded \n"
    );
    assert_eq!(env.messages(env.chat).await[0].1, "  padded \n");
}

// ---- persistence and events ---------------------------------------------------------------------

#[tokio::test]
async fn a_completed_turn_persists_the_message_the_turn_and_exactly_one_usage_and_audit_event() {
    let env = StreamEnv::new().await;
    env.script(&[
        oa::delta("Hello"),
        oa::delta(" world"),
        oa::completed(120, 34),
    ]);
    let request = rid(1);
    let streamed = env.stream(env.chat, &body("Hi there", request)).await;
    let message_id =
        Uuid::parse_str(streamed.frames[0].data["message_id"].as_str().unwrap()).unwrap();

    let messages = env.messages(env.chat).await;
    assert_eq!(
        messages,
        vec![
            ("user".to_owned(), "Hi there".to_owned(), None),
            (
                "assistant".to_owned(),
                "Hello world".to_owned(),
                Some("p".to_owned())
            ),
        ]
    );
    for (column, expected) in [
        ("provider_response_id", "'resp_9f3a1c'"),
        ("input_tokens", "120"),
        ("output_tokens", "34"),
        ("cache_read_input_tokens", "3"),
        ("cache_write_input_tokens", "0"),
        ("reasoning_tokens", "2"),
        ("model", "'p'"),
    ] {
        assert_eq!(
            env.assistant_col(request, column).await,
            expected,
            "{column}"
        );
    }
    assert_eq!(env.turn_state_of(request).await, "completed");
    assert_eq!(
        env.turn_col(request, "assistant_message_id").await,
        qblob(message_id)
    );
    assert_eq!(env.turn_text(request, "error_code").await, None);
    assert_ne!(env.turn_col(request, "completed_at").await, "NULL");
    assert_eq!(
        env.count("mini_chat__outbox_body").await,
        2,
        "one usage and one audit message"
    );

    let usage = env.usage_events().await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].terminal_state, TerminalState::Completed);
    assert_eq!(usage[0].billing_outcome, BillingOutcome::Completed);
    assert_eq!(usage[0].settlement_method, SettlementMethod::Actual);
    assert_eq!(usage[0].usage.unwrap().input_tokens, 120);
    assert_eq!(usage[0].effective_model, "p");
    assert_eq!(usage[0].selected_model, "p");

    let audit = env.audit_events().await;
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].event_type, TurnAuditEventType::TurnCompleted);
    assert_eq!(audit[0].tenant_id, USER_TENANT);
    assert_eq!(audit[0].requester_user_id, USER);
    assert_eq!(audit[0].chat_id, env.chat);
    assert_eq!(audit[0].request_id, request);
    assert_eq!(audit[0].selected_model, "p");
    assert_eq!(audit[0].effective_model, "p");
    assert_eq!(
        (audit[0].usage.input_tokens, audit[0].usage.output_tokens),
        (120, 34)
    );
    assert_eq!(audit[0].policy_decisions.quota.decision, "allow");
    assert_eq!(
        (audit[0].prompt.as_str(), audit[0].response.as_str()),
        ("", "")
    );
    assert!(audit[0].attachments.is_empty());
    assert_eq!(audit[0].license, None);
    assert_eq!(audit[0].quota_scope, None);
}

#[tokio::test]
async fn the_quota_is_settled_in_the_same_transaction() {
    let env = StreamEnv::new().await;
    env.script(&[oa::completed(120, 34)]);
    env.stream(env.chat, &body("Hi", rid(1))).await;
    for period in ["daily", "monthly"] {
        let total = env.quota.total(period).await.unwrap();
        assert_eq!(total.reserved, 0, "{period}: the reserve is released");
        assert!(total.spent > 0, "{period}: the usage is charged");
        assert_eq!(env.quota.premium(period).await.unwrap().reserved, 0);
    }
}

#[tokio::test]
async fn the_reserve_is_committed_before_the_provider_answers_and_done_follows_the_commit() {
    let env = StreamEnv::new().await;
    let feed = env.gate();
    let request = rid(1);
    let user_attachment = env.seed_attachment(env.chat, 1, |row| row).await;
    let mut stream = env
        .open(
            env.chat,
            &json!({ "content": "Hi", "request_id": request, "attachment_ids": [user_attachment] }),
        )
        .await;
    assert_eq!(stream.status, StatusCode::OK);
    let started = stream.next_frame().await.unwrap();
    assert_eq!(started.name, "stream_started");

    // The provider has not sent a byte, and everything of the reserve is durable.
    assert_eq!(env.turn_state_of(request).await, "running");
    assert_ne!(env.turn_col(request, "last_progress_at").await, "NULL");
    assert_eq!(
        env.messages(env.chat).await,
        vec![("user".to_owned(), "Hi".to_owned(), None)]
    );
    assert_eq!(env.count("message_attachments").await, 1);
    assert!(env.quota.total("daily").await.unwrap().reserved > 0);
    assert_eq!(env.turn_state_of(request).await, "running");
    let chat_updated = env
        .sql(&format!(
            "SELECT quote(updated_at) FROM chats WHERE id = {}",
            blob(env.chat)
        ))
        .await;
    assert!(chat_updated.contains("2026-03-10"), "{chat_updated}");

    feed.send_event(oa::delta("Hello").0, &oa::delta("Hello").1);
    assert_eq!(stream.next_frame().await.unwrap().name, "delta");
    assert_eq!(
        env.turn_state_of(request).await,
        "running",
        "nothing is committed before the provider ends"
    );
    assert_eq!(
        env.messages(env.chat).await.len(),
        1,
        "the assistant message is written at the end"
    );

    feed.send_event(oa::completed(5, 6).0, &oa::completed(5, 6).1);
    let done = stream.next_frame().await.unwrap();
    assert_eq!(done.name, "done");
    // `done` was observed: the commit is already visible.
    assert_eq!(env.turn_state_of(request).await, "completed");
    assert_eq!(env.messages(env.chat).await.len(), 2);
    assert_eq!(env.usage_events().await.len(), 1);
    assert_eq!(env.audit_events().await.len(), 1);
    assert!(
        stream.next_frame().await.is_none(),
        "closed right after done"
    );
}

#[tokio::test]
async fn a_finalization_that_fails_before_the_commit_sends_finalization_failed_and_leaves_the_turn_running()
 {
    let env = StreamEnv::with(Options {
        audit_enqueue_fails: true,
        ..Options::default()
    })
    .await;
    env.script(&[oa::delta("Hello"), oa::completed(5, 6)]);
    let request = rid(1);
    let streamed = env.stream(env.chat, &body("Hi", request)).await;
    assert_eq!(streamed.names(), ["stream_started", "delta", "error"]);
    assert_eq!(streamed.last().data["code"], "finalization_failed");
    assert!(
        !streamed.names().contains(&"done"),
        "done never precedes the commit"
    );
    assert_eq!(
        env.turn_state_of(request).await,
        "running",
        "left for the orphan watchdog"
    );
    assert_eq!(
        env.messages(env.chat).await.len(),
        1,
        "no assistant message"
    );
    assert!(env.usage_events().await.is_empty());
    assert!(env.audit_events().await.is_empty());
    assert!(
        env.quota.total("daily").await.unwrap().reserved > 0,
        "the reserve is still held"
    );
}

// ---- tool-free completion variants --------------------------------------------------------------

#[tokio::test]
async fn a_completion_without_any_delta_is_a_completed_turn_with_an_empty_message() {
    let env = StreamEnv::new().await;
    env.script(&[oa::completed(5, 0)]);
    let request = rid(1);
    let streamed = env.stream(env.chat, &body("Hi", request)).await;
    assert_eq!(streamed.names(), ["stream_started", "done"]);
    assert_eq!(env.turn_state_of(request).await, "completed");
    assert_eq!(
        env.messages(env.chat).await[1],
        ("assistant".to_owned(), String::new(), Some("p".to_owned()))
    );
}

#[tokio::test]
async fn an_incomplete_response_ends_with_done_a_completed_turn_and_no_citations() {
    let env = StreamEnv::new().await;
    env.script(&[
        oa::delta("cut"),
        oa::annotation(json!({ "type": "url_citation", "url": "https://e.org", "title": "T" })),
        oa::incomplete(10, 20, "max_output_tokens"),
    ]);
    let request = rid(1);
    let streamed = env.stream(env.chat, &body("Hi", request)).await;
    assert_eq!(streamed.names(), ["stream_started", "delta", "done"]);
    assert_eq!(
        streamed.last().data["usage"],
        json!({ "input_tokens": 10, "output_tokens": 20 })
    );
    assert_eq!(env.turn_state_of(request).await, "completed");
    assert_eq!(env.turn_text(request, "error_code").await, None);
}

// ==== refusals before the stream ===================================================================

#[tokio::test]
async fn empty_and_whitespace_content_is_empty_content() {
    let env = StreamEnv::new().await;
    for content in ["", "   ", "\n\t"] {
        let reply = refused(&env, body(content, rid(1))).await;
        reply.assert_invalid("content", "EMPTY_CONTENT");
    }
}

#[tokio::test]
async fn empty_content_is_reported_even_on_a_chat_of_another_user() {
    let env = StreamEnv::new().await;
    let foreign = env
        .seed_chat_of(2, USER_TENANT, Uuid::from_u128(0x99), "p")
        .await;
    let before = env.snapshot().await;
    let reply = env.post(foreign, &body("  ", rid(1))).await;
    reply.assert_invalid("content", "EMPTY_CONTENT");
    assert_eq!(env.snapshot().await, before);
    assert!(
        env.quota.pdp.requests().is_empty(),
        "no authorization for a body error"
    );
}

#[tokio::test]
async fn invalid_attachment_ids_are_refused() {
    let env = StreamEnv::new().await;
    let good = env.seed_attachment(env.chat, 1, |row| row).await;
    let other_chat = env.seed_chat(2, "p").await;
    let foreign_chat_attachment = env.seed_attachment(other_chat, 2, |row| row).await;
    let other_uploader = env
        .seed_attachment(env.chat, 3, |row| {
            row.set("uploaded_by_user_id", Uuid::from_u128(0xBEEF))
        })
        .await;
    let deleted = env.seed_attachment(env.chat, 4, |row| row).await;
    env.quota.db.soft_delete("attachments", deleted).await;
    let pending = env
        .seed_attachment(env.chat, 5, |row| row.set("status", "pending"))
        .await;
    let uploaded = env
        .seed_attachment(env.chat, 6, |row| row.set("status", "uploaded"))
        .await;
    let failed = env
        .seed_attachment(env.chat, 7, |row| row.set("status", "failed"))
        .await;
    let cases = [
        ("unknown id", vec![good, Uuid::from_u128(0xDEAD)]),
        ("another chat", vec![foreign_chat_attachment]),
        ("another uploader", vec![other_uploader]),
        ("deleted", vec![deleted]),
        ("pending", vec![pending]),
        ("uploaded", vec![uploaded]),
        ("failed", vec![failed]),
        ("duplicate", vec![good, good]),
    ];
    for (label, ids) in cases {
        let reply = refused(
            &env,
            json!({ "content": "see files", "request_id": rid(1), "attachment_ids": ids }),
        )
        .await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{label}");
        reply.assert_invalid("attachment", "invalid_attachment");
    }
}

#[tokio::test]
async fn more_attachment_ids_than_documents_plus_images_are_refused() {
    let env = StreamEnv::new().await;
    let limits = crate::config::MiniChatConfig::default().rag;
    let count = limits.max_documents_per_chat + limits.max_images_per_message + 1;
    let ids: Vec<Uuid> = (0..count)
        .map(|n| Uuid::from_u128(0x7000 + u128::from(n)))
        .collect();
    let reply = refused(&env, json!({ "content": "x", "attachment_ids": ids })).await;
    reply.assert_invalid("attachment", "invalid_attachment");
}

#[tokio::test]
async fn five_images_are_too_many_images() {
    let env = StreamEnv::new().await;
    let mut ids = Vec::new();
    for n in 1..=5 {
        ids.push(env.seed_image(env.chat, n).await);
    }
    let reply = refused(&env, json!({ "content": "look", "attachment_ids": ids })).await;
    assert_out_of_range(&reply, "image_count", "TOO_MANY_IMAGES");
}

#[tokio::test]
async fn images_under_the_disable_images_kill_switch_are_refused() {
    let env = StreamEnv::with(Options {
        switches: KillSwitches {
            disable_images: true,
            ..NO_SWITCHES
        },
        ..Options::default()
    })
    .await;
    let image = env.seed_image(env.chat, 1).await;
    let reply = refused(
        &env,
        json!({ "content": "look", "attachment_ids": [image] }),
    )
    .await;
    assert_precondition(&reply, "images");
}

#[tokio::test]
async fn images_on_a_model_without_vision_are_refused() {
    let env = StreamEnv::with(Options {
        catalog: vec![
            model("p", ModelTier::Premium).vision(false).build(),
            model("s", ModelTier::Standard).build(),
        ],
        ..Options::default()
    })
    .await;
    let image = env.seed_image(env.chat, 1).await;
    let reply = refused(
        &env,
        json!({ "content": "look", "attachment_ids": [image] }),
    )
    .await;
    reply.assert_invalid("attachment", "VISION_NOT_SUPPORTED");
}

#[tokio::test]
async fn images_after_a_downgrade_from_a_vision_model_are_refused_too() {
    let env = StreamEnv::with(Options {
        catalog: vec![
            model("p", ModelTier::Premium)
                .enabled(false)
                .vision(true)
                .build(),
            model("s", ModelTier::Standard).vision(false).build(),
        ],
        ..Options::default()
    })
    .await;
    let image = env.seed_image(env.chat, 1).await;
    let reply = refused(
        &env,
        json!({ "content": "look", "attachment_ids": [image] }),
    )
    .await;
    reply.assert_invalid("attachment", "VISION_NOT_SUPPORTED");
}

#[tokio::test]
async fn web_search_under_its_kill_switch_is_refused() {
    let env = StreamEnv::with(Options {
        switches: KillSwitches {
            disable_web_search: true,
            ..NO_SWITCHES
        },
        ..Options::default()
    })
    .await;
    let reply = refused(
        &env,
        json!({ "content": "news?", "web_search": { "enabled": true } }),
    )
    .await;
    assert_precondition(&reply, "web_search");
}

#[tokio::test]
async fn a_chat_model_removed_from_the_catalog_is_invalid_model() {
    let env = StreamEnv::new().await;
    let chat = env.seed_chat(2, "gone-model").await;
    let before = env.snapshot().await;
    let reply = env.post(chat, &body("Hi", rid(1))).await;
    assert_refused_cleanly(&env, &before, &reply).await;
    reply.assert_invalid("model", "INVALID_MODEL");
}

fn tight_model(max_input: u32) -> mini_chat_sdk::ModelCatalogEntry {
    let mut entry = model("p", ModelTier::Premium)
        .text_budgets(4, 100, 10)
        .build();
    entry.max_input_tokens = max_input;
    entry
}

#[tokio::test]
async fn a_message_above_max_input_tokens_is_input_too_long_and_one_below_passes() {
    let env = StreamEnv::with(Options {
        catalog: vec![tight_model(1000), model("s", ModelTier::Standard).build()],
        ..Options::default()
    })
    .await;
    let reply = refused(&env, body(&"x".repeat(4000), rid(1))).await;
    assert_out_of_range(&reply, "content", "INPUT_TOO_LONG");
    env.script(&[oa::completed(1, 1)]);
    let streamed = env.stream(env.chat, &body(&"x".repeat(2000), rid(2))).await;
    assert_eq!(streamed.names(), ["stream_started", "done"]);
}

#[tokio::test]
async fn max_input_tokens_zero_lets_the_long_message_through() {
    let env = StreamEnv::with(Options {
        catalog: vec![tight_model(0), model("s", ModelTier::Standard).build()],
        ..Options::default()
    })
    .await;
    env.script(&[oa::completed(1, 1)]);
    let streamed = env.stream(env.chat, &body(&"x".repeat(4000), rid(1))).await;
    assert_eq!(streamed.names(), ["stream_started", "done"]);
}

#[tokio::test]
async fn an_output_limit_that_leaves_no_context_room_is_context_budget_exceeded() {
    let mut tiny = model("p", ModelTier::Premium).build();
    tiny.context_window = 400; // below the 500 output tokens that apply
    let env = StreamEnv::with(Options {
        catalog: vec![tiny, model("s", ModelTier::Standard).build()],
        ..Options::default()
    })
    .await;
    let reply = refused(&env, body("Hi", rid(1))).await;
    assert_out_of_range(&reply, "content", "CONTEXT_BUDGET_EXCEEDED");
}

#[tokio::test]
async fn a_system_prompt_and_message_above_the_budget_are_context_budget_exceeded() {
    let mut small = model("p", ModelTier::Premium).build();
    small.context_window = 600; // 100 left for the input
    small.system_prompt = "S".repeat(80);
    let env = StreamEnv::with(Options {
        catalog: vec![small, model("s", ModelTier::Standard).build()],
        ..Options::default()
    })
    .await;
    let reply = refused(&env, body(&"m".repeat(60), rid(1))).await;
    assert_out_of_range(&reply, "content", "CONTEXT_BUDGET_EXCEEDED");
}

#[tokio::test]
async fn an_exhausted_quota_is_a_429_before_any_provider_call() {
    let env = StreamEnv::new().await;
    env.quota
        .seed_bucket(&crate::quota_test_env::BucketSeed::total(
            "daily", 60_000_000, 0,
        ))
        .await;
    let reply = refused(&env, body("Hi", rid(1))).await;
    assert_eq!(
        reply.status,
        StatusCode::TOO_MANY_REQUESTS,
        "{}",
        reply.text
    );
    assert!(category(&reply).contains("resource_exhausted"));
    assert_eq!(reply.json["context"]["violations"][0]["subject"], "tokens");
}

#[tokio::test]
async fn a_policy_plugin_failure_is_a_500_and_nothing_is_reserved() {
    let env = StreamEnv::new().await;
    env.quota
        .policy
        .fail(crate::domain::service::quota::test_support::PolicyStep::Snapshot);
    let reply = refused(&env, body("Hi", rid(1))).await;
    assert_eq!(
        reply.status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{}",
        reply.text
    );
    assert!(
        !reply.text.contains("Snapshot"),
        "no internal detail: {}",
        reply.text
    );
}

#[tokio::test]
async fn an_unknown_chat_is_a_404_of_the_chat_type() {
    let env = StreamEnv::new().await;
    let before = env.snapshot().await;
    let reply = env.post(Uuid::from_u128(0xDEAD), &body("Hi", rid(1))).await;
    assert_refused_cleanly(&env, &before, &reply).await;
    reply.assert_not_found(CHAT_TYPE);
}

// ---- extractor and gate errors ------------------------------------------------------------------

#[tokio::test]
async fn a_body_that_does_not_match_the_schema_is_422() {
    let env = StreamEnv::new().await;
    for bad in [
        json!({}),
        json!({ "content": 5 }),
        json!({ "content": "x", "request_id": "not-a-uuid" }),
        json!({ "content": "x", "attachment_ids": ["nope"] }),
        json!({ "content": "x", "web_search": { "enabled": "yes" } }),
    ] {
        let before = env.snapshot().await;
        let reply = env.post(env.chat, &bad).await;
        assert_refused_cleanly(&env, &before, &reply).await;
        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{bad}: {}",
            reply.text
        );
        assert_eq!(reply.first_violation().1, "invalid_json_body", "{bad}");
    }
}

#[tokio::test]
async fn malformed_json_is_400_and_a_non_json_content_type_is_415() {
    let env = StreamEnv::new().await;
    let reply = env
        .post_raw(
            &user_ctx(),
            env.chat,
            Some("application/json"),
            Some("{not json".to_owned()),
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert_eq!(reply.first_violation().1, "json_syntax_error");
    let reply = env
        .post_raw(
            &user_ctx(),
            env.chat,
            Some("text/plain"),
            Some("{\"content\":\"x\"}".to_owned()),
        )
        .await;
    assert_eq!(
        reply.status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "{}",
        reply.text
    );
    assert_eq!(reply.first_violation().1, "missing_json_content_type");
    assert_eq!(env.oagw.proxy_call_count(), 0);
}

#[tokio::test]
async fn a_chat_id_that_is_not_a_uuid_is_400_invalid_path_params() {
    let env = StreamEnv::new().await;
    let reply = env
        .post_raw_uri(
            &format!(
                "{}/v1/chats/not-a-uuid/messages:stream",
                crate::stream_test_env::PREFIX
            ),
            &body("Hi", rid(1)),
        )
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.text);
    assert_eq!(reply.first_violation().1, "invalid_path_params");
}

// ---- authorization ------------------------------------------------------------------------------

#[tokio::test]
async fn the_send_is_authorized_as_send_message_on_the_chat_without_read() {
    let env = StreamEnv::new().await;
    env.script(&[oa::completed(1, 1)]);
    env.stream(env.chat, &body("Hi", rid(1))).await;
    let requests = env.quota.pdp.requests();
    let send: Vec<_> = requests
        .iter()
        .filter(|r| r.action.name == "send_message")
        .collect();
    assert_eq!(send.len(), 1);
    assert_eq!(send[0].resource.id, Some(env.chat));
    assert!(
        requests.iter().all(|r| r.action.name != "read"),
        "read is not evaluated"
    );
}

#[tokio::test]
async fn a_pdp_deny_is_403_authz_denied_without_a_stream() {
    let env = StreamEnv::with(Options {
        pdp: PdpReply::Deny,
        ..Options::default()
    })
    .await;
    let reply = refused(&env, body("Hi", rid(1))).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{}", reply.text);
    assert_eq!(reply.json["context"]["reason"], "AUTHZ_DENIED");
}

#[tokio::test]
async fn a_pdp_failure_is_503_with_retry_after_5_without_a_stream() {
    let env = StreamEnv::with(Options {
        pdp: PdpReply::Failure,
        ..Options::default()
    })
    .await;
    let reply = refused(&env, body("Hi", rid(1))).await;
    assert_eq!(
        reply.status,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        reply.text
    );
    assert_eq!(reply.header("retry-after"), Some("5"));
}

#[tokio::test]
async fn a_chat_of_another_user_or_tenant_is_a_404_even_for_a_tenant_only_predicate() {
    let env = StreamEnv::new().await;
    let before = env.snapshot().await;
    // The static authz plugin returns only a tenant predicate: the owner part
    // of the scope is what hides another user's chat.
    let other_user = env
        .post_as(&other_user_ctx(), env.chat, &body("Hi", rid(1)))
        .await;
    other_user.assert_not_found(CHAT_TYPE);
    let other_tenant = env
        .post_as(&other_tenant_ctx(), env.chat, &body("Hi", rid(2)))
        .await;
    other_tenant.assert_not_found(CHAT_TYPE);
    assert_eq!(env.snapshot().await, before);
    assert_eq!(env.oagw.proxy_call_count(), 0);
}

// ==== idempotency, replay and the parallel-turn guard ==============================================

async fn completed_turn(env: &StreamEnv, request: Uuid, content: &str, text: &str) -> Streamed {
    env.script(&[oa::delta(text), oa::completed(120, 34)]);
    env.stream(env.chat, &body(content, request)).await
}

#[tokio::test]
async fn a_completed_request_id_is_replayed_exactly() {
    let env = StreamEnv::new().await;
    let request = rid(1);
    let first = completed_turn(&env, request, "Hi", "stored answer").await;
    let message_id = first.frames[0].data["message_id"].clone();

    let replay = env
        .stream(env.chat, &body("a different text is not compared", request))
        .await;
    assert_eq!(replay.names(), ["stream_started", "delta", "done"]);
    assert_eq!(
        replay.frames[0].data,
        json!({ "request_id": request.to_string(), "message_id": message_id, "is_new_turn": false })
    );
    assert_eq!(
        replay.frames[1].data,
        json!({ "type": "text", "content": "stored answer" })
    );
    assert_eq!(
        replay.frames[2].data,
        json!({
            "usage": { "input_tokens": 120, "output_tokens": 34 },
            "effective_model": "p",
            "selected_model": "p",
            "quota_decision": "allow"
        }),
        "no downgrade_reason, no quota_warnings"
    );
    assert!(replay.of("ping").is_empty());
    assert!(replay.of("citations").is_empty());
}

#[tokio::test]
async fn a_replay_has_no_side_effect_at_all() {
    let env = StreamEnv::new().await;
    let request = rid(1);
    completed_turn(&env, request, "Hi", "answer").await;
    let before = env.snapshot().await;
    let proxied = env.oagw.proxy_call_count();
    let metrics = env.metrics.events().len();
    let quota_metrics = env.quota.metrics.events().len();
    env.stream(env.chat, &body("Hi", request)).await;
    assert_eq!(
        env.snapshot().await,
        before,
        "no quota_usage, outbox, chat_turns or messages change"
    );
    assert_eq!(env.oagw.proxy_call_count(), proxied, "no provider request");
    assert_eq!(env.metrics.events().len(), metrics, "no stream metric");
    assert_eq!(
        env.quota.metrics.events().len(),
        quota_metrics,
        "no quota metric"
    );
}

#[tokio::test]
async fn a_replay_works_after_the_chat_model_was_removed_from_the_catalog() {
    let env = StreamEnv::new().await;
    let request = rid(1);
    completed_turn(&env, request, "Hi", "answer").await;
    env.quota.policy.snapshots.lock().unwrap().insert(
        7,
        crate::domain::service::quota::test_support::snapshot(
            7,
            vec![model("only-other", ModelTier::Standard).build()],
        ),
    );
    let replay = env.stream(env.chat, &body("Hi", request)).await;
    assert_eq!(replay.names(), ["stream_started", "delta", "done"]);
}

#[tokio::test]
async fn a_replay_works_while_another_turn_of_the_chat_is_running() {
    let env = StreamEnv::new().await;
    completed_turn(&env, rid(1), "Hi", "answer").await;
    let feed = env.gate();
    let mut running = env.open(env.chat, &body("second", rid(2))).await;
    running.next_frame().await.unwrap();

    let replay = env.stream(env.chat, &body("Hi", rid(1))).await;
    assert_eq!(
        replay.names(),
        ["stream_started", "delta", "done"],
        "replay before the guard"
    );
    let blocked = env.post(env.chat, &body("third", rid(3))).await;
    assert_conflict(&blocked, "turn_already_running");
    drop(feed);
}

#[tokio::test]
async fn a_replay_of_an_empty_answer_is_still_one_delta() {
    let env = StreamEnv::new().await;
    env.script(&[oa::completed(5, 0)]);
    env.stream(env.chat, &body("Hi", rid(1))).await;
    let replay = env.stream(env.chat, &body("Hi", rid(1))).await;
    assert_eq!(
        replay.frames[1].data,
        json!({ "type": "text", "content": "" })
    );
}

#[tokio::test]
async fn a_replay_after_a_downgrade_rebuilds_downgrade_without_a_reason() {
    let env = StreamEnv::with(Options {
        catalog: vec![
            model("p", ModelTier::Premium).enabled(false).build(),
            model("s", ModelTier::Standard).build(),
        ],
        ..Options::default()
    })
    .await;
    let request = rid(1);
    env.script(&[oa::delta("x"), oa::completed(1, 1)]);
    let first = env.stream(env.chat, &body("Hi", request)).await;
    assert_eq!(first.last().data["quota_decision"], "downgrade");
    assert_eq!(first.last().data["downgrade_reason"], "model_disabled");
    let replay = env.stream(env.chat, &body("Hi", request)).await;
    let done = &replay.last().data;
    assert_eq!(done["effective_model"], "s");
    assert_eq!(done["selected_model"], "p");
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(done["downgrade_from"], "p");
    assert!(done.get("downgrade_reason").is_none());
    assert!(done.get("quota_warnings").is_none());
}

#[tokio::test]
async fn reusing_a_running_failed_cancelled_or_deleted_request_id_is_a_conflict() {
    let env = StreamEnv::new().await;

    // running
    let feed = env.gate();
    let mut live = env.open(env.chat, &body("Hi", rid(1))).await;
    live.next_frame().await.unwrap();
    let before = env.snapshot().await;
    let reply = env.post(env.chat, &body("Hi", rid(1))).await;
    assert_conflict(&reply, "request_id_conflict");
    assert_eq!(env.snapshot().await, before);
    drop(live);
    drop(feed);
    env.wait_terminal(rid(1)).await;

    // cancelled (the disconnect above)
    assert_eq!(env.turn_state_of(rid(1)).await, "cancelled");
    assert_conflict(
        &env.post(env.chat, &body("Hi", rid(1))).await,
        "request_id_conflict",
    );

    // failed
    env.script(&[oa::failed("boom")]);
    env.stream(env.chat, &body("Hi", rid(2))).await;
    assert_eq!(env.turn_state_of(rid(2)).await, "failed");
    assert_conflict(
        &env.post(env.chat, &body("Hi", rid(2))).await,
        "request_id_conflict",
    );

    // completed, then soft-deleted (retried, edited or deleted)
    completed_turn(&env, rid(3), "Hi", "answer").await;
    env.quota
        .db
        .soft_delete("chat_turns", env.turn_id_of(rid(3)).await)
        .await;
    assert_conflict(
        &env.post(env.chat, &body("Hi", rid(3))).await,
        "request_id_conflict",
    );
    assert_eq!(
        env.oagw.proxy_call_count(),
        3,
        "no provider call for a conflict"
    );
}

#[tokio::test]
async fn a_different_request_id_while_a_turn_runs_is_turn_already_running_before_any_quota_step() {
    let env = StreamEnv::new().await;
    let _feed = env.gate();
    let mut live = env.open(env.chat, &body("Hi", rid(1))).await;
    live.next_frame().await.unwrap();
    let before = env.snapshot().await;
    let policy_calls = env.quota.policy.calls().len();
    for request in [
        json!({ "content": "again", "request_id": rid(2) }),
        json!({ "content": "again" }),
    ] {
        let reply = env.post(env.chat, &request).await;
        assert_conflict(&reply, "turn_already_running");
    }
    assert_eq!(env.snapshot().await, before);
    assert_eq!(
        env.quota.policy.calls().len(),
        policy_calls,
        "before any quota step"
    );
    assert_eq!(env.oagw.proxy_call_count(), 1);
}

#[tokio::test]
async fn two_concurrent_sends_with_the_same_new_request_id_make_one_stream_and_one_request_id_conflict()
 {
    let env = StreamEnv::with(Options {
        file_db: true,
        ..Options::default()
    })
    .await;
    let feed = env.gate();
    let request = rid(1);
    let payload = body("Hi", request);
    let (a, b) = tokio::join!(env.open(env.chat, &payload), env.open(env.chat, &payload));
    let mut opened = vec![a, b];
    let statuses = [opened[0].status, opened[1].status];
    assert_eq!(
        statuses.iter().filter(|s| **s == StatusCode::OK).count(),
        1,
        "exactly one stream: {statuses:?}"
    );
    let loser = opened.remove(usize::from(statuses[0] == StatusCode::OK));
    let winner = opened.pop().unwrap();
    let loser = loser.into_reply().await;
    // Whichever unique index fired first for the loser, the answer is the same
    // and it is never `turn_already_running`.
    assert_conflict(&loser, "request_id_conflict");
    assert_eq!(env.count("chat_turns").await, 1);
    assert_eq!(env.count("messages").await, 1, "the loser left no message");
    assert_eq!(env.oagw.proxy_call_count(), 1);
    drop((winner, feed));
}

#[tokio::test]
async fn two_concurrent_first_sends_with_different_request_ids_make_one_stream_and_one_turn_already_running()
 {
    let env = StreamEnv::with(Options {
        file_db: true,
        ..Options::default()
    })
    .await;
    let _feed = env.gate();
    let (first, second) = (body("one", rid(1)), body("two", rid(2)));
    let (a, b) = tokio::join!(env.open(env.chat, &first), env.open(env.chat, &second));
    let mut opened = vec![a, b];
    let statuses = [opened[0].status, opened[1].status];
    assert_eq!(
        statuses.iter().filter(|s| **s == StatusCode::OK).count(),
        1,
        "{statuses:?}"
    );
    let loser = opened.remove(usize::from(statuses[0] == StatusCode::OK));
    assert_conflict(&loser.into_reply().await, "turn_already_running");
    assert_eq!(env.count("chat_turns").await, 1, "the loser left no turn");
    assert_eq!(env.count("messages").await, 1, "and no message");
    let reserved = env.quota.total("daily").await.unwrap().reserved;
    assert_eq!(
        reserved,
        env.quota.premium("daily").await.unwrap().reserved,
        "exactly one reserve"
    );
    assert!(reserved > 0);
}

#[tokio::test]
async fn sends_to_different_chats_of_the_same_user_run_in_parallel() {
    let env = StreamEnv::new().await;
    let other = env.seed_chat(2, "p").await;
    let _first = env.gate();
    let _second = env.gate();
    let mut a = env.open(env.chat, &body("one", rid(1))).await;
    let mut b = env.open(other, &body("two", rid(2))).await;
    assert_eq!(a.next_frame().await.unwrap().name, "stream_started");
    assert_eq!(b.next_frame().await.unwrap().name, "stream_started");
    assert_eq!(env.turn_state_of(rid(1)).await, "running");
    assert_eq!(env.turn_state_of(rid(2)).await, "running");
}

#[tokio::test]
async fn a_new_turn_is_accepted_right_after_a_terminal_event() {
    let env = StreamEnv::new().await;
    completed_turn(&env, rid(1), "Hi", "a").await;
    completed_turn(&env, rid(2), "Hi again", "b").await;
    env.script(&[oa::failed("boom")]);
    env.stream(env.chat, &body("fails", rid(3))).await;
    completed_turn(&env, rid(4), "after the error", "c").await;
    assert_eq!(env.turn_state_of(rid(4)).await, "completed");
}

#[tokio::test]
async fn a_turn_finalized_by_the_watchdog_unblocks_the_chat() {
    let env = StreamEnv::with(Options {
        ping: std::time::Duration::from_millis(40),
        ..Options::default()
    })
    .await;
    let _feed = env.gate();
    let mut live = env.open(env.chat, &body("Hi", rid(1))).await;
    live.next_frame().await.unwrap();
    assert!(matches!(
        env.orphan_finalize(rid(1)).await,
        crate::domain::service::quota::finalize::FinalizeResult::Won(_)
    ));
    completed_turn(&env, rid(2), "next", "ok").await;
    assert_eq!(env.turn_state_of(rid(2)).await, "completed");
}

// ==== failures after the stream opened ============================================================

async fn failed_stream(env: &StreamEnv, events: &[(&str, Value)]) -> (Uuid, Streamed) {
    env.script(events);
    let request = Uuid::new_v4();
    let streamed = env.stream(env.chat, &body("Hi", request)).await;
    (request, streamed)
}

fn only_error(streamed: &Streamed) -> &Frame {
    let errors = streamed.of("error");
    assert_eq!(errors.len(), 1, "exactly one error: {:?}", streamed.names());
    assert_eq!(
        streamed.names().last(),
        Some(&"error"),
        "nothing follows the error"
    );
    errors[0]
}

#[tokio::test]
async fn a_failed_response_is_one_sanitized_provider_error_and_the_turn_fails() {
    let env = StreamEnv::new().await;
    let message = "Invalid file file-abc123def456ghi in resp_9xY2 see https://x.example/a with key sk-ABCDEFGHIJ12";
    let (request, streamed) =
        failed_stream(&env, &[oa::delta("partial"), oa::failed(message)]).await;
    let error = only_error(&streamed);
    assert_eq!(error.data["code"], "provider_error");
    let text = error.data["message"].as_str().unwrap();
    for placeholder in ["[provider_id]", "[url]", "[credential]"] {
        assert!(text.contains(placeholder), "{text}");
    }
    for raw in [
        "file-abc123def456ghi",
        "resp_9xY2",
        "https://x.example/a",
        "sk-ABCDEFGHIJ12",
    ] {
        assert!(!text.contains(raw), "{raw} in {text}");
    }
    assert_eq!(env.turn_state_of(request).await, "failed");
    assert_eq!(
        env.turn_text(request, "error_code").await.as_deref(),
        Some("provider_error")
    );
    let detail = env.turn_text(request, "error_detail").await.unwrap();
    assert!(detail.starts_with("invalid_request_error: "), "{detail}");
    for raw in [
        "file-abc123def456ghi",
        "resp_9xY2",
        "x.example",
        "ABCDEFGHIJ12",
    ] {
        assert!(!detail.contains(raw), "{raw} in {detail}");
    }
    assert_eq!(
        env.messages(env.chat).await.len(),
        1,
        "no assistant message after a provider error"
    );
    assert_eq!(env.usage_events().await.len(), 1);
    assert_eq!(
        env.audit_events().await[0].event_type,
        TurnAuditEventType::TurnFailed
    );
    assert_eq!(
        env.turn_status(request).await.json["error_code"],
        "provider_error"
    );
}

#[tokio::test]
async fn a_failed_turn_settles_on_the_usage_the_provider_reported() {
    let env = StreamEnv::new().await;
    failed_stream(&env, &[oa::failed("boom")]).await;
    let usage = env.usage_events().await;
    assert_eq!(usage[0].terminal_state, TerminalState::Failed);
    assert_eq!(usage[0].billing_outcome, BillingOutcome::Failed);
    assert_eq!(
        usage[0].settlement_method,
        SettlementMethod::Actual,
        "usage was reported"
    );
    assert_eq!(usage[0].usage.unwrap().input_tokens, 11);
}

#[tokio::test]
async fn the_sanitizer_patterns_reach_the_client_and_the_error_detail_replaced() {
    let env = StreamEnv::new().await;
    for (raw, secret) in [
        ("bad id req_7f3a9b2c1d here", "req_7f3a9b2c1d"),
        (
            "bad key sk-proj-AbC_12-xyZ987 here",
            "sk-proj-AbC_12-xyZ987",
        ),
        (
            "bad request 3f2b8c1e-9a4d-4e7f-b6a1-0c5d2e8f9a7b here",
            "3f2b8c1e-9a4d-4e7f-b6a1-0c5d2e8f9a7b",
        ),
        ("denied for bearer abcDEF123 here", "abcDEF123"),
        ("see x-request-id: abc123 here", "abc123"),
        (
            "see https://api.example.com/v1?token=secret42 here",
            "secret42",
        ),
    ] {
        let (request, streamed) = failed_stream(&env, &[oa::failed(raw)]).await;
        let message = only_error(&streamed).data["message"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(!message.contains(secret), "`{secret}` in `{message}`");
        let detail = env.turn_text(request, "error_detail").await.unwrap();
        assert!(!detail.contains(secret), "`{secret}` in `{detail}`");
    }
}

#[tokio::test]
async fn ordinary_words_survive_the_sanitizer() {
    let env = StreamEnv::new().await;
    let raw = "file-based search uses file_search and step-by-step logic";
    let (_, streamed) = failed_stream(&env, &[oa::failed(raw)]).await;
    assert_eq!(only_error(&streamed).data["message"], raw);
}

#[tokio::test]
async fn an_id_spanning_the_cut_leaves_no_fragment_in_the_error_detail() {
    let env = StreamEnv::new().await;
    let message = format!(
        "{} 3f2b8c1e-9a4d-4e7f-b6a1-0c5d2e8f9a7b trailing",
        "x".repeat(1010)
    );
    let (request, _) = failed_stream(&env, &[oa::failed(&message)]).await;
    let detail = env.turn_text(request, "error_detail").await.unwrap();
    assert!(detail.chars().count() <= 1024, "{}", detail.len());
    for fragment in ["3f2b8c1e", "9a4d", "0c5d2e8f9a7b"] {
        assert!(!detail.contains(fragment), "{fragment}");
    }
}

#[tokio::test]
async fn an_error_event_of_the_provider_is_a_provider_error() {
    let env = StreamEnv::new().await;
    let (request, streamed) =
        failed_stream(&env, &[oa::error_event("upstream connect error")]).await;
    let error = only_error(&streamed);
    assert_eq!(error.data["code"], "provider_error");
    assert_eq!(error.data["message"], "upstream connect error");
    assert_eq!(env.turn_state_of(request).await, "failed");
}

#[tokio::test]
async fn a_provider_429_is_rate_limited_with_the_retry_after_seconds() {
    let env = StreamEnv::new().await;
    env.oagw.push_proxy(
        crate::test_doubles::ProxyReply::json(429, &json!({ "error": { "message": "slow down" } }))
            .header("retry-after", "7")
            .from_upstream(),
    );
    let request = rid(1);
    let streamed = env.stream(env.chat, &body("Hi", request)).await;
    assert_eq!(streamed.names(), ["stream_started", "error"]);
    assert_eq!(streamed.last().data["code"], "rate_limited");
    assert!(
        streamed.last().data["message"]
            .as_str()
            .unwrap()
            .contains("7 seconds")
    );
    assert_eq!(
        env.turn_text(request, "error_code").await.as_deref(),
        Some("rate_limited")
    );
}

#[tokio::test]
async fn a_gateway_504_is_a_provider_timeout() {
    let env = StreamEnv::new().await;
    env.oagw.push_proxy(
        crate::test_doubles::ProxyReply::json(
            504,
            &json!({ "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.deadline_exceeded.v1~", "status": 504 }),
        )
        .from_gateway(),
    );
    let request = rid(1);
    let streamed = env.stream(env.chat, &body("Hi", request)).await;
    assert_eq!(streamed.last().data["code"], "provider_timeout");
    assert_eq!(
        env.turn_text(request, "error_code").await.as_deref(),
        Some("provider_timeout")
    );
}

#[tokio::test]
async fn a_gateway_deadline_error_of_the_call_is_a_provider_timeout() {
    let env = StreamEnv::new().await;
    env.oagw.push_proxy(crate::test_doubles::ProxyReply::Error(
        crate::test_doubles::Fault::DeadlineExceeded,
    ));
    let streamed = env.stream(env.chat, &body("Hi", rid(1))).await;
    assert_eq!(streamed.last().data["code"], "provider_timeout");
}

#[tokio::test]
async fn a_stream_that_ends_without_a_terminal_event_is_a_provider_error() {
    let env = StreamEnv::new().await;
    let (request, streamed) = failed_stream(&env, &[oa::delta("cut off")]).await;
    assert_eq!(streamed.names(), ["stream_started", "delta", "error"]);
    assert_eq!(streamed.last().data["code"], "provider_error");
    assert_eq!(env.turn_state_of(request).await, "failed");
}

#[tokio::test]
async fn the_third_web_search_start_fails_the_turn_with_web_search_calls_exceeded() {
    let env = StreamEnv::new().await;
    let (request, streamed) = failed_stream(
        &env,
        &[
            oa::web_start(),
            oa::web_done(),
            oa::web_start(),
            oa::web_done(),
            oa::web_start(),
            oa::delta("never"),
            oa::completed(1, 1),
        ],
    )
    .await;
    assert_eq!(
        streamed.names(),
        ["stream_started", "tool", "tool", "tool", "tool", "error"],
        "the breaching start is not relayed"
    );
    assert_eq!(streamed.last().data["code"], "web_search_calls_exceeded");
    assert_eq!(
        env.turn_text(request, "error_code").await.as_deref(),
        Some("web_search_calls_exceeded")
    );
    assert_eq!(env.usage_events().await[0].web_search_calls, 2);
}

#[tokio::test]
async fn a_function_call_without_knowledge_search_is_unexpected_tool_use() {
    let env = StreamEnv::new().await;
    let (request, streamed) = failed_stream(
        &env,
        &[(
            "response.completed",
            json!({ "response": { "id": "r", "usage": { "input_tokens": 9, "output_tokens": 1 },
                "output": [ { "type": "function_call", "call_id": "c1", "name": "search_knowledge", "arguments": "{}" } ] } }),
        )],
    )
    .await;
    assert_eq!(streamed.last().data["code"], "unexpected_tool_use");
    assert_eq!(
        env.turn_text(request, "error_code").await.as_deref(),
        Some("unexpected_tool_use")
    );
}

#[tokio::test]
async fn every_failure_is_one_sse_error_and_never_a_problem() {
    let env = StreamEnv::new().await;
    for events in [
        vec![oa::failed("a")],
        vec![oa::error_event("b")],
        vec![oa::delta("c")],
    ] {
        let (_, streamed) = failed_stream(&env, &events).await;
        assert_eq!(streamed.reply.status, StatusCode::OK);
        assert!(
            streamed
                .reply
                .header("content-type")
                .unwrap()
                .starts_with("text/event-stream")
        );
        only_error(&streamed);
    }
}

#[allow(dead_code)]
fn unused(_: &FakePdp, _: &LogCapture, _: &[mini_chat_sdk::ModelCatalogEntry]) {
    let _ = design_catalog;
}
