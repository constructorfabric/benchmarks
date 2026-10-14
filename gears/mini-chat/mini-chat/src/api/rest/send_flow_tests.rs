//! `POST /v1/chats/{id}/messages:stream` over the real router, SQLite, the real quota service and
//! a recording provider-task port: the replay stream, the 409 admission checks, owner isolation
//! and admission, and every pre-stream Problem of the send preflight with no rows, no reserve and
//! no provider task left behind.
//!
//! Slice S1-setup-path of `cpt-cf-mini-chat-feature-message-streaming`: flows `send-message`
//! (setup part) and `resend-request-id`; algorithms `turn-admission-checks`, `replay`,
//! `send-preflight`, `image-guards`, `context-budget`, `attachment-validation`,
//! `send-error-contract`; definitions of done `send-preflight`, `idempotency`,
//! `reserve-transaction`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::test_support::Pdp;
use crate::test_support::chats::{
    Reply, a1, a2, assert_chat_not_found, assert_invalid_argument, assert_problem, at, b1, keys,
    outbox_body_count, violation,
};
use crate::test_support::quota::{PREMIUM, STANDARD, row, seed_rows};
use crate::test_support::send::{
    AttachmentSeed, Footprint, MessageSeed, SendRig, SendRigOptions, TurnSeedFull, byte_budgets,
    catalog_of, default_catalog, footprint, parse_sse, prem_model, seed_att, seed_completed_turn,
    seed_msg, seed_turn_full, send_rig, send_rig_with, std_model, stream_uri, t,
};

const DAY: &str = "2026-02-28";

fn with_request(request_id: Uuid) -> Value {
    json!({ "content": "resend", "request_id": request_id.to_string() })
}

async fn assert_pre_stream_failure(
    rig: &SendRig,
    chat: &crate::infra::storage::repo_impl::ChatRecord,
    before: &Footprint,
    reply: &Reply,
) {
    assert!(
        reply.content_type.starts_with("application/problem+json"),
        "no SSE stream may open: {} {}",
        reply.content_type,
        reply.text
    );
    assert_eq!(
        &footprint(&rig.quota.db, chat).await,
        before,
        "rows were left behind"
    );
    assert_eq!(rig.provider.call_count(), 0, "a provider task was started");
}

// ── replay ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_completed_request_id_is_replayed_as_started_one_delta_and_done() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let (request_id, assistant) =
        seed_completed_turn(&rig.quota.db, &chat, "question", "the stored answer", t(10)).await;
    let reply = rig.stream(chat.id, &with_request(request_id)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert!(
        reply.content_type.starts_with("text/event-stream"),
        "{}",
        reply.content_type
    );

    let frames = parse_sse(&reply.text);
    let names: Vec<&str> = frames.iter().map(|f| f.event.as_str()).collect();
    assert_eq!(
        names,
        vec!["stream_started", "delta", "done"],
        "{}",
        reply.text
    );

    let started = &frames[0].data;
    assert_eq!(
        keys(started),
        BTreeSet::from(["request_id", "message_id", "is_new_turn"])
    );
    assert_eq!(started["request_id"], request_id.to_string());
    assert_eq!(started["message_id"], assistant.to_string());
    assert_eq!(started["is_new_turn"], false);

    let delta = &frames[1].data;
    assert_eq!(delta["type"], "text");
    assert_eq!(delta["content"], "the stored answer");

    let done = &frames[2].data;
    assert_eq!(
        keys(done),
        BTreeSet::from([
            "usage",
            "effective_model",
            "selected_model",
            "quota_decision"
        ])
    );
    assert_eq!(
        done["usage"],
        json!({ "input_tokens": 120, "output_tokens": 30 })
    );
    assert_eq!(done["effective_model"], STANDARD);
    assert_eq!(done["selected_model"], STANDARD);
    assert_eq!(done["quota_decision"], "allow");
}

#[tokio::test]
async fn an_empty_persisted_answer_is_still_replayed_as_one_empty_delta() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let (request_id, _) = seed_completed_turn(&rig.quota.db, &chat, "q", "", t(10)).await;
    let reply = rig.stream(chat.id, &with_request(request_id)).await;
    let frames = parse_sse(&reply.text);
    assert_eq!(frames.len(), 3, "{}", reply.text);
    assert_eq!(frames[1].data["content"], "");
}

#[tokio::test]
async fn a_replay_after_a_downgrade_reports_the_origin_without_a_reason_or_warnings() {
    let rig = send_rig().await;
    let chat = rig.prem_chat().await;
    let request_id = Uuid::new_v4();
    let assistant = Uuid::new_v4();
    seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("user", "q", t(10)).request(Some(request_id)),
    )
    .await;
    seed_msg(
        &rig.quota.db,
        &chat,
        MessageSeed::new("assistant", "a", t(11))
            .id(assistant)
            .request(Some(request_id))
            .model(STANDARD)
            .tokens(5, 6),
    )
    .await;
    seed_turn_full(
        &rig.quota.db,
        &chat,
        TurnSeedFull::new("completed")
            .request(request_id)
            .assistant(assistant)
            .model(STANDARD),
    )
    .await;
    let reply = rig.stream(chat.id, &with_request(request_id)).await;
    let done = parse_sse(&reply.text).remove(2).data;
    assert_eq!(done["quota_decision"], "downgrade");
    assert_eq!(done["downgrade_from"], PREMIUM);
    assert_eq!(done["selected_model"], PREMIUM);
    assert_eq!(done["effective_model"], STANDARD);
    for absent in [
        "downgrade_reason",
        "quota_warnings",
        "request_id",
        "message_id",
        "turn_id",
    ] {
        assert!(done.get(absent).is_none(), "`{absent}` in {done}");
    }
}

#[tokio::test]
async fn a_replay_has_no_side_effect_and_no_other_event() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let (request_id, _) = seed_completed_turn(&rig.quota.db, &chat, "q", "a", t(10)).await;
    let before = footprint(&rig.quota.db, &chat).await;
    let gateway_calls = rig.gateway.total_calls();
    let reply = rig.stream(chat.id, &with_request(request_id)).await;
    let names: Vec<String> = parse_sse(&reply.text)
        .into_iter()
        .map(|f| f.event)
        .collect();
    for event in ["ping", "tool", "citations", "error"] {
        assert!(!names.iter().any(|n| n == event), "{event} in {names:?}");
    }
    assert_eq!(footprint(&rig.quota.db, &chat).await, before);
    assert_eq!(rig.provider.call_count(), 0);
    assert_eq!(rig.gateway.total_calls(), gateway_calls, "no preflight");
    assert!(rig.quota.metrics.calls().is_empty());
    assert_eq!(outbox_body_count(&rig.quota.db).await, 0);
    assert!(
        crate::test_support::quota::all_quota_rows(&rig.quota.db)
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_replay_is_served_while_another_turn_of_the_chat_is_running() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let (request_id, _) = seed_completed_turn(&rig.quota.db, &chat, "q", "a", t(10)).await;
    seed_turn_full(&rig.quota.db, &chat, TurnSeedFull::new("running")).await;
    let reply = rig.stream(chat.id, &with_request(request_id)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert!(reply.content_type.starts_with("text/event-stream"));
}

#[tokio::test]
async fn a_replay_with_a_missing_assistant_message_is_a_generic_500_without_a_stream() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let seed = TurnSeedFull::new("completed")
        .assistant(Uuid::new_v4())
        .model(STANDARD);
    let request_id = seed.request_id;
    seed_turn_full(&rig.quota.db, &chat, seed).await;
    let reply = rig.stream(chat.id, &with_request(request_id)).await;
    assert_problem(&reply, 500, "internal");
}

// ── conflicts ────────────────────────────────────────────────────────────────

fn assert_conflict(reply: &Reply, reason: &str, hidden: &[Uuid]) {
    let body = assert_problem(reply, 409, "aborted");
    assert_eq!(body["context"]["reason"], reason, "{body}");
    for id in hidden {
        assert!(
            !reply.text.contains(&id.to_string()),
            "internal id {id} leaked: {body}"
        );
    }
}

#[tokio::test]
async fn reusing_a_request_id_in_any_other_state_is_a_409_request_id_conflict() {
    for (label, seed) in [
        ("running", TurnSeedFull::new("running")),
        (
            "failed",
            TurnSeedFull::new("failed").error("provider_error"),
        ),
        ("cancelled", TurnSeedFull::new("cancelled")),
        (
            "completed but deleted",
            TurnSeedFull::new("completed").deleted(t(5)),
        ),
        (
            "failed and deleted",
            TurnSeedFull::new("failed").deleted(t(5)),
        ),
    ] {
        let rig = send_rig().await;
        let chat = rig.std_chat().await;
        let (request_id, turn_id) = (seed.request_id, seed.id);
        seed_turn_full(&rig.quota.db, &chat, seed).await;
        let before = footprint(&rig.quota.db, &chat).await;
        let reply = rig.stream(chat.id, &with_request(request_id)).await;
        assert_conflict(&reply, "request_id_conflict", &[request_id, turn_id]);
        assert_pre_stream_failure(&rig, &chat, &before, &reply).await;
        assert!(
            rig.quota.metrics.calls().is_empty(),
            "{label}: no preflight ran"
        );
    }
}

#[tokio::test]
async fn a_second_turn_while_one_runs_is_a_409_turn_already_running_with_any_or_no_request_id() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let running = TurnSeedFull::new("running");
    let running_id = running.id;
    seed_turn_full(&rig.quota.db, &chat, running).await;
    let before = footprint(&rig.quota.db, &chat).await;
    for body in [json!({ "content": "no key" }), with_request(Uuid::new_v4())] {
        let reply = rig.stream(chat.id, &body).await;
        assert_conflict(&reply, "turn_already_running", &[running_id]);
        assert_pre_stream_failure(&rig, &chat, &before, &reply).await;
    }
    assert_eq!(
        rig.gateway.total_calls(),
        0,
        "the guard runs before the quota preflight"
    );
}

#[tokio::test]
async fn the_409_detail_is_generic_text() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    seed_turn_full(&rig.quota.db, &chat, TurnSeedFull::new("running")).await;
    let body = assert_problem(
        &rig.stream(chat.id, &json!({ "content": "x" })).await,
        409,
        "aborted",
    );
    let detail = body["detail"].as_str().unwrap().to_lowercase();
    assert!(
        !detail.contains("uuid") && !detail.contains("sqlite") && !detail.contains("unique"),
        "{body}"
    );
}

// ── admission and the chat ───────────────────────────────────────────────────

#[tokio::test]
async fn admission_evaluates_send_message_only() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let (request_id, _) = seed_completed_turn(&rig.quota.db, &chat, "q", "a", t(10)).await;
    rig.stream(chat.id, &with_request(request_id)).await;
    let request = rig.pdp.only_request();
    assert_eq!(request.action.name, "send_message");
    assert_eq!(request.resource.id, Some(chat.id));
}

#[tokio::test]
async fn a_chat_that_is_not_visible_is_a_404_and_nothing_is_written() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let foreign_user = rig.quota.chat(&a2(), STANDARD).await;
    let before = footprint(&rig.quota.db, &chat).await;
    for (who, id) in [
        (a2(), chat.id),
        (b1(), chat.id),
        (a1(), foreign_user.id),
        (a1(), Uuid::new_v4()),
    ] {
        let reply = rig
            .post_json(&stream_uri(id), &who, &json!({ "content": "hi" }))
            .await;
        assert_chat_not_found(&reply);
    }
    assert_eq!(footprint(&rig.quota.db, &chat).await, before);
    assert_eq!(rig.provider.call_count(), 0);
}

#[tokio::test]
async fn a_denied_or_failing_pdp_refuses_the_send_before_any_read() {
    let denied = send_rig_with(SendRigOptions::new().with_pdp(Pdp::Deny)).await;
    let chat = denied.std_chat().await;
    let reply = denied.stream(chat.id, &json!({ "content": "hi" })).await;
    let body = assert_problem(&reply, 403, "permission_denied");
    assert_eq!(body["context"]["reason"], "AUTHZ_DENIED");
    assert_eq!(denied.gateway.total_calls(), 0);

    let down = send_rig_with(SendRigOptions::new().with_pdp(Pdp::Unreachable)).await;
    let chat = down.std_chat().await;
    let reply = down.stream(chat.id, &json!({ "content": "hi" })).await;
    assert_problem(&reply, 503, "service_unavailable");
    assert_eq!(reply.retry_after.as_deref(), Some("5"));
}

// ── the preflight Problems ───────────────────────────────────────────────────

async fn rig_with_std(patch: &Value) -> SendRig {
    send_rig_with(
        SendRigOptions::new()
            .with_snapshot(catalog_of(vec![prem_model(&json!({})), std_model(patch)])),
    )
    .await
}

#[tokio::test]
async fn an_exhausted_token_quota_is_a_429_with_the_tokens_violation() {
    let rig = send_rig().await;
    rig.gateway.set_limits((1, 1), (1, 1));
    let chat = rig.std_chat().await;
    let before = footprint(&rig.quota.db, &chat).await;
    let reply = rig.stream(chat.id, &json!({ "content": "hi" })).await;
    let body = assert_problem(&reply, 429, "resource_exhausted");
    let violations = body["context"]["violations"].as_array().unwrap();
    assert_eq!(violations.len(), 1, "{body}");
    assert_eq!(violations[0]["subject"], "tokens");
    assert_eq!(violations[0]["description"], "quota_exceeded");
    assert_pre_stream_failure(&rig, &chat, &before, &reply).await;
}

#[tokio::test]
async fn an_exhausted_web_search_quota_is_a_429_with_the_web_search_violation() {
    let rig = send_rig().await;
    seed_rows(&rig.quota.db, &[row("daily", DAY, "total").web(75)]).await;
    let chat = rig.std_chat().await;
    let before = footprint(&rig.quota.db, &chat).await;
    let reply = rig
        .stream(
            chat.id,
            &json!({ "content": "hi", "web_search": { "enabled": true } }),
        )
        .await;
    let body = assert_problem(&reply, 429, "resource_exhausted");
    assert_eq!(body["context"]["violations"][0]["subject"], "web_search");
    assert_pre_stream_failure(&rig, &chat, &before, &reply).await;
}

#[tokio::test]
async fn the_kill_switches_are_400_failed_precondition_with_feature_disabled() {
    let mut snapshot = default_catalog();
    snapshot.kill_switches.disable_web_search = true;
    snapshot.kill_switches.disable_images = true;
    let rig = send_rig_with(SendRigOptions::new().with_snapshot(snapshot)).await;
    let chat = rig.prem_chat().await;
    let image = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
    let before = footprint(&rig.quota.db, &chat).await;

    let web = rig
        .stream(
            chat.id,
            &json!({ "content": "hi", "web_search": { "enabled": true } }),
        )
        .await;
    let body = assert_problem(&web, 400, "failed_precondition");
    assert_eq!(body["context"]["violations"][0]["subject"], "web_search");
    assert_eq!(body["context"]["violations"][0]["type"], "FEATURE_DISABLED");
    assert_pre_stream_failure(&rig, &chat, &before, &web).await;

    let images = rig
        .stream(
            chat.id,
            &json!({ "content": "hi", "attachment_ids": [image.to_string()] }),
        )
        .await;
    let body = assert_problem(&images, 400, "failed_precondition");
    assert_eq!(body["context"]["violations"][0]["subject"], "images");
    assert_eq!(body["context"]["violations"][0]["type"], "FEATURE_DISABLED");
    assert_pre_stream_failure(&rig, &chat, &before, &images).await;
}

#[tokio::test]
async fn a_message_above_the_input_limit_is_a_400_out_of_range_input_too_long() {
    let rig = rig_with_std(&json!({ "max_input_tokens": 50 })).await;
    let chat = rig.std_chat().await;
    let before = footprint(&rig.quota.db, &chat).await;
    let reply = rig
        .stream(chat.id, &json!({ "content": "y".repeat(400) }))
        .await;
    let body = assert_problem(&reply, 400, "out_of_range");
    assert_eq!(violation(&body, "content")["reason"], "INPUT_TOO_LONG");
    assert_pre_stream_failure(&rig, &chat, &before, &reply).await;
}

#[tokio::test]
async fn too_many_images_is_a_400_out_of_range_too_many_images() {
    let rig = send_rig().await;
    let chat = rig.prem_chat().await;
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(
            seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image"))
                .await
                .to_string(),
        );
    }
    let before = footprint(&rig.quota.db, &chat).await;
    let reply = rig
        .stream(
            chat.id,
            &json!({ "content": "pics", "attachment_ids": ids }),
        )
        .await;
    let body = assert_problem(&reply, 400, "out_of_range");
    assert_eq!(violation(&body, "image_count")["reason"], "TOO_MANY_IMAGES");
    assert_pre_stream_failure(&rig, &chat, &before, &reply).await;
}

#[tokio::test]
async fn images_on_a_model_without_vision_are_a_400_vision_not_supported() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let image = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
    let before = footprint(&rig.quota.db, &chat).await;
    let reply = rig
        .stream(
            chat.id,
            &json!({ "content": "see", "attachment_ids": [image.to_string()] }),
        )
        .await;
    assert_invalid_argument(&reply, 400, "content_type", "VISION_NOT_SUPPORTED");
    assert_pre_stream_failure(&rig, &chat, &before, &reply).await;
}

#[tokio::test]
async fn images_after_a_downgrade_from_a_vision_model_are_a_400_vision_not_supported() {
    let rig = send_rig().await;
    rig.gateway.set_limits((60_000_000, 600_000_000), (1, 1));
    let chat = rig.prem_chat().await;
    let image = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
    let before = footprint(&rig.quota.db, &chat).await;
    let reply = rig
        .stream(
            chat.id,
            &json!({ "content": "see", "attachment_ids": [image.to_string()] }),
        )
        .await;
    assert_invalid_argument(&reply, 400, "content_type", "VISION_NOT_SUPPORTED");
    assert_pre_stream_failure(&rig, &chat, &before, &reply).await;
}

#[tokio::test]
async fn mandatory_context_above_the_budget_is_a_400_out_of_range_context_budget_exceeded() {
    let rig = rig_with_std(&json!({
        "context_window": 600, "max_input_tokens": 0, "estimation_budgets": byte_budgets(),
    }))
    .await;
    let chat = rig.std_chat().await;
    let before = footprint(&rig.quota.db, &chat).await;
    let reply = rig
        .stream(chat.id, &json!({ "content": "z".repeat(150) }))
        .await;
    let body = assert_problem(&reply, 400, "out_of_range");
    assert_eq!(
        violation(&body, "context")["reason"],
        "CONTEXT_BUDGET_EXCEEDED"
    );
    assert_pre_stream_failure(&rig, &chat, &before, &reply).await;
}

#[tokio::test]
async fn a_chat_model_missing_from_the_catalog_is_a_400_invalid_model() {
    let rig = send_rig().await;
    let chat = rig.quota.chat_a1("ghost-model").await;
    let before = footprint(&rig.quota.db, &chat).await;
    let reply = rig.stream(chat.id, &json!({ "content": "hi" })).await;
    assert_invalid_argument(&reply, 400, "model", "INVALID_MODEL");
    assert_pre_stream_failure(&rig, &chat, &before, &reply).await;
}

#[tokio::test]
async fn an_invalid_attachment_found_in_the_reserve_transaction_is_a_400_invalid_attachment() {
    let rig = send_rig().await;
    let chat = rig.prem_chat().await;
    let foreign = seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("document").uploaded_by(crate::test_support::USER_A2),
    )
    .await;
    let not_ready = seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("document").status("uploaded"),
    )
    .await;
    let deleted = seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("document").deleted(at(3)),
    )
    .await;
    let before = footprint(&rig.quota.db, &chat).await;
    for bad in [Uuid::new_v4(), foreign, not_ready, deleted] {
        let reply = rig
            .stream(
                chat.id,
                &json!({ "content": "files", "attachment_ids": [bad.to_string()] }),
            )
            .await;
        assert_invalid_argument(&reply, 400, "attachment", "invalid_attachment");
        assert_pre_stream_failure(&rig, &chat, &before, &reply).await;
    }
}

#[tokio::test]
async fn policy_and_provider_failures_are_generic_500s_that_leak_no_cause() {
    // the policy plugin fails
    let rig = send_rig().await;
    rig.gateway
        .fail_current
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let chat = rig.std_chat().await;
    let before = footprint(&rig.quota.db, &chat).await;
    let reply = rig.stream(chat.id, &json!({ "content": "hi" })).await;
    let body = assert_problem(&reply, 500, "internal");
    assert!(
        !body.to_string().contains("provider-secret-detail"),
        "{body}"
    );
    assert_pre_stream_failure(&rig, &chat, &before, &reply).await;

    // the provider of the effective model cannot be resolved
    let rig = rig_with_std(&json!({ "provider_id": "ghost_provider" })).await;
    let chat = rig.std_chat().await;
    let before = footprint(&rig.quota.db, &chat).await;
    let reply = rig.stream(chat.id, &json!({ "content": "hi" })).await;
    let body = assert_problem(&reply, 500, "internal");
    assert!(!body.to_string().contains("ghost_provider"), "{body}");
    assert_pre_stream_failure(&rig, &chat, &before, &reply).await;
}
