//! Send request validation (slice S1-setup-path of
//! `cpt-cf-mini-chat-feature-message-streaming`): the emptiness check, the attachment shape
//! check, the defaults and the untouched content.
//!
//! Algorithm `cpt-cf-mini-chat-algo-message-streaming-request-validation`; definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-send-preflight`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use uuid::Uuid;

use super::{AttachmentLimits, RawSendBody, validate_send};
use crate::domain::error::DomainError;

const LIMITS: AttachmentLimits = AttachmentLimits {
    max_documents_per_chat: 50,
    max_images_per_message: 4,
};

fn body(content: &str) -> RawSendBody {
    RawSendBody {
        content: content.to_owned(),
        request_id: None,
        attachment_ids: None,
        web_search_enabled: None,
    }
}

fn ids(n: usize) -> Vec<Uuid> {
    (0..n).map(|_| Uuid::new_v4()).collect()
}

#[test]
fn empty_and_whitespace_only_content_is_rejected() {
    for content in ["", " ", "   ", "\n", "\t \r\n", "\u{a0}\u{2003}"] {
        let result = validate_send(body(content), LIMITS);
        assert!(
            matches!(result, Err(DomainError::EmptyContent)),
            "{content:?}: {result:?}"
        );
    }
}

#[test]
fn the_content_is_kept_exactly_as_sent() {
    for content in [
        "hi",
        "  padded  ",
        "\nline\n",
        "tab\there",
        "\u{fc}n\u{ef}code \u{2713}",
    ] {
        let validated = validate_send(body(content), LIMITS).unwrap();
        assert_eq!(validated.content, content);
    }
}

#[test]
fn the_defaults_apply_to_absent_fields() {
    let validated = validate_send(body("hello"), LIMITS).unwrap();
    assert_eq!(validated.request_id, None);
    assert!(validated.attachment_ids.is_empty());
    assert!(!validated.web_search_enabled);
}

#[test]
fn a_null_web_search_is_disabled_and_an_enabled_one_is_kept() {
    let mut raw = body("hello");
    raw.web_search_enabled = Some(false);
    assert!(
        !validate_send(raw.clone(), LIMITS)
            .unwrap()
            .web_search_enabled
    );
    raw.web_search_enabled = Some(true);
    assert!(validate_send(raw, LIMITS).unwrap().web_search_enabled);
}

#[test]
fn a_supplied_request_id_of_any_uuid_version_is_kept() {
    for id in [
        Uuid::new_v4(),
        Uuid::nil(),
        Uuid::from_u128(0x0123_4567_89ab_1def_8123_4567_89ab_cdef),
        Uuid::max(),
    ] {
        let mut raw = body("hello");
        raw.request_id = Some(id);
        assert_eq!(validate_send(raw, LIMITS).unwrap().request_id, Some(id));
    }
}

#[test]
fn attachment_ids_are_kept_in_request_order() {
    let wanted = ids(3);
    let mut raw = body("hello");
    raw.attachment_ids = Some(wanted.clone());
    assert_eq!(validate_send(raw, LIMITS).unwrap().attachment_ids, wanted);
}

#[test]
fn a_duplicate_attachment_id_is_invalid_attachment() {
    let id = Uuid::new_v4();
    let mut raw = body("hello");
    raw.attachment_ids = Some(vec![id, Uuid::new_v4(), id]);
    let result = validate_send(raw, LIMITS);
    assert!(
        matches!(result, Err(DomainError::InvalidAttachment)),
        "{result:?}"
    );
}

#[test]
fn the_attachment_count_cap_is_documents_plus_images() {
    let mut raw = body("hello");
    raw.attachment_ids = Some(ids(54));
    assert_eq!(validate_send(raw, LIMITS).unwrap().attachment_ids.len(), 54);

    let mut raw = body("hello");
    raw.attachment_ids = Some(ids(55));
    let result = validate_send(raw, LIMITS);
    assert!(
        matches!(result, Err(DomainError::InvalidAttachment)),
        "{result:?}"
    );
}

#[test]
fn the_cap_follows_the_configured_limits() {
    let limits = AttachmentLimits {
        max_documents_per_chat: 2,
        max_images_per_message: 1,
    };
    let mut raw = body("hello");
    raw.attachment_ids = Some(ids(3));
    assert!(validate_send(raw.clone(), limits).is_ok());
    raw.attachment_ids = Some(ids(4));
    assert!(matches!(
        validate_send(raw, limits),
        Err(DomainError::InvalidAttachment)
    ));
}

#[test]
fn empty_content_is_reported_before_a_bad_attachment_list() {
    let id = Uuid::new_v4();
    let mut raw = body("  ");
    raw.attachment_ids = Some(vec![id, id]);
    let result = validate_send(raw, LIMITS);
    assert!(
        matches!(result, Err(DomainError::EmptyContent)),
        "{result:?}"
    );
}

#[test]
fn an_empty_attachment_list_is_valid() {
    let mut raw = body("hello");
    raw.attachment_ids = Some(Vec::new());
    assert!(
        validate_send(raw, LIMITS)
            .unwrap()
            .attachment_ids
            .is_empty()
    );
}
