#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::audit::{MiniChatAuditEvent, TurnMutationAuditEvent};

#[test]
fn audit_event_is_tagged_with_snake_case_kind() {
    let event = MiniChatAuditEvent::Mutation(TurnMutationAuditEvent {
        event_type: "turn_retry".to_owned(),
        tenant_id: Uuid::from_u128(1),
        chat_id: Uuid::from_u128(2),
        actor_user_id: Uuid::from_u128(3),
        original_request_id: Some(Uuid::from_u128(4)),
        new_request_id: None,
        request_id: None,
        timestamp: OffsetDateTime::from_unix_timestamp(1_772_445_600).unwrap(),
    });
    let v = serde_json::to_value(&event).unwrap();
    assert_eq!(v["kind"], "mutation");
    assert_eq!(v["event_type"], "turn_retry");
    assert_eq!(v["timestamp"], "2026-03-02T10:00:00Z");
    assert!(v["new_request_id"].is_null());
    assert_eq!(
        serde_json::from_value::<MiniChatAuditEvent>(v).unwrap(),
        event
    );

    let bad = json!({"kind": "other"});
    assert!(serde_json::from_value::<MiniChatAuditEvent>(bad).is_err());
}
