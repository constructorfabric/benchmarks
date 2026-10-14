//! Wire shape of the chat DTOs and the canonical mapping of the chat-management errors.
//!
//! Feature `cpt-cf-mini-chat-feature-chat-management`: algorithm
//! `cpt-cf-mini-chat-algo-chat-management-chat-detail-projection`; the title and list-query
//! error mappings of `title-validation` and `list-query`; definitions of done
//! `cpt-cf-mini-chat-dod-chat-management-get`, `cpt-cf-mini-chat-dod-chat-management-list`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use serde_json::{Value, json};
use toolkit_canonical_errors::{CanonicalError, Problem};
use uuid::Uuid;

use super::dto::{ChatDetailDto, CreateChatReq, UpdateChatReq};
use crate::domain::chats::ChatDetail;
use crate::domain::error::DomainError;
use crate::test_support::chats::{CHAT_RESOURCE, ODATA_RESOURCE, at, detail_keys, keys, parse_ts};

fn detail(title: Option<&str>) -> ChatDetail {
    ChatDetail {
        id: Uuid::from_u128(7),
        model: "gpt-5.2".to_owned(),
        title: title.map(ToOwned::to_owned),
        is_temporary: false,
        message_count: 12,
        created_at: at(1),
        updated_at: at(2),
    }
}

fn dto_literal(title: Option<&str>) -> ChatDetailDto {
    ChatDetailDto {
        id: Uuid::from_u128(7),
        model: "gpt-5.2".to_owned(),
        title: title.map(ToOwned::to_owned),
        is_temporary: false,
        message_count: 12,
        created_at: at(1),
        updated_at: at(2),
    }
}

// ── projection ───────────────────────────────────────────────────────────────

#[test]
fn the_projection_of_a_titled_chat_carries_every_field_and_no_internal_one() {
    let value = serde_json::to_value(ChatDetailDto::from(detail(Some("Q3 plan")))).unwrap();
    assert_eq!(keys(&value), detail_keys(true));
    assert_eq!(value["id"], Uuid::from_u128(7).to_string());
    assert_eq!(value["model"], "gpt-5.2");
    assert_eq!(value["title"], "Q3 plan");
    assert_eq!(value["is_temporary"], false);
    assert_eq!(value["message_count"], 12);
    assert_eq!(parse_ts(&value["created_at"]), at(1));
    assert_eq!(parse_ts(&value["updated_at"]), at(2));
    for internal in ["user_id", "tenant_id", "deleted_at", "messages"] {
        assert!(value.get(internal).is_none(), "{internal}");
    }
}

#[test]
fn the_projection_of_an_untitled_chat_omits_the_title() {
    let value = serde_json::to_value(ChatDetailDto::from(detail(None))).unwrap();
    assert_eq!(keys(&value), detail_keys(false));
}

#[test]
fn the_dto_omits_a_missing_title_and_never_sends_null() {
    let untitled = serde_json::to_value(dto_literal(None)).unwrap();
    assert_eq!(keys(&untitled), detail_keys(false), "{untitled}");
    let titled = serde_json::to_value(dto_literal(Some("t"))).unwrap();
    assert_eq!(titled["title"], "t");
}

#[test]
fn the_dto_timestamps_are_rfc_3339_strings() {
    let value = serde_json::to_value(dto_literal(Some("t"))).unwrap();
    for field in ["created_at", "updated_at"] {
        let text = value[field]
            .as_str()
            .unwrap_or_else(|| panic!("{field}: {value}"));
        assert!(text.contains('T'), "{field}: {text}");
        assert!(text.ends_with('Z') || text.contains('+'), "{field}: {text}");
    }
    assert_eq!(parse_ts(&value["created_at"]), at(1));
}

// ── request bodies ───────────────────────────────────────────────────────────

#[test]
fn the_create_body_takes_optional_nullable_strings() {
    let empty: CreateChatReq = serde_json::from_value(json!({})).unwrap();
    assert_eq!((empty.title, empty.model), (None, None));
    let nulls: CreateChatReq =
        serde_json::from_value(json!({ "title": null, "model": null })).unwrap();
    assert_eq!((nulls.title, nulls.model), (None, None));
    let full: CreateChatReq =
        serde_json::from_value(json!({ "title": "t", "model": "m" })).unwrap();
    assert_eq!(full.title.as_deref(), Some("t"));
    assert_eq!(full.model.as_deref(), Some("m"));
    for bad in [json!({ "title": 5 }), json!({ "model": [] }), json!([])] {
        assert!(
            serde_json::from_value::<CreateChatReq>(bad.clone()).is_err(),
            "{bad}"
        );
    }
}

#[test]
fn the_update_body_requires_a_string_title_and_ignores_unknown_fields() {
    let ok: UpdateChatReq =
        serde_json::from_value(json!({ "title": "t", "model": "ignored", "extra": 1 })).unwrap();
    assert_eq!(ok.title, "t");
    for bad in [
        json!({}),
        json!({ "title": null }),
        json!({ "title": 5 }),
        json!({ "model": "m" }),
    ] {
        assert!(
            serde_json::from_value::<UpdateChatReq>(bad.clone()).is_err(),
            "{bad}"
        );
    }
}

// ── error mapping ────────────────────────────────────────────────────────────

fn problem(err: DomainError) -> Value {
    serde_json::to_value(Problem::from(CanonicalError::from(err))).unwrap()
}

fn reasons(problem: &Value) -> BTreeSet<(String, String)> {
    problem["context"]["field_violations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            (
                v["field"].as_str().unwrap().to_owned(),
                v["reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn an_invalid_title_maps_to_a_400_with_the_title_violation_on_the_chat_type() {
    let value = problem(DomainError::InvalidTitle);
    assert_eq!(value["status"], 400);
    assert!(value["type"].as_str().unwrap().contains("invalid_argument"));
    assert_eq!(value["context"]["resource_type"], CHAT_RESOURCE);
    assert_eq!(
        reasons(&value),
        BTreeSet::from([("title".to_owned(), "INVALID_TITLE".to_owned())])
    );
    assert!(value.get("code").is_none());
}

#[test]
fn list_query_errors_map_to_a_400_with_the_odata_resource_type_and_the_library_reasons() {
    use toolkit_odata::Error as E;
    let cases: Vec<(E, &str, &str)> = vec![
        (
            E::InvalidFilter("bad".to_owned()),
            "$filter",
            "INVALID_FILTER",
        ),
        (
            E::InvalidOrderByField("model".to_owned()),
            "$orderby",
            "INVALID_ORDERBY_FIELD",
        ),
        (E::OrderMismatch, "cursor", "ORDER_MISMATCH"),
        (E::FilterMismatch, "cursor", "FILTER_MISMATCH"),
        (E::InvalidCursor, "cursor", "INVALID_CURSOR"),
        (E::CursorInvalidBase64, "cursor", "INVALID_CURSOR"),
        (E::InvalidLimit, "$top", "INVALID_LIMIT"),
    ];
    for (error, field, reason) in cases {
        let label = format!("{error}");
        let value = problem(DomainError::ODataQuery(error));
        assert_eq!(value["status"], 400, "{label}");
        assert_eq!(value["context"]["resource_type"], ODATA_RESOURCE, "{label}");
        assert!(
            reasons(&value).contains(&(field.to_owned(), reason.to_owned())),
            "{label}: {value}"
        );
    }
    let both = problem(DomainError::ODataQuery(E::OrderWithCursor));
    let found = reasons(&both);
    assert!(found.contains(&("$orderby".to_owned(), "ORDER_WITH_CURSOR".to_owned())));
    assert!(found.contains(&("cursor".to_owned(), "ORDER_WITH_CURSOR".to_owned())));
}

#[test]
fn a_database_failure_of_the_list_query_is_a_generic_500() {
    let value = problem(DomainError::ODataQuery(toolkit_odata::Error::Db(
        "driver text with secret".to_owned(),
    )));
    assert_eq!(value["status"], 500);
    assert!(value["type"].as_str().unwrap().contains("internal"));
    assert!(!value.to_string().contains("driver text with secret"));
}
