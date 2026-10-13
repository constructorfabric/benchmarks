#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Payload building and enqueue error mapping of the chat-cleanup message
//! (`cpt-cf-mini-chat-algo-chat-crud-soft-delete-chat`, steps 7 to 10).

use std::collections::BTreeSet;

use serde_json::Value;
use time::OffsetDateTime;
use toolkit_db::outbox::OutboxError;
use uuid::Uuid;

use super::{CHAT_CLEANUP_REASON, ChatCleanupPayload, map_enqueue_error};
use crate::domain::error::DomainError;

fn deleted_at() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap()
}

fn payload() -> ChatCleanupPayload {
    ChatCleanupPayload::new(Uuid::from_u128(0x7E), Uuid::from_u128(0xC1), deleted_at())
}

#[test]
fn the_reason_is_chat_soft_delete() {
    assert_eq!(CHAT_CLEANUP_REASON, "chat_soft_delete");
}

#[test]
fn the_payload_carries_the_tenant_chat_reason_and_deletion_time() {
    let payload = payload();

    assert_eq!(payload.tenant_id, Uuid::from_u128(0x7E));
    assert_eq!(payload.chat_id, Uuid::from_u128(0xC1));
    assert_eq!(payload.reason, "chat_soft_delete");
    assert_eq!(payload.chat_deleted_at, deleted_at());
}

#[test]
fn the_system_request_id_is_a_fresh_uuid_v4() {
    let first = payload();
    let second = payload();

    assert_eq!(first.system_request_id.get_version_num(), 4);
    assert_eq!(second.system_request_id.get_version_num(), 4);
    assert_ne!(first.system_request_id, second.system_request_id);
    assert_ne!(first.system_request_id, first.chat_id);
    assert_ne!(first.system_request_id, first.tenant_id);
}

#[test]
fn the_serialized_payload_has_exactly_the_five_keys_with_their_values() {
    let payload = payload();

    let json: Value = serde_json::from_slice(&payload.encode().unwrap()).unwrap();

    let keys: BTreeSet<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "tenant_id",
            "chat_id",
            "system_request_id",
            "reason",
            "chat_deleted_at"
        ])
    );
    assert_eq!(json["tenant_id"], Uuid::from_u128(0x7E).to_string());
    assert_eq!(json["chat_id"], Uuid::from_u128(0xC1).to_string());
    assert_eq!(
        json["system_request_id"],
        payload.system_request_id.to_string()
    );
    assert_eq!(json["reason"], "chat_soft_delete");
    let at = json["chat_deleted_at"].as_str().unwrap();
    assert_eq!(
        OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339).unwrap(),
        deleted_at()
    );
}

#[test]
fn the_serialized_payload_round_trips() {
    let payload = payload();

    let decoded: ChatCleanupPayload = serde_json::from_slice(&payload.encode().unwrap()).unwrap();

    assert_eq!(decoded, payload);
}

#[test]
fn an_oversized_payload_maps_to_the_cleanup_payload_too_large_error() {
    let error = OutboxError::PayloadTooLarge {
        size: 70_000,
        max: 65_536,
    };

    match map_enqueue_error(&error) {
        DomainError::CleanupPayloadTooLarge { detail } => {
            assert!(!detail.is_empty());
            assert!(detail.contains("65536"), "{detail}");
        }
        other => panic!("expected CleanupPayloadTooLarge, got {other:?}"),
    }
}

#[test]
fn every_other_outbox_failure_maps_to_an_internal_error() {
    for error in [
        OutboxError::QueueNotRegistered("mini-chat.chat_cleanup".to_owned()),
        OutboxError::PartitionOutOfRange {
            queue: "q".to_owned(),
            partition: 9,
            max: 3,
        },
        OutboxError::Database(sea_orm::DbErr::Custom("connection refused".to_owned())),
    ] {
        match map_enqueue_error(&error) {
            DomainError::Database(_) | DomainError::Internal(_) => {}
            other => panic!("{error:?} must be internal, got {other:?}"),
        }
    }
}
