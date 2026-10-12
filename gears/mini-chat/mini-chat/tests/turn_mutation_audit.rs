//! Audit events of the turn mutations (slice S5): `turn_retry`, `turn_edit` and `turn_delete` are
//! enqueued in the mutation transaction on the audit queue and delivered to the audit plugin; a
//! rejected mutation emits nothing; the events carry identities, never content.
//!
//! Source of truth: DESIGN 3.9 "Audit Events for Turn Mutations" (the three event types and their
//! required fields: `actor_user_id`, `chat_id`, `original_request_id` / `new_request_id` or
//! `request_id`, `timestamp`; "enqueued in the mutation transaction to the `mini-chat.audit`
//! outbox queue"; a retry/edit turn that fails setup after the commit emits no `turn_failed`
//! event), 3.2 "Audit plugin and audit outbox", ADR-0009, the SDK contract `AuditEvent`.

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

use mini_chat_sdk::{AuditEvent, AuditPluginError};
use mutation_support::{
    EDIT_TEXT, MutationHarness, MutationOptions, Op, PairSeed, ProviderReply, TENANT_A, USER_A,
    assert_exhausted, assert_not_latest, assert_problem, audit_of, within,
};
use time::OffsetDateTime;
use uuid::Uuid;

const Q: &str = "what is the capital of France";

fn keys_of(event: &AuditEvent) -> BTreeSet<String> {
    serde_json::to_value(event)
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

fn key_set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| (*s).to_owned()).collect()
}

fn index_of(events: &[AuditEvent], event_type: &str) -> usize {
    events
        .iter()
        .position(|e| e.event_type() == event_type)
        .unwrap_or_else(|| panic!("no {event_type} in {events:?}"))
}

// ═════════════════════════════════════════════════════════════════════════════
// the three event types
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_retry_emits_turn_retry_with_both_request_ids() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    let before = OffsetDateTime::now_utc();

    let sent = h.retried(chat, pair.request_id).await;
    let after = OffsetDateTime::now_utc();

    // the retry event and the finalization event of the new turn
    let events = h.audit.wait_events(2).await;
    h.audit.assert_events_stay(2).await;
    let retries = audit_of(&events, "turn_retry");
    assert_eq!(retries.len(), 1, "{events:?}");
    let AuditEvent::TurnRetry {
        tenant_id,
        actor_user_id,
        chat_id,
        original_request_id,
        new_request_id,
        timestamp,
    } = retries[0].clone()
    else {
        panic!("not a retry event: {:?}", retries[0]);
    };
    assert_eq!(tenant_id, TENANT_A);
    assert_eq!(actor_user_id, USER_A);
    assert_eq!(chat_id, chat);
    assert_eq!(original_request_id, pair.request_id);
    assert_eq!(new_request_id, sent.request_id());
    assert!(within(timestamp, before, after), "{timestamp}");
    assert_eq!(
        keys_of(&retries[0]),
        key_set(&[
            "event_type",
            "tenant_id",
            "actor_user_id",
            "chat_id",
            "original_request_id",
            "new_request_id",
            "timestamp"
        ])
    );
    assert!(
        index_of(&events, "turn_retry") < index_of(&events, "turn_completed"),
        "the mutation event is enqueued before the new turn finalizes: {events:?}"
    );
}

#[tokio::test]
async fn an_edit_emits_turn_edit_with_both_request_ids() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    let before = OffsetDateTime::now_utc();

    let sent = h.edited(chat, pair.request_id, EDIT_TEXT).await;
    let after = OffsetDateTime::now_utc();

    let events = h.audit.wait_events(2).await;
    h.audit.assert_events_stay(2).await;
    let edits = audit_of(&events, "turn_edit");
    assert_eq!(edits.len(), 1, "{events:?}");
    let AuditEvent::TurnEdit {
        tenant_id,
        actor_user_id,
        chat_id,
        original_request_id,
        new_request_id,
        timestamp,
    } = edits[0].clone()
    else {
        panic!("not an edit event: {:?}", edits[0]);
    };
    assert_eq!(tenant_id, TENANT_A);
    assert_eq!(actor_user_id, USER_A);
    assert_eq!(chat_id, chat);
    assert_eq!(original_request_id, pair.request_id);
    assert_eq!(new_request_id, sent.request_id());
    assert!(within(timestamp, before, after), "{timestamp}");
    assert!(audit_of(&events, "turn_retry").is_empty());
    assert_eq!(
        keys_of(&edits[0]),
        key_set(&[
            "event_type",
            "tenant_id",
            "actor_user_id",
            "chat_id",
            "original_request_id",
            "new_request_id",
            "timestamp"
        ])
    );
}

#[tokio::test]
async fn a_delete_emits_turn_delete_with_the_deleted_request_id() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    let before = OffsetDateTime::now_utc();

    assert_eq!(h.delete(chat, pair.request_id).await.status.as_u16(), 204);
    let after = OffsetDateTime::now_utc();

    let events = h.audit.wait_events(1).await;
    h.audit.assert_events_stay(1).await;
    let AuditEvent::TurnDelete {
        tenant_id,
        actor_user_id,
        chat_id,
        request_id,
        timestamp,
    } = events[0].clone()
    else {
        panic!("not a delete event: {:?}", events[0]);
    };
    assert_eq!(tenant_id, TENANT_A);
    assert_eq!(actor_user_id, USER_A);
    assert_eq!(chat_id, chat);
    assert_eq!(request_id, pair.request_id);
    assert!(within(timestamp, before, after), "{timestamp}");
    assert_eq!(
        keys_of(&events[0]),
        key_set(&[
            "event_type",
            "tenant_id",
            "actor_user_id",
            "chat_id",
            "request_id",
            "timestamp"
        ])
    );
    h.policy.assert_published_stays(0).await;
}

#[tokio::test]
async fn the_mutation_events_carry_identities_and_never_content() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h
        .seed_pair(
            chat,
            PairSeed::completed("SECRET-QUESTION-TEXT", "SECRET-ANSWER-TEXT"),
        )
        .await;
    h.oagw.push(ProviderReply::text(&["SECRET-NEW-ANSWER"]));
    h.edited(chat, pair.request_id, "SECRET-EDITED-TEXT").await;
    let events = h.audit.wait_events(2).await;
    let blob = serde_json::to_string(&events).unwrap();
    for secret in [
        "SECRET-QUESTION-TEXT",
        "SECRET-ANSWER-TEXT",
        "SECRET-NEW-ANSWER",
        "SECRET-EDITED-TEXT",
    ] {
        assert!(
            !blob.contains(secret),
            "audit events never carry content: {blob}"
        );
    }
}

#[tokio::test]
async fn every_mutation_of_a_chain_emits_its_own_event() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let first = h.say(chat, Q).await;
    let second = h.retried(chat, first.request_id()).await;
    let third = h.edited(chat, second.request_id(), EDIT_TEXT).await;
    assert_eq!(
        h.delete(chat, third.request_id()).await.status.as_u16(),
        204
    );

    // 3 finalizations + retry + edit + delete
    let events = h.audit.wait_events(6).await;
    h.audit.assert_events_stay(6).await;
    let kinds: Vec<&str> = events.iter().map(AuditEvent::event_type).collect();
    for kind in ["turn_retry", "turn_edit", "turn_delete"] {
        assert_eq!(kinds.iter().filter(|k| **k == kind).count(), 1, "{kinds:?}");
    }
    assert_eq!(kinds.iter().filter(|k| **k == "turn_completed").count(), 3);
}

// ═════════════════════════════════════════════════════════════════════════════
// nothing is emitted for a rejected mutation
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn rejected_mutations_emit_no_audit_event() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pairs = h.seed_history(chat, 2).await;

    for op in [Op::Retry, Op::Edit, Op::Delete] {
        // not the latest turn
        assert_not_latest(&h.reject(op, chat, pairs[0].request_id).await);
        // unknown turn
        assert_eq!(
            h.reject(op, chat, Uuid::new_v4()).await.status.as_u16(),
            404
        );
    }
    // empty content, and an exhausted quota
    let raw = h.edit(chat, pairs[1].request_id, "  ").await;
    assert_eq!(raw.into_resp().await.status.as_u16(), 400);
    h.policy
        .set_limits(mutation_support::LimitsSpec::new(0, 0, 0, 0));
    assert_exhausted(
        &h.reject(Op::Retry, chat, pairs[1].request_id).await,
        "tokens",
    );

    h.audit.assert_events_stay(0).await;
    h.policy.assert_published_stays(0).await;
}

// ═════════════════════════════════════════════════════════════════════════════
// the event is part of the mutation transaction
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_delete_whose_audit_event_cannot_be_enqueued_rolls_back() {
    let h = MutationHarness::with(MutationOptions {
        register_audit_queue: false,
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    let before = h.snapshot().await;

    let resp = h.delete(chat, pair.request_id).await;

    assert_problem(&resp, 500, "internal");
    assert_eq!(
        h.snapshot().await,
        before,
        "the soft delete rolled back with the event"
    );
    assert!(h.turn(pair.request_id).await.deleted_at.is_none());
}

#[tokio::test]
async fn a_retry_or_edit_whose_audit_event_cannot_be_enqueued_rolls_back() {
    for op in [Op::Retry, Op::Edit] {
        let h = MutationHarness::with(MutationOptions {
            register_audit_queue: false,
            ..Default::default()
        })
        .await;
        let chat = h.seed_chat().await;
        let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
        let before = h.snapshot().await;

        let resp = h.reject(op, chat, pair.request_id).await;

        assert_problem(&resp, 500, "internal");
        assert_eq!(
            h.snapshot().await,
            before,
            "{op:?}: no new turn, the old one is intact"
        );
        assert_eq!(h.turns_in(chat).await.len(), 1);
        h.oagw.assert_no_provider_call();
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// delivery
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_transient_plugin_failure_delays_the_mutation_event_but_does_not_lose_it() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;
    h.audit
        .script(vec![Err(AuditPluginError::Transient("busy".to_owned()))]);

    assert_eq!(h.delete(chat, pair.request_id).await.status.as_u16(), 204);

    let events = h.audit.wait_events(1).await;
    assert_eq!(events[0].event_type(), "turn_delete");
    assert!(
        h.audit.calls.load(std::sync::atomic::Ordering::SeqCst) >= 2,
        "the first delivery failed and was retried"
    );
}

#[tokio::test]
async fn a_delete_publishes_no_usage_event() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let pair = h.seed_pair(chat, PairSeed::completed(Q, "A")).await;

    assert_eq!(h.delete(chat, pair.request_id).await.status.as_u16(), 204);

    h.audit.wait_events(1).await;
    h.policy.assert_published_stays(0).await;
    assert_eq!(
        h.policy
            .publish_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}
