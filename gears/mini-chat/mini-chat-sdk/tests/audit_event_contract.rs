//! JSON contract of the audit events the audit outbox carries to the audit
//! plugin.
//!
//! Source of truth: DESIGN 3.9 "Audit Events for Turn Mutations" (required
//! fields of `turn_retry`, `turn_edit`, `turn_delete`) and PRD `cpt-cf-mini-chat-fr-audit`
//! (`event_type` values: `turn_completed`, `turn_failed`, `turn_retry`,
//! `turn_edit`, `turn_delete`; no `turn_cancelled`).
//!
//! The finalization events (`turn_completed` / `turn_failed`) carry many
//! fields that the design does not pin to names; this file therefore pins only
//! the discriminator for them (see the interface spec).

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

use mini_chat_sdk::AuditEvent;
use serde_json::{Value, json};
use uuid::Uuid;

fn retry_json() -> Value {
    json!({
        "event_type": "turn_retry",
        "tenant_id": Uuid::new_v4(),
        "actor_user_id": Uuid::new_v4(),
        "chat_id": Uuid::new_v4(),
        "original_request_id": Uuid::new_v4(),
        "new_request_id": Uuid::new_v4(),
        "timestamp": "2026-03-02T10:00:00Z",
    })
}

#[test]
fn turn_retry_requires_original_and_new_request_ids() {
    let v = retry_json();
    let ev: AuditEvent = serde_json::from_value(v.clone()).expect("turn_retry parses");
    assert_eq!(ev.event_type(), "turn_retry");
    assert_eq!(serde_json::to_value(&ev).unwrap(), v, "exact round trip");

    for missing in [
        "actor_user_id",
        "chat_id",
        "original_request_id",
        "new_request_id",
        "timestamp",
    ] {
        let mut broken = v.clone();
        broken.as_object_mut().unwrap().remove(missing);
        assert!(
            serde_json::from_value::<AuditEvent>(broken).is_err(),
            "turn_retry without `{missing}` must not deserialize"
        );
    }
}

#[test]
fn turn_edit_has_the_same_required_fields_as_retry() {
    let mut v = retry_json();
    v["event_type"] = json!("turn_edit");
    let ev: AuditEvent = serde_json::from_value(v.clone()).expect("turn_edit parses");
    assert_eq!(ev.event_type(), "turn_edit");
    assert_eq!(serde_json::to_value(&ev).unwrap(), v);
}

#[test]
fn turn_delete_carries_a_single_request_id() {
    let v = json!({
        "event_type": "turn_delete",
        "tenant_id": Uuid::new_v4(),
        "actor_user_id": Uuid::new_v4(),
        "chat_id": Uuid::new_v4(),
        "request_id": Uuid::new_v4(),
        "timestamp": "2026-03-02T10:00:00Z",
    });
    let ev: AuditEvent = serde_json::from_value(v.clone()).expect("turn_delete parses");
    assert_eq!(ev.event_type(), "turn_delete");
    assert_eq!(serde_json::to_value(&ev).unwrap(), v);

    let mut broken = v;
    broken.as_object_mut().unwrap().remove("request_id");
    assert!(serde_json::from_value::<AuditEvent>(broken).is_err());
}

#[test]
fn unknown_event_types_are_rejected_and_there_is_no_turn_cancelled() {
    for bad in ["turn_cancelled", "chat_deleted", "", "TURN_RETRY"] {
        let mut v = retry_json();
        v["event_type"] = json!(bad);
        assert!(
            serde_json::from_value::<AuditEvent>(v).is_err(),
            "event_type `{bad}` must be rejected"
        );
    }
}

#[test]
fn event_type_discriminator_values() {
    // The two finalization events are produced by the gear and consumed by the
    // plugin; both must expose their documented discriminator.
    assert_eq!(AuditEvent::TURN_COMPLETED, "turn_completed");
    assert_eq!(AuditEvent::TURN_FAILED, "turn_failed");
    assert_eq!(AuditEvent::TURN_RETRY, "turn_retry");
    assert_eq!(AuditEvent::TURN_EDIT, "turn_edit");
    assert_eq!(AuditEvent::TURN_DELETE, "turn_delete");
}
