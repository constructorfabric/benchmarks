#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 5: the cleanup payload codec
//! (`cpt-cf-mini-chat-algo-attachments-encode-cleanup-payload`): exactly ten
//! keys, the null rules, and what the handler treats as malformed.

use std::collections::BTreeSet;

use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{decode_cleanup, encode_cleanup};
use crate::domain::service::attachments::model::AttachmentKind;
use crate::domain::service::attachments::payload::{CleanupEvent, CleanupMessage, SecondaryRef};

const TEN_KEYS: [&str; 10] = [
    "event_type",
    "tenant_id",
    "chat_id",
    "attachment_id",
    "provider_file_id",
    "vector_store_id",
    "storage_backend",
    "attachment_kind",
    "deleted_at",
    "secondary_ref",
];

fn at() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_773_144_000).unwrap()
        + time::Duration::microseconds(250_001)
}

fn message(event: CleanupEvent) -> CleanupMessage {
    CleanupMessage {
        event,
        tenant_id: Uuid::from_u128(0x7E),
        chat_id: Uuid::from_u128(0xC1),
        attachment_id: Uuid::from_u128(0xA1),
        provider_file_id: Some("file-abc".to_owned()),
        storage_backend: "openai".to_owned(),
        kind: AttachmentKind::Document,
        deleted_at: at(),
        secondary_ref: None,
    }
}

fn encoded(message: &CleanupMessage) -> Value {
    serde_json::from_slice(&encode_cleanup(message).unwrap()).expect("the payload is JSON")
}

#[test]
fn the_payload_has_exactly_the_ten_keys_for_every_event() {
    for event in [
        CleanupEvent::AttachmentDeleted,
        CleanupEvent::AttachmentUploadAbandoned,
        CleanupEvent::AttachmentIndexingFailed,
        CleanupEvent::AttachmentUploadFailed,
        CleanupEvent::AttachmentOrphanFile,
    ] {
        let json = encoded(&message(event));

        let keys: BTreeSet<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            TEN_KEYS.iter().copied().collect(),
            "{}",
            event.as_str()
        );
        assert_eq!(json["event_type"], event.as_str());
    }
}

#[test]
fn the_values_are_the_documented_wire_forms() {
    let json = encoded(&message(CleanupEvent::AttachmentDeleted));

    assert_eq!(
        json,
        json!({
            "event_type": "attachment_deleted",
            "tenant_id": Uuid::from_u128(0x7E).to_string(),
            "chat_id": Uuid::from_u128(0xC1).to_string(),
            "attachment_id": Uuid::from_u128(0xA1).to_string(),
            "provider_file_id": "file-abc",
            "vector_store_id": null,
            "storage_backend": "openai",
            "attachment_kind": "document",
            "deleted_at": "2026-03-10T12:00:00.250001Z",
            "secondary_ref": null,
        })
    );
}

#[test]
fn the_vector_store_id_is_always_null() {
    for event in [
        CleanupEvent::AttachmentDeleted,
        CleanupEvent::AttachmentIndexingFailed,
    ] {
        assert!(encoded(&message(event))["vector_store_id"].is_null());
    }
}

#[test]
fn a_row_that_never_reached_the_provider_has_a_null_file_id() {
    let mut deleted = message(CleanupEvent::AttachmentDeleted);
    deleted.provider_file_id = None;

    let json = encoded(&deleted);

    assert!(json["provider_file_id"].is_null());
    assert!(
        json.as_object().unwrap().contains_key("provider_file_id"),
        "the key is present"
    );
}

#[test]
fn an_image_kind_is_named_image() {
    let mut image = message(CleanupEvent::AttachmentDeleted);
    image.kind = AttachmentKind::Image;

    assert_eq!(encoded(&image)["attachment_kind"], "image");
}

#[test]
fn a_secondary_reference_is_an_object_with_the_provider_kind() {
    let mut deleted = message(CleanupEvent::AttachmentDeleted);
    deleted.secondary_ref = Some(SecondaryRef {
        file_id: "file_ant_1".to_owned(),
        upstream_alias: "api.anthropic.test".to_owned(),
    });

    let json = encoded(&deleted);

    assert_eq!(
        json["secondary_ref"],
        json!({ "file_id": "file_ant_1", "provider_kind": "anthropic", "upstream_alias": "api.anthropic.test" })
    );
}

#[test]
fn a_message_survives_a_round_trip() {
    let mut full = message(CleanupEvent::AttachmentUploadFailed);
    full.secondary_ref = Some(SecondaryRef {
        file_id: "file_ant_1".to_owned(),
        upstream_alias: "api.anthropic.test".to_owned(),
    });
    let mut bare = message(CleanupEvent::AttachmentDeleted);
    bare.provider_file_id = None;
    bare.kind = AttachmentKind::Image;

    for original in [full, bare, message(CleanupEvent::AttachmentOrphanFile)] {
        let decoded = decode_cleanup(&encode_cleanup(&original).unwrap()).unwrap();

        assert_eq!(decoded, original);
    }
}

fn valid() -> Value {
    encoded(&message(CleanupEvent::AttachmentDeleted))
}

fn decode(value: &Value) -> Result<CleanupMessage, super::MalformedPayload> {
    decode_cleanup(&serde_json::to_vec(value).unwrap())
}

#[test]
fn a_payload_that_is_not_a_json_object_is_malformed() {
    let valid = br#"{"event_type":"attachment_deleted","tenant_id":"00000000-0000-0000-0000-00000000007e","chat_id":"00000000-0000-0000-0000-0000000000c1","attachment_id":"00000000-0000-0000-0000-0000000000a1","provider_file_id":"file-abc","vector_store_id":null,"storage_backend":"openai","attachment_kind":"document","deleted_at":"2026-03-10T12:00:00Z","secondary_ref":null}"#;
    assert!(
        decode_cleanup(valid).is_ok(),
        "a well-formed payload decodes"
    );
    for bytes in [
        &b""[..],
        b"not json",
        b"[]",
        b"42",
        b"\"text\"",
        b"null",
        b"{",
    ] {
        assert!(
            decode_cleanup(bytes).is_err(),
            "{}",
            String::from_utf8_lossy(bytes)
        );
    }
}

#[test]
fn an_unknown_event_type_is_malformed() {
    for event in [
        json!("attachment_exploded"),
        json!(""),
        json!(7),
        json!(null),
    ] {
        let mut value = valid();
        value["event_type"] = event.clone();

        assert!(decode(&value).is_err(), "{event}");
    }
}

#[test]
fn a_missing_required_key_is_malformed() {
    for key in [
        "event_type",
        "tenant_id",
        "chat_id",
        "attachment_id",
        "storage_backend",
        "attachment_kind",
        "deleted_at",
    ] {
        let mut value = valid();
        value.as_object_mut().unwrap().remove(key);

        assert!(decode(&value).is_err(), "{key}");
    }
}

#[test]
fn a_non_uuid_id_is_malformed() {
    for key in ["tenant_id", "chat_id", "attachment_id"] {
        for bad in [json!("not-a-uuid"), json!(""), json!(5), json!(null)] {
            let mut value = valid();
            value[key] = bad.clone();

            assert!(decode(&value).is_err(), "{key} = {bad}");
        }
    }
}

#[test]
fn a_wrong_type_is_malformed() {
    for (key, bad) in [
        ("provider_file_id", json!(12)),
        ("storage_backend", json!(["openai"])),
        ("attachment_kind", json!("video")),
        ("attachment_kind", json!(1)),
        ("deleted_at", json!("yesterday")),
        ("deleted_at", json!(5)),
        ("secondary_ref", json!("file_ant_1")),
        ("secondary_ref", json!({ "provider_kind": "anthropic" })),
    ] {
        let mut value = valid();
        value[key] = bad.clone();

        assert!(decode(&value).is_err(), "{key} = {bad}");
    }
}
