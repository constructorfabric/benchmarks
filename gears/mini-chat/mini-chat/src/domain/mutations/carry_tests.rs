//! The attachment carry-forward: the filter that silently excludes deleted attachments, the
//! deterministic order (original link `created_at`, then `attachment_id`), the image subset that
//! becomes model input and the documents that stay associations only.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: algorithm
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-attachment-carry-forward`
//! (`inst-acf-exclude-deleted`, `inst-acf-no-revalidation`, `inst-acf-order`, `inst-acf-classify`,
//! `inst-acf-return`); definition of done
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-carry-forward`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::{CarriedAttachments, LinkedAttachment, carry_forward_attachments};
use crate::domain::streaming::AttachmentFact;

fn instant(micros: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap() + Duration::microseconds(micros)
}

fn linked(id: u128, kind: &str, link_micros: i64, deleted: bool) -> LinkedAttachment {
    LinkedAttachment {
        fact: AttachmentFact {
            id: Uuid::from_u128(id),
            kind: kind.to_owned(),
            provider_file_id: Some(format!("file-{id}")),
            secondary_file_id: None,
            secondary_status: "not_attempted".to_owned(),
        },
        link_created_at: instant(link_micros),
        deleted,
    }
}

fn ids(carried: &CarriedAttachments) -> Vec<u128> {
    carried.attachment_ids.iter().map(Uuid::as_u128).collect()
}

fn image_ids(carried: &CarriedAttachments) -> Vec<u128> {
    carried
        .images
        .iter()
        .map(|fact| fact.id.as_u128())
        .collect()
}

#[test]
fn no_links_carry_nothing() {
    let carried = carry_forward_attachments(Vec::new());
    assert!(carried.attachment_ids.is_empty());
    assert!(carried.images.is_empty());
}

#[test]
fn a_deleted_attachment_is_silently_excluded() {
    // inst-acf-exclude-deleted: no error, no warning, just absent
    let carried = carry_forward_attachments(vec![
        linked(1, "document", 10, false),
        linked(2, "image", 20, true),
        linked(3, "image", 30, false),
    ]);
    assert_eq!(ids(&carried), vec![1, 3]);
    assert_eq!(image_ids(&carried), vec![3]);
}

#[test]
fn when_every_attachment_is_deleted_nothing_is_carried() {
    let carried = carry_forward_attachments(vec![
        linked(1, "image", 10, true),
        linked(2, "document", 20, true),
    ]);
    assert!(carried.attachment_ids.is_empty());
    assert!(carried.images.is_empty());
}

#[test]
fn the_carried_attachments_are_ordered_by_the_original_link_time() {
    // inst-acf-order: not by id and not by the order the rows were read
    let carried = carry_forward_attachments(vec![
        linked(1, "document", 300, false),
        linked(2, "image", 100, false),
        linked(3, "image", 200, false),
    ]);
    assert_eq!(ids(&carried), vec![2, 3, 1]);
    assert_eq!(image_ids(&carried), vec![2, 3]);
}

#[test]
fn links_created_at_the_same_instant_are_ordered_by_attachment_id() {
    // inst-acf-order: `attachment_id` is the tie-break, so the provider request is reproducible
    let carried = carry_forward_attachments(vec![
        linked(9, "image", 50, false),
        linked(4, "image", 50, false),
        linked(7, "image", 50, false),
    ]);
    assert_eq!(ids(&carried), vec![4, 7, 9]);
    assert_eq!(image_ids(&carried), vec![4, 7, 9]);
}

#[test]
fn microsecond_differences_in_the_link_time_decide_the_order() {
    let carried = carry_forward_attachments(vec![
        linked(1, "image", 123_001, false),
        linked(2, "image", 123_000, false),
    ]);
    assert_eq!(ids(&carried), vec![2, 1]);
}

#[test]
fn only_images_become_model_input_documents_stay_associations() {
    // inst-acf-classify
    let carried = carry_forward_attachments(vec![
        linked(1, "document", 10, false),
        linked(2, "image", 20, false),
        linked(3, "document", 30, false),
    ]);
    assert_eq!(
        ids(&carried),
        vec![1, 2, 3],
        "every kind keeps its association"
    );
    assert_eq!(image_ids(&carried), vec![2]);
    assert!(carried.images.iter().all(|fact| fact.kind == "image"));
}

#[test]
fn an_image_keeps_its_internal_file_ids_for_the_provider_request() {
    let mut image = linked(5, "image", 10, false);
    image.fact.secondary_file_id = Some("sec-5".to_owned());
    image.fact.secondary_status = "uploaded".to_owned();
    let expected = image.fact.clone();
    let carried = carry_forward_attachments(vec![image]);
    assert_eq!(carried.images, vec![expected]);
}

#[test]
fn the_carry_forward_does_not_revalidate_the_attachment_status() {
    // inst-acf-no-revalidation: the links were validated when the original message was sent, so
    // an attachment that is no longer `ready` but not deleted is still carried
    let mut not_ready = linked(1, "document", 10, false);
    not_ready.fact.provider_file_id = None;
    let carried = carry_forward_attachments(vec![not_ready]);
    assert_eq!(ids(&carried), vec![1]);
}
