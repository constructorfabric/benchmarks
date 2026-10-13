//! Error contract of the chat-cleanup enqueue (deferred review finding F-003).
//!
//! DESIGN 3.3 REST error table: when `DELETE /chats/{id}` cannot enqueue its
//! chat-cleanup outbox message because the payload exceeds the outbox size
//! limit, the response is 400 `invalid_argument`; `detail` carries the message
//! and the same message is in `context.format`. (The same outbox failure on
//! attachment `DELETE` and on turn retry / edit / delete stays 500 `internal`,
//! so the conversion of a raw outbox error is the chat-delete path's decision.)

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

use mini_chat::domain::error::DomainError;
use sea_orm::DbErr;
use serde_json::{Value, json};
use toolkit_canonical_errors::{CanonicalError, Problem};
use toolkit_db::outbox::OutboxError;

/// Serialized Problem exactly as it goes on the wire.
fn problem(err: DomainError) -> (u16, Value) {
    let canonical = CanonicalError::from(err);
    let status = canonical.status_code();
    let problem = Problem::from_error(&canonical).expect("problem serializes");
    (
        status,
        serde_json::to_value(&problem).expect("problem is JSON"),
    )
}

#[test]
fn oversized_cleanup_payload_is_400_invalid_argument_with_the_message_in_format() {
    let message = "payload size 70000 exceeds maximum 65536";
    let (status, body) = problem(DomainError::OutboxPayloadTooLarge {
        message: message.to_owned(),
    });

    assert_eq!(status, 400);
    assert_eq!(body["status"], json!(400));
    assert!(
        body["type"]
            .as_str()
            .unwrap()
            .contains("cf.core.err.invalid_argument.v1"),
        "{body}"
    );
    assert_eq!(body["detail"], json!(message), "detail is the message");
    assert_eq!(
        body["context"]["format"],
        json!(message),
        "the same message is in context.format"
    );
    assert!(
        body["context"].get("field_violations").is_none(),
        "a format violation, not a field violation: {body}"
    );
    assert!(body.get("code").is_none(), "ADR-0004: no top-level code");
}

#[test]
fn an_outbox_payload_too_large_error_converts_to_the_400_variant() {
    let err = DomainError::from(OutboxError::PayloadTooLarge {
        size: 70_000,
        max: 65_536,
    });

    let DomainError::OutboxPayloadTooLarge { message } = &err else {
        panic!("expected OutboxPayloadTooLarge, got {err:?}");
    };
    assert!(message.contains("70000"), "{message}");
    assert!(message.contains("65536"), "{message}");

    let (status, body) = problem(err);
    assert_eq!(status, 400);
    assert_eq!(body["context"]["format"], body["detail"]);
}

#[test]
fn any_other_outbox_error_is_an_opaque_internal_error() {
    for err in [
        OutboxError::QueueNotRegistered("mini-chat.chat_cleanup".to_owned()),
        OutboxError::Database(DbErr::Custom("disk on fire".to_owned())),
        OutboxError::InvalidQueueName { reason: "bad" },
    ] {
        let domain = DomainError::from(err);
        assert!(
            matches!(domain, DomainError::Internal { .. }),
            "expected Internal, got {domain:?}"
        );
        let (status, body) = problem(domain);
        assert_eq!(status, 500);
        let wire = body.to_string();
        assert!(
            !wire.contains("mini-chat.chat_cleanup") && !wire.contains("disk on fire"),
            "the wire detail is generic: {wire}"
        );
    }
}
