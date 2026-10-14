//! The attachment detail projection: required fields always, `error_code` only for `failed`,
//! `img_thumbnail` only for a `ready` image with stored thumbnail bytes, no summary field, no
//! internal or provider field, and the optional null fields omitted.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`;
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-detail-projection`; definition of
//! done `cpt-cf-mini-chat-dod-attachments-and-file-search-get-api`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use base64::Engine;
use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use super::dto::AttachmentDetailDto;
use crate::domain::attachments::{
    AttachmentKind, AttachmentRecord, AttachmentStatus, SecondaryStatus, StoredThumbnail,
};
use crate::test_support::attachments::key_set;
use crate::test_support::{TENANT_A, USER_A1};

const CREATED: i64 = 1_800_000_000;

fn record(status: AttachmentStatus, kind: AttachmentKind) -> AttachmentRecord {
    AttachmentRecord {
        id: Uuid::from_u128(0xAAAA_0000_0000_0000_0000_0000_0000_0001),
        tenant_id: TENANT_A,
        chat_id: Uuid::from_u128(0xCCCC_0000_0000_0000_0000_0000_0000_0001),
        uploaded_by_user_id: USER_A1,
        filename: "report.pdf".to_owned(),
        content_type: "application/pdf".to_owned(),
        size_bytes: 4321,
        storage_backend: "azure_openai".to_owned(),
        provider_file_id: Some("file-PROVIDER-SECRET".to_owned()),
        status,
        error_code: None,
        kind,
        for_file_search: kind == AttachmentKind::Document,
        for_code_interpreter: false,
        thumbnail: None,
        secondary_file_id: Some("file_ant_SECONDARY".to_owned()),
        secondary_status: SecondaryStatus::Uploaded,
        secondary_provider_kind: Some("anthropic".to_owned()),
        cleanup_status: Some("pending".to_owned()),
        cleanup_attempts: 3,
        created_at: OffsetDateTime::from_unix_timestamp(CREATED).unwrap(),
        updated_at: OffsetDateTime::from_unix_timestamp(CREATED + 60).unwrap(),
        deleted_at: None,
    }
}

fn dto(record: AttachmentRecord) -> Value {
    serde_json::to_value(AttachmentDetailDto::from(record)).expect("serializes")
}

fn thumb() -> StoredThumbnail {
    StoredThumbnail {
        bytes: vec![1, 2, 3, 4, 5, 250],
        width: 128,
        height: 64,
    }
}

#[test]
fn a_ready_document_carries_exactly_the_seven_required_fields() {
    // inst-proj-required, inst-proj-omit-null
    let value = dto(record(AttachmentStatus::Ready, AttachmentKind::Document));
    let expected: std::collections::BTreeSet<String> = [
        "id",
        "filename",
        "content_type",
        "size_bytes",
        "status",
        "kind",
        "created_at",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(key_set(&value), expected, "{value}");
}

#[test]
fn the_required_fields_have_their_values() {
    let value = dto(record(AttachmentStatus::Ready, AttachmentKind::Document));
    assert_eq!(value["id"], "aaaa0000-0000-0000-0000-000000000001");
    assert_eq!(value["filename"], "report.pdf");
    assert_eq!(value["content_type"], "application/pdf");
    assert_eq!(value["size_bytes"], 4321);
    assert_eq!(value["status"], "ready");
    assert_eq!(value["kind"], "document");
    let created = value["created_at"].as_str().unwrap();
    let parsed =
        OffsetDateTime::parse(created, &time::format_description::well_known::Rfc3339).unwrap();
    assert_eq!(parsed.unix_timestamp(), CREATED);
}

#[test]
fn every_status_and_kind_has_its_wire_string() {
    for (status, text) in [
        (AttachmentStatus::Pending, "pending"),
        (AttachmentStatus::Uploaded, "uploaded"),
        (AttachmentStatus::Ready, "ready"),
        (AttachmentStatus::Failed, "failed"),
    ] {
        assert_eq!(
            dto(record(status, AttachmentKind::Document))["status"],
            text
        );
    }
    assert_eq!(
        dto(record(AttachmentStatus::Ready, AttachmentKind::Image))["kind"],
        "image"
    );
}

#[test]
fn error_code_appears_only_for_a_failed_row() {
    // inst-proj-error-code
    for status in [
        AttachmentStatus::Pending,
        AttachmentStatus::Uploaded,
        AttachmentStatus::Ready,
    ] {
        let mut row = record(status, AttachmentKind::Document);
        row.error_code = Some("indexing_failed".to_owned());
        let value = dto(row);
        assert!(value.get("error_code").is_none(), "{status:?}: {value}");
    }
    let mut failed = record(AttachmentStatus::Failed, AttachmentKind::Document);
    failed.error_code = Some("upload_abandoned".to_owned());
    assert_eq!(dto(failed)["error_code"], "upload_abandoned");
}

#[test]
fn a_failed_row_without_a_code_omits_the_field() {
    let value = dto(record(AttachmentStatus::Failed, AttachmentKind::Document));
    assert!(value.get("error_code").is_none(), "{value}");
}

#[test]
fn a_ready_image_with_a_stored_thumbnail_carries_the_webp_object() {
    // inst-proj-thumbnail
    let mut row = record(AttachmentStatus::Ready, AttachmentKind::Image);
    row.thumbnail = Some(thumb());
    let value = dto(row);
    assert_eq!(
        value["img_thumbnail"],
        json!({
            "content_type": "image/webp",
            "width": 128,
            "height": 64,
            "data_base64": base64::engine::general_purpose::STANDARD.encode([1, 2, 3, 4, 5, 250]),
        })
    );
}

#[test]
fn the_thumbnail_is_omitted_unless_the_row_is_a_ready_image_with_bytes() {
    // a ready image without bytes
    let ready_without = dto(record(AttachmentStatus::Ready, AttachmentKind::Image));
    assert!(
        ready_without.get("img_thumbnail").is_none(),
        "{ready_without}"
    );
    for status in [
        AttachmentStatus::Pending,
        AttachmentStatus::Uploaded,
        AttachmentStatus::Failed,
    ] {
        let mut row = record(status, AttachmentKind::Image);
        row.thumbnail = Some(thumb());
        let value = dto(row);
        assert!(value.get("img_thumbnail").is_none(), "{status:?}: {value}");
    }
    let mut document = record(AttachmentStatus::Ready, AttachmentKind::Document);
    document.thumbnail = Some(thumb());
    assert!(dto(document).get("img_thumbnail").is_none());
}

#[test]
fn the_summary_fields_are_never_present() {
    // inst-proj-omit-null: doc_summary and summary_updated_at are always null (ADR-0007)
    for status in [AttachmentStatus::Ready, AttachmentStatus::Failed] {
        let value = dto(record(status, AttachmentKind::Document));
        assert!(value.get("doc_summary").is_none(), "{value}");
        assert!(value.get("summary_updated_at").is_none(), "{value}");
    }
}

#[test]
fn no_internal_or_provider_field_is_projected() {
    // inst-proj-no-internal
    let mut row = record(AttachmentStatus::Ready, AttachmentKind::Image);
    row.thumbnail = Some(thumb());
    let value = dto(row);
    for key in [
        "provider_file_id",
        "secondary_file_id",
        "secondary_status",
        "secondary_provider_kind",
        "storage_backend",
        "for_file_search",
        "for_code_interpreter",
        "cleanup_status",
        "cleanup_attempts",
        "last_cleanup_error",
        "uploaded_by_user_id",
        "tenant_id",
        "chat_id",
        "vector_store_id",
        "updated_at",
        "deleted_at",
    ] {
        assert!(value.get(key).is_none(), "`{key}` is projected: {value}");
    }
    let rendered = value.to_string();
    for secret in ["PROVIDER-SECRET", "SECONDARY", "azure_openai"] {
        assert!(!rendered.contains(secret), "`{secret}` leaked: {rendered}");
    }
}
