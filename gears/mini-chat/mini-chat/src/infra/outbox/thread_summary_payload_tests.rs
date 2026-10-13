#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 4 and 5: the thread-summary work item payload
//! (`cpt-cf-mini-chat-algo-background-workers-evaluate-summary-trigger`,
//! `inst-build-summary-payload`; `-handle-thread-summary`, `inst-decode-summary`):
//! exactly the eight keys, timestamps in RFC 3339 UTC with microseconds, the
//! base pair both `null` or both set, and a strict decode.

use std::collections::BTreeSet;

use serde_json::{Value, json};
use time::OffsetDateTime;
use uuid::Uuid;

use super::{decode_task, encode_task};
use crate::domain::service::stream::ports::MessageBoundary;
use crate::domain::service::workers::summary_trigger::SummaryTask;

fn at_micros(unix: i64, micros: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix).unwrap() + time::Duration::microseconds(micros)
}

fn task(base: Option<MessageBoundary>) -> SummaryTask {
    SummaryTask {
        tenant_id: Uuid::from_u128(0x7E),
        chat_id: Uuid::from_u128(0xC1),
        system_request_id: Uuid::from_u128(0x5157),
        base,
        target: MessageBoundary {
            created_at: at_micros(1_773_144_000, 250_001),
            id: Uuid::from_u128(0xD2),
        },
    }
}

fn base() -> MessageBoundary {
    MessageBoundary {
        created_at: at_micros(1_773_143_000, 0),
        id: Uuid::from_u128(0xD1),
    }
}

fn json_of(task: &SummaryTask) -> Value {
    serde_json::from_slice(&encode_task(task).unwrap()).unwrap()
}

fn valid() -> Value {
    json_of(&task(Some(base())))
}

#[test]
fn the_payload_has_exactly_the_eight_keys_with_their_values() {
    let json = json_of(&task(Some(base())));

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
            "base_frontier_created_at",
            "base_frontier_message_id",
            "frozen_target_created_at",
            "frozen_target_message_id",
            "system_task_type"
        ])
    );
    assert_eq!(json["tenant_id"], Uuid::from_u128(0x7E).to_string());
    assert_eq!(json["chat_id"], Uuid::from_u128(0xC1).to_string());
    assert_eq!(
        json["system_request_id"],
        Uuid::from_u128(0x5157).to_string()
    );
    assert_eq!(
        json["base_frontier_message_id"],
        Uuid::from_u128(0xD1).to_string()
    );
    assert_eq!(
        json["frozen_target_message_id"],
        Uuid::from_u128(0xD2).to_string()
    );
    assert_eq!(json["system_task_type"], "thread_summary_update");
}

#[test]
fn timestamps_are_rfc_3339_utc_with_exactly_six_fractional_digits() {
    let json = json_of(&task(Some(base())));

    assert_eq!(
        json["frozen_target_created_at"],
        "2026-03-10T12:00:00.250001Z"
    );
    assert_eq!(
        json["base_frontier_created_at"], "2026-03-10T11:43:20.000000Z",
        "a whole second still carries six digits"
    );
}

#[test]
fn a_first_summary_has_a_null_base_pair() {
    let json = json_of(&task(None));

    assert!(json["base_frontier_created_at"].is_null());
    assert!(json["base_frontier_message_id"].is_null());
    assert_eq!(
        json.as_object().unwrap().len(),
        8,
        "the keys are present, with null values"
    );
}

#[test]
fn the_payload_round_trips_with_and_without_a_base() {
    for original in [task(None), task(Some(base()))] {
        let decoded = decode_task(&encode_task(&original).unwrap()).unwrap();

        assert_eq!(decoded, original);
    }
}

#[test]
fn a_timestamp_with_an_offset_is_normalized_to_utc() {
    let mut json = valid();
    json["frozen_target_created_at"] = json!("2026-03-10T14:00:00.250001+02:00");

    let decoded = decode_task(&serde_json::to_vec(&json).unwrap()).unwrap();

    assert_eq!(decoded.target.created_at, at_micros(1_773_144_000, 250_001));
}

fn malformed(json: &Value) -> bool {
    decode_task(&serde_json::to_vec(json).unwrap()).is_err()
}

#[test]
fn every_missing_key_is_malformed() {
    for key in [
        "tenant_id",
        "chat_id",
        "system_request_id",
        "base_frontier_created_at",
        "base_frontier_message_id",
        "frozen_target_created_at",
        "frozen_target_message_id",
        "system_task_type",
    ] {
        let mut json = valid();
        json.as_object_mut().unwrap().remove(key);

        assert!(malformed(&json), "missing `{key}` must be rejected");
    }
}

#[test]
fn an_unknown_key_is_malformed() {
    let mut json = valid();
    json["extra"] = json!(1);

    assert!(malformed(&json));
}

#[test]
fn a_non_uuid_id_is_malformed() {
    for key in [
        "tenant_id",
        "chat_id",
        "system_request_id",
        "base_frontier_message_id",
        "frozen_target_message_id",
    ] {
        let mut json = valid();
        json[key] = json!("not-a-uuid");

        assert!(malformed(&json), "`{key}`");
    }
}

#[test]
fn a_timestamp_that_is_not_rfc_3339_is_malformed() {
    for key in ["base_frontier_created_at", "frozen_target_created_at"] {
        let mut json = valid();
        json[key] = json!("yesterday at noon");

        assert!(malformed(&json), "`{key}`");
    }
}

#[test]
fn only_one_half_of_the_base_pair_is_malformed() {
    let mut only_id = valid();
    only_id["base_frontier_created_at"] = Value::Null;
    let mut only_time = valid();
    only_time["base_frontier_message_id"] = Value::Null;

    assert!(malformed(&only_id));
    assert!(malformed(&only_time));
}

#[test]
fn another_system_task_type_is_malformed() {
    let mut json = valid();
    json["system_task_type"] = json!("doc_summary");

    assert!(malformed(&json));
}

#[test]
fn not_json_and_wrong_shapes_are_malformed() {
    for bytes in [&b"not json"[..], b"[]", b"null", b"42", b"{}"] {
        assert!(
            decode_task(bytes).is_err(),
            "{}",
            String::from_utf8_lossy(bytes)
        );
    }
}

#[test]
fn a_decode_error_does_not_echo_the_payload_values() {
    let error = decode_task(br#"{"tenant_id":"leaky-value"}"#).unwrap_err();

    assert!(!error.to_string().contains("leaky-value"), "{error}");
}
