use serde_json::json;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    AttachmentCleanupEventType, AttachmentCleanupPayload, CHAT_CLEANUP_REASON, ChatCleanupPayload,
    SecondaryRef, THREAD_SUMMARY_TASK_TYPE, ThreadSummaryPayload,
};

fn t(unix: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix).unwrap()
}

#[test]
fn attachment_cleanup_wire_shape() {
    let (tenant, chat, att) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let p = AttachmentCleanupPayload {
        event_type: AttachmentCleanupEventType::AttachmentUploadAbandoned,
        tenant_id: tenant,
        chat_id: chat,
        attachment_id: att,
        provider_file_id: None,
        vector_store_id: None,
        storage_backend: "openai".to_owned(),
        attachment_kind: "image".to_owned(),
        deleted_at: t(1_700_000_000),
        secondary_ref: Some(SecondaryRef {
            file_id: "file_01".to_owned(),
            provider_kind: "anthropic".to_owned(),
            upstream_alias: "anthropic-main".to_owned(),
        }),
    };
    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(
        v,
        json!({
            "event_type": "attachment_upload_abandoned",
            "tenant_id": tenant,
            "chat_id": chat,
            "attachment_id": att,
            "provider_file_id": null,
            "vector_store_id": null,
            "storage_backend": "openai",
            "attachment_kind": "image",
            "deleted_at": "2023-11-14T22:13:20Z",
            "secondary_ref": {
                "file_id": "file_01",
                "provider_kind": "anthropic",
                "upstream_alias": "anthropic-main"
            }
        })
    );
    let back: AttachmentCleanupPayload = serde_json::from_value(v).unwrap();
    assert_eq!(back, p);
}

#[test]
fn attachment_cleanup_event_types() {
    for (ty, wire) in [
        (
            AttachmentCleanupEventType::AttachmentDeleted,
            "attachment_deleted",
        ),
        (
            AttachmentCleanupEventType::AttachmentUploadAbandoned,
            "attachment_upload_abandoned",
        ),
        (
            AttachmentCleanupEventType::AttachmentIndexingFailed,
            "attachment_indexing_failed",
        ),
    ] {
        assert_eq!(serde_json::to_value(ty).unwrap(), json!(wire));
    }
}

#[test]
fn chat_cleanup_new_sets_reason_and_fresh_system_request_id() {
    let (tenant, chat) = (Uuid::new_v4(), Uuid::new_v4());
    let a = ChatCleanupPayload::new(tenant, chat, t(1_700_000_000));
    let b = ChatCleanupPayload::new(tenant, chat, t(1_700_000_000));
    assert_eq!(a.reason, CHAT_CLEANUP_REASON);
    assert_eq!(CHAT_CLEANUP_REASON, "chat_soft_delete");
    assert_ne!(a.system_request_id, b.system_request_id);
    assert_eq!(a.system_request_id.get_version_num(), 4);
    let v = serde_json::to_value(&a).unwrap();
    assert_eq!(
        v,
        json!({
            "tenant_id": tenant,
            "chat_id": chat,
            "system_request_id": a.system_request_id,
            "reason": "chat_soft_delete",
            "chat_deleted_at": "2023-11-14T22:13:20Z"
        })
    );
    assert_eq!(serde_json::from_value::<ChatCleanupPayload>(v).unwrap(), a);
}

#[test]
fn thread_summary_wire_shape_with_and_without_base() {
    let (tenant, chat, target, base) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let first = ThreadSummaryPayload::new(tenant, chat, None, (t(1_700_000_100), target));
    assert_eq!(first.system_task_type, THREAD_SUMMARY_TASK_TYPE);
    assert_eq!(first.system_request_id.get_version_num(), 4);
    let v = serde_json::to_value(&first).unwrap();
    assert_eq!(
        v,
        json!({
            "tenant_id": tenant,
            "chat_id": chat,
            "system_request_id": first.system_request_id,
            "base_frontier_created_at": null,
            "base_frontier_message_id": null,
            "frozen_target_created_at": "2023-11-14T22:15:00Z",
            "frozen_target_message_id": target,
            "system_task_type": "thread_summary_update"
        })
    );
    assert_eq!(
        serde_json::from_value::<ThreadSummaryPayload>(v).unwrap(),
        first
    );

    let next = ThreadSummaryPayload::new(
        tenant,
        chat,
        Some((t(1_700_000_000), base)),
        (t(1_700_000_100), target),
    );
    let v = serde_json::to_value(&next).unwrap();
    assert_eq!(v["base_frontier_created_at"], json!("2023-11-14T22:13:20Z"));
    assert_eq!(v["base_frontier_message_id"], json!(base));
    assert_eq!(next.base_frontier(), Some((t(1_700_000_000), base)));
    assert_eq!(next.frozen_target(), (t(1_700_000_100), target));
    assert_eq!(
        serde_json::from_value::<ThreadSummaryPayload>(v).unwrap(),
        next
    );
}

#[test]
fn half_set_base_frontier_is_treated_as_absent() {
    let mut p = ThreadSummaryPayload::new(
        Uuid::new_v4(),
        Uuid::new_v4(),
        None,
        (t(1_700_000_100), Uuid::new_v4()),
    );
    p.base_frontier_message_id = Some(Uuid::new_v4());
    assert_eq!(p.base_frontier(), None);
}
