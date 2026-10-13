#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 5: the cleanup message rules of
//! `cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload` (`inst-secondary-ref-rule`).

use super::model::AttachmentKind;
use super::payload::{CleanupEvent, CleanupMessage, SecondaryRef};
use super::test_support::{stored, t0};

fn secondary() -> SecondaryRef {
    SecondaryRef {
        file_id: "file_ant_1".to_owned(),
        upstream_alias: "api.anthropic.test".to_owned(),
    }
}

const ALL: [CleanupEvent; 5] = [
    CleanupEvent::AttachmentDeleted,
    CleanupEvent::AttachmentUploadAbandoned,
    CleanupEvent::AttachmentIndexingFailed,
    CleanupEvent::AttachmentUploadFailed,
    CleanupEvent::AttachmentOrphanFile,
];

#[test]
fn the_event_types_have_their_wire_names() {
    let names: Vec<&str> = ALL.iter().map(|e| e.as_str()).collect();

    assert_eq!(
        names,
        [
            "attachment_deleted",
            "attachment_upload_abandoned",
            "attachment_indexing_failed",
            "attachment_upload_failed",
            "attachment_orphan_file"
        ]
    );
    for event in ALL {
        assert_eq!(CleanupEvent::parse(event.as_str()), Some(event));
    }
    assert_eq!(CleanupEvent::parse("attachment_exploded"), None);
    assert_eq!(CleanupEvent::parse(""), None);
}

#[test]
fn a_message_carries_the_identity_of_its_row() {
    let row = stored(|r| {
        r.kind = AttachmentKind::Image;
        r.storage_backend = "azure".to_owned();
    });

    let message = CleanupMessage::for_row(
        CleanupEvent::AttachmentDeleted,
        &row,
        row.provider_file_id.clone(),
        None,
        t0(),
    );

    assert_eq!(message.event, CleanupEvent::AttachmentDeleted);
    assert_eq!(message.tenant_id, row.tenant_id);
    assert_eq!(message.chat_id, row.chat_id);
    assert_eq!(message.attachment_id, row.id);
    assert_eq!(message.provider_file_id.as_deref(), Some("file-1"));
    assert_eq!(message.storage_backend, "azure");
    assert_eq!(message.kind, AttachmentKind::Image);
    assert_eq!(message.deleted_at, t0());
    assert_eq!(message.secondary_ref, None);
}

#[test]
fn the_provider_file_of_the_message_can_differ_from_the_row() {
    let row = stored(|r| r.provider_file_id = None);

    let message = CleanupMessage::for_row(
        CleanupEvent::AttachmentUploadFailed,
        &row,
        Some("file-in-memory".to_owned()),
        None,
        t0(),
    );

    assert_eq!(message.provider_file_id.as_deref(), Some("file-in-memory"));
}

#[test]
fn a_secondary_reference_is_kept_only_for_deleted_and_upload_failed() {
    let row = stored(|_| {});

    for event in ALL {
        let message = CleanupMessage::for_row(
            event,
            &row,
            Some("file-1".to_owned()),
            Some(secondary()),
            t0(),
        );

        let carries = matches!(
            event,
            CleanupEvent::AttachmentDeleted | CleanupEvent::AttachmentUploadFailed
        );
        assert_eq!(
            message.secondary_ref.is_some(),
            carries,
            "{}",
            event.as_str()
        );
    }
}

#[test]
fn a_deleted_message_of_a_row_that_never_reached_the_provider_has_no_file() {
    let row = stored(|r| r.provider_file_id = None);

    let message = CleanupMessage::for_row(CleanupEvent::AttachmentDeleted, &row, None, None, t0());

    assert_eq!(message.provider_file_id, None);
}

#[test]
fn an_orphan_message_names_the_file_and_never_a_secondary_copy() {
    let row = stored(|r| {
        r.provider_file_id = None;
        r.storage_backend = "openai".to_owned();
    });

    let message = CleanupMessage::orphan(&row, "file-lost", t0());

    assert_eq!(message.event, CleanupEvent::AttachmentOrphanFile);
    assert_eq!(message.provider_file_id.as_deref(), Some("file-lost"));
    assert_eq!(message.secondary_ref, None);
    assert_eq!(message.attachment_id, row.id);
    assert_eq!(message.tenant_id, row.tenant_id);
    assert_eq!(message.chat_id, row.chat_id);
    assert_eq!(message.storage_backend, "openai");
}
