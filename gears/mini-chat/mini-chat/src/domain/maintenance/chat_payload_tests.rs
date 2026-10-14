//! The chat-cleanup payload parser: the ids, `chat_deleted_at`, the `reason`
//! `chat_soft_delete`, the `dedupe_key` prefix, and a malformed reason that names the field and
//! never holds a payload value.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: algorithm
//! `cpt-cf-mini-chat-algo-background-maintenance-chat-cleanup-payload` (`inst-cp-parse`,
//! `inst-cp-fields`, `inst-cp-reason`, `inst-cp-dedupe`, `inst-cp-malformed`,
//! `inst-cp-return-malformed`, `inst-cp-return`); definition of done
//! `cpt-cf-mini-chat-dod-background-maintenance-chat-cleanup-handler`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::{Value, json};
use uuid::Uuid;

use super::{ChatCleanupWork, parse_chat_cleanup_payload};
use crate::test_support::maintenance::chat_cleanup_payload;
use crate::test_support::quota::default_now;
use crate::test_support::{TENANT_A, TENANT_B};

const CHAT: Uuid = Uuid::from_u128(0xC4A7_0000_0000_0000_0000_0000_0000_0003);
const REQUEST: Uuid = Uuid::from_u128(0x5151_0000_0000_0000_0000_0000_0000_0009);

fn payload() -> Value {
    chat_cleanup_payload(TENANT_A, CHAT, REQUEST)
}

fn parse(value: &Value) -> Result<ChatCleanupWork, String> {
    parse_chat_cleanup_payload(&serde_json::to_vec(value).unwrap())
}

fn reason_of(value: &Value) -> String {
    parse(value).expect_err("the payload is malformed")
}

#[test]
fn a_well_formed_payload_yields_the_ids_and_the_deletion_instant() {
    // inst-cp-fields, inst-cp-return
    let work = parse(&payload()).expect("well-formed");
    assert_eq!(work.tenant_id, TENANT_A);
    assert_eq!(work.chat_id, CHAT);
    assert_eq!(work.system_request_id, REQUEST);
    assert_eq!(work.chat_deleted_at, default_now());
}

#[test]
fn bytes_that_are_not_a_json_object_are_malformed() {
    // inst-cp-parse, inst-cp-malformed
    for bytes in [&b""[..], b"garbage", b"[]", b"7", b"\"s\"", b"null"] {
        assert!(
            parse_chat_cleanup_payload(bytes).is_err(),
            "{}",
            String::from_utf8_lossy(bytes)
        );
    }
}

#[test]
fn every_id_must_be_a_uuid_and_the_reason_names_the_field() {
    // inst-cp-fields, inst-cp-return-malformed
    for field in ["tenant_id", "chat_id", "system_request_id"] {
        let mut missing = payload();
        missing.as_object_mut().unwrap().remove(field);
        let reason = reason_of(&missing);
        assert!(reason.contains(field), "{field}: {reason}");

        let mut bad = payload();
        bad[field] = json!("definitely-not-a-uuid");
        let reason = reason_of(&bad);
        assert!(reason.contains(field), "{field}: {reason}");
        assert!(
            !reason.contains("definitely-not"),
            "no payload value: {reason}"
        );
    }
}

#[test]
fn the_deletion_instant_must_be_rfc_3339() {
    // inst-cp-fields
    let mut missing = payload();
    missing.as_object_mut().unwrap().remove("chat_deleted_at");
    assert!(reason_of(&missing).contains("chat_deleted_at"));
    let mut bad = payload();
    bad["chat_deleted_at"] = json!("last tuesday");
    let reason = reason_of(&bad);
    assert!(reason.contains("chat_deleted_at"), "{reason}");
    assert!(!reason.contains("tuesday"), "{reason}");
}

#[test]
fn the_reason_must_be_chat_soft_delete() {
    // inst-cp-reason
    let mut other = payload();
    other["reason"] = json!("tenant_purge");
    let reason = reason_of(&other);
    assert!(reason.contains("reason"), "{reason}");
    assert!(!reason.contains("tenant_purge"), "{reason}");
    other.as_object_mut().unwrap().remove("reason");
    assert!(reason_of(&other).contains("reason"));
}

#[test]
fn a_dedupe_key_is_optional_but_checked_when_present() {
    // inst-cp-dedupe
    let mut value = payload();
    value.as_object_mut().unwrap().remove("dedupe_key");
    assert!(parse(&value).is_ok());
    value["dedupe_key"] = json!(null);
    assert!(parse(&value).is_ok());

    let tenant_hex = TENANT_A.simple().to_string();
    for key in [
        format!("{}/chat_cleanup/x", TENANT_B.simple()),
        format!("{}/chat_cleanup/x", tenant_hex.to_uppercase()),
        format!("{TENANT_A}/chat_cleanup/x"),
        format!("{tenant_hex}chat_cleanup/x"),
        String::new(),
    ] {
        let mut bad = payload();
        bad["dedupe_key"] = json!(key);
        let reason = reason_of(&bad);
        assert!(reason.contains("dedupe_key"), "{key}: {reason}");
        assert!(!reason.contains(&tenant_hex), "no payload value: {reason}");
    }
    let mut good = payload();
    good["dedupe_key"] = json!(format!("{tenant_hex}/chat_cleanup/anything"));
    assert!(parse(&good).is_ok());
}

#[test]
fn the_handler_gets_no_row_list_from_the_payload() {
    // inst-cp-return: only the ids and the instant are parsed; extra fields are ignored
    let mut value = payload();
    value["attachments"] = json!([{ "id": Uuid::new_v4(), "file": "file-from-payload" }]);
    value["vector_store_id"] = json!("vs_from_payload");
    let work = parse(&value).expect("extra fields are ignored");
    assert_eq!(work.chat_id, CHAT);
    let debug = format!("{work:?}");
    assert!(!debug.contains("file-from-payload") && !debug.contains("vs_from_payload"));
}
