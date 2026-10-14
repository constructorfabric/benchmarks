//! The attachment carry-forward
//! (`cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward`): the filter
//! of deleted attachments, the deterministic order and the image subset.

use time::OffsetDateTime;
use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::streaming::AttachmentFact;

/// An attachment linked to the original user message, as the carry-forward read returns it.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedAttachment {
    /// The attachment facts (`kind` `image` or `document`, the internal provider file ids).
    pub fact: AttachmentFact,
    /// `message_attachments.created_at` of the original link.
    pub link_created_at: OffsetDateTime,
    /// `attachments.deleted_at` IS NOT NULL.
    pub deleted: bool,
}

/// What the replacement message carries.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CarriedAttachments {
    /// Every carried attachment id, ordered by the original link `created_at`, then by id.
    pub attachment_ids: Vec<Uuid>,
    /// The image subset in the same order: the model input of the replacement turn.
    pub images: Vec<AttachmentFact>,
}

/// Keep the non-deleted attachments (silently excluding the deleted ones), order them by the
/// original link `created_at` then `attachment_id` and classify the images.
#[must_use]
pub fn carry_forward_attachments(links: Vec<LinkedAttachment>) -> CarriedAttachments {
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward:p1:inst-acf-exclude-deleted
    let mut kept: Vec<LinkedAttachment> = links.into_iter().filter(|link| !link.deleted).collect();
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward:p1:inst-acf-exclude-deleted
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward:p1:inst-acf-order
    kept.sort_by_key(|link| (link.link_created_at, link.fact.id));
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward:p1:inst-acf-order
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward:p1:inst-acf-classify
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward:p1:inst-acf-return
    CarriedAttachments {
        attachment_ids: kept.iter().map(|link| link.fact.id).collect(),
        images: kept
            .into_iter()
            .filter(|link| link.fact.kind == "image")
            .map(|link| link.fact)
            .collect(),
    }
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward:p1:inst-acf-return
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward:p1:inst-acf-classify
}
