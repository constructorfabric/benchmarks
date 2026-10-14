//! `PATCH /v1/chats/{id}/turns/{request_id}` end to end over the real router, SQLite, the real
//! quota service, the real provider task and a mock OAGW: the body validation before admission
//! (415, `json_syntax_error`, 422, `EMPTY_CONTENT`, `invalid_path_params`), the content kept
//! exactly as sent, unknown body fields ignored (the client chooses neither the new `request_id`
//! nor the attachments), the same stream, settlement and audit contract as retry with a
//! `turn_edit` event, the single PDP evaluation with the `edit_turn` action, and the preview and
//! preflight rejections of the retry flow applying to the edited content.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: flow
//! `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-edit-turn` (`inst-edit-request`,
//! `inst-edit-validate`, `inst-edit-invalid`, `inst-edit-return-invalid`, `inst-edit-admission`,
//! `inst-edit-refused`, `inst-edit-return-refusal`, `inst-edit-preview`, `inst-edit-preflight`,
//! `inst-edit-new-request-id`, `inst-edit-commit`, `inst-edit-stream`, `inst-edit-return`);
//! algorithms `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-edit-request-validation`
//! (`inst-ev-path`, `inst-ev-return-path`, `inst-ev-content-type`, `inst-ev-return-415`,
//! `inst-ev-json-syntax`, `inst-ev-return-syntax`, `inst-ev-schema`, `inst-ev-return-422`,
//! `inst-ev-empty`, `inst-ev-return-empty`, `inst-ev-content-only`, `inst-ev-content-kept`,
//! `inst-ev-return`) and `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight`;
//! definition of done `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-edit`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use axum::http::StatusCode;
use mini_chat_sdk::AuditEvent;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::test_support::chats::{
    a1, a2, assert_chat_not_found, assert_invalid_argument, assert_problem, request,
    request_with_body, violation,
};
use crate::test_support::mutations::{
    assert_authz_denied, assert_conflict, assert_not_found_of, assert_state_violation,
    assert_untouched, current_of, history_of, live_messages, mutation_events, replacement_of, seed,
    snapshot, sse_of, started_ids, streaming_chat, tool_of, turn, turn_by_request,
};
use crate::test_support::provider::{MockProvider, Reply, happy_steps};
use crate::test_support::send::{
    AttachmentSeed, SendRigOptions, TURN_RESOURCE, byte_budgets, catalog_of, link_rows, names,
    prem_model, seed_att, send_rig, send_rig_with, std_model, turn_uri,
};
use crate::test_support::{Pdp, USER_A1, USER_A2};

fn quiet_upstream() -> Arc<MockProvider> {
    MockProvider::answering(Reply::Sse(happy_steps(&["ok"], (50, 5))))
}

fn edited(content: &str) -> Value {
    json!({ "content": content })
}

// ── the body validation, before admission ────────────────────────────────────

#[tokio::test]
async fn a_body_without_a_json_content_type_is_a_415_missing_json_content_type() {
    // inst-ev-content-type, inst-ev-return-415
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
    ] {
        let reply = rig
            .edit_raw(
                chat.id,
                target.request_id,
                content_type,
                r#"{"content":"hi"}"#,
            )
            .await;
        assert_invalid_argument(&reply, 415, "body", "missing_json_content_type");
    }
    assert!(
        rig.pdp.requests().is_empty(),
        "validation precedes the PDP call"
    );
}

#[tokio::test]
async fn malformed_json_is_a_400_json_syntax_error() {
    // inst-ev-json-syntax, inst-ev-return-syntax
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    for body in ["{", r#"{"content": }"#, "not json", r#"{"content":"x""#] {
        let reply = rig
            .edit_raw(chat.id, target.request_id, Some("application/json"), body)
            .await;
        assert_invalid_argument(&reply, 400, "body", "json_syntax_error");
    }
    assert!(rig.pdp.requests().is_empty());
}

#[tokio::test]
async fn a_body_that_does_not_match_the_schema_is_a_422_invalid_json_body() {
    // inst-ev-schema, inst-ev-return-422: no `content`, or `content` not a string
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    for body in [
        json!({}),
        json!({ "text": "hi" }),
        json!({ "content": 5 }),
        json!({ "content": null }),
        json!({ "content": ["a"] }),
        json!([]),
        json!("just a string"),
    ] {
        let reply = rig.edit(chat.id, target.request_id, &body).await;
        let problem = assert_invalid_argument(&reply, 422, "body", "invalid_json_body");
        assert_eq!(
            violation(&problem, "body")["reason"],
            "invalid_json_body",
            "{body}"
        );
    }
    assert!(rig.pdp.requests().is_empty());
}

#[tokio::test]
async fn an_empty_or_whitespace_only_content_is_a_400_empty_content() {
    // inst-ev-empty, inst-ev-return-empty: field `content`
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    for content in ["", " ", "   ", "\t\n", "\u{00a0}"] {
        let reply = rig.edit(chat.id, target.request_id, &edited(content)).await;
        assert_invalid_argument(&reply, 400, "content", "EMPTY_CONTENT");
    }
    assert!(rig.pdp.requests().is_empty());
    assert_eq!(snapshot(&rig.quota.db, &chat).await, before);
}

#[tokio::test]
async fn a_non_uuid_path_is_a_400_invalid_path_params_before_the_body_is_read() {
    // inst-ev-path, inst-ev-return-path
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let good = Uuid::new_v4();
    for uri in [
        format!("/mini-chat/v1/chats/nope/turns/{good}"),
        format!("/mini-chat/v1/chats/{}/turns/nope", chat.id),
    ] {
        // even a body that would be a 415 or a 422 answers the path problem first
        for (content_type, body) in [
            (Some("application/json"), r#"{"content":"hi"}"#),
            (None, r#"{"content":"hi"}"#),
            (Some("application/json"), "{}"),
        ] {
            let reply = rig
                .send(request_with_body("PATCH", &uri, &a1(), content_type, body))
                .await;
            let problem = assert_problem(&reply, 400, "invalid_argument");
            let violations = problem["context"]["field_violations"].as_array().unwrap();
            assert!(
                violations
                    .iter()
                    .any(|v| v["reason"] == "invalid_path_params"),
                "{uri}: {problem}"
            );
        }
    }
    assert!(rig.pdp.requests().is_empty());
}

#[tokio::test]
async fn the_validation_does_not_depend_on_the_existence_of_the_chat_or_the_turn() {
    // inst-ev-return: these checks run before the PEP call and before the chat is loaded
    let rig = send_rig().await;
    let reply = rig
        .edit(Uuid::new_v4(), Uuid::new_v4(), &edited("  "))
        .await;
    assert_invalid_argument(&reply, 400, "content", "EMPTY_CONTENT");
    let reply = rig
        .edit_raw(Uuid::new_v4(), Uuid::new_v4(), None, "{}")
        .await;
    assert_invalid_argument(&reply, 415, "body", "missing_json_content_type");
    assert!(rig.pdp.requests().is_empty());
    // a valid body for an unknown chat is the 404 of admission
    let reply = rig
        .edit(Uuid::new_v4(), Uuid::new_v4(), &edited("fine"))
        .await;
    assert_chat_not_found(&reply);
}

// ── the stream ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_edit_streams_like_a_retry_with_a_new_request_id_and_the_content_exactly_as_sent() {
    // inst-edit-new-request-id, inst-edit-stream, inst-ev-content-kept
    let (rig, chat, upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("old text").answer("old answer")).await;
    let content = "  edited with spaces \n and a break  ";
    let reply = rig.edit(chat.id, target.request_id, &edited(content)).await;
    let frames = sse_of(&reply);
    assert_eq!(
        names(&frames),
        vec!["stream_started", "delta", "delta", "done"],
        "{}",
        reply.text
    );
    let (new_request, message_id) = started_ids(&frames);
    assert_ne!(new_request, target.request_id);
    assert_eq!(new_request.get_version_num(), 4);
    assert_eq!(frames[0].data["is_new_turn"], true);
    rig.wait_turn_terminal(chat.id, new_request).await;

    // the content reached the provider and the store unchanged
    assert_eq!(
        current_of(&upstream.only_call().body),
        &json!({ "role": "user", "content": content })
    );
    let live = live_messages(&rig.quota.db, chat.id).await;
    assert_eq!(live.len(), 2);
    assert_eq!(live[0].content, content);
    assert_eq!(live[0].request_id, Some(new_request));
    assert_eq!(live[1].id, message_id);
    let items = rig.list_messages(chat.id).await;
    assert_eq!(items[0]["content"], content);
    assert!(
        !items
            .iter()
            .any(|i| i["content"] == "old text" || i["content"] == "old answer")
    );
}

#[tokio::test]
async fn after_an_edit_the_previous_turn_is_replaced_and_one_turn_edit_event_is_enqueued() {
    // inst-tv-to-replaced, the `turn_edit` audit event next to the replacement's own events
    let (rig, chat, _upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("old")).await;
    let reply = rig.edit(chat.id, target.request_id, &edited("new")).await;
    let (new_request, _) = started_ids(&sse_of(&reply));
    rig.wait_turn_terminal(chat.id, new_request).await;

    let old = turn_by_request(&rig.quota.db, chat.id, target.request_id).await;
    assert!(old.deleted_at.is_some());
    assert_eq!(old.replaced_by_request_id, Some(new_request));
    assert_eq!(
        replacement_of(&rig.quota.db, chat.id, target.request_id)
            .await
            .request_id,
        new_request
    );
    let events = mutation_events(&rig).await;
    assert_eq!(events.len(), 1, "{events:?}");
    let AuditEvent::TurnEdit(edit) = &events[0] else {
        panic!("expected turn_edit, got {events:?}");
    };
    assert_eq!(edit.actor_user_id, USER_A1);
    assert_eq!(edit.chat_id, chat.id);
    assert_eq!(edit.original_request_id, target.request_id);
    assert_eq!(edit.new_request_id, new_request);
    let wire = serde_json::to_string(&events[0]).unwrap();
    assert!(
        !wire.contains("content"),
        "no message text in the event: {wire}"
    );
    assert_eq!(
        rig.events().await.len(),
        1,
        "one usage event, no separate settlement path"
    );
}

#[tokio::test]
async fn an_edit_proxies_the_edited_content_after_the_history_before_the_turn() {
    let (rig, chat, upstream) = streaming_chat().await;
    seed(
        &rig.quota.db,
        &chat,
        turn("first").at(10).answer("first answer"),
    )
    .await;
    let second = seed(&rig.quota.db, &chat, turn("second").at(100).answer("old")).await;
    let reply = rig
        .edit(chat.id, second.request_id, &edited("second, rephrased"))
        .await;
    sse_of(&reply);
    let call = upstream.only_call();
    assert_eq!(
        history_of(&call.body),
        vec![
            ("user".to_owned(), "first".to_owned()),
            ("assistant".to_owned(), "first answer".to_owned())
        ]
    );
    assert_eq!(
        current_of(&call.body),
        &json!({ "role": "user", "content": "second, rephrased" })
    );
}

#[tokio::test]
async fn unknown_body_fields_are_ignored_the_client_chooses_neither_request_id_nor_attachments() {
    // inst-ev-content-only
    let (rig, chat, upstream) = streaming_chat().await;
    let db = &rig.quota.db;
    let original = seed_att(db, &chat, AttachmentSeed::ready("document")).await;
    let foreign = seed_att(db, &chat, AttachmentSeed::ready("document")).await;
    let target = seed(db, &chat, turn("old").attach(original, 10)).await;
    let wanted_id = Uuid::new_v4();
    let reply = rig
        .edit(
            chat.id,
            target.request_id,
            &json!({
                "content": "new",
                "request_id": wanted_id.to_string(),
                "attachment_ids": [foreign.to_string()],
                "web_search": { "enabled": true },
                "model": "premium-1",
            }),
        )
        .await;
    let frames = sse_of(&reply);
    let (new_request, _) = started_ids(&frames);
    assert_ne!(
        new_request, wanted_id,
        "the new request id is server-generated"
    );
    rig.wait_turn_terminal(chat.id, new_request).await;
    let replacement = replacement_of(db, chat.id, target.request_id).await;
    assert_eq!(replacement.request_id, new_request);
    assert!(
        !replacement.web_search_enabled,
        "the client cannot change the flag"
    );
    assert!(tool_of(&upstream.only_call().body, "web_search").is_none());
    let new_message = live_messages(db, chat.id).await.remove(0);
    let linked: Vec<Uuid> = link_rows(db, chat.id)
        .await
        .into_iter()
        .filter(|l| l.message_id == new_message.id)
        .map(|l| l.attachment_id)
        .collect();
    assert_eq!(
        linked,
        vec![original],
        "only the original attachments are carried"
    );
}

#[tokio::test]
async fn an_edit_with_the_same_content_is_accepted() {
    let (rig, chat, _upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("same")).await;
    let reply = rig.edit(chat.id, target.request_id, &edited("same")).await;
    assert_eq!(names(&sse_of(&reply)).last(), Some(&"done"));
}

// ── authorization ────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_edit_evaluates_the_pdp_exactly_once_with_the_edit_action() {
    // inst-edit-admission
    let (rig, chat, _upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    sse_of(&rig.edit(chat.id, target.request_id, &edited("x")).await);
    let request = rig.pdp.only_request();
    assert_eq!(request.action.name, "edit_turn");
    assert_eq!(request.resource.id, Some(chat.id));
}

#[tokio::test]
async fn a_pdp_deny_and_a_pdp_failure_are_403_and_503_before_any_write() {
    // inst-edit-refused, inst-edit-return-refusal
    for (pdp, status) in [(Pdp::Deny, 403), (Pdp::Unreachable, 503)] {
        let upstream = quiet_upstream();
        let rig = send_rig_with(
            SendRigOptions::new()
                .with_pdp(pdp)
                .with_upstream(upstream.clone()),
        )
        .await;
        let chat = rig.std_chat().await;
        let target = seed(&rig.quota.db, &chat, turn("q")).await;
        let before = snapshot(&rig.quota.db, &chat).await;
        let reply = rig.edit(chat.id, target.request_id, &edited("x")).await;
        if status == 403 {
            assert_authz_denied(&reply);
        } else {
            assert_problem(&reply, 503, "service_unavailable");
            assert_eq!(reply.retry_after.as_deref(), Some("5"));
        }
        assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
    }
}

#[tokio::test]
async fn a_chat_of_another_user_or_tenant_is_a_404_of_the_chat_type() {
    let (rig, chat, upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    for who in [a2(), crate::test_support::chats::b1()] {
        let reply = rig
            .edit_as(&who, chat.id, target.request_id, &edited("x"))
            .await;
        assert_chat_not_found(&reply);
        assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
    }
}

// ── the preview and the preflight of the edit ────────────────────────────────

#[tokio::test]
async fn the_preview_rejections_of_the_retry_flow_apply_to_an_edit() {
    // "every error scenario of the retry flow, with the same codes"
    let (rig, chat, upstream) = streaming_chat().await;
    let first = seed(&rig.quota.db, &chat, turn("first").at(10)).await;
    seed(&rig.quota.db, &chat, turn("second").at(100)).await;
    let before = snapshot(&rig.quota.db, &chat).await;

    let reply = rig.edit(chat.id, Uuid::new_v4(), &edited("x")).await;
    assert_not_found_of(&reply, TURN_RESOURCE);
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;

    let reply = rig.edit(chat.id, first.request_id, &edited("x")).await;
    assert_conflict(&reply, "NOT_LATEST_TURN");
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;

    let (rig, chat, upstream) = streaming_chat().await;
    let foreign = seed(&rig.quota.db, &chat, turn("q").requester(Some(USER_A2))).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.edit(chat.id, foreign.request_id, &edited("x")).await;
    assert_authz_denied(&reply);
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;

    let (rig, chat, upstream) = streaming_chat().await;
    let running = seed(&rig.quota.db, &chat, turn("q").running()).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.edit(chat.id, running.request_id, &edited("x")).await;
    assert_state_violation(&reply, "turn_state");
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
}

#[tokio::test]
async fn an_edited_content_above_the_input_limit_is_a_400_input_too_long_and_the_answer_stays() {
    // inst-mpf-input-too-long: evaluated before the commit with the edited content
    let upstream = quiet_upstream();
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_snapshot(catalog_of(vec![
                prem_model(&json!({})),
                std_model(&json!({ "max_input_tokens": 50, "estimation_budgets": byte_budgets() })),
            ]))
            .with_upstream(upstream.clone()),
    )
    .await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("short").answer("keep")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig
        .edit(chat.id, target.request_id, &edited(&"y".repeat(300)))
        .await;
    let body = assert_problem(&reply, 400, "out_of_range");
    assert_eq!(violation(&body, "content")["reason"], "INPUT_TOO_LONG");
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
    assert_eq!(
        live_messages(&rig.quota.db, chat.id).await[1].content,
        "keep"
    );
}

#[tokio::test]
async fn an_exhausted_quota_is_a_429_and_the_previous_turn_stays() {
    let upstream = quiet_upstream();
    let rig = send_rig_with(SendRigOptions::new().with_upstream(upstream.clone())).await;
    rig.gateway.set_limits((1, 1), (1, 1));
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.edit(chat.id, target.request_id, &edited("x")).await;
    let body = assert_problem(&reply, 429, "resource_exhausted");
    assert_eq!(body["context"]["violations"][0]["subject"], "tokens");
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
}

#[tokio::test]
async fn an_unknown_model_is_a_400_invalid_model_and_nothing_changes() {
    let upstream = quiet_upstream();
    let rig = send_rig_with(SendRigOptions::new().with_upstream(upstream.clone())).await;
    let chat = rig.quota.chat_a1("ghost-model").await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.edit(chat.id, target.request_id, &edited("x")).await;
    assert_invalid_argument(&reply, 400, "model", "INVALID_MODEL");
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
}

#[tokio::test]
async fn an_edit_of_a_get_or_post_path_is_not_a_second_edit_route() {
    // the resource path serves GET (status), PATCH (edit) and DELETE only
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let reply = rig
        .send(request("PUT", &turn_uri(chat.id, target.request_id), &a1()))
        .await;
    assert_eq!(
        reply.status,
        StatusCode::METHOD_NOT_ALLOWED,
        "{}",
        reply.text
    );
}
