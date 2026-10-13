//! The `AttachmentDetail` projection
//! (`cpt-cf-mini-chat-algo-attachments-project-detail`).

use super::model::{AttachmentDetail, AttachmentKind, AttachmentStatus, StoredAttachment};

/// Projects a row: always `id`, `filename`, `content_type`, `size_bytes`,
/// `status`, `kind`, `created_at`; `error_code` only for a `failed` row that has
/// one; the thumbnail only for a `ready` image with stored thumbnail bytes.
#[must_use]
pub fn project_detail(row: &StoredAttachment) -> AttachmentDetail {
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-project-detail:p1:inst-thumbnail-field
    let thumbnail = (row.kind == AttachmentKind::Image && row.status == AttachmentStatus::Ready)
        .then(|| row.thumbnail.clone())
        .flatten();
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-project-detail:p1:inst-thumbnail-field
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-project-detail:p1:inst-return-projection
    AttachmentDetail {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-project-detail:p1:inst-required-fields
        id: row.id,
        filename: row.filename.clone(),
        content_type: row.content_type.clone(),
        size_bytes: row.size_bytes,
        status: row.status,
        kind: row.kind,
        created_at: row.created_at,
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-project-detail:p1:inst-required-fields
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-project-detail:p1:inst-error-code
        error_code: (row.status == AttachmentStatus::Failed)
            .then(|| row.error_code.clone())
            .flatten(),
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-project-detail:p1:inst-error-code
        thumbnail,
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-project-detail:p1:inst-return-projection
}
