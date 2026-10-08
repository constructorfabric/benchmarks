//! Router tests: retry / edit / delete of the latest turn (acceptance: Turn
//! Mutations, Cleanup & Recovery "mutation-driven summary invalidation").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;

use serde_json::json;
use uuid::Uuid;

use crate::infra::llm::sse_parser::SseParser;
use crate::test_support::{TestEnv, json_resp, live_provider, next_event, user_a};

fn turn_uri(chat: &str, rid: Uuid) -> String {
    format!("/mini-chat/v1/chats/{chat}/turns/{rid}")
}

async fn send(env: &TestEnv, chat: &str, content: &str) -> Uuid {
    let rid = Uuid::new_v4();
    let r = env.stream(&user_a(), chat, json!({"content": content, "request_id": rid})).await;
    assert_eq!(r.event_names().last().unwrap(), "done", "{}", r.text());
    rid
}

async fn turn_cols(env: &TestEnv, rid: Uuid) -> (Option<String>, Option<Vec<u8>>, String) {
    let rows = env
        .sql_rows(&format!(
            "SELECT deleted_at, replaced_by_request_id, state FROM chat_turns WHERE request_id = x'{}'",
            rid.simple()
        ))
        .await;
    (
        rows[0].try_get_by_index::<Option<String>>(0).unwrap(),
        rows[0].try_get_by_index::<Option<Vec<u8>>>(1).unwrap(),
        rows[0].try_get_by_index::<String>(2).unwrap(),
    )
}

#[tokio::test]
async fn retry_creates_new_turn_with_server_request_id() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let first = send(&env, &chat, "question").await;
    let r = env.call(&user_a(), "POST", &format!("{}/retry", turn_uri(&chat, first)), None).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.event_names(), vec!["stream_started", "delta", "delta", "done"]);
    let started = r.event("stream_started").unwrap();
    let new_rid = Uuid::parse_str(started["request_id"].as_str().unwrap()).unwrap();
    assert_ne!(new_rid, first);
    assert_eq!(new_rid.get_version_num(), 4);
    assert_eq!(started["is_new_turn"], true);
    // old turn soft-deleted and linked
    let (deleted, replaced, _) = turn_cols(&env, first).await;
    assert!(deleted.is_some());
    assert_eq!(replaced.unwrap(), new_rid.as_bytes().to_vec());
    // retry re-sends the original content; history excludes the replaced turn
    let req = env.proxy.chat_requests().last().unwrap().json();
    let input = req["input"].as_array().unwrap();
    assert_eq!(input.len(), 1, "replaced turn is not context: {input:?}");
    assert_eq!(input[0]["content"], "question");
    let msgs = env.get(&format!("/mini-chat/v1/chats/{chat}/messages")).await.json();
    let items = msgs["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(items.iter().all(|m| m["request_id"] == new_rid.to_string()));
    assert_eq!(env.get(&format!("/mini-chat/v1/chats/{chat}")).await.json()["message_count"], 2);
    // the old request id is no longer replayable; its status is 404
    assert_eq!(env.get(&turn_uri(&chat, first)).await.status, 404);
    env.stream(&user_a(), &chat, json!({"content": "question", "request_id": first}))
        .await
        .assert_problem(409, "request_id_conflict");
    // mutation audit event
    env.eventually("mutation audit", |e| e.audit_events().iter().any(|a| a["event_type"] == "turn_retry")).await;
    let a = env.audit_events().into_iter().find(|a| a["event_type"] == "turn_retry").unwrap();
    assert_eq!(a["original_request_id"], first.to_string());
    assert_eq!(a["new_request_id"], new_rid.to_string());
    assert_eq!(a["actor_user_id"], crate::test_support::USER_A.to_string());
}

#[tokio::test]
async fn edit_replaces_content_and_validates() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let first = send(&env, &chat, "original").await;
    let r = env.call(&user_a(), "PATCH", &turn_uri(&chat, first), Some(json!({"content": "  "}))).await;
    r.assert_problem(400, "EMPTY_CONTENT");
    let r = env.call(&user_a(), "PATCH", &turn_uri(&chat, first), Some(json!({}))).await;
    assert_eq!(r.status, 422);
    assert!(turn_cols(&env, first).await.0.is_none(), "rejected edit changes nothing");
    let r = env.call(&user_a(), "PATCH", &turn_uri(&chat, first), Some(json!({"content": "edited"}))).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.event_names().last().unwrap(), "done");
    let new_rid = r.event("stream_started").unwrap()["request_id"].as_str().unwrap().to_owned();
    let req = env.proxy.chat_requests().last().unwrap().json();
    assert_eq!(req["input"].as_array().unwrap().last().unwrap()["content"], "edited");
    let msgs = env.get(&format!("/mini-chat/v1/chats/{chat}/messages")).await.json();
    assert_eq!(msgs["items"][0]["content"], "edited");
    assert_eq!(msgs["items"][0]["request_id"], new_rid.as_str());
    env.eventually("edit audit", |e| e.audit_events().iter().any(|a| a["event_type"] == "turn_edit")).await;
}

#[tokio::test]
async fn delete_last_turn_and_not_latest_rules() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let t1 = send(&env, &chat, "one").await;
    let t2 = send(&env, &chat, "two").await;
    // only the latest turn can be mutated
    for (m, uri, body) in [
        ("POST", format!("{}/retry", turn_uri(&chat, t1)), None),
        ("PATCH", turn_uri(&chat, t1), Some(json!({"content": "x"}))),
        ("DELETE", turn_uri(&chat, t1), None),
    ] {
        let r = env.call(&user_a(), m, &uri, body).await;
        r.assert_problem(409, "NOT_LATEST_TURN");
    }
    let r = env.call(&user_a(), "DELETE", &turn_uri(&chat, t2), None).await;
    assert_eq!(r.status, 204);
    let (deleted, replaced, _) = turn_cols(&env, t2).await;
    assert!(deleted.is_some());
    assert!(replaced.is_none(), "delete sets no replacement");
    // deleted turn: further mutations → NOT_LATEST_TURN
    let r = env.call(&user_a(), "DELETE", &turn_uri(&chat, t2), None).await;
    r.assert_problem(409, "NOT_LATEST_TURN");
    let r = env.call(&user_a(), "POST", &format!("{}/retry", turn_uri(&chat, t2)), None).await;
    r.assert_problem(409, "NOT_LATEST_TURN");
    // t1 is the latest again
    let msgs = env.get(&format!("/mini-chat/v1/chats/{chat}/messages")).await.json();
    assert_eq!(msgs["items"].as_array().unwrap().len(), 2);
    let r = env.call(&user_a(), "DELETE", &turn_uri(&chat, t1), None).await;
    assert_eq!(r.status, 204);
    assert_eq!(env.get(&format!("/mini-chat/v1/chats/{chat}")).await.json()["message_count"], 0);
    // unknown turn
    let r = env.call(&user_a(), "DELETE", &turn_uri(&chat, Uuid::new_v4()), None).await;
    assert_eq!(r.status, 404);
    env.eventually("delete audit", |e| {
        e.audit_events().iter().filter(|a| a["event_type"] == "turn_delete").count() == 2
    })
    .await;
    let a = env.audit_events().into_iter().find(|a| a["event_type"] == "turn_delete").unwrap();
    assert_eq!(a["request_id"], t2.to_string());
}

#[tokio::test]
async fn running_turn_and_ownership_rules() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let done = send(&env, &chat, "one").await;
    let tx = live_provider(&env.proxy);
    let running = Uuid::new_v4();
    let (_, mut body) = env
        .open(
            &user_a(),
            "POST",
            &format!("/mini-chat/v1/chats/{chat}/messages:stream"),
            json!({"content": "two", "request_id": running}),
        )
        .await;
    let mut p = SseParser::new();
    let mut q = VecDeque::new();
    assert_eq!(next_event(&mut body, &mut p, &mut q).await.unwrap().0, "stream_started");
    // running target → 400 failed_precondition turn_state / STATE
    let r = env.call(&user_a(), "POST", &format!("{}/retry", turn_uri(&chat, running)), None).await;
    assert_eq!(r.status, 400, "{}", r.text());
    let v = r.json();
    assert_eq!(v["context"]["violations"][0]["subject"], "turn_state");
    assert_eq!(v["context"]["violations"][0]["type"], "STATE");
    let r = env.call(&user_a(), "DELETE", &turn_uri(&chat, running), None).await;
    assert_eq!(r.status, 400);
    // a newer running turn makes the older target non-latest
    let r = env.call(&user_a(), "POST", &format!("{}/retry", turn_uri(&chat, done)), None).await;
    r.assert_problem(409, "NOT_LATEST_TURN");
    drop(body);
    drop(tx);
    for _ in 0..200 {
        if turn_cols(&env, running).await.2 != "running" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // a turn requested by someone else → 403 permission_denied
    env.sql_exec(&format!(
        "UPDATE chat_turns SET requester_user_id = x'{}' WHERE request_id = x'{}'",
        Uuid::new_v4().simple(),
        running.simple()
    ))
    .await;
    let r = env.call(&user_a(), "DELETE", &turn_uri(&chat, running), None).await;
    assert_eq!(r.status, 403, "{}", r.text());
}

#[tokio::test]
async fn preflight_rejection_leaves_previous_turn() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let first = send(&env, &chat, "one").await;
    env.sql_exec("UPDATE quota_usage SET spent_credits_micro = 900000000000000").await;
    let calls = env.proxy.chat_requests().len();
    let r = env.call(&user_a(), "POST", &format!("{}/retry", turn_uri(&chat, first)), None).await;
    r.assert_problem(429, "quota_exceeded");
    let r = env.call(&user_a(), "PATCH", &turn_uri(&chat, first), Some(json!({"content": "x"}))).await;
    r.assert_problem(429, "quota_exceeded");
    assert_eq!(env.proxy.chat_requests().len(), calls);
    let (deleted, _, state) = turn_cols(&env, first).await;
    assert!(deleted.is_none(), "previous answer stays");
    assert_eq!(state, "completed");
    assert_eq!(env.count("SELECT COUNT(*) FROM chat_turns").await, 1);
    let msgs = env.get(&format!("/mini-chat/v1/chats/{chat}/messages")).await.json();
    assert_eq!(msgs["items"][1]["content"], "Hello world");
}

#[tokio::test]
async fn post_commit_setup_failure_marks_new_turn_failed() {
    let env = TestEnv::new().await;
    let chat = env.chat(Some("tiny-ctx")).await;
    let first = send(&env, &chat, "short").await;
    // edit to content that passes the input check but not the assembled budget
    let r = env
        .call(&user_a(), "PATCH", &turn_uri(&chat, first), Some(json!({"content": "a".repeat(10_500)})))
        .await;
    assert_eq!(r.status, 400, "{}", r.text());
    r.assert_problem(400, "CONTEXT_BUDGET_EXCEEDED");
    let rows = env
        .sql_rows("SELECT state, error_code FROM chat_turns WHERE deleted_at IS NULL")
        .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].try_get_by_index::<String>(0).unwrap(), "failed");
    assert_eq!(rows[0].try_get_by_index::<Option<String>>(1).unwrap().as_deref(), Some("context_length_exceeded"));
    // the chat is not blocked
    let r = env.send_msg(&chat, "next").await;
    assert_eq!(r.event_names().last().unwrap(), "done");
}

#[tokio::test]
async fn retry_after_failure_and_web_search_flag_carried_forward() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let slot = std::sync::Mutex::new(Some(json_resp(500, &json!({}))));
    env.proxy.respond(move |r| if r.uri.contains("/responses") { slot.lock().unwrap().take() } else { None });
    let rid = Uuid::new_v4();
    let r = env
        .stream(&user_a(), &chat, json!({"content": "search it", "request_id": rid, "web_search": {"enabled": true}}))
        .await;
    assert_eq!(r.event_names().last().unwrap(), "error");
    let r = env.call(&user_a(), "POST", &format!("{}/retry", turn_uri(&chat, rid)), None).await;
    assert_eq!(r.event_names().last().unwrap(), "done", "{}", r.text());
    let req = env.proxy.chat_requests().last().unwrap().json();
    let tools = req["tools"].as_array().expect("web_search tool carried forward");
    assert!(tools.iter().any(|t| t["type"] == "web_search" || t["type"] == "web_search_preview"), "{tools:?}");
    let new_rid = r.event("stream_started").unwrap()["request_id"].as_str().unwrap().to_owned();
    let ws = env
        .count(&format!(
            "SELECT web_search_enabled FROM chat_turns WHERE request_id = x'{}'",
            new_rid.replace('-', "")
        ))
        .await;
    assert_eq!(ws, 1);
}

#[tokio::test]
async fn mutation_invalidates_covering_summary() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    send(&env, &chat, "one").await;
    let last = send(&env, &chat, "two").await;
    let rows = env
        .sql_rows(&format!(
            "SELECT hex(id), created_at FROM messages WHERE request_id = x'{}' AND role = 'user'",
            last.simple()
        ))
        .await;
    let (mid, created): (String, String) = (rows[0].try_get_by_index(0).unwrap(), rows[0].try_get_by_index(1).unwrap());
    env.sql_exec(&format!(
        "INSERT INTO thread_summaries (id, tenant_id, chat_id, summary_text, summarized_up_to_created_at, summarized_up_to_message_id, token_estimate, created_at, updated_at) \
         SELECT x'{}', tenant_id, id, 'old summary', '{created}', x'{mid}', 10, '{created}', '{created}' FROM chats WHERE id = x'{}'",
        Uuid::new_v4().simple(),
        chat.replace('-', "")
    ))
    .await;
    env.sql_exec(&format!("UPDATE messages SET is_compressed = 1 WHERE chat_id = x'{}'", chat.replace('-', ""))).await;
    let r = env.call(&user_a(), "DELETE", &turn_uri(&chat, last), None).await;
    assert_eq!(r.status, 204);
    assert_eq!(env.count("SELECT COUNT(*) FROM thread_summaries").await, 0, "covering summary removed");
    assert_eq!(env.count("SELECT COUNT(*) FROM messages WHERE is_compressed = 1").await, 0);
}

#[tokio::test]
async fn concurrent_mutations_resolve_deterministically() {
    let env = TestEnv::new().await;
    let chat = env.chat(None).await;
    let first = send(&env, &chat, "one").await;
    let uri = format!("{}/retry", turn_uri(&chat, first));
    let edit_uri = turn_uri(&chat, first);
    let ctx = user_a();
    let (a, b) = tokio::join!(
        env.call(&ctx, "POST", &uri, None),
        env.call(&ctx, "PATCH", &edit_uri, Some(json!({"content": "edited"}))),
    );
    let mut statuses = [a.status, b.status];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 409], "a: {} b: {}", a.text(), b.text());
    let loser = if a.status == 409 { &a } else { &b };
    let reason = loser.json()["context"]["reason"].as_str().unwrap_or_default().to_owned();
    assert!(
        reason == "GENERATION_IN_PROGRESS" || reason == "NOT_LATEST_TURN",
        "unexpected reason: {}",
        loser.text()
    );
    assert_eq!(env.count("SELECT COUNT(*) FROM chat_turns WHERE deleted_at IS NULL").await, 1);
}
