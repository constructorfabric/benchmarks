//! `POST /v1/chats/{id}/turns/{request_id}/retry` end to end over the real router, SQLite, the
//! real quota service, the real provider task and a mock OAGW: the stream (`stream_started` with
//! a new server-generated `request_id` and `is_new_turn` `true`, the send events, `done`), the
//! persisted replacement turn and messages, the single settlement with one usage event, one
//! `turn_completed` and one `turn_retry` audit event, the replay and status of the replaced
//! `request_id`, the thread summary in the stream, the single PDP evaluation, and every
//! pre-commit Problem (path, admission, preview, preflight) with the previous turn, its answer and
//! the chat unchanged and no proxied request.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: flow
//! `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-retry-turn` (`inst-retry-request`,
//! `inst-retry-bad-path`, `inst-retry-return-bad-path`, `inst-retry-admission`,
//! `inst-retry-refused`, `inst-retry-return-refusal`, `inst-retry-spawn-setup`,
//! `inst-retry-preview`, `inst-retry-preview-rejected`, `inst-retry-return-preview`,
//! `inst-retry-preflight`, `inst-retry-preflight-rejected`, `inst-retry-return-preflight`,
//! `inst-retry-new-request-id`, `inst-retry-commit`, `inst-retry-commit-rejected`,
//! `inst-retry-return-commit`, `inst-retry-stream`, `inst-retry-return`);
//! algorithms `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preview`,
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-preflight`,
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-message-order` (`inst-mo-assistant`,
//! `inst-mo-exclude-deleted`, `inst-mo-count`, `inst-mo-old-request-id`),
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event`
//! (`inst-ae-replacement-turn`) and
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation`; states
//! `cpt-cf-mini-chat-state-turn-mutations-and-reactions-turn-visibility` and
//! `cpt-cf-mini-chat-state-turn-mutations-and-reactions-replacement-turn-setup`
//! (`inst-rts-to-finalized`); definitions of done
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-retry`,
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-latest-terminal-turn` and
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-summary-invalidation`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::http::StatusCode;
use mini_chat_sdk::AuditEvent;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::test_support::chats::{
    a1, a2, assert_chat_not_found, assert_invalid_argument, assert_problem, b1, keys, request,
    violation,
};
use crate::test_support::mutations::{
    ArmedHook, FailingMutationOutbox, assert_authz_denied, assert_conflict, assert_internal,
    assert_not_found_of, assert_state_violation, assert_untouched, current_of, history_of,
    live_messages, mutation_events, replacement_of, seed, sse_of, started_ids, streaming_chat,
    summary_row, turn, turn_by_request, turn_events,
};
use crate::test_support::provider::{MockProvider, Reply, happy_steps};
use crate::test_support::quota::{PREMIUM, STANDARD, default_now, row, seed_rows};
use crate::test_support::send::{
    AttachmentSeed, SendRig, SendRigOptions, TURN_RESOURCE, byte_budgets, catalog_of,
    default_catalog, names, parse_sse, prem_model, seed_att, seed_summary, send_rig_with,
    std_model, stream_uri, t, turn_uri,
};
use crate::test_support::{Pdp, USER_A1, USER_A2};

use crate::test_support::mutations::{running_turn_sql, snapshot};

const DAY: &str = "2026-02-28";

async fn rig_over(upstream: &Arc<MockProvider>, options: SendRigOptions) -> SendRig {
    send_rig_with(options.with_upstream(upstream.clone())).await
}

fn quiet_upstream() -> Arc<MockProvider> {
    MockProvider::answering(Reply::Sse(happy_steps(&["ok"], (50, 5))))
}

// ── the stream ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_retry_streams_the_new_answer_with_a_new_request_id_and_is_new_turn_true() {
    // inst-retry-new-request-id, inst-retry-stream, inst-retry-return
    let (rig, chat, _upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("what is rust?")).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let frames = sse_of(&reply);
    assert_eq!(
        names(&frames),
        vec!["stream_started", "delta", "delta", "done"],
        "{}",
        reply.text
    );

    let started = &frames[0].data;
    assert_eq!(
        keys(started),
        BTreeSet::from(["request_id", "message_id", "is_new_turn"]),
        "{started}"
    );
    let (new_request, message_id) = started_ids(&frames);
    assert_ne!(new_request, target.request_id);
    assert_eq!(
        new_request.get_version_num(),
        4,
        "a server-generated UUID v4"
    );
    assert_eq!(started["is_new_turn"], true);
    assert_ne!(Some(message_id), target.assistant_message_id);

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
}

#[tokio::test]
async fn the_retry_stream_has_the_response_head_of_the_send_stream() {
    // inst-retry-stream: the same SSE contract as `messages:stream`
    let (rig, chat, _upstream) = streaming_chat().await;
    let mut sent = rig
        .open_stream(chat.id, &json!({ "content": "first" }))
        .await;
    let send_status = sent.status;
    let send_headers = sent.headers.clone();
    let send_type = sent.content_type.clone();
    let frames = sent.rest().await;
    let (target, _) = started_ids(&frames);
    rig.wait_turn_terminal(chat.id, target).await;
    let mut retry = rig.open_request(rig.retry_request(chat.id, target)).await;
    assert_eq!(retry.status, send_status);
    assert_eq!(retry.content_type, send_type);
    assert_eq!(
        retry.headers.get("cache-control"),
        send_headers.get("cache-control")
    );
    assert_eq!(
        retry.headers.get("content-encoding"),
        send_headers.get("content-encoding")
    );
    assert!(retry.headers.get("connection").is_none());
    let frames = retry.rest().await;
    assert_eq!(names(&frames).last(), Some(&"done"));
}

#[tokio::test]
async fn after_a_retry_the_previous_turn_is_replaced_and_its_messages_are_soft_deleted() {
    // inst-tv-to-replaced, inst-mo-exclude-deleted
    let (rig, chat, _upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("question").answer("old answer")).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let (new_request, message_id) = started_ids(&sse_of(&reply));
    rig.wait_turn_terminal(chat.id, new_request).await;

    let old = turn_by_request(&rig.quota.db, chat.id, target.request_id).await;
    assert!(old.deleted_at.is_some());
    assert_eq!(old.replaced_by_request_id, Some(new_request));
    let new = replacement_of(&rig.quota.db, chat.id, target.request_id).await;
    assert_eq!(new.request_id, new_request);
    assert_eq!(new.state, "completed");
    assert_eq!(new.assistant_message_id, Some(message_id));
    assert_eq!(new.started_at, default_now(), "the preflight instant");

    let live = live_messages(&rig.quota.db, chat.id).await;
    assert_eq!(live.len(), 2);
    assert_eq!(live[0].role, "user");
    assert_eq!(
        live[0].content, "question",
        "the original content is re-submitted"
    );
    assert_eq!(live[0].request_id, Some(new_request));
    assert_eq!(live[1].role, "assistant");
    assert_eq!(live[1].id, message_id);
    assert_eq!(live[1].content, "Hello");
    assert!(
        live[1].created_at > live[0].created_at,
        "the answer follows its question"
    );
}

#[tokio::test]
async fn the_messages_list_and_the_message_count_follow_the_retry() {
    // inst-mo-count: two messages before, the replacement user message and the new answer after
    let (rig, chat, _upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("question")).await;
    assert_eq!(rig.message_count(chat.id).await, 2);
    let reply = rig.retry(chat.id, target.request_id).await;
    let (new_request, _) = started_ids(&sse_of(&reply));
    rig.wait_turn_terminal(chat.id, new_request).await;
    assert_eq!(rig.message_count(chat.id).await, 2);
    let items = rig.list_messages(chat.id).await;
    let contents: Vec<&str> = items
        .iter()
        .map(|i| i["content"].as_str().unwrap())
        .collect();
    assert_eq!(contents, vec!["question", "Hello"]);
    assert!(
        items
            .iter()
            .all(|i| i["request_id"] == new_request.to_string())
    );
    assert!(items.iter().all(|i| i["my_reaction"].is_null()));
}

#[tokio::test]
async fn the_chat_activity_moves_forward_on_a_retry() {
    // inst-mc-activity through the endpoint
    let (rig, chat, _upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let before = crate::test_support::send::chat_model(&rig.quota.db, chat.id)
        .await
        .updated_at;
    rig.retry(chat.id, target.request_id).await;
    let after = crate::test_support::send::chat_model(&rig.quota.db, chat.id)
        .await
        .updated_at;
    assert!(after > before, "{before} -> {after}");
}

#[tokio::test]
async fn the_retry_proxies_the_original_content_after_the_history_before_it() {
    // inst-retry-preflight, inst-pcs-context: the replaced turn's own messages are not history
    let (rig, chat, upstream) = streaming_chat().await;
    seed(
        &rig.quota.db,
        &chat,
        turn("first question").at(10).answer("first answer"),
    )
    .await;
    let second = seed(
        &rig.quota.db,
        &chat,
        turn("second question").at(100).answer("old second"),
    )
    .await;
    let reply = rig.retry(chat.id, second.request_id).await;
    sse_of(&reply);
    let call = upstream.only_call();
    assert_eq!(
        history_of(&call.body),
        vec![
            ("user".to_owned(), "first question".to_owned()),
            ("assistant".to_owned(), "first answer".to_owned())
        ]
    );
    assert_eq!(
        current_of(&call.body),
        &json!({ "role": "user", "content": "second question" })
    );
    assert!(!call.body.to_string().contains("old second"));
}

#[tokio::test]
async fn the_replacement_turn_is_settled_once_with_its_own_events_and_the_mutation_event() {
    // inst-retry-stream: no separate settlement or outbox path for retry
    let (rig, chat, _upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("question")).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let (new_request, _) = started_ids(&sse_of(&reply));
    rig.wait_turn_terminal(chat.id, new_request).await;

    let events = rig.events().await;
    assert_eq!(events.len(), 1, "one usage event: {events:?}");
    assert_eq!(events[0].request_id, new_request);
    assert_eq!(events[0].terminal_state, "completed");
    assert_eq!(events[0].billing_outcome, "completed");
    assert_eq!(events[0].settlement_method, "actual");
    assert_eq!(
        crate::test_support::send::footprint(&rig.quota.db, &chat)
            .await
            .reserved_credits,
        0,
        "the reserve is settled"
    );

    let audits = rig.audit_events().await;
    assert_eq!(audits.len(), 2, "{audits:?}");
    assert_eq!(turn_events(&rig).await.len(), 1);
    let mutations = mutation_events(&rig).await;
    assert_eq!(mutations.len(), 1);
    let AuditEvent::TurnRetry(retry) = &mutations[0] else {
        panic!("expected turn_retry, got {mutations:?}");
    };
    assert_eq!(retry.actor_user_id, USER_A1);
    assert_eq!(retry.chat_id, chat.id);
    assert_eq!(retry.original_request_id, target.request_id);
    assert_eq!(retry.new_request_id, new_request);
    let AuditEvent::TurnCompleted(completed) = &turn_events(&rig).await[0] else {
        panic!("expected turn_completed");
    };
    assert_eq!(completed.request_id, new_request);
}

#[tokio::test]
async fn the_replaced_turn_is_not_settled_again_and_keeps_its_counters() {
    // inst-tcf-counters, inst-tcf-no-refund
    let (rig, chat, _upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q").web_search().counts(2, 0, 1)).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let (new_request, _) = started_ids(&sse_of(&reply));
    rig.wait_turn_terminal(chat.id, new_request).await;
    let old = turn_by_request(&rig.quota.db, chat.id, target.request_id).await;
    assert_eq!(
        (
            old.web_search_completed_count,
            old.code_interpreter_completed_count,
            old.file_search_completed_count
        ),
        (2, 0, 1)
    );
    assert_eq!(
        rig.events().await.len(),
        1,
        "only the replacement turn publishes usage"
    );
}

#[tokio::test]
async fn a_replaced_request_id_has_no_status_and_its_replay_is_a_409() {
    // inst-mo-old-request-id: 404 through the turn status, 409 `request_id_conflict` on replay
    let (rig, chat, _upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let (new_request, _) = started_ids(&sse_of(&reply));
    rig.wait_turn_terminal(chat.id, new_request).await;

    let status = rig.get(&turn_uri(chat.id, target.request_id), &a1()).await;
    assert_not_found_of(&status, TURN_RESOURCE);
    let resend = rig
        .post_json(
            &stream_uri(chat.id),
            &a1(),
            &json!({ "content": "again", "request_id": target.request_id.to_string() }),
        )
        .await;
    let body = assert_problem(&resend, 409, "aborted");
    assert_eq!(body["context"]["reason"], "request_id_conflict");

    // the new turn reports its status and its assistant message
    let new_status = rig.get(&turn_uri(chat.id, new_request), &a1()).await;
    assert_eq!(new_status.status, StatusCode::OK, "{}", new_status.text);
    assert_eq!(new_status.json()["state"], "done");
}

#[tokio::test]
async fn a_retry_of_a_failed_or_a_cancelled_turn_streams_a_new_answer() {
    // the allowed target states: completed, failed and cancelled
    for plan in [
        turn("failed one").failed(),
        turn("cancelled with partial").cancelled(Some("part")),
        turn("cancelled without text").cancelled(None),
    ] {
        let (rig, chat, upstream) = streaming_chat().await;
        let target = seed(&rig.quota.db, &chat, plan).await;
        let reply = rig.retry(chat.id, target.request_id).await;
        let frames = sse_of(&reply);
        assert_eq!(names(&frames).last(), Some(&"done"), "{}", reply.text);
        assert_eq!(upstream.call_count(), 1);
    }
}

#[tokio::test]
async fn a_retry_after_a_delete_of_the_newer_turn_acts_on_the_previous_one() {
    // acceptance: after deleting T2, T1 becomes the latest turn and can be retried
    let (rig, chat, _upstream) = streaming_chat().await;
    let first = seed(&rig.quota.db, &chat, turn("first").at(10)).await;
    let second = seed(&rig.quota.db, &chat, turn("second").at(100)).await;
    assert_eq!(
        rig.delete_turn(chat.id, second.request_id).await.status,
        StatusCode::NO_CONTENT
    );
    let reply = rig.retry(chat.id, first.request_id).await;
    let frames = sse_of(&reply);
    assert_eq!(names(&frames).last(), Some(&"done"));
}

// ── the thread summary ───────────────────────────────────────────────────────

#[tokio::test]
async fn a_summary_that_does_not_cover_the_target_is_applied_and_kept() {
    // inst-si-keep: the next context is the summary followed by the replacement turn
    let (rig, chat, upstream) = streaming_chat().await;
    let first = seed(&rig.quota.db, &chat, turn("first").at(10)).await;
    let second = seed(&rig.quota.db, &chat, turn("second").at(100)).await;
    seed_summary(
        &rig.quota.db,
        &chat,
        "the earlier talk",
        33,
        (t(11), first.assistant_message_id.unwrap()),
    )
    .await;
    let reply = rig.retry(chat.id, second.request_id).await;
    let frames = sse_of(&reply);
    assert_eq!(
        frames[0].data["thread_summary_applied"],
        json!({ "token_estimate": 33 }),
        "{}",
        frames[0].data
    );
    assert!(summary_row(&rig.quota.db, chat.id).await.is_some());
    let call = upstream.only_call();
    assert!(call.body.to_string().contains("the earlier talk"));
}

#[tokio::test]
async fn a_summary_that_covers_the_target_is_invalidated_and_the_stream_has_none() {
    // inst-si-covers, inst-si-return-invalidated: no `thread_summary_applied` in the next stream
    let (rig, chat, upstream) = streaming_chat().await;
    seed(&rig.quota.db, &chat, turn("first").at(10)).await;
    let second = seed(&rig.quota.db, &chat, turn("second").at(100)).await;
    seed_summary(
        &rig.quota.db,
        &chat,
        "covers everything",
        33,
        (t(100), second.user_message_id),
    )
    .await;
    crate::test_support::mutations::mark_compressed(&rig.quota.db, chat.id).await;
    let reply = rig.retry(chat.id, second.request_id).await;
    let frames = sse_of(&reply);
    assert!(
        frames[0].data.get("thread_summary_applied").is_none(),
        "{}",
        frames[0].data
    );
    assert!(summary_row(&rig.quota.db, chat.id).await.is_none());
    let call = upstream.only_call();
    assert!(!call.body.to_string().contains("covers everything"));
    // until the next summary the context is the uncompressed history before the turn
    assert_eq!(
        history_of(&call.body),
        vec![
            ("user".to_owned(), "first".to_owned()),
            ("assistant".to_owned(), "the answer".to_owned())
        ]
    );
}

// ── authorization ────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_retry_evaluates_the_pdp_exactly_once_with_the_retry_action() {
    // inst-retry-admission: the mutation, the preflight and the stream reuse the scope
    let (rig, chat, _upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    sse_of(&reply);
    let request = rig.pdp.only_request();
    assert_eq!(request.action.name, "retry_turn");
    assert_eq!(request.resource.id, Some(chat.id));
}

#[tokio::test]
async fn a_pdp_deny_is_a_403_authz_denied_before_any_write() {
    // inst-retry-refused, inst-retry-return-refusal
    let upstream = quiet_upstream();
    let rig = rig_over(&upstream, SendRigOptions::new().with_pdp(Pdp::Deny)).await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    assert_authz_denied(&reply);
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
}

#[tokio::test]
async fn a_pdp_failure_is_a_503_with_retry_after_5_before_any_write() {
    let upstream = quiet_upstream();
    let rig = rig_over(&upstream, SendRigOptions::new().with_pdp(Pdp::Unreachable)).await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    assert_problem(&reply, 503, "service_unavailable");
    assert_eq!(reply.retry_after.as_deref(), Some("5"));
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
}

#[tokio::test]
async fn a_missing_foreign_or_deleted_chat_is_a_404_of_the_chat_type() {
    // inst-retry-return-refusal: 404 for the chat type, never a 403
    let (rig, chat, upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    for (who, chat_id) in [(a1(), Uuid::new_v4()), (a2(), chat.id), (b1(), chat.id)] {
        let reply = rig.retry_as(&who, chat_id, target.request_id).await;
        assert_chat_not_found(&reply);
        assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
    }
    crate::test_support::mutations::soft_delete_chat(&rig.quota.db, chat.id).await;
    let after_delete = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    assert_chat_not_found(&reply);
    assert_untouched(&rig, &chat, &after_delete, &upstream, &reply).await;
}

#[tokio::test]
async fn a_non_uuid_chat_id_or_request_id_is_a_400_invalid_path_params() {
    // inst-retry-bad-path, inst-retry-return-bad-path
    let (rig, chat, upstream) = streaming_chat().await;
    let good = Uuid::new_v4();
    for uri in [
        format!("/mini-chat/v1/chats/nope/turns/{good}/retry"),
        format!("/mini-chat/v1/chats/{}/turns/nope/retry", chat.id),
        format!("/mini-chat/v1/chats/{}/turns/123/retry", chat.id),
    ] {
        let reply = rig.send(request("POST", &uri, &a1())).await;
        let body = assert_problem(&reply, 400, "invalid_argument");
        let violations = body["context"]["field_violations"].as_array().unwrap();
        assert!(
            violations
                .iter()
                .any(|v| v["reason"] == "invalid_path_params"),
            "{uri}: {body}"
        );
    }
    assert!(
        rig.pdp.requests().is_empty(),
        "the path is checked before admission"
    );
    assert_eq!(upstream.call_count(), 0);
}

// ── the preview ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_unknown_request_id_is_a_404_of_the_turn_type() {
    // inst-retry-preview-rejected, inst-retry-return-preview
    let (rig, chat, upstream) = streaming_chat().await;
    seed(&rig.quota.db, &chat, turn("q")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, Uuid::new_v4()).await;
    assert_not_found_of(&reply, TURN_RESOURCE);
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
}

#[tokio::test]
async fn a_turn_of_another_chat_of_the_same_user_is_a_404_of_the_turn_type() {
    let (rig, chat, upstream) = streaming_chat().await;
    let other = rig.std_chat().await;
    let foreign = seed(&rig.quota.db, &other, turn("elsewhere")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, foreign.request_id).await;
    assert_not_found_of(&reply, TURN_RESOURCE);
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
}

#[tokio::test]
async fn a_turn_of_another_requester_is_a_403_authz_denied() {
    // inst-mp-not-owner: a NULL requester included
    let (rig, chat, upstream) = streaming_chat().await;
    let other = seed(&rig.quota.db, &chat, turn("q").requester(Some(USER_A2))).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, other.request_id).await;
    assert_authz_denied(&reply);
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;

    let (rig, chat, upstream) = streaming_chat().await;
    let orphan = seed(&rig.quota.db, &chat, turn("q").requester(None)).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, orphan.request_id).await;
    assert_authz_denied(&reply);
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
}

#[tokio::test]
async fn a_turn_that_is_not_the_latest_is_a_409_not_latest_turn() {
    // inst-mp-not-latest: T1 behind T2
    let (rig, chat, upstream) = streaming_chat().await;
    let first = seed(&rig.quota.db, &chat, turn("first").at(10)).await;
    seed(&rig.quota.db, &chat, turn("second").at(100)).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, first.request_id).await;
    let body = assert_conflict(&reply, "NOT_LATEST_TURN");
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
    let detail = body["detail"].as_str().unwrap();
    assert!(!detail.contains(&first.request_id.to_string()), "{detail}");
}

#[tokio::test]
async fn a_turn_behind_a_newer_running_turn_is_a_409_not_latest_turn() {
    let (rig, chat, upstream) = streaming_chat().await;
    let first = seed(&rig.quota.db, &chat, turn("first").at(10)).await;
    seed(&rig.quota.db, &chat, turn("second").at(100).running()).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, first.request_id).await;
    assert_conflict(&reply, "NOT_LATEST_TURN");
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
}

#[tokio::test]
async fn an_already_replaced_turn_is_a_409_not_latest_turn() {
    // inst-tv-final: a second retry of the same request id
    let (rig, chat, upstream) = streaming_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let first = rig.retry(chat.id, target.request_id).await;
    let (new_request, _) = started_ids(&sse_of(&first));
    rig.wait_turn_terminal(chat.id, new_request).await;
    let calls = upstream.call_count();
    let before = snapshot(&rig.quota.db, &chat).await;
    let again = rig.retry(chat.id, target.request_id).await;
    assert_conflict(&again, "NOT_LATEST_TURN");
    assert_eq!(snapshot(&rig.quota.db, &chat).await, before);
    assert_eq!(upstream.call_count(), calls);
}

#[tokio::test]
async fn a_running_turn_is_a_400_turn_state_precondition() {
    // inst-mp-running: checked before the latest-turn check
    let (rig, chat, upstream) = streaming_chat().await;
    let running = seed(&rig.quota.db, &chat, turn("q").running()).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, running.request_id).await;
    assert_state_violation(&reply, "turn_state");
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
}

#[tokio::test]
async fn a_running_turn_behind_a_newer_turn_is_still_a_turn_state_precondition() {
    let (rig, chat, upstream) = streaming_chat().await;
    let stuck = seed(&rig.quota.db, &chat, turn("stuck").at(10).running()).await;
    seed(&rig.quota.db, &chat, turn("newer").at(100)).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, stuck.request_id).await;
    assert_state_violation(&reply, "turn_state");
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
}

// ── the preflight ────────────────────────────────────────────────────────────

fn rig_catalog(patch: &Value) -> SendRigOptions {
    SendRigOptions::new().with_snapshot(catalog_of(vec![prem_model(&json!({})), std_model(patch)]))
}

#[tokio::test]
async fn a_chat_model_missing_from_the_catalog_is_a_400_invalid_model_and_nothing_changes() {
    // inst-mpf-return-invalid-model
    let upstream = quiet_upstream();
    let rig = rig_over(&upstream, SendRigOptions::new()).await;
    let chat = rig.quota.chat_a1("ghost-model").await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    assert_invalid_argument(&reply, 400, "model", "INVALID_MODEL");
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
}

#[tokio::test]
async fn more_carried_images_than_the_limit_are_a_400_too_many_images() {
    // inst-mpf-return-too-many-images: `rag.max_images_per_message` is 4 in the rig
    let upstream = quiet_upstream();
    let rig = rig_over(&upstream, SendRigOptions::new()).await;
    let chat = rig.prem_chat().await;
    let mut plan = turn("five");
    for index in 0..5 {
        let image = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
        plan = plan.attach(image, 10 + index);
    }
    let target = seed(&rig.quota.db, &chat, plan).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let body = assert_problem(&reply, 400, "out_of_range");
    assert_eq!(violation(&body, "image_count")["reason"], "TOO_MANY_IMAGES");
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
}

#[tokio::test]
async fn the_kill_switches_are_400_failed_precondition_with_feature_disabled() {
    // inst-mpf-return-quota (web_search), inst-mpf-image-guards (images)
    let mut switched = default_catalog();
    switched.kill_switches.disable_web_search = true;
    switched.kill_switches.disable_images = true;
    let upstream = quiet_upstream();
    let rig = rig_over(&upstream, SendRigOptions::new().with_snapshot(switched)).await;
    let chat = rig.prem_chat().await;
    let web = seed(&rig.quota.db, &chat, turn("web").at(10).web_search()).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, web.request_id).await;
    let body = assert_problem(&reply, 400, "failed_precondition");
    assert_eq!(body["context"]["violations"][0]["subject"], "web_search");
    assert_eq!(body["context"]["violations"][0]["type"], "FEATURE_DISABLED");
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;

    let chat = rig.prem_chat().await;
    let image = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
    let with_image = seed(&rig.quota.db, &chat, turn("img").attach(image, 10)).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, with_image.request_id).await;
    let body = assert_problem(&reply, 400, "failed_precondition");
    assert_eq!(body["context"]["violations"][0]["subject"], "images");
    assert_eq!(body["context"]["violations"][0]["type"], "FEATURE_DISABLED");
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
}

#[tokio::test]
async fn an_exhausted_quota_is_a_429_with_the_scope_and_the_previous_answer_intact() {
    // inst-mpf-return-quota: scope `tokens`, `web_search` and `code_interpreter`
    let upstream = quiet_upstream();
    let rig = rig_over(&upstream, SendRigOptions::new()).await;
    rig.gateway.set_limits((1, 1), (1, 1));
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q").answer("keep me")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let body = assert_problem(&reply, 429, "resource_exhausted");
    assert_eq!(body["context"]["violations"][0]["subject"], "tokens");
    assert_eq!(
        body["context"]["violations"][0]["description"],
        "quota_exceeded"
    );
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
    // the previous turn and its answer are intact
    let live = live_messages(&rig.quota.db, chat.id).await;
    assert_eq!(live.len(), 2);
    assert_eq!(live[1].content, "keep me");
    assert!(
        turn_by_request(&rig.quota.db, chat.id, target.request_id)
            .await
            .deleted_at
            .is_none()
    );

    let upstream = quiet_upstream();
    let rig = rig_over(&upstream, SendRigOptions::new()).await;
    seed_rows(
        &rig.quota.db,
        &[row("daily", DAY, "total").web(75).code(50)],
    )
    .await;
    let chat = rig.std_chat().await;
    let web = seed(&rig.quota.db, &chat, turn("web").web_search()).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, web.request_id).await;
    let body = assert_problem(&reply, 429, "resource_exhausted");
    assert_eq!(body["context"]["violations"][0]["subject"], "web_search");
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;

    let chat = rig.std_chat().await;
    seed_att(
        &rig.quota.db,
        &chat,
        AttachmentSeed::ready("document")
            .no_file_search()
            .code_interpreter(),
    )
    .await;
    let plain = seed(&rig.quota.db, &chat, turn("calc")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, plain.request_id).await;
    let body = assert_problem(&reply, 429, "resource_exhausted");
    assert_eq!(
        body["context"]["violations"][0]["subject"],
        "code_interpreter"
    );
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
}

#[tokio::test]
async fn a_content_above_the_input_limit_is_a_400_input_too_long_before_the_commit() {
    // inst-mpf-return-input-too-long
    let upstream = quiet_upstream();
    let rig = rig_over(
        &upstream,
        rig_catalog(&json!({ "max_input_tokens": 50, "estimation_budgets": byte_budgets() })),
    )
    .await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn(&"x".repeat(200))).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let body = assert_problem(&reply, 400, "out_of_range");
    assert_eq!(violation(&body, "content")["reason"], "INPUT_TOO_LONG");
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
}

#[tokio::test]
async fn carried_images_on_a_model_without_vision_are_a_400_vision_not_supported() {
    // inst-mpf-return-quota / inst-mpf-image-guards: images are never dropped to pass
    let upstream = quiet_upstream();
    let rig = rig_over(&upstream, SendRigOptions::new()).await;
    let chat = rig.std_chat().await;
    let image = seed_att(&rig.quota.db, &chat, AttachmentSeed::ready("image")).await;
    let target = seed(&rig.quota.db, &chat, turn("see").attach(image, 10)).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    assert_invalid_argument(&reply, 400, "content_type", "VISION_NOT_SUPPORTED");
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
}

#[tokio::test]
async fn a_policy_plugin_failure_is_a_generic_500_and_nothing_changes() {
    // inst-mpf-return-policy-500
    let upstream = quiet_upstream();
    let rig = rig_over(&upstream, SendRigOptions::new()).await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    rig.gateway
        .fail_current
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let body = assert_internal(&reply);
    assert!(
        !body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("provider-secret-detail"),
        "{body}"
    );
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
}

#[tokio::test]
async fn a_retry_through_a_premium_chat_keeps_the_premium_model() {
    let (rig, _chat, _upstream) = streaming_chat().await;
    let chat = rig.prem_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let frames = sse_of(&reply);
    let done = frames.last().unwrap();
    assert_eq!(done.event, "done");
    assert_eq!(done.data["selected_model"], PREMIUM);
}

#[tokio::test]
async fn a_failed_retry_stream_ends_with_one_terminal_error_event_as_on_the_send_path() {
    // inst-retry-stream: failed after the stream opened; one terminal `error` with a streaming code
    let upstream = MockProvider::answering(Reply::http(
        500,
        &json!({ "error": { "message": "the provider is not available" } }),
    ));
    let rig = rig_over(&upstream, SendRigOptions::new()).await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    let frames = sse_of(&reply);
    assert_eq!(
        names(&frames),
        vec!["stream_started", "error"],
        "{}",
        reply.text
    );
    assert_eq!(frames[1].data["code"], "provider_error");
    let (new_request, _) = started_ids(&frames);
    let new = turn_by_request(&rig.quota.db, chat.id, new_request).await;
    assert_eq!(new.state, "failed");
    assert_eq!(new.error_code.as_deref(), Some("provider_error"));
    // the settlement and the events are those of the send path
    assert_eq!(rig.events().await.len(), 1);
    assert_eq!(turn_events(&rig).await.len(), 1);
    assert_eq!(parse_sse(&reply.text).len(), 2);
}

// ── the commit rejections ────────────────────────────────────────────────────

#[tokio::test]
async fn a_retry_that_loses_the_commit_to_a_concurrent_send_is_a_409_and_changes_nothing() {
    // inst-retry-commit-rejected, inst-retry-return-commit: a newer turn committed after the
    // preview and the preflight
    let upstream = quiet_upstream();
    let hook = ArmedHook::new();
    let rig = rig_over(
        &upstream,
        SendRigOptions::new().with_commit_hook(hook.clone()),
    )
    .await;
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    hook.arm_before(vec![running_turn_sql(&chat, "2030-01-01T00:00:00Z")]);
    let reply = rig.retry(chat.id, target.request_id).await;
    let body = assert_conflict(&reply, "NOT_LATEST_TURN");
    assert!(
        !body["detail"]
            .as_str()
            .unwrap()
            .contains(&target.request_id.to_string())
    );
    // nothing of the mutation remains: only the concurrent turn is new
    let after = snapshot(&rig.quota.db, &chat).await;
    assert_eq!(after.messages, before.messages);
    assert_eq!(after.links, before.links);
    assert_eq!(after.chat_updated_at, before.chat_updated_at);
    assert_eq!(after.outbox, before.outbox);
    assert_eq!(after.turns.len(), before.turns.len() + 1);
    let old = turn_by_request(&rig.quota.db, chat.id, target.request_id).await;
    assert!(old.deleted_at.is_none());
    assert_eq!(old.replaced_by_request_id, None);
    assert!(mutation_events(&rig).await.is_empty());
    assert!(
        crate::test_support::quota::all_quota_rows(&rig.quota.db)
            .await
            .is_empty()
    );
    assert_eq!(upstream.call_count(), 0);
}

#[tokio::test]
async fn a_retry_that_loses_the_running_turn_index_is_a_409_generation_in_progress() {
    // inst-retry-return-commit: the replacement insert hits the one-running-turn index
    let (rig, chat, upstream) = streaming_chat().await;
    seed(&rig.quota.db, &chat, turn("stuck").at(10).running()).await;
    let target = seed(&rig.quota.db, &chat, turn("target").at(100)).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    assert_conflict(&reply, "GENERATION_IN_PROGRESS");
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
}

#[tokio::test]
async fn a_database_error_at_the_commit_is_a_generic_500_and_changes_nothing() {
    // inst-retry-commit-rejected, inst-retry-return-commit: "or 500 `internal`"
    let outbox = FailingMutationOutbox::internal();
    let upstream = quiet_upstream();
    let rig = rig_over(
        &upstream,
        SendRigOptions::new().with_turn_outbox(outbox.clone()),
    )
    .await;
    outbox.bind(rig.quota.enqueuer.clone());
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    assert_internal(&reply);
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
}

#[tokio::test]
async fn a_forced_audit_payload_overflow_is_a_500_and_the_previous_turn_stays() {
    // inst-mc-payload-too-large through the endpoint: not the 400 of chat deletion
    let outbox = FailingMutationOutbox::payload_too_large();
    let upstream = quiet_upstream();
    let rig = rig_over(
        &upstream,
        SendRigOptions::new().with_turn_outbox(outbox.clone()),
    )
    .await;
    outbox.bind(rig.quota.enqueuer.clone());
    let chat = rig.std_chat().await;
    let target = seed(&rig.quota.db, &chat, turn("q").answer("kept")).await;
    let before = snapshot(&rig.quota.db, &chat).await;
    let reply = rig.retry(chat.id, target.request_id).await;
    assert_internal(&reply);
    assert_untouched(&rig, &chat, &before, &upstream, &reply).await;
    assert_eq!(
        live_messages(&rig.quota.db, chat.id).await[1].content,
        "kept"
    );
}
