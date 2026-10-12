#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    TurnAuditEventType, TurnDeleteAudit, TurnMutationAuditEvent, TurnMutationEventType,
    TurnReplacementAudit,
};

#[test]
fn event_types_use_the_documented_wire_names() {
    assert_eq!(
        serde_json::to_value(TurnAuditEventType::Completed).unwrap(),
        json!("turn_completed")
    );
    assert_eq!(
        serde_json::to_value(TurnAuditEventType::Failed).unwrap(),
        json!("turn_failed")
    );
    for (event_type, name) in [
        (TurnMutationEventType::Retry, "turn_retry"),
        (TurnMutationEventType::Edit, "turn_edit"),
        (TurnMutationEventType::Delete, "turn_delete"),
    ] {
        assert_eq!(serde_json::to_value(event_type).unwrap(), json!(name));
    }
    assert!(serde_json::from_value::<TurnAuditEventType>(json!("turn_cancelled")).is_err());
}

fn keys(json: &serde_json::Value) -> BTreeSet<&str> {
    json.as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect()
}

fn replacement() -> TurnReplacementAudit {
    TurnReplacementAudit {
        timestamp: OffsetDateTime::UNIX_EPOCH,
        tenant_id: Uuid::new_v4(),
        actor_user_id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        original_request_id: Uuid::new_v4(),
        new_request_id: Uuid::new_v4(),
    }
}

#[test]
fn retry_and_edit_events_carry_original_and_new_request_ids() {
    for (event, name) in [
        (TurnMutationAuditEvent::Retry(replacement()), "turn_retry"),
        (TurnMutationAuditEvent::Edit(replacement()), "turn_edit"),
    ] {
        let json = serde_json::to_value(&event).unwrap();

        assert_eq!(
            keys(&json),
            BTreeSet::from([
                "event_type",
                "timestamp",
                "tenant_id",
                "actor_user_id",
                "chat_id",
                "original_request_id",
                "new_request_id",
            ])
        );
        assert_eq!(json["event_type"], json!(name));
        assert_eq!(
            serde_json::from_value::<TurnMutationAuditEvent>(json).unwrap(),
            event
        );
    }
}

#[test]
fn delete_event_carries_request_id_only() {
    let event = TurnMutationAuditEvent::Delete(TurnDeleteAudit {
        timestamp: OffsetDateTime::UNIX_EPOCH,
        tenant_id: Uuid::new_v4(),
        actor_user_id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
    });

    let json = serde_json::to_value(&event).unwrap();

    assert_eq!(
        keys(&json),
        BTreeSet::from([
            "event_type",
            "timestamp",
            "tenant_id",
            "actor_user_id",
            "chat_id",
            "request_id",
        ])
    );
    assert_eq!(json["event_type"], json!("turn_delete"));
    assert_eq!(
        serde_json::from_value::<TurnMutationAuditEvent>(json).unwrap(),
        event
    );
}

#[test]
fn mutation_event_accessors_follow_the_variant() {
    let replacement = replacement();
    let retry = TurnMutationAuditEvent::Retry(replacement.clone());
    assert_eq!(retry.event_type(), TurnMutationEventType::Retry);
    assert_eq!(retry.mutated_request_id(), replacement.original_request_id);

    let delete = TurnDeleteAudit {
        timestamp: OffsetDateTime::UNIX_EPOCH,
        tenant_id: Uuid::new_v4(),
        actor_user_id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
    };
    let event = TurnMutationAuditEvent::Delete(delete.clone());
    assert_eq!(event.event_type(), TurnMutationEventType::Delete);
    assert_eq!(event.mutated_request_id(), delete.request_id);
}
