//! Turn status API, turn lifecycle and tail-only turn mutations
//! (acceptance criteria: Turn Mutations, Turn Lifecycle; DESIGN §3.3
//! "Turn Status API", §3.9 "Turn Mutation Rules").

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use mini_chat::domain::service::stream::{SendRequest, StreamEvent, StreamStart};
use serde_json::{Value, json};
use uuid::Uuid;

// ── helpers ──────────────────────────────────────────────────────────────

fn turn_uri(chat: Uuid, rid: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat}/turns/{rid}")
}

fn retry_uri(chat: Uuid, rid: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat}/turns/{rid}/retry")
}

/// `request_id` of the `stream_started` event of an SSE response.
fn started_rid(r: &HttpResp) -> Uuid {
    let evs = r.sse();
    let (name, data) = evs.first().expect("no SSE events");
    assert_eq!(name, "stream_started", "{}", r.text);
    Uuid::parse_str(data["request_id"].as_str().unwrap()).unwrap()
}

fn event_names(r: &HttpResp) -> Vec<String> {
    r.sse().into_iter().map(|(n, _)| n).collect()
}

/// Sends a message with an explicit request id; asserts a successful stream.
async fn send_ok(h: &Harness, u: &toolkit_security::SecurityContext, chat: Uuid, content: &str) -> Uuid {
    let rid = Uuid::new_v4();
    let r = h.send(u, chat, json!({"content": content, "request_id": rid})).await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(event_names(&r).last().map(String::as_str), Some("done"), "{}", r.text);
    rid
}

async fn messages(h: &Harness, u: &toolkit_security::SecurityContext, chat: Uuid) -> Vec<Value> {
    let r = h.call(u, "GET", &format!("/mini-chat/v1/chats/{chat}/messages"), None).await;
    assert_eq!(r.status, 200, "{}", r.text);
    r.json()["items"].as_array().unwrap().clone()
}

async fn turn_rows(h: &Harness, chat: Uuid) -> i64 {
    h.scalar_i64(&format!("SELECT COUNT(*) FROM chat_turns WHERE hex(chat_id) = '{}'", hex_uuid(chat)))
        .await
}

/// Text of the last user input item of a provider request body.
fn last_user_text(body: &Value) -> String {
    let input = body["input"].as_array().expect("request has input array");
    let msg = input
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .expect("request has a user message");
    match &msg["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        other => panic!("unexpected content {other}"),
    }
}

fn has_input_image(body: &Value) -> bool {
    body["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .any(|p| p["type"] == "input_image")
}

fn assert_conflict(r: &HttpResp, expected_reason: &str) {
    assert_eq!(r.status, 409, "{}", r.text);
    assert_eq!(r.json()["context"]["reason"], expected_reason, "{}", r.text);
}

fn send_req(content: &str) -> SendRequest {
    SendRequest {
        content: content.to_owned(),
        request_id: Some(Uuid::new_v4()),
        attachment_ids: vec![],
        web_search: false,
    }
}

async fn wait_mutation(h: &Harness, event_type: &str) -> mini_chat_sdk::TurnMutationAuditEvent {
    h.eventually(|| async { h.audit.mutations.lock().iter().any(|e| e.event_type == event_type) })
        .await;
    h.audit
        .mutations
        .lock()
        .iter()
        .find(|e| e.event_type == event_type)
        .cloned()
        .unwrap()
}

// ── 1. retry ─────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn retry_latest_turn_replaces_it() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({})).await;
    let old = send_ok(&h, &u, chat, "what is rust?").await;
    let before = messages(&h, &u, chat).await;
    assert_eq!(before.len(), 2);

    let r = h.call(&u, "POST", &retry_uri(chat, old), None).await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert!(r.headers["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    let evs = r.sse();
    assert_eq!(evs[0].0, "stream_started");
    assert_eq!(evs[0].1["is_new_turn"], true);
    let new = started_rid(&r);
    assert_ne!(new, old, "retry must use a server-generated request_id");
    assert_eq!(evs.last().unwrap().0, "done", "{}", r.text);

    // old turn is gone from the status API
    let g = h.call(&u, "GET", &turn_uri(chat, old), None).await;
    assert_eq!(g.status, 404, "{}", g.text);
    assert_eq!(g.json()["context"]["resource_type"], "gts.cf.core.mini_chat.turn.v1~");
    // new turn is visible and done
    let g = h.call(&u, "GET", &turn_uri(chat, new), None).await;
    assert_eq!(g.status, 200, "{}", g.text);
    assert_eq!(g.json()["state"], "done");

    // old request_id is no longer replayable
    let resend = h.send(&u, chat, json!({"content": "what is rust?", "request_id": old})).await;
    assert_conflict(&resend, "request_id_conflict");

    // only the replacement pair is visible
    let msgs = messages(&h, &u, chat).await;
    assert_eq!(msgs.len(), 2, "{msgs:?}");
    assert!(msgs.iter().all(|m| m["request_id"] == new.to_string().as_str()), "{msgs:?}");
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert!(roles.contains(&"user") && roles.contains(&"assistant"));
    let user_msg = msgs.iter().find(|m| m["role"] == "user").unwrap();
    assert_eq!(user_msg["content"], "what is rust?");
    for m in &before {
        assert!(msgs.iter().all(|n| n["id"] != m["id"]), "old message still listed");
    }
    let chat_json = h.call(&u, "GET", &format!("/mini-chat/v1/chats/{chat}"), None).await.json();
    assert_eq!(chat_json["message_count"], 2);

    // provider received the same user content again
    let reqs = h.provider.chat_requests();
    let streamed: Vec<&Value> = reqs.iter().filter(|b| b["stream"] != false).collect();
    assert_eq!(streamed.len(), 2);
    assert_eq!(last_user_text(streamed[1]), "what is rust?");
    // the retried turn's old messages are not in the retry context
    let ctx_dump = streamed[1]["input"].to_string();
    assert_eq!(ctx_dump.matches("what is rust?").count(), 1, "{ctx_dump}");

    // DB: old row soft-deleted and linked to the replacement
    let rows = h
        .query(&format!(
            "SELECT deleted_at IS NOT NULL, hex(replaced_by_request_id) FROM chat_turns WHERE hex(request_id) = '{}'",
            hex_uuid(old)
        ))
        .await;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].try_get_by_index::<bool>(0).unwrap());
    assert_eq!(rows[0].try_get_by_index::<Option<String>>(1).unwrap(), Some(hex_uuid(new)));
    let old_msgs_deleted = h
        .scalar_i64(&format!(
            "SELECT COUNT(*) FROM messages WHERE hex(request_id) = '{}' AND deleted_at IS NOT NULL",
            hex_uuid(old)
        ))
        .await;
    assert_eq!(old_msgs_deleted, 2, "old messages are soft-deleted, not removed");

    // audit
    let ev = wait_mutation(&h, "turn_retry").await;
    assert_eq!(ev.original_request_id, Some(old));
    assert_eq!(ev.new_request_id, Some(new));
    assert_eq!(ev.chat_id, chat);
    assert_eq!(ev.actor_user_id, u.subject_id());
    h.shutdown().await;
}

// ── 2. edit ──────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn edit_latest_turn() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({})).await;
    let old = send_ok(&h, &u, chat, "original").await;

    for bad in ["", "   \n\t"] {
        let r = h.call(&u, "PATCH", &turn_uri(chat, old), Some(json!({"content": bad}))).await;
        assert_eq!(r.status, 400, "{}", r.text);
        assert_eq!(reason(&r.json()), "EMPTY_CONTENT");
        assert_eq!(r.json()["context"]["field_violations"][0]["field"], "content");
    }
    let r = h.call(&u, "PATCH", &turn_uri(chat, old), Some(json!({}))).await;
    assert_eq!(r.status, 422, "{}", r.text);
    // rejected edits left the turn untouched
    assert_eq!(h.call(&u, "GET", &turn_uri(chat, old), None).await.status, 200);
    assert_eq!(turn_rows(&h, chat).await, 1);

    let r = h.call(&u, "PATCH", &turn_uri(chat, old), Some(json!({"content": "edited"}))).await;
    assert_eq!(r.status, 200, "{}", r.text);
    let new = started_rid(&r);
    assert_ne!(new, old);
    assert_eq!(r.sse()[0].1["is_new_turn"], true);
    assert_eq!(event_names(&r).last().map(String::as_str), Some("done"));

    let reqs = h.provider.chat_requests();
    assert_eq!(last_user_text(reqs.last().unwrap()), "edited");
    assert!(!reqs.last().unwrap()["input"].to_string().contains("original"));

    let msgs = messages(&h, &u, chat).await;
    assert_eq!(msgs.len(), 2);
    let um = msgs.iter().find(|m| m["role"] == "user").unwrap();
    assert_eq!(um["content"], "edited");
    assert_eq!(um["request_id"], new.to_string().as_str());
    assert_eq!(h.call(&u, "GET", &turn_uri(chat, old), None).await.status, 404);
    let replaced = h
        .scalar_str(&format!(
            "SELECT hex(replaced_by_request_id) FROM chat_turns WHERE hex(request_id) = '{}'",
            hex_uuid(old)
        ))
        .await;
    assert_eq!(replaced, Some(hex_uuid(new)));

    let ev = wait_mutation(&h, "turn_edit").await;
    assert_eq!(ev.original_request_id, Some(old));
    assert_eq!(ev.new_request_id, Some(new));
    h.shutdown().await;
}

// ── 3. delete ────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn delete_latest_turn() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({})).await;
    let first = send_ok(&h, &u, chat, "one").await;
    let second = send_ok(&h, &u, chat, "two").await;

    let d = h.call(&u, "DELETE", &turn_uri(chat, second), None).await;
    assert_eq!(d.status, 204, "{}", d.text);
    assert!(d.text.is_empty());
    let msgs = messages(&h, &u, chat).await;
    assert_eq!(msgs.len(), 2);
    assert!(msgs.iter().all(|m| m["request_id"] == first.to_string().as_str()));
    assert_eq!(h.call(&u, "GET", &turn_uri(chat, second), None).await.status, 404);
    let chat_json = h.call(&u, "GET", &format!("/mini-chat/v1/chats/{chat}"), None).await.json();
    assert_eq!(chat_json["message_count"], 2);

    // delete sets deleted_at only (no replacement)
    let rows = h
        .query(&format!(
            "SELECT deleted_at IS NOT NULL, replaced_by_request_id IS NULL FROM chat_turns WHERE hex(request_id) = '{}'",
            hex_uuid(second)
        ))
        .await;
    assert!(rows[0].try_get_by_index::<bool>(0).unwrap());
    assert!(rows[0].try_get_by_index::<bool>(1).unwrap());

    let ev = wait_mutation(&h, "turn_delete").await;
    assert_eq!(ev.request_id, Some(second));
    assert_eq!(ev.chat_id, chat);
    assert_eq!(ev.actor_user_id, u.subject_id());

    // deleting the deleted turn again
    let again = h.call(&u, "DELETE", &turn_uri(chat, second), None).await;
    assert_conflict(&again, "NOT_LATEST_TURN");
    // retry/edit of a soft-deleted turn
    assert_conflict(&h.call(&u, "POST", &retry_uri(chat, second), None).await, "NOT_LATEST_TURN");
    assert_conflict(
        &h.call(&u, "PATCH", &turn_uri(chat, second), Some(json!({"content": "x"}))).await,
        "NOT_LATEST_TURN",
    );
    // the previous turn became the latest and is mutable again
    assert_eq!(h.call(&u, "DELETE", &turn_uri(chat, first), None).await.status, 204);
    assert!(messages(&h, &u, chat).await.is_empty());
    h.shutdown().await;
}

// ── 4. only the latest turn ──────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn only_latest_turn_can_be_mutated() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({})).await;
    let first = send_ok(&h, &u, chat, "one").await;
    let _second = send_ok(&h, &u, chat, "two").await;
    let calls = h.provider.chat_requests().len();

    assert_conflict(&h.call(&u, "POST", &retry_uri(chat, first), None).await, "NOT_LATEST_TURN");
    assert_conflict(
        &h.call(&u, "PATCH", &turn_uri(chat, first), Some(json!({"content": "x"}))).await,
        "NOT_LATEST_TURN",
    );
    assert_conflict(&h.call(&u, "DELETE", &turn_uri(chat, first), None).await, "NOT_LATEST_TURN");
    let r = h.call(&u, "DELETE", &turn_uri(chat, first), None).await;
    assert_eq!(r.json()["status"], 409);

    assert_eq!(h.provider.chat_requests().len(), calls, "no provider call on rejection");
    assert_eq!(messages(&h, &u, chat).await.len(), 4);
    assert_eq!(
        h.scalar_i64(&format!(
            "SELECT COUNT(*) FROM chat_turns WHERE hex(chat_id) = '{}' AND deleted_at IS NULL",
            hex_uuid(chat)
        ))
        .await,
        2
    );
    h.shutdown().await;
}

// ── 5. running target ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn running_turn_cannot_be_mutated() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({})).await;
    h.provider.push(Reply::Hang(vec![delta("partial")]));
    let req = send_req("long question");
    let rid = req.request_id.unwrap();
    let start = h.svc.send_message(&u, chat, req).await.unwrap();
    let StreamStart::Live { mut events, cancel } = start else {
        panic!("expected live stream")
    };
    let first = next_n(&mut events, 2).await;
    assert_eq!(names(&first), vec!["stream_started", "delta"]);

    for (method, uri, body) in [
        ("POST", retry_uri(chat, rid), None),
        ("DELETE", turn_uri(chat, rid), None),
        ("PATCH", turn_uri(chat, rid), Some(json!({"content": "edited"}))),
    ] {
        let r = h.call(&u, method, &uri, body).await;
        assert_eq!(r.status, 400, "{method} {}", r.text);
        let v = r.json();
        assert_eq!(v["context"]["violations"][0]["subject"], "turn_state", "{v}");
        assert_eq!(v["context"]["violations"][0]["type"], "STATE", "{v}");
    }
    assert_eq!(h.call(&u, "GET", &turn_uri(chat, rid), None).await.json()["state"], "running");
    assert_eq!(turn_rows(&h, chat).await, 1);

    drop(events);
    cancel.cancel();
    wait_terminal(&h, rid).await;
    h.shutdown().await;
}

// ── 6. visibility ────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn foreign_users_and_unknown_turns_get_404() {
    let h = Harness::new().await;
    let owner = user();
    let chat = h.create_chat(&owner, json!({})).await;
    let rid = send_ok(&h, &owner, chat, "mine").await;

    let same_tenant = ctx(Uuid::new_v4(), owner.subject_tenant_id());
    let other_tenant = user();
    for who in [&same_tenant, &other_tenant] {
        for (method, uri, body) in [
            ("GET", turn_uri(chat, rid), None),
            ("POST", retry_uri(chat, rid), None),
            ("PATCH", turn_uri(chat, rid), Some(json!({"content": "x"}))),
            ("DELETE", turn_uri(chat, rid), None),
        ] {
            let r = h.call(who, method, &uri, body).await;
            assert_eq!(r.status, 404, "{method} {uri}: {}", r.text);
            assert_eq!(r.json()["context"]["resource_type"], "gts.cf.core.mini_chat.chat.v1~");
        }
    }

    let unknown = Uuid::new_v4();
    for (method, uri, body) in [
        ("GET", turn_uri(chat, unknown), None),
        ("POST", retry_uri(chat, unknown), None),
        ("PATCH", turn_uri(chat, unknown), Some(json!({"content": "x"}))),
        ("DELETE", turn_uri(chat, unknown), None),
    ] {
        let r = h.call(&owner, method, &uri, body).await;
        assert_eq!(r.status, 404, "{method}: {}", r.text);
        assert_eq!(r.json()["context"]["resource_type"], "gts.cf.core.mini_chat.turn.v1~");
    }
    // the owner's turn is untouched
    let g = h.call(&owner, "GET", &turn_uri(chat, rid), None).await;
    assert_eq!(g.json()["state"], "done");
    assert!(h.audit.mutations.lock().is_empty());
    h.shutdown().await;
}

// ── 7. preflight before mutation ─────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn mutation_runs_full_preflight_and_leaves_turn_unchanged() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({})).await;
    let rid = send_ok(&h, &u, chat, "hello").await;
    let calls = h.provider.chat_requests().len();

    let tenant = hex_uuid(u.subject_tenant_id());
    let uid = hex_uuid(u.subject_id());
    let today = chrono::Utc::now().date_naive();
    let month = today.format("%Y-%m-01").to_string();
    let day = today.format("%Y-%m-%d").to_string();
    let ts = chrono::Utc::now().to_rfc3339();
    // Exhaust every bucket of every period (rows may already exist from settlement).
    h.exec(&format!(
        "UPDATE quota_usage SET spent_credits_micro = 1000000000000 WHERE hex(user_id) = '{uid}'"
    ))
    .await;
    for (period, start) in [("daily", day.as_str()), ("monthly", month.as_str())] {
        for bucket in ["total", "tier:premium"] {
            h.exec(&format!(
                "INSERT OR IGNORE INTO quota_usage (id, tenant_id, user_id, period_type, period_start, bucket, \
                 spent_credits_micro, reserved_credits_micro, calls, input_tokens, output_tokens, file_search_calls, \
                 web_search_calls, code_interpreter_calls, rag_retrieval_calls, image_inputs, image_upload_bytes, updated_at) \
                 VALUES (X'{}', X'{tenant}', X'{uid}', '{period}', '{start}', '{bucket}', 1000000000000, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, '{ts}')",
                hex_uuid(Uuid::new_v4())
            ))
            .await;
        }
    }
    let exhausted = h
        .scalar_i64(&format!(
            "SELECT COUNT(*) FROM quota_usage WHERE hex(user_id) = '{uid}' AND spent_credits_micro >= 1000000000000"
        ))
        .await;
    assert!(exhausted >= 4, "seeded {exhausted} rows");

    for (method, uri, body) in [
        ("POST", retry_uri(chat, rid), None),
        ("PATCH", turn_uri(chat, rid), Some(json!({"content": "edited"}))),
    ] {
        let r = h.call(&u, method, &uri, body).await;
        assert_eq!(r.status, 429, "{method}: {}", r.text);
        assert_eq!(r.json()["context"]["violations"][0]["subject"], "tokens", "{}", r.text);
    }

    // the previous turn and chat are unchanged
    let g = h.call(&u, "GET", &turn_uri(chat, rid), None).await;
    assert_eq!(g.status, 200);
    assert_eq!(g.json()["state"], "done");
    assert_eq!(turn_rows(&h, chat).await, 1, "no new turn rows");
    let msgs = messages(&h, &u, chat).await;
    assert_eq!(msgs.len(), 2);
    assert!(msgs.iter().all(|m| m["request_id"] == rid.to_string().as_str()));
    assert_eq!(h.provider.chat_requests().len(), calls);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(h.audit.mutations.lock().is_empty(), "no mutation audit on preflight rejection");
    h.shutdown().await;
}

// ── 8. attachments are carried forward ───────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn retry_carries_attachments_forward() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({})).await; // default "prem" has VISION_INPUT
    let up = h.upload(&u, chat, "pic.png", "image/png", &png(10, 10)).await;
    assert_eq!(up.status, 201, "{}", up.text);
    let aid = Uuid::parse_str(up.json()["id"].as_str().unwrap()).unwrap();
    h.eventually(|| async {
        h.call(&u, "GET", &format!("/mini-chat/v1/chats/{chat}/attachments/{aid}"), None)
            .await
            .json()["status"]
            == "ready"
    })
    .await;

    let old = Uuid::new_v4();
    let r = h
        .send(&u, chat, json!({"content": "describe", "request_id": old, "attachment_ids": [aid]}))
        .await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(event_names(&r).last().map(String::as_str), Some("done"), "{}", r.text);
    assert!(has_input_image(h.provider.chat_requests().last().unwrap()));

    let rr = h.call(&u, "POST", &retry_uri(chat, old), None).await;
    assert_eq!(rr.status, 200, "{}", rr.text);
    let new = started_rid(&rr);
    assert_eq!(event_names(&rr).last().map(String::as_str), Some("done"), "{}", rr.text);

    let msgs = messages(&h, &u, chat).await;
    let um = msgs.iter().find(|m| m["role"] == "user").unwrap();
    assert_eq!(um["request_id"], new.to_string().as_str());
    let atts = um["attachments"].as_array().unwrap();
    assert_eq!(atts.len(), 1, "{um}");
    assert_eq!(atts[0]["attachment_id"], aid.to_string().as_str());
    assert_eq!(atts[0]["kind"], "image");

    let retry_req = h.provider.chat_requests().last().unwrap().clone();
    assert!(has_input_image(&retry_req), "{retry_req}");
    assert_eq!(last_user_text(&retry_req), "describe");

    // old links remain on the soft-deleted message (copy, not move)
    let links = h
        .scalar_i64(&format!(
            "SELECT COUNT(*) FROM message_attachments WHERE hex(attachment_id) = '{}'",
            hex_uuid(aid)
        ))
        .await;
    assert_eq!(links, 2);
    h.shutdown().await;
}

// ── 9. concurrent mutations ──────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_retries_resolve_deterministically() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({})).await;
    let old = send_ok(&h, &u, chat, "race").await;

    let uri = retry_uri(chat, old);
    let (a, b) = tokio::join!(h.call(&u, "POST", &uri, None), h.call(&u, "POST", &uri, None));
    let mut statuses = [a.status, b.status];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 409], "a={} b={}", a.text, b.text);
    let (ok, lost) = if a.status == 200 { (&a, &b) } else { (&b, &a) };
    let why = lost.json()["context"]["reason"].as_str().unwrap_or_default().to_owned();
    assert!(
        why == "NOT_LATEST_TURN" || why == "GENERATION_IN_PROGRESS",
        "unexpected reason {why}: {}",
        lost.text
    );
    let new = started_rid(ok);
    assert_eq!(event_names(ok).last().map(String::as_str), Some("done"));

    // exactly one live turn: the winner's
    let live = h
        .query(&format!(
            "SELECT hex(request_id) FROM chat_turns WHERE hex(chat_id) = '{}' AND deleted_at IS NULL",
            hex_uuid(chat)
        ))
        .await;
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].try_get_by_index::<String>(0).unwrap(), hex_uuid(new));
    assert_eq!(messages(&h, &u, chat).await.len(), 2);
    h.shutdown().await;
}

// ── 10. turn lifecycle ───────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn turn_status_running_then_done() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({})).await;

    h.provider.push(Reply::Hang(vec![delta("partial")]));
    let req = send_req("slow");
    let rid = req.request_id.unwrap();
    let StreamStart::Live { mut events, cancel } = h.svc.send_message(&u, chat, req).await.unwrap() else {
        panic!("expected live stream")
    };
    let first = next_n(&mut events, 2).await;
    assert_eq!(names(&first), vec!["stream_started", "delta"]);

    let g = h.call(&u, "GET", &turn_uri(chat, rid), None).await;
    assert_eq!(g.status, 200, "{}", g.text);
    let v = g.json();
    assert_eq!(v["request_id"], rid.to_string().as_str());
    assert_eq!(v["state"], "running");
    assert!(v.get("assistant_message_id").is_none(), "{v}");
    assert!(v.get("error_code").is_none(), "{v}");
    assert!(v.get("chat_id").is_none(), "{v}");
    assert!(v["updated_at"].is_string());
    let hex = hex_uuid(rid);
    assert_eq!(
        h.scalar_i64(&format!(
            "SELECT COUNT(*) FROM chat_turns WHERE hex(request_id) = '{hex}' AND state = 'running' \
             AND last_progress_at IS NOT NULL AND completed_at IS NULL"
        ))
        .await,
        1,
        "running rows have last_progress_at"
    );
    drop(events);
    cancel.cancel();
    wait_terminal(&h, rid).await;

    // a completed turn
    let done = send_ok(&h, &u, chat, "fast").await;
    let v = h.call(&u, "GET", &turn_uri(chat, done), None).await.json();
    assert_eq!(v["state"], "done");
    assert!(v.get("error_code").is_none());
    let amid = v["assistant_message_id"].as_str().expect("done has assistant_message_id").to_owned();
    let msgs = messages(&h, &u, chat).await;
    let am = msgs.iter().find(|m| m["id"] == amid.as_str()).expect("assistant message listed");
    assert_eq!(am["role"], "assistant");
    assert_eq!(am["content"], "Hello world");
    let row = h
        .scalar_str(&format!(
            "SELECT state FROM chat_turns WHERE hex(request_id) = '{}' AND completed_at IS NOT NULL",
            hex_uuid(done)
        ))
        .await;
    assert_eq!(row.as_deref(), Some("completed"));
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_status_error_on_provider_failure() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({})).await;
    h.provider.push(Reply::Events(vec![ev(
        "response.failed",
        json!({"type": "response.failed", "response": {"id": "resp_x", "status": "failed", "error": {"code": "server_error", "message": "boom"}}}),
    )]));
    let rid = Uuid::new_v4();
    let r = h.send(&u, chat, json!({"content": "fail please", "request_id": rid})).await;
    assert_eq!(r.status, 200, "{}", r.text);
    let evs = r.sse();
    let (last, data) = evs.last().unwrap();
    assert_eq!(last, "error", "{}", r.text);
    assert_eq!(data["code"], "provider_error");

    let v = h.call(&u, "GET", &turn_uri(chat, rid), None).await.json();
    assert_eq!(v["state"], "error", "{v}");
    assert_eq!(v["error_code"], "provider_error");
    assert!(v.get("assistant_message_id").is_none(), "{v}");
    let row = h
        .scalar_str(&format!(
            "SELECT state FROM chat_turns WHERE hex(request_id) = '{}' AND completed_at IS NOT NULL",
            hex_uuid(rid)
        ))
        .await;
    assert_eq!(row.as_deref(), Some("failed"));

    // a failed turn is terminal and can be retried
    let rr = h.call(&u, "POST", &retry_uri(chat, rid), None).await;
    assert_eq!(rr.status, 200, "{}", rr.text);
    assert_eq!(event_names(&rr).last().map(String::as_str), Some("done"));
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_status_cancelled_with_partial_content() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({})).await;
    h.provider.push(Reply::Hang(vec![delta("partial")]));
    let req = send_req("cancel me");
    let rid = req.request_id.unwrap();
    let start = h.svc.send_message(&u, chat, req).await.unwrap();
    {
        let StreamStart::Live { mut events, cancel } = start else {
            panic!("expected live stream")
        };
        let _guard = cancel.drop_guard();
        let first = next_n(&mut events, 2).await;
        assert_eq!(names(&first), vec!["stream_started", "delta"]);
        assert!(matches!(&first[1], StreamEvent::Delta { content, .. } if content == "partial"));
        // receiver and guard dropped here: client disconnect
    }
    wait_terminal(&h, rid).await;

    let v = h.call(&u, "GET", &turn_uri(chat, rid), None).await.json();
    assert_eq!(v["state"], "cancelled", "{v}");
    assert!(v.get("error_code").is_none(), "{v}");
    let amid = v["assistant_message_id"]
        .as_str()
        .expect("cancelled turn with partial content has assistant_message_id")
        .to_owned();
    let msgs = messages(&h, &u, chat).await;
    let am = msgs.iter().find(|m| m["id"] == amid.as_str()).expect("partial message listed");
    assert_eq!(am["content"], "partial");
    assert_eq!(am["role"], "assistant");
    let row = h
        .scalar_str(&format!(
            "SELECT state FROM chat_turns WHERE hex(request_id) = '{}' AND completed_at IS NOT NULL",
            hex_uuid(rid)
        ))
        .await;
    assert_eq!(row.as_deref(), Some("cancelled"));

    // a cancelled turn is terminal: delete is allowed
    assert_eq!(h.call(&u, "DELETE", &turn_uri(chat, rid), None).await.status, 204);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn turn_status_cancelled_without_content() {
    let h = Harness::new().await;
    let u = user();
    let chat = h.create_chat(&u, json!({})).await;
    h.provider.push(Reply::Hang(vec![]));
    let req = send_req("cancel before tokens");
    let rid = req.request_id.unwrap();
    let start = h.svc.send_message(&u, chat, req).await.unwrap();
    {
        let StreamStart::Live { mut events, cancel } = start else {
            panic!("expected live stream")
        };
        let _guard = cancel.drop_guard();
        let first = next_n(&mut events, 1).await;
        assert_eq!(names(&first), vec!["stream_started"]);
        // wait until the provider call is in flight
        h.eventually(|| async { !h.provider.chat_requests().is_empty() }).await;
    }
    wait_terminal(&h, rid).await;

    let v = h.call(&u, "GET", &turn_uri(chat, rid), None).await.json();
    assert_eq!(v["state"], "cancelled", "{v}");
    assert!(v.get("assistant_message_id").is_none(), "{v}");
    assert!(v.get("error_code").is_none(), "{v}");
    let msgs = messages(&h, &u, chat).await;
    assert!(msgs.iter().all(|m| m["role"] != "assistant"), "{msgs:?}");

    // DB invariant over every terminal row of the run
    let bad = h
        .scalar_i64(
            "SELECT COUNT(*) FROM chat_turns WHERE state IN ('completed','failed','cancelled') AND completed_at IS NULL",
        )
        .await;
    assert_eq!(bad, 0);
    let bad_running = h
        .scalar_i64("SELECT COUNT(*) FROM chat_turns WHERE state = 'running' AND last_progress_at IS NULL")
        .await;
    assert_eq!(bad_running, 0);
    h.shutdown().await;
}
