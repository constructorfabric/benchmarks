//! T050: retry / edit / delete of the latest terminal turn.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names
)]
mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use common::*;
use mini_chat_sdk::AuditEvent;
use serde_json::{Value, json};
use tokio::sync::Notify;
use uuid::Uuid;

fn turn_url(chat: Uuid, rid: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat}/turns/{rid}")
}

async fn retry(h: &Harness, chat: Uuid, rid: Uuid) -> SseResult {
    h.sse(
        &h.ctx(),
        "POST",
        &format!("{}/retry", turn_url(chat, rid)),
        None,
    )
    .await
}

async fn edit(h: &Harness, chat: Uuid, rid: Uuid, content: &str) -> SseResult {
    h.sse(
        &h.ctx(),
        "PATCH",
        &turn_url(chat, rid),
        Some(json!({"content": content})),
    )
    .await
}

fn mutation_events(h: &Harness) -> Vec<mini_chat_sdk::TurnMutationAuditEvent> {
    h.audit_events()
        .into_iter()
        .filter_map(|e| match e {
            AuditEvent::Mutation(m) => Some(m),
            AuditEvent::Turn(_) => None,
        })
        .collect()
}

#[tokio::test]
async fn retry_replaces_latest_turn_with_new_request_id() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.provider.push(Script::ok("first answer"));
    let rid = Uuid::new_v4();
    h.send_body(chat, json!({"content": "question", "request_id": rid}))
        .await;
    let (_, before) = h.get(&format!("/mini-chat/v1/chats/{chat}")).await;
    tokio::time::sleep(Duration::from_millis(5)).await;

    h.provider.push(Script::ok("second answer"));
    let r = retry(&h, chat, rid).await;
    assert_eq!(r.status, StatusCode::OK, "{r:?}");
    assert_eq!(r.names().first(), Some(&"stream_started"));
    assert_eq!(r.names().last(), Some(&"done"));
    let new_rid = r.request_id();
    assert_ne!(new_rid, rid);
    assert_eq!(r.first("stream_started").unwrap()["is_new_turn"], true);

    let msgs = h.messages(chat).await;
    let contents: Vec<&str> = msgs
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(
        contents,
        vec!["question", "second answer"],
        "old turn messages hidden, user message copied"
    );
    assert_eq!(msgs[0]["request_id"], new_rid.to_string());

    let turns = db::turns(&h, chat).await;
    let old = turns.iter().find(|t| t.request_id == rid).unwrap();
    assert!(old.deleted_at.is_some());
    assert_eq!(old.replaced_by_request_id, Some(new_rid));
    let (s, _) = h.get(&turn_url(chat, rid)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (_, v) = h.get(&turn_url(chat, new_rid)).await;
    assert_eq!(v["state"], "done");

    let (_, after) = h.get(&format!("/mini-chat/v1/chats/{chat}")).await;
    assert!(after["updated_at"].as_str().unwrap() > before["updated_at"].as_str().unwrap());
    assert_eq!(after["message_count"], 2);

    assert!(
        h.eventually(|| mutation_events(&h)
            .iter()
            .any(|m| m.event_type == "turn_retry"))
            .await
    );
    let m = mutation_events(&h)
        .into_iter()
        .find(|m| m.event_type == "turn_retry")
        .unwrap();
    assert_eq!(m.original_request_id, Some(rid));
    assert_eq!(m.new_request_id, Some(new_rid));
    assert_eq!(m.actor_user_id, h.user);

    // the provider received the same user content on retry
    let reqs = h.provider.chat_requests();
    let last = reqs.last().unwrap()["input"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(last["content"][0]["text"], "question");
    // and the replaced turn is no longer part of the context
    assert_eq!(reqs.last().unwrap()["input"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn edit_replaces_content() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let rid = Uuid::new_v4();
    h.send_body(chat, json!({"content": "orig", "request_id": rid}))
        .await;
    h.provider.push(Script::ok("edited answer"));
    let r = edit(&h, chat, rid, "  edited question ").await;
    assert_eq!(r.names().last(), Some(&"done"), "{r:?}");
    let msgs = h.messages(chat).await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[1]["content"], "edited answer");
    assert!(
        msgs[0]["content"]
            .as_str()
            .unwrap()
            .contains("edited question")
    );
    let r = edit(&h, chat, r.request_id(), "   ").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_field_reason(&r.error, "content", "EMPTY_CONTENT");
    assert!(
        h.eventually(|| mutation_events(&h)
            .iter()
            .any(|m| m.event_type == "turn_edit"))
            .await
    );
}

#[tokio::test]
async fn only_latest_terminal_turn_can_be_mutated() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let r1 = Uuid::new_v4();
    let r2 = Uuid::new_v4();
    h.send_body(chat, json!({"content": "1", "request_id": r1}))
        .await;
    h.send_body(chat, json!({"content": "2", "request_id": r2}))
        .await;

    // not latest
    let r = retry(&h, chat, r1).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_reason(&r.error, "NOT_LATEST_TURN");
    let r = edit(&h, chat, r1, "x").await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    let (s, v, _) = h.req(&h.ctx(), "DELETE", &turn_url(chat, r1), None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_reason(&v, "NOT_LATEST_TURN");
    // unknown turn
    let r = retry(&h, chat, Uuid::new_v4()).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    // running turn -> 400 turn_state
    let gate = Arc::new(Notify::new());
    h.provider.push(Script::Gated {
        parts: vec!["x".into()],
        gate: gate.clone(),
        usage: (1, 1),
    });
    let r3 = Uuid::new_v4();
    let mut s = h
        .open_send(chat, json!({"content": "3", "request_id": r3}))
        .await;
    assert!(s.until("delta").await);
    let r = retry(&h, chat, r3).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{r:?}");
    assert!(r.error.to_string().contains("turn_state"), "{}", r.error);
    let (s3, _, _) = h.req(&h.ctx(), "DELETE", &turn_url(chat, r3), None).await;
    assert_eq!(s3, StatusCode::BAD_REQUEST);
    // r2 is no longer latest either
    let r = retry(&h, chat, r2).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    gate.notify_one();
    assert!(s.until("done").await);
    h.wait_turn_terminal(chat).await;

    // delete latest, then the deleted turn is not mutable (409)
    let (st, _, _) = h.req(&h.ctx(), "DELETE", &turn_url(chat, r3), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let r = retry(&h, chat, r3).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_reason(&r.error, "NOT_LATEST_TURN");
    // after deleting r3, r2 is the latest again
    let r = retry(&h, chat, r2).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(
        h.eventually(|| mutation_events(&h)
            .iter()
            .any(|m| m.event_type == "turn_delete" && m.request_id == Some(r3)))
            .await
    );
}

#[tokio::test]
async fn foreign_requester_gets_403() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let rid = Uuid::new_v4();
    h.send_body(chat, json!({"content": "1", "request_id": rid}))
        .await;
    db::set_turn_requester(&h, rid, Uuid::new_v4()).await;
    let r = retry(&h, chat, rid).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_reason(&r.error, "AUTHZ_DENIED");
    let (s, _, _) = h.req(&h.ctx(), "DELETE", &turn_url(chat, rid), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn quota_rejection_leaves_previous_turn_intact() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let rid = Uuid::new_v4();
    h.send_body(chat, json!({"content": "1", "request_id": rid}))
        .await;
    let mut p = default_policy();
    p["default_standard_limits"] =
        json!({"limit_daily_credits_micro": 1, "limit_monthly_credits_micro": 1});
    p["default_premium_limits"] =
        json!({"limit_daily_credits_micro": 1, "limit_monthly_credits_micro": 1});
    h.set_policy(p);
    let calls = h.provider.chat_requests().len();
    let r = retry(&h, chat, rid).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "{r:?}");
    assert_eq!(h.provider.chat_requests().len(), calls);
    let turns = db::turns(&h, chat).await;
    assert_eq!(turns.len(), 1);
    assert!(turns[0].deleted_at.is_none());
    assert_eq!(h.messages(chat).await.len(), 2);
}

#[tokio::test]
async fn mutations_carry_attachments_and_web_search() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let img = h.upload_ok(chat, "a.png", "image/png", &png(8, 8)).await;
    let rid = Uuid::new_v4();
    h.send_body(chat, json!({"content": "look", "request_id": rid, "attachment_ids": [img], "web_search": {"enabled": true}})).await;
    let first = h.provider.chat_requests().last().unwrap().clone();
    let has_ws = |j: &Value| {
        j["tools"].as_array().is_some_and(|t| {
            t.iter()
                .any(|t| t["type"] == "web_search" || t["type"] == "web_search_preview")
        })
    };
    assert!(has_ws(&first), "{first}");

    let r = retry(&h, chat, rid).await;
    assert_eq!(r.names().last(), Some(&"done"));
    let second = h.provider.chat_requests().last().unwrap().clone();
    assert!(has_ws(&second), "web_search flag reused: {second}");
    let user = second["input"].as_array().unwrap().last().unwrap().clone();
    assert!(
        user["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["type"] == "input_image"),
        "{user}"
    );
    let msgs = h.messages(chat).await;
    assert_eq!(msgs[0]["attachments"][0]["attachment_id"], img.to_string());

    // edit keeps the attachment links too
    let r = edit(&h, chat, r.request_id(), "look again").await;
    assert_eq!(r.names().last(), Some(&"done"));
    let msgs = h.messages(chat).await;
    assert_eq!(msgs[0]["attachments"][0]["attachment_id"], img.to_string());
    assert_eq!(msgs[0]["content"], "look again");
}

#[tokio::test]
async fn concurrent_mutations_one_wins() {
    let h = Arc::new(Harness::new().await);
    let chat = h.create_chat().await;
    let rid = Uuid::new_v4();
    h.send_body(chat, json!({"content": "1", "request_id": rid}))
        .await;
    let gate = Arc::new(Notify::new());
    for _ in 0..3 {
        h.provider.push(Script::Gated {
            parts: vec!["x".into()],
            gate: gate.clone(),
            usage: (1, 1),
        });
    }
    let mut handles = Vec::new();
    for _ in 0..3 {
        let h = h.clone();
        handles.push(tokio::spawn(async move {
            let mut s = h
                .open(
                    &h.ctx(),
                    "POST",
                    &format!("{}/retry", turn_url(chat, rid)),
                    None,
                )
                .await;
            if s.status == StatusCode::OK {
                s.until("stream_started").await;
            }
            s
        }));
    }
    let mut streams = Vec::new();
    for hd in handles {
        streams.push(hd.await.unwrap());
    }
    let ok = streams
        .iter()
        .filter(|s| s.status == StatusCode::OK)
        .count();
    assert_eq!(
        ok,
        1,
        "{:?}",
        streams.iter().map(|s| s.status).collect::<Vec<_>>()
    );
    assert!(
        streams
            .iter()
            .filter(|s| s.status != StatusCode::OK)
            .all(|s| s.status == StatusCode::CONFLICT)
    );
    gate.notify_waiters();
    drop(streams);
    let turns = h.wait_turn_terminal(chat).await;
    assert_eq!(turns.iter().filter(|t| t.deleted_at.is_none()).count(), 1);
}
