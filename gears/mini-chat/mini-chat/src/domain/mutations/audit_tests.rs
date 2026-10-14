//! The mutation audit payloads: the `turn_retry`, `turn_edit` and `turn_delete` field sets, the
//! `event_type` tags and the absence of any content, attachment, model or provider identifier.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: algorithm
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event` (`inst-ae-match`,
//! `inst-ae-retry`, `inst-ae-edit`, `inst-ae-delete`, `inst-ae-no-content`, `inst-ae-return`);
//! definition of done `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-audit-events`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use mini_chat_sdk::{AuditEvent, TurnDeleteAuditEvent, TurnMutationAuditEvent};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{MutationOp, build_mutation_audit};

const ACTOR: Uuid = Uuid::from_u128(0xA1);
const CHAT: Uuid = Uuid::from_u128(0xC4);
const TARGET: Uuid = Uuid::from_u128(0x7A);
const NEW: Uuid = Uuid::from_u128(0x4E);

fn when() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
}

fn keys(event: &AuditEvent) -> BTreeSet<String> {
    serde_json::to_value(event)
        .unwrap()
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

#[test]
fn a_retry_is_a_turn_retry_with_both_request_ids() {
    // inst-ae-retry
    let event =
        build_mutation_audit(MutationOp::Retry, ACTOR, CHAT, TARGET, Some(NEW), when()).unwrap();
    assert_eq!(
        event,
        AuditEvent::TurnRetry(TurnMutationAuditEvent {
            actor_user_id: ACTOR,
            chat_id: CHAT,
            original_request_id: TARGET,
            new_request_id: NEW,
            timestamp: when(),
        })
    );
}

#[test]
fn an_edit_is_a_turn_edit_with_the_same_fields_as_a_retry() {
    // inst-ae-edit
    let event =
        build_mutation_audit(MutationOp::Edit, ACTOR, CHAT, TARGET, Some(NEW), when()).unwrap();
    assert_eq!(
        event,
        AuditEvent::TurnEdit(TurnMutationAuditEvent {
            actor_user_id: ACTOR,
            chat_id: CHAT,
            original_request_id: TARGET,
            new_request_id: NEW,
            timestamp: when(),
        })
    );
}

#[test]
fn a_delete_is_a_turn_delete_with_the_target_request_id_only() {
    // inst-ae-delete
    let event =
        build_mutation_audit(MutationOp::Delete, ACTOR, CHAT, TARGET, None, when()).unwrap();
    assert_eq!(
        event,
        AuditEvent::TurnDelete(TurnDeleteAuditEvent {
            actor_user_id: ACTOR,
            chat_id: CHAT,
            request_id: TARGET,
            timestamp: when(),
        })
    );
}

#[test]
fn the_wire_payloads_carry_exactly_the_documented_fields() {
    // DESIGN 3.9 `Audit Events for Turn Mutations`
    let retry =
        build_mutation_audit(MutationOp::Retry, ACTOR, CHAT, TARGET, Some(NEW), when()).unwrap();
    let edit =
        build_mutation_audit(MutationOp::Edit, ACTOR, CHAT, TARGET, Some(NEW), when()).unwrap();
    let delete =
        build_mutation_audit(MutationOp::Delete, ACTOR, CHAT, TARGET, None, when()).unwrap();
    let set = |names: &[&str]| {
        names
            .iter()
            .map(|n| (*n).to_owned())
            .collect::<BTreeSet<_>>()
    };
    let mutation_keys = [
        "event_type",
        "actor_user_id",
        "chat_id",
        "original_request_id",
        "new_request_id",
        "timestamp",
    ];
    assert_eq!(keys(&retry), set(&mutation_keys));
    assert_eq!(keys(&edit), set(&mutation_keys));
    assert_eq!(
        keys(&delete),
        set(&[
            "event_type",
            "actor_user_id",
            "chat_id",
            "request_id",
            "timestamp"
        ])
    );
    let tags: Vec<String> = [&retry, &edit, &delete]
        .iter()
        .map(|event| {
            serde_json::to_value(event).unwrap()["event_type"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(tags, vec!["turn_retry", "turn_edit", "turn_delete"]);
}

#[test]
fn no_payload_carries_content_attachments_model_or_provider_identifiers() {
    // inst-ae-no-content
    for (op, new) in [
        (MutationOp::Retry, Some(NEW)),
        (MutationOp::Edit, Some(NEW)),
        (MutationOp::Delete, None),
    ] {
        let event = build_mutation_audit(op, ACTOR, CHAT, TARGET, new, when()).unwrap();
        let wire = serde_json::to_string(&event).unwrap();
        for forbidden in [
            "content",
            "prompt",
            "response",
            "attachments",
            "model",
            "provider",
            "tenant_id",
        ] {
            assert!(!wire.contains(forbidden), "`{forbidden}` in {wire}");
        }
    }
}

#[test]
fn a_retry_or_an_edit_without_a_new_request_id_is_an_internal_error() {
    for op in [MutationOp::Retry, MutationOp::Edit] {
        // the same input with a new request id is accepted
        build_mutation_audit(op, ACTOR, CHAT, TARGET, Some(NEW), when()).unwrap();
        let result = build_mutation_audit(op, ACTOR, CHAT, TARGET, None, when());
        assert!(
            matches!(
                result,
                Err(crate::domain::error::DomainError::Internal { .. })
            ),
            "{op:?}: {result:?}"
        );
    }
}

#[test]
fn the_timestamp_is_the_commit_time_as_given() {
    let event =
        build_mutation_audit(MutationOp::Delete, ACTOR, CHAT, TARGET, None, when()).unwrap();
    let AuditEvent::TurnDelete(delete) = event else {
        panic!("expected a turn_delete");
    };
    assert_eq!(delete.timestamp, when());
}
