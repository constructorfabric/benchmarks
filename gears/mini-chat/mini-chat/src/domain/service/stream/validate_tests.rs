#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 1: request validation
//! (`cpt-cf-mini-chat-algo-streaming-turns-validate-request`).

use uuid::Uuid;

use super::*;

const LIMITS: ValidationLimits = ValidationLimits {
    max_documents_per_chat: 3,
    max_images_per_message: 4,
};

fn request(content: &str) -> SendRequest {
    SendRequest {
        content: content.to_owned(),
        ..SendRequest::default()
    }
}

fn ids(count: u128) -> Vec<Uuid> {
    (1..=count).map(|n| Uuid::from_u128(0x9000 + n)).collect()
}

#[test]
fn empty_content_is_rejected() {
    assert!(matches!(
        validate_request(&request(""), &LIMITS),
        Err(DomainError::EmptyContent)
    ));
}

#[test]
fn whitespace_only_content_is_rejected() {
    for content in ["   ", "\n\t \r\n", "\u{a0}", "\u{3000}\u{3000}"] {
        assert!(
            matches!(
                validate_request(&request(content), &LIMITS),
                Err(DomainError::EmptyContent)
            ),
            "{content:?} must be empty content"
        );
    }
}

#[test]
fn content_is_kept_unchanged_not_trimmed() {
    let normalized = validate_request(&request("  hello\n"), &LIMITS).unwrap();
    assert_eq!(normalized.content, "  hello\n");
}

#[test]
fn a_given_request_id_is_kept_whatever_its_version() {
    // A version 7 style id and a nil-like id are both accepted as they are.
    for id in [
        Uuid::parse_str("0190f3c2-7a4b-7c3d-8e5f-123456789abc").unwrap(),
        Uuid::parse_str("6ba7b810-9dad-11d1-80b4-00c04fd430c8").unwrap(),
        Uuid::from_u128(1),
    ] {
        let mut req = request("hi");
        req.request_id = Some(id);
        assert_eq!(validate_request(&req, &LIMITS).unwrap().request_id, id);
    }
}

#[test]
fn an_absent_request_id_is_a_fresh_uuid_v4() {
    let first = validate_request(&request("hi"), &LIMITS)
        .unwrap()
        .request_id;
    let second = validate_request(&request("hi"), &LIMITS)
        .unwrap()
        .request_id;
    assert_eq!(first.get_version_num(), 4);
    assert_eq!(second.get_version_num(), 4);
    assert_ne!(first, second);
}

#[test]
fn the_web_search_flag_defaults_to_false() {
    let mut req = request("hi");
    assert!(!validate_request(&req, &LIMITS).unwrap().web_search_enabled);
    req.web_search_enabled = Some(false);
    assert!(!validate_request(&req, &LIMITS).unwrap().web_search_enabled);
    req.web_search_enabled = Some(true);
    assert!(validate_request(&req, &LIMITS).unwrap().web_search_enabled);
}

#[test]
fn absent_attachment_ids_are_an_empty_list() {
    let normalized = validate_request(&request("hi"), &LIMITS).unwrap();
    assert!(normalized.attachment_ids.is_empty());
}

#[test]
fn attachment_ids_keep_their_order() {
    let mut req = request("hi");
    req.attachment_ids = vec![Uuid::from_u128(3), Uuid::from_u128(1), Uuid::from_u128(2)];
    let normalized = validate_request(&req, &LIMITS).unwrap();
    assert_eq!(normalized.attachment_ids, req.attachment_ids);
}

#[test]
fn duplicate_attachment_ids_are_invalid_attachments() {
    let mut req = request("hi");
    req.attachment_ids = vec![Uuid::from_u128(7), Uuid::from_u128(8), Uuid::from_u128(7)];
    assert!(matches!(
        validate_request(&req, &LIMITS),
        Err(DomainError::InvalidAttachmentIds)
    ));
}

#[test]
fn the_attachment_count_limit_is_documents_plus_images() {
    // 3 documents + 4 images = 7 ids are allowed, 8 are not.
    let mut req = request("hi");
    req.attachment_ids = ids(7);
    assert_eq!(
        validate_request(&req, &LIMITS)
            .unwrap()
            .attachment_ids
            .len(),
        7
    );
    req.attachment_ids = ids(8);
    assert!(matches!(
        validate_request(&req, &LIMITS),
        Err(DomainError::InvalidAttachmentIds)
    ));
}

#[test]
fn empty_content_is_reported_before_invalid_attachment_ids() {
    let mut req = request("   ");
    req.attachment_ids = vec![Uuid::from_u128(7), Uuid::from_u128(7)];
    assert!(matches!(
        validate_request(&req, &LIMITS),
        Err(DomainError::EmptyContent)
    ));
}
