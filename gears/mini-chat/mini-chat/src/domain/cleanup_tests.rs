#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;
use uuid::Uuid;

use super::{AttachmentCleanupEvent, AttachmentCleanupPayload};

fn payload(event: &str) -> serde_json::Value {
    json!({
        "event_type": event,
        "tenant_id": Uuid::new_v4(),
        "chat_id": Uuid::new_v4(),
        "attachment_id": Uuid::new_v4(),
        "provider_file_id": "file-1",
        "vector_store_id": null,
        "storage_backend": "openai",
        "attachment_kind": "document",
        "deleted_at": "2026-01-02T00:00:00Z",
        "secondary_ref": null,
    })
}

#[test]
fn the_three_event_types_use_their_wire_names() {
    for (name, event) in [
        ("attachment_deleted", AttachmentCleanupEvent::Deleted),
        (
            "attachment_upload_abandoned",
            AttachmentCleanupEvent::UploadAbandoned,
        ),
        (
            "attachment_indexing_failed",
            AttachmentCleanupEvent::IndexingFailed,
        ),
    ] {
        let parsed: AttachmentCleanupPayload = serde_json::from_value(payload(name)).unwrap();
        assert_eq!(parsed.event_type, event);
        let written = serde_json::to_value(&parsed).unwrap();
        assert_eq!(written["event_type"], json!(name));
    }
}

#[test]
fn the_payload_round_trips_with_its_secondary_reference() {
    let mut value = payload("attachment_deleted");
    value["secondary_ref"] = json!({
        "file_id": "file_011",
        "provider_kind": "anthropic",
        "upstream_alias": "anthropic.test",
    });

    let parsed: AttachmentCleanupPayload = serde_json::from_value(value.clone()).unwrap();

    assert_eq!(parsed.secondary_ref.as_ref().unwrap().file_id, "file_011");
    assert_eq!(
        serde_json::to_value(&parsed).unwrap()["secondary_ref"],
        value["secondary_ref"]
    );
}

#[test]
fn an_unknown_event_type_or_a_missing_member_is_malformed() {
    assert!(
        serde_json::from_value::<AttachmentCleanupPayload>(payload("attachment_moved")).is_err()
    );
    let mut missing = payload("attachment_deleted");
    missing.as_object_mut().unwrap().remove("storage_backend");
    assert!(serde_json::from_value::<AttachmentCleanupPayload>(missing).is_err());
    assert!(
        serde_json::from_value::<AttachmentCleanupPayload>(
            json!({ "event_type": "attachment_deleted" })
        )
        .is_err()
    );
}
