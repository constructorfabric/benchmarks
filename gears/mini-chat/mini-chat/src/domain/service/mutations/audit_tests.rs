#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The mutation audit payloads: the three event types with their fields, the
//! encoded wire shape and the absence of any content.

use mini_chat_sdk::{MiniChatAuditEvent, TurnMutationAuditEvent, encode_audit_event};
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{AuditInput, mutation_audit_event};
use crate::domain::error::DomainError;
use crate::domain::service::mutations::types::MutationOp;

const TENANT: Uuid = Uuid::from_u128(1);
const ACTOR: Uuid = Uuid::from_u128(2);
const CHAT: Uuid = Uuid::from_u128(3);
const OLD: Uuid = Uuid::from_u128(4);
const NEW: Uuid = Uuid::from_u128(5);

fn at() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
}

fn input(op: MutationOp, new_request_id: Option<Uuid>) -> AuditInput {
    AuditInput {
        op,
        tenant_id: TENANT,
        actor_user_id: ACTOR,
        chat_id: CHAT,
        target_request_id: OLD,
        new_request_id,
        at: at(),
    }
}

fn wire(event: TurnMutationAuditEvent) -> Value {
    let bytes = encode_audit_event(&MiniChatAuditEvent::Mutation(event)).unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[test]
fn a_retry_is_a_turn_retry_with_the_original_and_the_new_request() {
    let event = mutation_audit_event(&input(MutationOp::Retry, Some(NEW))).unwrap();

    assert_eq!(
        event,
        TurnMutationAuditEvent::TurnRetry {
            tenant_id: TENANT,
            actor_user_id: ACTOR,
            chat_id: CHAT,
            original_request_id: OLD,
            new_request_id: NEW,
            timestamp: at(),
        }
    );
}

#[test]
fn an_edit_is_a_turn_edit_with_the_same_fields() {
    let event = mutation_audit_event(&input(MutationOp::Edit, Some(NEW))).unwrap();

    assert_eq!(
        event,
        TurnMutationAuditEvent::TurnEdit {
            tenant_id: TENANT,
            actor_user_id: ACTOR,
            chat_id: CHAT,
            original_request_id: OLD,
            new_request_id: NEW,
            timestamp: at(),
        }
    );
}

#[test]
fn a_delete_is_a_turn_delete_with_the_deleted_request_and_no_new_one() {
    let event = mutation_audit_event(&input(MutationOp::Delete, None)).unwrap();

    assert_eq!(
        event,
        TurnMutationAuditEvent::TurnDelete {
            tenant_id: TENANT,
            actor_user_id: ACTOR,
            chat_id: CHAT,
            request_id: OLD,
            timestamp: at(),
        }
    );
}

#[test]
fn a_delete_ignores_a_new_request_id() {
    let event = mutation_audit_event(&input(MutationOp::Delete, Some(NEW))).unwrap();

    assert!(
        matches!(event, TurnMutationAuditEvent::TurnDelete { request_id, .. } if request_id == OLD)
    );
}

#[test]
fn a_retry_or_an_edit_without_a_new_request_id_is_an_internal_error() {
    for op in [MutationOp::Retry, MutationOp::Edit] {
        assert!(
            mutation_audit_event(&input(op, Some(NEW))).is_ok(),
            "{op:?} with a new request id is fine"
        );
        let error = mutation_audit_event(&input(op, None)).unwrap_err();
        assert!(
            matches!(error, DomainError::Internal(_)),
            "{op:?}: {error:?}"
        );
    }
}

#[test]
fn the_encoded_retry_has_exactly_the_documented_fields() {
    let value = wire(mutation_audit_event(&input(MutationOp::Retry, Some(NEW))).unwrap());

    assert_eq!(
        value,
        json!({
            "event_type": "turn_retry",
            "tenant_id": TENANT,
            "actor_user_id": ACTOR,
            "chat_id": CHAT,
            "original_request_id": OLD,
            "new_request_id": NEW,
            "timestamp": "2027-01-15T08:00:00Z",
        })
    );
}

#[test]
fn the_encoded_edit_and_delete_name_their_event_types() {
    let edit = wire(mutation_audit_event(&input(MutationOp::Edit, Some(NEW))).unwrap());
    let delete = wire(mutation_audit_event(&input(MutationOp::Delete, None)).unwrap());

    assert_eq!(edit["event_type"], "turn_edit");
    assert_eq!(delete["event_type"], "turn_delete");
    assert_eq!(delete["request_id"], json!(OLD));
    assert!(delete.get("new_request_id").is_none());
    assert!(delete.get("original_request_id").is_none());
}

#[test]
fn no_payload_carries_content_or_provider_identifiers() {
    for (op, new) in [
        (MutationOp::Retry, Some(NEW)),
        (MutationOp::Edit, Some(NEW)),
        (MutationOp::Delete, None),
    ] {
        let value = wire(mutation_audit_event(&input(op, new)).unwrap());
        let keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        for forbidden in [
            "content",
            "prompt",
            "response",
            "provider_response_id",
            "attachments",
        ] {
            assert!(!keys.contains(&forbidden), "{op:?} carries `{forbidden}`");
        }
    }
}
