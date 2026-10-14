//! The thread-summary payload parser: every field of the work item, the empty and the set base
//! frontier, a base frontier with one half missing, the wrong `system_task_type`, the `dedupe_key`
//! prefix, and a malformed reason that names the field and never holds a payload value.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: algorithm
//! `cpt-cf-mini-chat-algo-background-maintenance-summary-payload` (`inst-sp-parse`, `inst-sp-ids`,
//! `inst-sp-task-type`, `inst-sp-target`, `inst-sp-base`, `inst-sp-dedupe`, `inst-sp-malformed`,
//! `inst-sp-return-malformed`, `inst-sp-return`); definition of done
//! `cpt-cf-mini-chat-dod-background-maintenance-summary-execution`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use super::parse_summary_payload;
use crate::domain::streaming::summary::Frontier;
use crate::test_support::maintenance::{rfc3339, summary_payload};
use crate::test_support::send::t;
use crate::test_support::{TENANT_A, TENANT_B};

const CHAT: Uuid = Uuid::from_u128(0xC4A7_0000_0000_0000_0000_0000_0000_0001);
const REQUEST: Uuid = Uuid::from_u128(0x5151_0000_0000_0000_0000_0000_0000_0007);
const BASE_ID: Uuid = Uuid::from_u128(0xBA5E_0000_0000_0000_0000_0000_0000_0001);
const TARGET_ID: Uuid = Uuid::from_u128(0x7A67_0000_0000_0000_0000_0000_0000_0001);

fn payload() -> Value {
    summary_payload(
        TENANT_A,
        CHAT,
        Some((t(10), BASE_ID)),
        (t(90), TARGET_ID),
        REQUEST,
    )
}

fn parse(value: &Value) -> Result<crate::domain::streaming::summary::ThreadSummaryWork, String> {
    parse_summary_payload(&serde_json::to_vec(value).unwrap())
}

fn reason_of(value: &Value) -> String {
    parse(value).expect_err("the payload is malformed")
}

#[test]
fn a_well_formed_payload_yields_the_work_item() {
    // inst-sp-ids, inst-sp-target, inst-sp-base, inst-sp-return
    let work = parse(&payload()).expect("well-formed");
    assert_eq!(work.tenant_id, TENANT_A);
    assert_eq!(work.chat_id, CHAT);
    assert_eq!(work.system_request_id, REQUEST);
    assert_eq!(
        work.base_frontier,
        Some(Frontier {
            created_at: t(10),
            message_id: BASE_ID
        })
    );
    assert_eq!(
        work.frozen_target,
        Frontier {
            created_at: t(90),
            message_id: TARGET_ID
        }
    );
}

#[test]
fn the_target_is_taken_from_the_payload_as_frozen() {
    // inst-sp-return: the handler never recomputes the target; the instant keeps its microseconds
    let target: OffsetDateTime = t(123_456);
    let value = summary_payload(TENANT_A, CHAT, None, (target, TARGET_ID), REQUEST);
    assert_eq!(parse(&value).unwrap().frozen_target.created_at, target);
}

#[test]
fn both_base_fields_null_mean_an_empty_base() {
    // inst-sp-base
    let value = summary_payload(TENANT_A, CHAT, None, (t(90), TARGET_ID), REQUEST);
    assert!(value["base_frontier_created_at"].is_null());
    assert!(value["base_frontier_message_id"].is_null());
    assert_eq!(parse(&value).unwrap().base_frontier, None);
}

#[test]
fn absent_base_fields_mean_an_empty_base_too() {
    // inst-sp-base: null and absent are the same
    let mut value = payload();
    value
        .as_object_mut()
        .unwrap()
        .remove("base_frontier_created_at");
    value
        .as_object_mut()
        .unwrap()
        .remove("base_frontier_message_id");
    assert_eq!(parse(&value).unwrap().base_frontier, None);
}

#[test]
fn a_base_with_only_its_timestamp_is_malformed() {
    // inst-sp-base, inst-sp-malformed
    let mut value = payload();
    value["base_frontier_message_id"] = Value::Null;
    let reason = reason_of(&value);
    assert!(reason.contains("base_frontier"), "{reason}");
}

#[test]
fn a_base_with_only_its_message_id_is_malformed() {
    // inst-sp-base, inst-sp-malformed
    let mut value = payload();
    value["base_frontier_created_at"] = Value::Null;
    let reason = reason_of(&value);
    assert!(reason.contains("base_frontier"), "{reason}");
}

#[test]
fn bytes_that_are_not_json_are_malformed() {
    // inst-sp-parse
    assert!(parse_summary_payload(b"not json at all").is_err());
    assert!(parse_summary_payload(b"").is_err());
}

#[test]
fn json_that_is_not_an_object_is_malformed() {
    // inst-sp-parse
    for text in ["[]", "42", "\"text\"", "null", "true"] {
        assert!(parse_summary_payload(text.as_bytes()).is_err(), "{text}");
    }
}

#[test]
fn every_id_must_be_a_uuid_and_the_reason_names_the_field() {
    // inst-sp-ids, inst-sp-return-malformed
    for field in ["tenant_id", "chat_id", "system_request_id"] {
        let mut missing = payload();
        missing.as_object_mut().unwrap().remove(field);
        let reason = reason_of(&missing);
        assert!(reason.contains(field), "{field}: {reason}");

        let mut bad = payload();
        bad[field] = json!("not-a-uuid-at-all");
        let reason = reason_of(&bad);
        assert!(reason.contains(field), "{field}: {reason}");
        assert!(
            !reason.contains("not-a-uuid-at-all"),
            "the reason holds no payload value: {reason}"
        );

        let mut number = payload();
        number[field] = json!(7);
        assert!(parse(&number).is_err(), "{field}");
    }
}

#[test]
fn the_system_task_type_must_be_thread_summary_update() {
    // inst-sp-task-type
    let mut value = payload();
    value["system_task_type"] = json!("chat_cleanup");
    let reason = reason_of(&value);
    assert!(reason.contains("system_task_type"), "{reason}");
    assert!(
        !reason.contains("chat_cleanup"),
        "no payload value: {reason}"
    );
    value.as_object_mut().unwrap().remove("system_task_type");
    assert!(reason_of(&value).contains("system_task_type"));
}

#[test]
fn the_target_needs_a_timestamp_and_a_message_id() {
    // inst-sp-target
    for field in ["frozen_target_created_at", "frozen_target_message_id"] {
        let mut missing = payload();
        missing.as_object_mut().unwrap().remove(field);
        let reason = reason_of(&missing);
        assert!(reason.contains(field), "{field}: {reason}");
    }
    let mut bad_time = payload();
    bad_time["frozen_target_created_at"] = json!("yesterday at noon");
    let reason = reason_of(&bad_time);
    assert!(reason.contains("frozen_target_created_at"), "{reason}");
    assert!(!reason.contains("yesterday"), "{reason}");
    let mut bad_id = payload();
    bad_id["frozen_target_message_id"] = json!("zzz");
    assert!(reason_of(&bad_id).contains("frozen_target_message_id"));
}

#[test]
fn a_malformed_base_timestamp_or_id_is_reported() {
    // inst-sp-base: both set but one does not parse
    let mut bad_time = payload();
    bad_time["base_frontier_created_at"] = json!("soon");
    assert!(reason_of(&bad_time).contains("base_frontier_created_at"));
    let mut bad_id = payload();
    bad_id["base_frontier_message_id"] = json!("nope");
    assert!(reason_of(&bad_id).contains("base_frontier_message_id"));
}

#[test]
fn a_dedupe_key_is_optional() {
    // inst-sp-dedupe: the check applies only when a key is present
    let mut value = payload();
    value.as_object_mut().unwrap().remove("dedupe_key");
    assert!(parse(&value).is_ok());
    value["dedupe_key"] = Value::Null;
    assert!(parse(&value).is_ok());
}

#[test]
fn a_dedupe_key_must_start_with_the_lowercase_hex_tenant_and_a_slash() {
    // inst-sp-dedupe, inst-sp-malformed
    let tenant_hex = TENANT_A.simple().to_string();
    for key in [
        format!("{}/thread_summary_update/x", TENANT_B.simple()),
        format!("{}/thread_summary_update/x", tenant_hex.to_uppercase()),
        format!("{TENANT_A}/thread_summary_update/x"),
        format!("{tenant_hex}thread_summary_update/x"),
        "thread_summary_update".to_owned(),
        String::new(),
    ] {
        let mut value = payload();
        value["dedupe_key"] = json!(key);
        let reason = reason_of(&value);
        assert!(reason.contains("dedupe_key"), "{key}: {reason}");
        assert!(
            !reason.contains(&tenant_hex),
            "the reason holds no payload value: {reason}"
        );
    }
    let mut good = payload();
    good["dedupe_key"] = json!(format!("{tenant_hex}/thread_summary_update/anything"));
    assert!(parse(&good).is_ok());
}

#[test]
fn the_payload_written_by_the_trigger_parses_back_to_the_same_work_item() {
    // the enqueue contract of `inst-sum-enqueue` and the parser agree on every field name
    let work = crate::test_support::maintenance::work(
        TENANT_A,
        CHAT,
        Some((t(10), BASE_ID)),
        (t(90), TARGET_ID),
    );
    let value = summary_payload(
        work.tenant_id,
        work.chat_id,
        Some((t(10), BASE_ID)),
        (t(90), TARGET_ID),
        work.system_request_id,
    );
    assert_eq!(parse(&value).unwrap(), work);
    assert_eq!(value["frozen_target_created_at"], rfc3339(t(90)));
}
