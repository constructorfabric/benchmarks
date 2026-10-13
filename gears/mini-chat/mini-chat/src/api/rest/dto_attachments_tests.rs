#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 1: the wire shape of `AttachmentDetailDto`
//! (`cpt-cf-mini-chat-algo-attachments-project-detail`): optional keys are
//! omitted, never `null`; no internal column is ever exposed.

use std::collections::BTreeSet;

use base64::Engine as _;
use serde_json::{Value, json};

use super::AttachmentDetailDto;
use crate::domain::service::attachments::detail::project_detail;
use crate::domain::service::attachments::model::{AttachmentKind, AttachmentStatus, Thumbnail};
use crate::domain::service::attachments::test_support::{stored, t0};

fn wire(row: &crate::domain::service::attachments::model::StoredAttachment) -> Value {
    serde_json::to_value(AttachmentDetailDto::from(project_detail(row))).unwrap()
}

fn keys(value: &Value) -> BTreeSet<String> {
    value.as_object().unwrap().keys().cloned().collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

#[test]
fn a_document_has_exactly_the_required_keys() {
    let row = stored(|_| {});

    let json = wire(&row);

    assert_eq!(
        keys(&json),
        set(&[
            "id",
            "filename",
            "content_type",
            "size_bytes",
            "status",
            "kind",
            "created_at"
        ])
    );
    assert_eq!(json["id"], row.id.to_string());
    assert_eq!(json["filename"], "report.pdf");
    assert_eq!(json["content_type"], "application/pdf");
    assert_eq!(json["size_bytes"], 1234);
    assert_eq!(json["status"], "ready");
    assert_eq!(json["kind"], "document");
}

#[test]
fn created_at_is_rfc_3339_utc() {
    let json = wire(&stored(|r| {
        r.created_at = t0() + time::Duration::microseconds(250_001);
    }));

    assert_eq!(json["created_at"], "2026-03-10T12:00:00.250001Z");
}

#[test]
fn every_status_and_kind_has_its_lower_case_name() {
    for (status, name) in [
        (AttachmentStatus::Pending, "pending"),
        (AttachmentStatus::Uploaded, "uploaded"),
        (AttachmentStatus::Ready, "ready"),
        (AttachmentStatus::Failed, "failed"),
    ] {
        assert_eq!(wire(&stored(|r| r.status = status))["status"], name);
    }
    assert_eq!(
        wire(&stored(|r| r.kind = AttachmentKind::Image))["kind"],
        "image"
    );
}

#[test]
fn a_failed_row_adds_its_error_code_and_nothing_else() {
    let row = stored(|r| {
        r.status = AttachmentStatus::Failed;
        r.error_code = Some("upload_abandoned".to_owned());
    });

    let json = wire(&row);

    assert_eq!(json["error_code"], "upload_abandoned");
    assert_eq!(keys(&json).len(), 8);
}

#[test]
fn a_ready_image_adds_its_thumbnail_as_standard_padded_base64() {
    let bytes = vec![0xFF_u8, 0xFE, 0x01, 0x02, 0x03];
    let row = stored(|r| {
        r.kind = AttachmentKind::Image;
        r.thumbnail = Some(Thumbnail {
            bytes: bytes.clone(),
            width: 128,
            height: 64,
        });
    });

    let json = wire(&row);

    assert_eq!(
        json["img_thumbnail"],
        json!({
            "content_type": "image/webp",
            "width": 128,
            "height": 64,
            "data_base64": base64::engine::general_purpose::STANDARD.encode(&bytes)
        })
    );
    assert!(
        json["img_thumbnail"]["data_base64"]
            .as_str()
            .unwrap()
            .ends_with('=')
    );
}

#[test]
fn an_absent_optional_key_is_omitted_never_null() {
    let json = wire(&stored(|r| {
        r.kind = AttachmentKind::Image;
    }));

    let text = json.to_string();
    for key in [
        "error_code",
        "img_thumbnail",
        "doc_summary",
        "summary_updated_at",
    ] {
        assert!(json.get(key).is_none(), "{key} is omitted");
    }
    assert!(!text.contains("null"), "{text}");
}

#[test]
fn no_internal_column_is_ever_exposed() {
    let row = stored(|r| {
        r.secondary_file_id = Some("file_ant_9".to_owned());
        r.secondary_status = "uploaded".to_owned();
        r.cleanup_status = Some("pending".to_owned());
        r.deleted_at = Some(t0());
    });

    let json = wire(&row);

    for key in [
        "provider_file_id",
        "secondary_file_id",
        "secondary_status",
        "vector_store_id",
        "storage_backend",
        "cleanup_status",
        "cleanup_attempts",
        "for_file_search",
        "for_code_interpreter",
        "chat_id",
        "tenant_id",
        "uploaded_by_user_id",
        "updated_at",
        "deleted_at",
    ] {
        assert!(json.get(key).is_none(), "{key} must not be exposed");
    }
    assert!(!json.to_string().contains("file-1"));
    assert!(!json.to_string().contains("file_ant_9"));
}
