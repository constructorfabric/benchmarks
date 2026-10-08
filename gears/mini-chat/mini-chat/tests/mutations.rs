//! US4: tail-only turn mutations (retry / edit / delete).
//!
//! AC: Turn Mutations (latest+terminal only, full pipeline + new request id, deterministic
//! concurrency, attachment/tool carry-over), Cleanup & Recovery (summary invalidation).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use mini_chat_sdk::AuditEvent;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::json;
use toolkit_db::secure::{SecureEntityExt, SecureUpdateExt, secure_insert};
use toolkit_security::AccessScope;
use uuid::Uuid;

fn reason(p: &serde_json::Value) -> String {
    p["context"]["reason"].as_str().unwrap_or_default().to_owned()
}

async fn list_messages(h: &Harness, chat: Uuid) -> Vec<serde_json::Value> {
    let r = h.send(ALICE, "GET", &format!("/chats/{chat}/messages"), None).await;
    assert_eq!(r.status, 200, "{}", r.text());
    r.json()["items"].as_array().unwrap().clone()
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

fn started_rid(ev: &[(String, serde_json::Value)]) -> Uuid {
    Uuid::parse_str(find(ev, "stream_started")["request_id"].as_str().unwrap()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retry_latest_turn_replaces_it_with_new_request_id() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.say(ALICE, chat, "first").await;
    let old = Uuid::new_v4();
    h.stream(ALICE, chat, json!({"content": "question", "request_id": old})).await;
    h.provider.push(Reply::text("A better answer", 5, 6));
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/turns/{old}/retry"), None).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.headers["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    let ev = r.sse();
    assert_eq!(names(&ev).first(), Some(&"stream_started"));
    assert_eq!(names(&ev).last(), Some(&"done"));
    let new = started_rid(&ev);
    assert_ne!(new, old, "mutation gets a new request id");
    assert_eq!(find(&ev, "stream_started")["is_new_turn"], json!(true));

    // Old turn soft-deleted and linked to the new one.
    let turns = h.turns(chat).await;
    let old_t = turns.iter().find(|t| t.request_id == old).unwrap();
    assert!(old_t.deleted_at.is_some());
    assert_eq!(old_t.replaced_by_request_id, Some(new));
    let new_t = turns.iter().find(|t| t.request_id == new).unwrap();
    assert_eq!(new_t.state, "completed");
    h.send(ALICE, "GET", &format!("/chats/{chat}/turns/{old}"), None).await.problem(404);
    let s = h.send(ALICE, "GET", &format!("/chats/{chat}/turns/{new}"), None).await.json();
    assert_eq!(s["state"], json!("done"));

    // Visible history: first turn + the retried turn (same user content, new answer).
    let msgs = list_messages(&h, chat).await;
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[2]["content"], json!("question"));
    assert_eq!(msgs[2]["request_id"], json!(new.to_string()));
    assert_eq!(msgs[3]["content"], json!("A better answer"));
    let r = h.send(ALICE, "GET", &format!("/chats/{chat}"), None).await;
    assert_eq!(r.json()["message_count"], json!(4));

    // The provider got the previous history without the replaced turn.
    let req = h.provider.chat_requests().pop().unwrap();
    let input = req["input"].as_array().unwrap();
    assert_eq!(input.len(), 3, "{input:?}");
    assert!(input[2].to_string().contains("question"));
    assert!(!req.to_string().contains("question\"}]},{\"role\":\"assistant"), "old answer not resent");

    // Old request id is now a conflict on messages:stream.
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "q", "request_id": old}))).await;
    assert_eq!(reason(&r.problem(409)), "request_id_conflict");

    // Audit + full billing pipeline for the new turn.
    h.eventually("retry audit", || mutation_events(&h).iter().any(|m| m.event_type == "turn_retry")).await;
    let m = mutation_events(&h).into_iter().find(|m| m.event_type == "turn_retry").unwrap();
    assert_eq!(m.original_request_id, Some(old));
    assert_eq!(m.new_request_id, Some(new));
    assert_eq!(m.actor_user_id, ALICE.user);
    h.eventually("usage", || h.usage_events().iter().any(|u| u.request_id == new)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn edit_latest_turn_uses_new_content() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let old = Uuid::new_v4();
    h.stream(ALICE, chat, json!({"content": "orignal typo", "request_id": old})).await;
    let r = h.send(ALICE, "PATCH", &format!("/chats/{chat}/turns/{old}"), Some(json!({"content": "original fixed"}))).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let ev = r.sse();
    let new = started_rid(&ev);
    assert_ne!(new, old);
    let msgs = list_messages(&h, chat).await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["content"], json!("original fixed"));
    let req = h.provider.chat_requests().pop().unwrap();
    assert!(req.to_string().contains("original fixed"));
    assert!(!req.to_string().contains("orignal typo"));
    h.eventually("edit audit", || mutation_events(&h).iter().any(|m| m.event_type == "turn_edit")).await;

    // Empty content is rejected before anything changes.
    let r = h.send(ALICE, "PATCH", &format!("/chats/{chat}/turns/{new}"), Some(json!({"content": "  "}))).await;
    assert_eq!(fv_reason(&r.problem(400)), "EMPTY_CONTENT");
    assert!(h.turns(chat).await.iter().any(|t| t.request_id == new && t.deleted_at.is_none()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_latest_turn() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.say(ALICE, chat, "keep").await;
    let rid = Uuid::new_v4();
    h.stream(ALICE, chat, json!({"content": "drop", "request_id": rid})).await;
    let r = h.send(ALICE, "DELETE", &format!("/chats/{chat}/turns/{rid}"), None).await;
    assert_eq!(r.status, 204, "{}", r.text());
    let msgs = list_messages(&h, chat).await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["content"], json!("keep"));
    assert_eq!(h.send(ALICE, "GET", &format!("/chats/{chat}"), None).await.json()["message_count"], json!(2));
    // Deleting again → NOT_LATEST_TURN (already deleted).
    let r = h.send(ALICE, "DELETE", &format!("/chats/{chat}/turns/{rid}"), None).await;
    assert_eq!(reason(&r.problem(409)), "NOT_LATEST_TURN");
    h.eventually("delete audit", || mutation_events(&h).iter().any(|m| m.event_type == "turn_delete")).await;
    let m = mutation_events(&h).into_iter().find(|m| m.event_type == "turn_delete").unwrap();
    assert_eq!(m.request_id, Some(rid));
    assert!(h.provider.chat_requests().len() == 2, "delete makes no provider call");
    // The earlier turn became the latest and can now be mutated.
    let first = h.turns(chat).await.into_iter().find(|t| t.deleted_at.is_none()).unwrap().request_id;
    let r = h.send(ALICE, "DELETE", &format!("/chats/{chat}/turns/{first}"), None).await;
    assert_eq!(r.status, 204);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_latest_terminal_turn_can_be_mutated() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let first = Uuid::new_v4();
    h.stream(ALICE, chat, json!({"content": "1", "request_id": first})).await;
    h.say(ALICE, chat, "2").await;
    for (m, p, b) in [
        ("POST", format!("/chats/{chat}/turns/{first}/retry"), None),
        ("PATCH", format!("/chats/{chat}/turns/{first}"), Some(json!({"content": "x"}))),
        ("DELETE", format!("/chats/{chat}/turns/{first}"), None),
    ] {
        let r = h.send(ALICE, m, &p, b).await;
        assert_eq!(reason(&r.problem(409)), "NOT_LATEST_TURN", "{m} {p}");
    }
    // Unknown turn → 404.
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/turns/{}/retry", Uuid::new_v4()), None).await;
    r.problem(404);

    // Running turn → 400 STATE.
    h.provider.push(Reply::slow("slow slow slow", Duration::from_millis(300)));
    let running = Uuid::new_v4();
    let resp = h.open(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), json!({"content": "3", "request_id": running})).await;
    for _ in 0..100 {
        if h.turns(chat).await.iter().any(|t| t.request_id == running) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for (m, p, b) in [
        ("POST", format!("/chats/{chat}/turns/{running}/retry"), None),
        ("PATCH", format!("/chats/{chat}/turns/{running}"), Some(json!({"content": "x"}))),
        ("DELETE", format!("/chats/{chat}/turns/{running}"), None),
    ] {
        let r = h.send(ALICE, m, &p, b).await;
        let pr = r.problem(400);
        assert_eq!(pr["context"]["violations"][0]["type"], json!("STATE"), "{pr}");
        assert_eq!(pr["context"]["violations"][0]["subject"], json!("turn_state"));
    }
    drop(resp);
    // Foreign users cannot see the chat at all.
    let r = h.send(BOB, "POST", &format!("/chats/{chat}/turns/{first}/retry"), None).await;
    r.problem(404);
    let r = h.send(CAROL, "DELETE", &format!("/chats/{chat}/turns/{first}"), None).await;
    r.problem(404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_and_cancelled_turns_can_be_retried() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let failed = Uuid::new_v4();
    h.provider.push(Reply::Status { status: 500, body: json!({}), retry_after: None });
    h.stream(ALICE, chat, json!({"content": "try", "request_id": failed})).await;
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/turns/{failed}/retry"), None).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(names(&r.sse()).last(), Some(&"done"));
    let msgs = list_messages(&h, chat).await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["content"], json!("try"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_mutations_resolve_deterministically() {
    let h = Arc::new(Harness::new().await);
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let rid = Uuid::new_v4();
    h.stream(ALICE, chat, json!({"content": "x", "request_id": rid})).await;
    for _ in 0..5 {
        h.provider.push(Reply::slow("one two", Duration::from_millis(50)));
    }
    let mut tasks = Vec::new();
    for i in 0..5 {
        let h = Arc::clone(&h);
        tasks.push(tokio::spawn(async move {
            if i % 2 == 0 {
                h.send(ALICE, "POST", &format!("/chats/{chat}/turns/{rid}/retry"), None).await
            } else {
                h.send(ALICE, "PATCH", &format!("/chats/{chat}/turns/{rid}"), Some(json!({"content": format!("e{i}")}))).await
            }
        }));
    }
    let mut ok = 0;
    for t in tasks {
        let r = t.await.unwrap();
        match r.status {
            200 => ok += 1,
            409 => {
                let why = reason(&r.json());
                assert!(why == "NOT_LATEST_TURN" || why == "GENERATION_IN_PROGRESS", "{why}");
            }
            400 => {
                // Lost to a winner whose new turn is still running.
                assert_eq!(r.json()["context"]["violations"][0]["type"], json!("STATE"));
            }
            s => panic!("unexpected {s}: {}", r.text()),
        }
    }
    assert_eq!(ok, 1, "exactly one mutation wins");
    let live: Vec<_> = h.turns(chat).await.into_iter().filter(|t| t.deleted_at.is_none()).collect();
    assert_eq!(live.len(), 1);
    assert_ne!(live[0].request_id, rid);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rejected_mutation_leaves_previous_turn_intact() {
    let h = Harness::with(Opts {
        policy: policy_cfg(catalog(), json!({
            "default_standard_limits": {"limit_daily_credits_micro": 1_000_000, "limit_monthly_credits_micro": 1_000_000},
            "default_premium_limits": {"limit_daily_credits_micro": 1_000_000, "limit_monthly_credits_micro": 1_000_000}})),
        ..Default::default()
    })
    .await;
    let chat = h.chat(ALICE, Some("tiny-m")).await;
    let rid = Uuid::new_v4();
    h.stream(ALICE, chat, json!({"content": "small", "request_id": rid})).await;

    // Context budget: edit to an over-long message.
    let r = h.send(ALICE, "PATCH", &format!("/chats/{chat}/turns/{rid}"), Some(json!({"content": "x".repeat(3000)}))).await;
    assert_eq!(fv_reason(&r.problem(400)), "INPUT_TOO_LONG");

    // Quota exhausted → 429 before the mutation commits.
    h.seed_spent(ALICE, "total", 1_000_000).await;
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/turns/{rid}/retry"), None).await;
    let p = r.problem(429);
    assert_eq!(p["context"]["violations"][0]["description"], json!("quota_exceeded"));

    let turns = h.turns(chat).await;
    assert_eq!(turns.len(), 1, "no new turn row");
    assert_eq!(turns[0].request_id, rid);
    assert!(turns[0].deleted_at.is_none());
    assert_eq!(turns[0].state, "completed");
    assert_eq!(list_messages(&h, chat).await.len(), 2);
    assert_eq!(h.provider.chat_requests().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mutation_carries_attachments_and_tool_settings() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let img1 = h.upload(ALICE, chat, "a.png", "image/png", &png(8, 8)).await;
    assert_eq!(img1.status, 201, "{}", img1.text());
    let img1 = Uuid::parse_str(img1.json()["id"].as_str().unwrap()).unwrap();
    let img2 = h.upload(ALICE, chat, "b.png", "image/png", &png(4, 4)).await;
    let img2 = Uuid::parse_str(img2.json()["id"].as_str().unwrap()).unwrap();
    let rid = Uuid::new_v4();
    let (r, _) = h
        .stream(ALICE, chat, json!({"content": "look", "request_id": rid, "attachment_ids": [img1, img2], "web_search": {"enabled": true}}))
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let first_req = h.provider.chat_requests().pop().unwrap();
    assert_eq!(first_req.to_string().matches("input_image").count(), 2);

    // Soft-delete one linked attachment directly: it must not be carried over.
    let conn = h.db.conn().unwrap();
    ent::attachments::Entity::update_many()
        .col_expr(ent::attachments::Column::DeletedAt, sea_orm::sea_query::Expr::value(time::OffsetDateTime::now_utc()))
        .filter(ent::attachments::Column::Id.eq(img2))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();

    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/turns/{rid}/retry"), None).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let new = started_rid(&r.sse());
    let req = h.provider.chat_requests().pop().unwrap();
    let s = req.to_string();
    assert_eq!(s.matches("input_image").count(), 1, "only the live image is re-sent: {s}");
    assert!(req["tools"].as_array().unwrap().iter().any(|t| t["type"] == json!("web_search") || t["type"] == json!("web_search_preview")), "web search carried over");

    let msgs = list_messages(&h, chat).await;
    let user = msgs.iter().find(|m| m["role"] == json!("user")).unwrap();
    assert_eq!(user["request_id"], json!(new.to_string()));
    let atts = user["attachments"].as_array().unwrap();
    assert_eq!(atts.len(), 1);
    assert_eq!(atts[0]["attachment_id"], json!(img1.to_string()));
    assert_eq!(atts[0]["kind"], json!("image"));
    let new_turn = h.turns(chat).await.into_iter().find(|t| t.request_id == new).unwrap();
    assert!(new_turn.web_search_enabled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mutation_invalidates_covering_thread_summary() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let rid = Uuid::new_v4();
    h.stream(ALICE, chat, json!({"content": "covered", "request_id": rid})).await;
    let msgs = h.messages(chat).await;
    let frontier = msgs.last().unwrap();
    let ts = time::OffsetDateTime::now_utc();
    let am = ent::thread_summaries::ActiveModel {
        id: sea_orm::Set(Uuid::new_v4()),
        tenant_id: sea_orm::Set(ALICE.tenant),
        chat_id: sea_orm::Set(chat),
        summary_text: sea_orm::Set("summary".into()),
        summarized_up_to_created_at: sea_orm::Set(frontier.created_at),
        summarized_up_to_message_id: sea_orm::Set(frontier.id),
        token_estimate: sea_orm::Set(3),
        created_at: sea_orm::Set(ts),
        updated_at: sea_orm::Set(ts),
    };
    let conn = h.db.conn().unwrap();
    secure_insert::<ent::thread_summaries::Entity>(am, &AccessScope::allow_all(), &conn).await.unwrap();
    ent::messages::Entity::update_many()
        .col_expr(ent::messages::Column::IsCompressed, sea_orm::sea_query::Expr::value(true))
        .filter(ent::messages::Column::ChatId.eq(chat))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();

    let r = h.send(ALICE, "PATCH", &format!("/chats/{chat}/turns/{rid}"), Some(json!({"content": "changed"}))).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(find(&r.sse(), "stream_started").get("thread_summary_applied").is_none_or(serde_json::Value::is_null));
    let summaries = ent::thread_summaries::Entity::find()
        .filter(ent::thread_summaries::Column::ChatId.eq(chat))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .all(&conn)
        .await
        .unwrap();
    assert!(summaries.is_empty(), "summary deleted");
    assert!(h.messages(chat).await.iter().all(|m| !m.is_compressed), "messages uncompressed");
    let req = h.provider.chat_requests().pop().unwrap();
    assert!(!req.to_string().contains("summarized"), "no summary preamble");
}
