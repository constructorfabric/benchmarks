#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 1: the `AttachmentDetail` projection
//! (`cpt-cf-mini-chat-algo-attachments-project-detail`).

use super::detail::project_detail;
use super::model::{AttachmentKind, AttachmentStatus, Thumbnail};
use super::test_support::{stored, t0};

fn thumb() -> Thumbnail {
    Thumbnail {
        bytes: vec![1, 2, 3, 4],
        width: 128,
        height: 64,
    }
}

#[test]
fn the_required_fields_are_projected() {
    let row = stored(|_| {});

    let detail = project_detail(&row);

    assert_eq!(detail.id, row.id);
    assert_eq!(detail.filename, "report.pdf");
    assert_eq!(detail.content_type, "application/pdf");
    assert_eq!(detail.size_bytes, 1234);
    assert_eq!(detail.status, AttachmentStatus::Ready);
    assert_eq!(detail.kind, AttachmentKind::Document);
    assert_eq!(detail.created_at, t0());
    assert_eq!(detail.error_code, None);
    assert_eq!(detail.thumbnail, None);
}

#[test]
fn the_error_code_is_projected_only_for_a_failed_row() {
    for status in [
        AttachmentStatus::Pending,
        AttachmentStatus::Uploaded,
        AttachmentStatus::Ready,
    ] {
        let row = stored(|r| {
            r.status = status;
            r.error_code = Some("upload_failed".to_owned());
        });

        assert_eq!(project_detail(&row).error_code, None, "{status:?}");
    }
    let failed = stored(|r| {
        r.status = AttachmentStatus::Failed;
        r.error_code = Some("indexing_failed".to_owned());
    });
    assert_eq!(
        project_detail(&failed).error_code.as_deref(),
        Some("indexing_failed")
    );
}

#[test]
fn a_failed_row_without_an_error_code_has_none() {
    let row = stored(|r| r.status = AttachmentStatus::Failed);

    assert_eq!(project_detail(&row).error_code, None);
}

#[test]
fn the_thumbnail_is_projected_only_for_a_ready_image_with_stored_bytes() {
    let image = stored(|r| {
        r.kind = AttachmentKind::Image;
        r.thumbnail = Some(thumb());
    });
    assert_eq!(project_detail(&image).thumbnail, Some(thumb()));

    let no_bytes = stored(|r| r.kind = AttachmentKind::Image);
    assert_eq!(project_detail(&no_bytes).thumbnail, None);

    let document = stored(|r| r.thumbnail = Some(thumb()));
    assert_eq!(
        project_detail(&document).thumbnail,
        None,
        "a document never shows a thumbnail"
    );

    for status in [
        AttachmentStatus::Pending,
        AttachmentStatus::Uploaded,
        AttachmentStatus::Failed,
    ] {
        let row = stored(|r| {
            r.kind = AttachmentKind::Image;
            r.status = status;
            r.thumbnail = Some(thumb());
        });
        assert_eq!(project_detail(&row).thumbnail, None, "{status:?}");
    }
}
