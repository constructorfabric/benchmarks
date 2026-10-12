//! `GET /v1/chats/{id}/turns/{request_id}`: the authoritative, read-only turn
//! status.
//!
//! Source of truth: DESIGN 3.3 "Turn Status API" (fields, omitted-when-null
//! rules, internal-to-API state mapping, `error_code` semantics, 404 for a
//! soft-deleted turn or another user's chat), 3.8 (`read_turn` action on the
//! chat resource), the REST error table (404 with `turn` resource type,
//! `invalid_path_params`) and the OpenAPI schemas `TurnStatusResponse` /
//! `TurnStatusState`.

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

mod chat_support;
mod common;
mod stream_support;

use std::collections::BTreeSet;

use chat_support::{CHAT_PDP_TYPE, assert_not_found, assert_violation_reason, parse_ts, ts_micros};
use serde_json::{Value, json};
use stream_support::{
    ChatSeed, FakePdp, MessageSeed, Opts, ProviderReply, StreamHarness, StreamOptions, TENANT_B,
    TurnSeed, USER_B, assert_problem, ctx_a, second_user, tenant_b_user,
};
use uuid::Uuid;

/// (seed, expected API state, expected error code, has an assistant message id)
type Case<'a> = (TurnSeed, &'a str, Option<&'a str>, bool);

fn keys(body: &Value) -> BTreeSet<String> {
    body.as_object().unwrap().keys().cloned().collect()
}

fn key_set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| (*s).to_owned()).collect()
}

#[tokio::test]
async fn a_running_turn_reports_running_without_error_code_or_message_id() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::text_with(
        &["a", "b"],
        &Opts::default().pause_after(1),
    ));
    let mut live = h
        .open(chat, &ctx_a(), &json!({"content": "q", "request_id": rid}))
        .await
        .live();
    live.expect("stream_started").await;
    live.next_non_ping().await.unwrap();
    h.oagw.wait_paused(1).await;

    let resp = h.turn_status(chat, rid, &ctx_a()).await;
    assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
    assert!(resp.content_type().starts_with("application/json"));
    let body = resp.json();
    assert_eq!(body["request_id"], json!(rid.to_string()));
    assert_eq!(body["state"], json!("running"));
    assert_eq!(keys(&body), key_set(&["request_id", "state", "updated_at"]));
    parse_ts(&body["updated_at"]);

    h.oagw.release();
    live.drain().await;
}

#[tokio::test]
async fn a_completed_turn_is_done_and_names_the_assistant_message() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let sent = h.say(chat, "q").await;
    let resp = h.turn_status(chat, sent.request_id(), &ctx_a()).await;
    assert_eq!(resp.status.as_u16(), 200);
    let body = resp.json();
    assert_eq!(body["state"], json!("done"));
    assert_eq!(
        body["assistant_message_id"],
        json!(sent.message_id().to_string())
    );
    assert_eq!(
        keys(&body),
        key_set(&["request_id", "state", "assistant_message_id", "updated_at"]),
        "error_code is omitted for a done turn"
    );

    // the id resolves to the stored answer
    let list = h
        .chat
        .list_messages(
            chat,
            &format!("$filter=id%20eq%20{}", sent.message_id()),
            &ctx_a(),
        )
        .await;
    assert_eq!(list.status.as_u16(), 200, "{}", list.raw);
    assert_eq!(list.json()["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn a_failed_turn_is_error_with_its_error_code_and_no_message_id() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::http_error(429, "slow down"));
    let sent = h.say(chat, "q").await;
    let body = h
        .turn_status(chat, sent.request_id(), &ctx_a())
        .await
        .json();
    assert_eq!(body["state"], json!("error"));
    assert_eq!(body["error_code"], json!("rate_limited"));
    assert_eq!(
        keys(&body),
        key_set(&["request_id", "state", "error_code", "updated_at"])
    );
}

#[tokio::test]
async fn every_internal_state_maps_to_its_api_state() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let assistant = h
        .chat
        .seed_message(MessageSeed::assistant(chat).content("partial").at(3))
        .await;

    let cases: Vec<Case<'_>> = vec![
        (
            TurnSeed::new(chat, Uuid::new_v4(), "running"),
            "running",
            None,
            false,
        ),
        (
            TurnSeed::new(chat, Uuid::new_v4(), "completed").assistant(assistant),
            "done",
            None,
            true,
        ),
        (
            TurnSeed::new(chat, Uuid::new_v4(), "failed").error("orphan_timeout"),
            "error",
            Some("orphan_timeout"),
            false,
        ),
        (
            TurnSeed::new(chat, Uuid::new_v4(), "failed").error("provider_timeout"),
            "error",
            Some("provider_timeout"),
            false,
        ),
        (
            TurnSeed::new(chat, Uuid::new_v4(), "cancelled"),
            "cancelled",
            None,
            false,
        ),
        (
            TurnSeed::new(chat, Uuid::new_v4(), "cancelled").assistant(assistant),
            "cancelled",
            None,
            true,
        ),
    ];
    for (seed, state, error_code, has_message) in cases {
        let rid = seed.request_id;
        h.seed_turn(seed).await;
        let resp = h.turn_status(chat, rid, &ctx_a()).await;
        assert_eq!(resp.status.as_u16(), 200, "{state}: {}", resp.raw);
        let body = resp.json();
        assert_eq!(body["state"], json!(state), "{body}");
        assert_eq!(body["request_id"], json!(rid.to_string()));
        match error_code {
            Some(code) => assert_eq!(body["error_code"], json!(code)),
            None => assert!(body.get("error_code").is_none(), "{state}: {body}"),
        }
        if has_message {
            assert_eq!(
                body["assistant_message_id"],
                json!(assistant.to_string()),
                "{state}"
            );
        } else {
            assert!(
                body.get("assistant_message_id").is_none(),
                "{state}: {body}"
            );
        }
        assert_eq!(
            parse_ts(&body["updated_at"]),
            ts_micros(12, 345_678),
            "updated_at is the turn's updated_at"
        );
    }
}

#[tokio::test]
async fn an_unknown_request_id_is_404_for_the_turn_resource() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let resp = h.turn_status(chat, Uuid::new_v4(), &ctx_a()).await;
    assert_not_found(&resp, "turn");
}

#[tokio::test]
async fn a_soft_deleted_turn_is_404() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.seed_turn(TurnSeed::new(chat, rid, "completed").deleted())
        .await;
    let resp = h.turn_status(chat, rid, &ctx_a()).await;
    assert_not_found(&resp, "turn");
}

#[tokio::test]
async fn the_turn_of_a_foreign_unknown_or_deleted_chat_is_404() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.seed_turn(TurnSeed::new(chat, rid, "completed")).await;

    for ctx in [second_user(), tenant_b_user()] {
        let resp = h.turn_status(chat, rid, &ctx).await;
        assert_problem(&resp, 404, "not_found");
        assert!(
            !resp.raw.contains("completed"),
            "nothing of the turn leaks: {}",
            resp.raw
        );
    }
    let resp = h.turn_status(Uuid::new_v4(), rid, &ctx_a()).await;
    assert_not_found(&resp, "chat");

    let deleted = h.chat.seed_chat(ChatSeed::of_a().deleted()).await;
    let rid_deleted = Uuid::new_v4();
    h.seed_turn(TurnSeed::new(deleted, rid_deleted, "completed"))
        .await;
    let resp = h.turn_status(deleted, rid_deleted, &ctx_a()).await;
    assert_not_found(&resp, "chat");

    // a turn of another chat of the same user is not found under this chat
    let other = h.seed_chat().await;
    let resp = h.turn_status(other, rid, &ctx_a()).await;
    assert_not_found(&resp, "turn");
}

#[tokio::test]
async fn non_uuid_path_parameters_are_400_invalid_path_params() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    for uri in [
        format!("{}/v1/chats/{chat}/turns/not-a-uuid", h.chat.prefix),
        format!(
            "{}/v1/chats/not-a-uuid/turns/{}",
            h.chat.prefix,
            Uuid::new_v4()
        ),
    ] {
        let resp = common::get(&h.router, &uri, &ctx_a()).await;
        let problem = assert_problem(&resp, 400, "invalid_argument");
        assert_violation_reason(&problem, "invalid_path_params");
    }
}

#[tokio::test]
async fn reading_the_status_is_authorized_as_read_turn_on_the_chat() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.seed_turn(TurnSeed::new(chat, rid, "completed")).await;
    let resp = h.turn_status(chat, rid, &ctx_a()).await;
    assert_eq!(resp.status.as_u16(), 200);
    let req = h.pdp.last();
    assert_eq!(req.action.name, "read_turn");
    assert_eq!(req.resource.resource_type, CHAT_PDP_TYPE);
    assert_eq!(req.resource.id, Some(chat));
    assert!(req.context.require_constraints);
}

#[tokio::test]
async fn a_denied_or_unreachable_pdp_fails_closed() {
    for (pdp, status, category) in [
        (FakePdp::deny("rule-9"), 403, "permission_denied"),
        (
            FakePdp::unreachable("pdp-secret"),
            503,
            "service_unavailable",
        ),
    ] {
        let h = StreamHarness::with(StreamOptions {
            pdp,
            ..Default::default()
        })
        .await;
        let chat = h.seed_chat().await;
        let rid = Uuid::new_v4();
        h.seed_turn(TurnSeed::new(chat, rid, "completed")).await;
        let resp = h.turn_status(chat, rid, &ctx_a()).await;
        assert_problem(&resp, status, category);
        assert!(!resp.raw.contains("rule-9") && !resp.raw.contains("pdp-secret"));
    }
}

#[tokio::test]
async fn reading_the_status_changes_nothing_and_never_calls_the_provider() {
    let h = StreamHarness::new().await;
    let chat = h.seed_chat().await;
    let sent = h.say(chat, "q").await;
    let calls = h.oagw.request_count();
    let counts = h.row_counts().await;
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    let chat_row = h.chat.chat_row(chat).await.unwrap();

    for _ in 0..3 {
        h.turn_status(chat, sent.request_id(), &ctx_a()).await;
    }
    assert_eq!(h.oagw.request_count(), calls);
    assert_eq!(h.row_counts().await, counts);
    assert_eq!(h.turn_of(sent.request_id()).await.unwrap(), turn);
    assert_eq!(
        h.chat.chat_row(chat).await.unwrap().updated_at,
        chat_row.updated_at
    );
}

#[tokio::test]
async fn the_status_of_a_seeded_tenant_b_turn_is_invisible_to_tenant_a() {
    let h = StreamHarness::new().await;
    let b_chat = h
        .chat
        .seed_chat(ChatSeed::new(TENANT_B, USER_B).at(0))
        .await;
    let rid = Uuid::new_v4();
    h.seed_turn(TurnSeed::new(b_chat, rid, "completed")).await;
    let resp = h.turn_status(b_chat, rid, &ctx_a()).await;
    assert_problem(&resp, 404, "not_found");
}
