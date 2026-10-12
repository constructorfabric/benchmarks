//! Turn mutations of the latest turn (slice S5): `POST /v1/chats/{id}/turns/{request_id}/retry`,
//! `PATCH /v1/chats/{id}/turns/{request_id}` and `DELETE /v1/chats/{id}/turns/{request_id}`: what
//! they do to the turn and its messages, the preconditions and the error table, authorization,
//! the request-id invariants and the registered operations.
//!
//! Source of truth: DESIGN 3.9 "Turn Mutation Rules" (rules 1 - 9, "Turn Mutation API Contracts"),
//! 3.3 "REST error mapping" (`NOT_LATEST_TURN`, `GENERATION_IN_PROGRESS`, `turn_state` / `STATE`),
//! 3.8 (authorization of the mutations), 3.3 "Turn Status API" (a soft-deleted turn is 404),
//! 4 "Idempotency Rules" (a soft-deleted key is `request_id_conflict`), and the OpenAPI operations
//! `mini_chat.retry_turn`, `mini_chat.edit_turn`, `mini_chat.delete_turn`.
//!
//! The pipeline half (quota, kill switches, context, settlement, attachments) is in
//! `turn_mutation_pipeline.rs`, the audit events and the summary rule in `turn_mutation_audit.rs`,
//! the races in `turn_mutation_concurrency.rs`.

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
mod mutation_support;
mod quota_support;
mod stream_support;

use std::collections::BTreeSet;

use mutation_support::{
    ALL_OPS, EDIT_TEXT, FakePdp, MutationHarness, MutationOptions, Op, PREMIUM, PairSeed,
    ProviderReply, TurnRowSeed, USER_A2, assert_conflict, assert_field_violation, assert_not_found,
    assert_not_latest, assert_problem, assert_turn_not_found, assert_turn_running,
    contains_provider_identifier, ctx_a, from_db, hex, second_user, tenant_b_user, ts,
};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

/// `(registry key, operation id, declared statuses, content type of the 200 answer)`.
type OperationCase = (&'static str, &'static str, &'static [u16], &'static str);

const Q: &str = "what is the capital of France";

/// Run `op` as `USER_A`, expect a JSON answer and check that nothing changed: no provider call,
/// no row, no reserve, no event.
async fn rejected_untouched(
    h: &MutationHarness,
    op: Op,
    chat: Uuid,
    rid: Uuid,
) -> mutation_support::Resp {
    let before = h.snapshot().await;
    let calls = h.oagw.request_count();
    let resp = h.reject(op, chat, rid).await;
    assert_eq!(
        h.snapshot().await,
        before,
        "{op:?}: a rejected mutation changes nothing"
    );
    assert_eq!(h.oagw.request_count(), calls, "{op:?}: no provider call");
    resp
}

fn key_set(value: &Value) -> BTreeSet<String> {
    value.as_object().unwrap().keys().cloned().collect()
}

// ═════════════════════════════════════════════════════════════════════════════
// retry
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_retry_replaces_the_latest_turn_and_streams_a_new_answer() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text(&["Paris."]));
    let first = h.say(chat, Q).await;
    let old = first.request_id();
    let old_assistant = first.message_id();
    h.oagw.push(ProviderReply::text(&["It is ", "Paris."]));
    let before = OffsetDateTime::now_utc() - time::Duration::seconds(1);

    let sent = h.retried(chat, old).await;

    sent.assert_well_formed();
    assert_eq!(sent.status.as_u16(), 200);
    assert_eq!(sent.names_no_ping().first().unwrap(), "stream_started");
    assert_eq!(sent.terminal().event, "done");
    assert_eq!(sent.started()["is_new_turn"], json!(true));
    assert_eq!(sent.text(), "It is Paris.");
    let new = sent.request_id();
    assert_ne!(new, old, "the server generates a new request id");
    assert_ne!(sent.message_id(), old_assistant);

    let old_turn = h.turn(old).await;
    assert_eq!(old_turn.state, "completed", "the old turn keeps its state");
    let deleted_at = from_db(old_turn.deleted_at.expect("the old turn is soft-deleted"));
    assert!(deleted_at >= before, "{deleted_at}");
    assert_eq!(old_turn.replaced_by_request_id, Some(new));
    let new_turn = h.turn(new).await;
    assert_eq!(new_turn.state, "completed");
    assert!(new_turn.deleted_at.is_none());
    assert_eq!(new_turn.replaced_by_request_id, None);
    assert_eq!(new_turn.assistant_message_id, Some(sent.message_id()));

    let rows = h.messages_in(chat).await;
    assert_eq!(rows.len(), 4, "the old pair stays in storage");
    for row in rows.iter().filter(|m| m.request_id == Some(old)) {
        assert!(row.deleted_at.is_some(), "{} is soft-deleted", row.role);
    }
    for row in rows.iter().filter(|m| m.request_id == Some(new)) {
        assert!(row.deleted_at.is_none(), "{} is live", row.role);
    }
    assert_eq!(
        h.live_texts(chat).await,
        vec![
            ("user".to_owned(), Q.to_owned()),
            ("assistant".to_owned(), "It is Paris.".to_owned())
        ]
    );
}

#[tokio::test]
async fn a_retry_resubmits_the_original_user_message() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let first = h.say(chat, Q).await;
    h.retried(chat, first.request_id()).await;

    let requests = h.oagw.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].conversation_texts(),
        vec![format!("user:{Q}")],
        "the retried turn is replaced, not part of its own history"
    );
    assert_eq!(requests[1].model(), requests[0].model());
}

#[tokio::test]
async fn a_new_turn_gets_a_server_generated_v4_request_id_and_fresh_message_ids() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let first = h.say(chat, Q).await;
    let second = h.retried(chat, first.request_id()).await;
    let third = h.edited(chat, second.request_id(), "again").await;
    for sent in [&second, &third] {
        assert_eq!(sent.request_id().get_version_num(), 4);
    }
    let ids: BTreeSet<Uuid> = [&first, &second, &third]
        .iter()
        .flat_map(|s| [s.request_id(), s.message_id()])
        .collect();
    assert_eq!(ids.len(), 6, "every request and message id is new");
}

#[tokio::test]
async fn a_failed_turn_can_be_retried() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::failed(Q)).await;

    let sent = h.retried(chat, pair.request_id).await;

    assert_eq!(sent.terminal().event, "done");
    let old = h.turn(pair.request_id).await;
    assert_eq!(
        old.state, "failed",
        "the failed turn keeps its state and code"
    );
    assert_eq!(old.error_code.as_deref(), Some("provider_error"));
    assert!(old.deleted_at.is_some());
    assert_eq!(old.replaced_by_request_id, Some(sent.request_id()));
    assert_eq!(
        h.live_texts(chat).await.first(),
        Some(&("user".to_owned(), Q.to_owned()))
    );
    assert_eq!(
        h.oagw.last_request().conversation_texts(),
        vec![format!("user:{Q}")]
    );
}

#[tokio::test]
async fn a_cancelled_turn_can_be_retried_and_its_partial_answer_goes_with_it() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::cancelled(Q, Some("Par"))).await;

    let sent = h.retried(chat, pair.request_id).await;

    assert_eq!(sent.terminal().event, "done");
    let rows = h.messages_in(chat).await;
    let old: Vec<_> = rows
        .iter()
        .filter(|m| m.request_id == Some(pair.request_id))
        .collect();
    assert_eq!(old.len(), 2);
    assert!(old.iter().all(|m| m.deleted_at.is_some()), "{old:?}");
    assert!(
        h.live_texts(chat)
            .await
            .iter()
            .all(|(_, text)| text != "Par"),
        "the partial answer is gone from the conversation"
    );
}

#[tokio::test]
async fn a_retry_is_a_normal_turn_stream_with_the_same_event_shapes_as_a_send() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let first = h.say(chat, Q).await;
    let retried = h.retried(chat, first.request_id()).await;

    assert_eq!(key_set(first.started()), key_set(retried.started()));
    assert_eq!(key_set(first.done()), key_set(retried.done()));
    let done = retried.done();
    assert_eq!(done["effective_model"], json!(PREMIUM));
    assert_eq!(done["selected_model"], json!(PREMIUM));
    assert_eq!(done["quota_decision"], json!("allow"));
    assert_eq!(done["usage"], first.done()["usage"]);
    assert!(done["quota_warnings"].is_array(), "{done}");
}

#[tokio::test]
async fn a_retry_bumps_the_chat_activity_time() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    let before = h.chat_updated_at(chat).await;
    h.retried(chat, pair.request_id).await;
    assert!(h.chat_updated_at(chat).await > before);
}

#[tokio::test]
async fn a_retried_turn_can_be_retried_again_and_only_the_last_one_is_live() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let first = h.say(chat, Q).await;
    let second = h.retried(chat, first.request_id()).await;
    let third = h.retried(chat, second.request_id()).await;

    let turns = h.turns_in(chat).await;
    assert_eq!(turns.len(), 3);
    assert_eq!(
        turns
            .iter()
            .map(|t| t.deleted_at.is_some())
            .collect::<Vec<_>>(),
        vec![true, true, false]
    );
    assert_eq!(turns[0].replaced_by_request_id, Some(second.request_id()));
    assert_eq!(turns[1].replaced_by_request_id, Some(third.request_id()));
    assert_eq!(h.live_texts(chat).await.len(), 2);
    assert_eq!(
        h.oagw.last_request().conversation_texts(),
        vec![format!("user:{Q}")],
        "the replaced turns are not part of the context"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// edit
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn an_edit_replaces_the_user_message_and_regenerates_the_answer() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let first = h.say(chat, Q).await;
    let old = first.request_id();
    h.oagw.push(ProviderReply::text(&["Lyon."]));

    let sent = h.edited(chat, old, "what is the capital of Italy").await;

    sent.assert_well_formed();
    assert_eq!(sent.terminal().event, "done");
    assert_eq!(sent.text(), "Lyon.");
    let new = sent.request_id();
    assert_ne!(new, old);
    assert_eq!(
        h.oagw.last_request().conversation_texts(),
        vec!["user:what is the capital of Italy".to_owned()],
        "the provider sees the edited text and not the original"
    );
    assert_eq!(
        h.live_texts(chat).await,
        vec![
            ("user".to_owned(), "what is the capital of Italy".to_owned()),
            ("assistant".to_owned(), "Lyon.".to_owned())
        ]
    );
    let old_user = h.user_message_of(chat, old).await;
    assert_eq!(old_user.content, Q, "the replaced message keeps its text");
    assert!(old_user.deleted_at.is_some());
    let old_turn = h.turn(old).await;
    assert!(old_turn.deleted_at.is_some());
    assert_eq!(old_turn.replaced_by_request_id, Some(new));
}

#[tokio::test]
async fn an_edit_stores_the_new_content_verbatim() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    let content = "  padded\n  text \n";
    let sent = h.edited(chat, pair.request_id, content).await;
    let stored = h.user_message_of(chat, sent.request_id()).await;
    assert_eq!(stored.content, content);
}

#[tokio::test]
async fn an_edit_of_a_failed_or_cancelled_turn_is_allowed() {
    let h = MutationHarness::new().await;
    let failed_chat = h.seed_chat().await;
    let failed = h.seed_pair(failed_chat, PairSeed::failed(Q)).await;
    let sent = h.edited(failed_chat, failed.request_id, "new words").await;
    assert_eq!(sent.terminal().event, "done");

    let cancelled_chat = h.seed_chat().await;
    let cancelled = h
        .seed_pair(cancelled_chat, PairSeed::cancelled(Q, Some("half")))
        .await;
    let sent = h
        .edited(cancelled_chat, cancelled.request_id, "other words")
        .await;
    assert_eq!(sent.terminal().event, "done");
    assert_eq!(
        h.live_texts(cancelled_chat).await.first(),
        Some(&("user".to_owned(), "other words".to_owned()))
    );
}

#[tokio::test]
async fn empty_or_blank_edit_content_is_400_empty_content_and_changes_nothing() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    let before = h.snapshot().await;
    for content in ["", " ", "   \n\t  \r\n", "\u{a0}\u{2003}"] {
        let raw = h.edit(chat, pair.request_id, content).await;
        assert!(!raw.is_sse());
        let resp = raw.into_resp().await;
        let problem = assert_problem(&resp, 400, "invalid_argument");
        assert_field_violation(&problem, "content", "EMPTY_CONTENT");
    }
    assert_eq!(h.snapshot().await, before);
    h.oagw.assert_no_provider_call();
}

#[tokio::test]
async fn edit_bodies_the_json_extractor_refuses_are_422_400_or_415() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    let uri = h.turn_uri(chat, pair.request_id);
    let before = h.snapshot().await;

    for payload in ["{}", r#"{"content": 5}"#, r#"{"content": null}"#, "[]", "5"] {
        let resp = h
            .raw(
                "PATCH",
                &uri,
                Some(&ctx_a()),
                Some("application/json"),
                payload,
            )
            .await
            .into_resp()
            .await;
        let problem = assert_problem(&resp, 422, "invalid_argument");
        assert_field_violation(&problem, "body", "invalid_json_body");
    }
    let resp = h
        .raw(
            "PATCH",
            &uri,
            Some(&ctx_a()),
            Some("application/json"),
            r#"{"content": "#,
        )
        .await
        .into_resp()
        .await;
    let problem = assert_problem(&resp, 400, "invalid_argument");
    assert_field_violation(&problem, "body", "json_syntax_error");
    for content_type in [None, Some("text/plain")] {
        let resp = h
            .raw(
                "PATCH",
                &uri,
                Some(&ctx_a()),
                content_type,
                r#"{"content": "x"}"#,
            )
            .await
            .into_resp()
            .await;
        let problem = assert_problem(&resp, 415, "invalid_argument");
        assert_field_violation(&problem, "body", "missing_json_content_type");
    }
    assert_eq!(h.snapshot().await, before);
    h.oagw.assert_no_provider_call();
}

#[tokio::test]
async fn an_edit_stream_has_the_same_event_shapes_as_a_send() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let first = h.say(chat, Q).await;
    let edited = h.edited(chat, first.request_id(), "different").await;
    edited.assert_well_formed();
    assert_eq!(key_set(first.started()), key_set(edited.started()));
    assert_eq!(key_set(first.done()), key_set(edited.done()));
    assert_eq!(edited.started()["is_new_turn"], json!(true));
}

// ═════════════════════════════════════════════════════════════════════════════
// delete
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_delete_is_204_without_a_body_and_hides_the_turn_everywhere() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let first = h.say(chat, Q).await;
    let rid = first.request_id();
    let calls = h.oagw.request_count();
    let before = OffsetDateTime::now_utc() - time::Duration::seconds(1);

    let resp = h.delete(chat, rid).await;

    assert_eq!(resp.status.as_u16(), 204, "{}", resp.raw);
    assert!(resp.raw.is_empty(), "no body: {}", resp.raw);
    let turn = h.turn(rid).await;
    assert!(from_db(turn.deleted_at.expect("soft-deleted")) >= before);
    assert_eq!(
        turn.replaced_by_request_id, None,
        "a delete has no replacement"
    );
    assert_eq!(turn.state, "completed");
    let rows = h.messages_in(chat).await;
    assert_eq!(rows.len(), 2, "soft delete: the rows stay for audit");
    assert!(rows.iter().all(|m| m.deleted_at.is_some()));
    assert_eq!(
        h.oagw.request_count(),
        calls,
        "a delete never calls the provider"
    );

    assert_turn_not_found(&h.turn_status(chat, rid, &ctx_a()).await);
    assert!(h.listed_messages(chat).await.is_empty());
    let detail = h.chat.get(&h.chat.chat_uri(chat), &ctx_a()).await;
    assert_eq!(detail.json()["message_count"], json!(0));
}

#[tokio::test]
async fn a_deleted_turn_is_not_part_of_the_next_context() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    h.seed_history(chat, 2).await;
    let third = h.say(chat, "question 3").await;
    h.delete(chat, third.request_id()).await;

    h.say(chat, "question 4").await;

    assert_eq!(
        h.oagw.last_request().conversation_texts(),
        vec![
            "user:question 1".to_owned(),
            "assistant:answer 1".to_owned(),
            "user:question 2".to_owned(),
            "assistant:answer 2".to_owned(),
            "user:question 4".to_owned()
        ]
    );
}

#[tokio::test]
async fn deleting_the_latest_turn_makes_the_previous_one_the_latest() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_history(chat, 2).await;

    assert_eq!(
        h.delete(chat, pairs[1].request_id).await.status.as_u16(),
        204
    );
    let resp = h.reject(Op::Delete, chat, pairs[1].request_id).await;
    assert_not_latest(&resp);

    let sent = h.retried(chat, pairs[0].request_id).await;
    assert_eq!(sent.terminal().event, "done");
    let first = h.turn(pairs[0].request_id).await;
    assert_eq!(first.replaced_by_request_id, Some(sent.request_id()));
}

#[tokio::test]
async fn failed_and_cancelled_turns_can_be_deleted() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let failed = h.seed_pair(chat, PairSeed::failed("q1").at(10)).await;
    let cancelled = h
        .seed_pair(chat, PairSeed::cancelled("q2", Some("half")).at(20))
        .await;

    assert_eq!(
        h.delete(chat, cancelled.request_id).await.status.as_u16(),
        204
    );
    assert_eq!(h.delete(chat, failed.request_id).await.status.as_u16(), 204);
    assert!(h.listed_messages(chat).await.is_empty());
    assert!(
        h.turns_in(chat)
            .await
            .iter()
            .all(|t| t.deleted_at.is_some())
    );
}

#[tokio::test]
async fn deleting_the_only_turn_leaves_a_chat_that_accepts_a_new_message() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let first = h.say(chat, Q).await;
    h.delete(chat, first.request_id()).await;

    let sent = h.say(chat, "fresh start").await;

    assert_eq!(sent.terminal().event, "done");
    assert_eq!(
        h.oagw.last_request().conversation_texts(),
        vec!["user:fresh start".to_owned()]
    );
    assert_eq!(h.live_texts(chat).await.len(), 2);
}

#[tokio::test]
async fn a_delete_settles_nothing_and_publishes_no_usage() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let first = h.say(chat, Q).await;
    h.policy.wait_published(1).await;
    let usage = h.snapshot().await.usage;
    assert!(!usage.is_empty());

    assert_eq!(
        h.delete(chat, first.request_id()).await.status.as_u16(),
        204
    );

    assert_eq!(
        h.snapshot().await.usage,
        usage,
        "the spent credits stay spent"
    );
    h.assert_nothing_reserved().await;
    h.policy.assert_published_stays(1).await;
}

// ═════════════════════════════════════════════════════════════════════════════
// preconditions (every operation)
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_turn_that_is_not_the_latest_is_409_not_latest_turn() {
    for op in ALL_OPS {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let pairs = h.seed_history(chat, 2).await;
        let resp = rejected_untouched(&h, op, chat, pairs[0].request_id).await;
        let problem = assert_not_latest(&resp);
        assert_eq!(
            problem["context"]["resource_type"],
            json!("gts.cf.core.mini_chat.turn.v1~")
        );
    }
}

#[tokio::test]
async fn an_already_soft_deleted_turn_is_409_not_latest_turn() {
    for op in ALL_OPS {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let pair = h
            .seed_pair(chat, PairSeed::completed(Q, "A").deleted())
            .await;
        let resp = rejected_untouched(&h, op, chat, pair.request_id).await;
        assert_not_latest(&resp);
    }
}

#[tokio::test]
async fn a_replaced_turn_cannot_be_mutated_again_by_its_old_request_id() {
    for op in ALL_OPS {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let first = h.say(chat, Q).await;
        h.retried(chat, first.request_id()).await;
        let resp = rejected_untouched(&h, op, chat, first.request_id()).await;
        assert_not_latest(&resp);
    }
}

#[tokio::test]
async fn a_deleted_turn_cannot_be_mutated_again() {
    for op in ALL_OPS {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let first = h.say(chat, Q).await;
        assert_eq!(
            h.delete(chat, first.request_id()).await.status.as_u16(),
            204
        );
        let resp = rejected_untouched(&h, op, chat, first.request_id()).await;
        assert_not_latest(&resp);
    }
}

#[tokio::test]
async fn a_running_target_is_400_failed_precondition_turn_state() {
    for op in ALL_OPS {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let (mut live, rid) = h.hold_running(chat, "still going").await;

        let resp = rejected_untouched(&h, op, chat, rid).await;
        assert_turn_running(&resp);
        assert_eq!(
            h.turn(rid).await.state,
            "running",
            "{op:?}: the turn goes on"
        );

        h.oagw.release();
        let rest = live.drain().await;
        assert_eq!(rest.last().unwrap().event, "done", "{op:?}");
        assert!(h.turn(rid).await.deleted_at.is_none());
    }
}

#[tokio::test]
async fn a_newer_running_turn_makes_the_target_not_the_latest() {
    // rule 1: NOT_LATEST_TURN, not the running-turn error and not GENERATION_IN_PROGRESS
    for op in ALL_OPS {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let old = h.seed_pair(chat, PairSeed::completed("older", "A")).await;
        let (mut live, _running) = h.hold_running(chat, "newer, still going").await;

        let resp = rejected_untouched(&h, op, chat, old.request_id).await;
        assert_not_latest(&resp);

        h.oagw.release();
        live.drain().await;
    }
}

#[tokio::test]
async fn the_running_check_comes_before_the_latest_turn_check() {
    // DESIGN 3.9 error table: "Target turn is still running (checked before the latest-turn
    // check)". An older turn that is still `running` and a newer terminal one: the target is
    // both not the latest and running.
    for op in ALL_OPS {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let stuck = h
            .insert_turn(TurnRowSeed::running(chat).started(ts(5)))
            .await;
        h.seed_pair(chat, PairSeed::completed("newer", "A").at(20))
            .await;
        let resp = rejected_untouched(&h, op, chat, stuck).await;
        assert_turn_running(&resp);
    }
}

#[tokio::test]
async fn an_unknown_turn_is_404() {
    for op in ALL_OPS {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
        let resp = rejected_untouched(&h, op, chat, Uuid::new_v4()).await;
        assert_turn_not_found(&resp);
    }
}

#[tokio::test]
async fn the_turn_of_another_chat_is_404() {
    for op in ALL_OPS {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let other = h.seed_chat().await;
        let elsewhere = h.seed_pair(other, PairSeed::completed(Q, "A")).await;
        h.seed_pair(chat, PairSeed::completed("here", "A")).await;
        let resp = rejected_untouched(&h, op, chat, elsewhere.request_id).await;
        assert_turn_not_found(&resp);
    }
}

#[tokio::test]
async fn an_unknown_or_deleted_chat_is_404() {
    for op in ALL_OPS {
        let h = MutationHarness::new().await;
        let unknown = rejected_untouched(&h, op, Uuid::new_v4(), Uuid::new_v4()).await;
        assert_not_found(&unknown, "chat");

        let deleted = h
            .chat
            .seed_chat(chat_support::ChatSeed::of_a().deleted())
            .await;
        let pair = h.seed_pair(deleted, PairSeed::completed(Q, "A")).await;
        let resp = rejected_untouched(&h, op, deleted, pair.request_id).await;
        assert_not_found(&resp, "chat");
    }
}

#[tokio::test]
async fn the_chat_of_another_user_or_tenant_is_404_and_stays_untouched() {
    for op in ALL_OPS {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
        let before = h.snapshot().await;
        for intruder in [second_user(), tenant_b_user()] {
            let resp = h.reject_op(op, chat, pair.request_id, &intruder).await;
            assert_not_found(&resp, "chat");
        }
        assert_eq!(h.snapshot().await, before, "{op:?}");
        h.oagw.assert_no_provider_call();
    }
}

#[tokio::test]
async fn a_turn_of_another_requester_is_403_permission_denied() {
    // DESIGN 3.9 rule 2: `requester_user_id` must match the subject
    for op in ALL_OPS {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let pair = h
            .seed_pair(chat, PairSeed::completed(Q, "A").requester(USER_A2))
            .await;
        let resp = rejected_untouched(&h, op, chat, pair.request_id).await;
        assert_problem(&resp, 403, "permission_denied");
    }
}

#[tokio::test]
async fn non_uuid_path_parameters_are_400_invalid_path_params() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    let prefix = &h.chat.prefix;
    let valid = pair.request_id;
    for (method, template, body) in [
        ("POST", "retry", None),
        ("PATCH", "", Some(json!({"content": "x"}))),
        ("DELETE", "", None),
    ] {
        let suffix = if template.is_empty() {
            String::new()
        } else {
            format!("/{template}")
        };
        for uri in [
            format!("{prefix}/v1/chats/not-a-uuid/turns/{valid}{suffix}"),
            format!("{prefix}/v1/chats/{chat}/turns/not-a-uuid{suffix}"),
        ] {
            let body_text = body.as_ref().map(ToString::to_string).unwrap_or_default();
            let content_type = body.as_ref().map(|_| "application/json");
            let resp = h
                .raw(method, &uri, Some(&ctx_a()), content_type, &body_text)
                .await
                .into_resp()
                .await;
            let problem = assert_problem(&resp, 400, "invalid_argument");
            chat_support::assert_violation_reason(&problem, "invalid_path_params");
        }
    }
    h.oagw.assert_no_provider_call();
}

// ═════════════════════════════════════════════════════════════════════════════
// authorization (DESIGN 3.8)
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn each_operation_asks_the_pdp_once_for_its_action_on_the_chat() {
    for op in ALL_OPS {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
        let before = h.pdp.recorded().len();

        match op {
            Op::Retry => assert_eq!(
                h.retried(chat, pair.request_id).await.terminal().event,
                "done"
            ),
            Op::Edit => assert_eq!(
                h.edited(chat, pair.request_id, EDIT_TEXT)
                    .await
                    .terminal()
                    .event,
                "done"
            ),
            Op::Delete => assert_eq!(h.delete(chat, pair.request_id).await.status.as_u16(), 204),
        }
        // let the stream's finalization run before counting
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let sent: Vec<_> = h.pdp.recorded().into_iter().skip(before).collect();
        assert_eq!(
            sent.len(),
            1,
            "{op:?}: one PDP evaluation (retry / edit reuse it for the preflight and the stream), got {:?}",
            sent.iter()
                .map(|r| r.action.name.clone())
                .collect::<Vec<_>>()
        );
        let request = &sent[0];
        assert_eq!(request.action.name, op.action());
        assert_eq!(request.resource.resource_type, chat_support::CHAT_PDP_TYPE);
        assert_eq!(request.resource.id, Some(chat));
        assert!(request.context.require_constraints);
    }
}

#[tokio::test]
async fn a_pdp_denial_is_403_authz_denied_and_nothing_happens() {
    for op in ALL_OPS {
        let h = MutationHarness::with(MutationOptions {
            pdp: FakePdp::deny("internal-policy-rule-42"),
            ..Default::default()
        })
        .await;
        let chat = h.seed_chat().await;
        let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
        let resp = rejected_untouched(&h, op, chat, pair.request_id).await;
        let problem = assert_problem(&resp, 403, "permission_denied");
        assert_eq!(problem["context"]["reason"], json!("AUTHZ_DENIED"));
        assert!(
            !resp.raw.contains("internal-policy-rule-42"),
            "the PDP reason stays server side: {}",
            resp.raw
        );
    }
}

#[tokio::test]
async fn an_unreachable_pdp_is_503_with_retry_after_and_nothing_happens() {
    for op in ALL_OPS {
        let h = MutationHarness::with(MutationOptions {
            pdp: FakePdp::unreachable("pdp-host-10.0.0.7 timed out"),
            ..Default::default()
        })
        .await;
        let chat = h.seed_chat().await;
        let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
        let resp = rejected_untouched(&h, op, chat, pair.request_id).await;
        assert_problem(&resp, 503, "service_unavailable");
        assert_eq!(
            resp.headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok()),
            Some("5"),
            "{op:?}"
        );
        assert!(!resp.raw.contains("10.0.0.7"), "{}", resp.raw);
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// request ids: the replaced key is dead, the new one is a normal turn
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_old_request_id_is_a_conflict_on_messages_stream_after_a_retry_edit_or_delete() {
    for op in ALL_OPS {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let first = h.say(chat, Q).await;
        let old = first.request_id();
        match op {
            Op::Retry => {
                h.retried(chat, old).await;
            }
            Op::Edit => {
                h.edited(chat, old, EDIT_TEXT).await;
            }
            Op::Delete => {
                assert_eq!(h.delete(chat, old).await.status.as_u16(), 204);
            }
        }
        let calls = h.oagw.request_count();
        let before = h.snapshot().await;

        let resp = h
            .reject_send(chat, json!({"content": Q, "request_id": old}))
            .await;

        assert_conflict(&resp, "request_id_conflict");
        assert_eq!(h.oagw.request_count(), calls, "{op:?}: no provider call");
        assert_eq!(h.snapshot().await, before, "{op:?}");
    }
}

#[tokio::test]
async fn the_new_request_id_is_replayed_like_any_completed_turn() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let first = h.say(chat, Q).await;
    h.oagw.push(ProviderReply::text(&["second answer"]));
    let retried = h.retried(chat, first.request_id()).await;
    h.policy.wait_published(2).await;
    let calls = h.oagw.request_count();
    let before = h.snapshot().await;

    let replay = h.say_with(chat, "ignored", retried.request_id()).await;

    assert_eq!(replay.started()["is_new_turn"], json!(false));
    assert_eq!(
        replay.started()["message_id"],
        json!(retried.message_id().to_string())
    );
    assert_eq!(replay.text(), "second answer");
    assert_eq!(h.oagw.request_count(), calls);
    assert_eq!(h.snapshot().await, before, "a replay writes nothing");
    h.policy.assert_published_stays(2).await;
}

#[tokio::test]
async fn the_turn_status_follows_the_mutation() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let first = h.say(chat, Q).await;
    let retried = h.retried(chat, first.request_id()).await;

    assert_turn_not_found(&h.turn_status(chat, first.request_id(), &ctx_a()).await);
    let status = h.turn_status(chat, retried.request_id(), &ctx_a()).await;
    assert_eq!(status.status.as_u16(), 200, "{}", status.raw);
    assert_eq!(status.json()["state"], json!("done"));
    assert_eq!(
        status.json()["assistant_message_id"],
        json!(retried.message_id().to_string())
    );
}

#[tokio::test]
async fn the_message_list_shows_only_the_replacement_turn() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let first = h.say(chat, Q).await;
    let retried = h.retried(chat, first.request_id()).await;

    let items = h.listed_messages(chat).await;
    assert_eq!(items.len(), 2, "{items:?}");
    let request_ids: BTreeSet<String> = items
        .iter()
        .map(|m| m["request_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        request_ids,
        BTreeSet::from([retried.request_id().to_string()])
    );
    let roles: BTreeSet<String> = items
        .iter()
        .map(|m| m["role"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        roles,
        BTreeSet::from(["user".to_owned(), "assistant".to_owned()])
    );
    let detail = h.chat.get(&h.chat.chat_uri(chat), &ctx_a()).await;
    assert_eq!(detail.json()["message_count"], json!(2));
}

// ═════════════════════════════════════════════════════════════════════════════
// wire hygiene and registered operations
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn mutation_errors_leak_no_internal_ids() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_history(chat, 2).await;
    let running = h
        .insert_turn(TurnRowSeed::running(chat).started(ts(30)))
        .await;
    let secrets: Vec<String> = pairs
        .iter()
        .flat_map(|p| [p.turn_id.to_string(), hex(p.turn_id)])
        .chain(pairs.iter().flat_map(|p| {
            [
                p.user_message.to_string(),
                p.assistant_message.unwrap().to_string(),
            ]
        }))
        .collect();
    for op in ALL_OPS {
        for target in [pairs[0].request_id, running] {
            let resp = h.reject(op, chat, target).await;
            assert!(resp.status.as_u16() >= 400, "{op:?}");
            for secret in &secrets {
                assert!(
                    !resp.raw.contains(secret),
                    "{op:?}: {} leaks {secret}",
                    resp.raw
                );
            }
            assert!(!contains_provider_identifier(&resp.raw), "{}", resp.raw);
        }
    }
}

#[tokio::test]
async fn the_mutation_operations_are_registered_with_stable_ids_auth_license_and_statuses() {
    let h = MutationHarness::new().await;
    let cases: [OperationCase; 3] = [
        (
            "POST:/mini-chat/v1/chats/{id}/turns/{request_id}/retry",
            "mini_chat.retry_turn",
            &[200, 400, 401, 403, 404, 409, 429, 500, 503],
            "text/event-stream",
        ),
        (
            "PATCH:/mini-chat/v1/chats/{id}/turns/{request_id}",
            "mini_chat.edit_turn",
            &[200, 400, 401, 403, 404, 409, 422, 429, 500, 503],
            "text/event-stream",
        ),
        (
            "DELETE:/mini-chat/v1/chats/{id}/turns/{request_id}",
            "mini_chat.delete_turn",
            &[204, 400, 401, 403, 404, 409, 500, 503],
            "",
        ),
    ];
    for (key, operation_id, statuses, content_type) in cases {
        let spec = h
            .chat
            .openapi
            .operation_specs
            .get(key)
            .unwrap_or_else(|| panic!("operation `{key}` is not registered"));
        assert_eq!(spec.operation_id.as_deref(), Some(operation_id), "{key}");
        assert!(spec.authenticated, "{key} requires authentication");
        assert!(
            spec.tags.iter().any(|t| t == "Mini Chat Turns"),
            "{key}: {:?}",
            spec.tags
        );
        let license = spec
            .license_requirement
            .as_ref()
            .unwrap_or_else(|| panic!("{key} must require a license feature"));
        assert_eq!(
            license.license_names,
            vec!["gts.cf.core.lic.feat.v1~cf.core.global.base.v1".to_owned()],
            "{key}"
        );
        let declared: Vec<u16> = spec.responses.iter().map(|r| r.status).collect();
        for status in statuses {
            assert!(
                declared.contains(status),
                "{key} declares {status}: {declared:?}"
            );
        }
        if !content_type.is_empty() {
            let ok = spec.responses.iter().find(|r| r.status == 200).unwrap();
            assert_eq!(ok.content_type, content_type, "{key}");
        }
    }
    let specs = &h.chat.openapi.operation_specs;
    let edit = specs
        .get("PATCH:/mini-chat/v1/chats/{id}/turns/{request_id}")
        .unwrap();
    assert!(edit.request_body.is_some(), "the edit body is declared");
    let retry = specs
        .get("POST:/mini-chat/v1/chats/{id}/turns/{request_id}/retry")
        .unwrap();
    assert!(retry.request_body.is_none(), "retry has no request body");
    let delete = specs
        .get("DELETE:/mini-chat/v1/chats/{id}/turns/{request_id}")
        .unwrap();
    assert!(delete.request_body.is_none(), "delete has no request body");
}

#[tokio::test]
async fn the_mutation_routes_need_authentication() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    // the in-process router has no auth middleware: a request without a security context is
    // refused by the handlers' extractor instead of reaching the services
    let before = h.snapshot().await;
    for (method, uri) in [
        ("POST", h.retry_uri(chat, pair.request_id)),
        ("PATCH", h.turn_uri(chat, pair.request_id)),
        ("DELETE", h.turn_uri(chat, pair.request_id)),
    ] {
        let resp = h
            .raw(
                method,
                &uri,
                None,
                Some("application/json"),
                r#"{"content":"x"}"#,
            )
            .await
            .into_resp()
            .await;
        assert!(
            resp.status.as_u16() >= 400,
            "{method} without a security context is refused, got {}",
            resp.status
        );
    }
    assert_eq!(h.snapshot().await, before);
    h.oagw.assert_no_provider_call();
}
