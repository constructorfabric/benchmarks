//! Authorization cross-checks on every route: foreign tenant / foreign user →
//! 404 (or empty lists), PDP deny → 403, PDP failure → 503 + Retry-After
//! (acceptance: Authorization, Principles "tenant and owner isolation").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::test_support::{AuthzMode, TENANT_A, TENANT_B, TestEnv, USER_A, USER_A2, USER_B, ctx, user_a};

struct Fixture {
    chat: String,
    rid: Uuid,
    asst: String,
    att: String,
}

async fn fixture(env: &TestEnv) -> Fixture {
    let chat = env.chat(None).await;
    let rid = Uuid::new_v4();
    env.stream(&user_a(), &chat, json!({"content": "hi", "request_id": rid})).await;
    let msgs = env.get(&format!("/mini-chat/v1/chats/{chat}/messages")).await.json();
    let asst = msgs["items"][1]["id"].as_str().unwrap().to_owned();
    // an attachment row owned by user A (inserted directly; upload is covered elsewhere)
    let att = Uuid::new_v4();
    env.sql_exec(&format!(
        "INSERT INTO attachments (id, tenant_id, chat_id, uploaded_by_user_id, filename, content_type, size_bytes, storage_backend, provider_file_id, status, attachment_kind, for_file_search, for_code_interpreter, created_at, updated_at) \
         VALUES (x'{}', x'{}', x'{}', x'{}', 'a.pdf', 'application/pdf', 10, 'openai', 'file-abcdefghijkl0001', 'ready', 'document', 1, 0, '2026-01-01T00:00:00.000001Z', '2026-01-01T00:00:00.000001Z')",
        att.simple(),
        TENANT_A.simple(),
        chat.replace('-', ""),
        USER_A.simple()
    ))
    .await;
    Fixture { chat, rid, asst, att: att.to_string() }
}

/// Every chat-scoped route with a body that would otherwise succeed.
fn chat_routes(f: &Fixture) -> Vec<(&'static str, String, Option<Value>)> {
    let c = &f.chat;
    vec![
        ("GET", format!("/mini-chat/v1/chats/{c}"), None),
        ("PATCH", format!("/mini-chat/v1/chats/{c}"), Some(json!({"title": "t"}))),
        ("GET", format!("/mini-chat/v1/chats/{c}/messages"), None),
        ("POST", format!("/mini-chat/v1/chats/{c}/messages:stream"), Some(json!({"content": "x"}))),
        ("GET", format!("/mini-chat/v1/chats/{c}/turns/{}", f.rid), None),
        ("POST", format!("/mini-chat/v1/chats/{c}/turns/{}/retry", f.rid), None),
        ("PATCH", format!("/mini-chat/v1/chats/{c}/turns/{}", f.rid), Some(json!({"content": "x"}))),
        ("PUT", format!("/mini-chat/v1/chats/{c}/messages/{}/reaction", f.asst), Some(json!({"reaction": "like"}))),
        ("DELETE", format!("/mini-chat/v1/chats/{c}/messages/{}/reaction", f.asst), None),
        ("GET", format!("/mini-chat/v1/chats/{c}/attachments/{}", f.att), None),
        ("DELETE", format!("/mini-chat/v1/chats/{c}/attachments/{}", f.att), None),
        ("DELETE", format!("/mini-chat/v1/chats/{c}/turns/{}", f.rid), None),
        ("DELETE", format!("/mini-chat/v1/chats/{c}"), None),
    ]
}

fn outsiders() -> Vec<SecurityContext> {
    vec![ctx(TENANT_A, USER_A2), ctx(TENANT_B, USER_B), ctx(TENANT_B, USER_A)]
}

#[tokio::test]
async fn foreign_users_and_tenants_get_404_everywhere() {
    let env = TestEnv::new().await;
    let f = fixture(&env).await;
    for who in outsiders() {
        for (m, uri, body) in chat_routes(&f) {
            let r = env.call(&who, m, &uri, body).await;
            assert_eq!(r.status, 404, "{m} {uri} as {:?}: {}", who.subject_id(), r.text());
            assert_eq!(r.json()["context"]["resource_type"], "gts.cf.core.mini_chat.chat.v1~", "{m} {uri}");
        }
        let r = env.call(&who, "GET", "/mini-chat/v1/chats", None).await;
        assert_eq!(r.json()["items"], json!([]));
    }
    // nothing changed for the owner
    let v = env.get(&format!("/mini-chat/v1/chats/{}", f.chat)).await.json();
    assert_eq!(v["message_count"], 2);
    assert!(v.get("title").is_none());
    assert_eq!(env.count("SELECT COUNT(*) FROM message_reactions").await, 0);
    assert_eq!(env.count("SELECT COUNT(*) FROM chat_turns WHERE deleted_at IS NULL").await, 1);
}

#[tokio::test]
async fn pdp_denial_is_403_and_failure_is_503_on_every_route() {
    let env = TestEnv::new().await;
    let f = fixture(&env).await;
    let mut routes = chat_routes(&f);
    routes.push(("GET", "/mini-chat/v1/chats".into(), None));
    routes.push(("POST", "/mini-chat/v1/chats".into(), Some(json!({}))));
    routes.push(("GET", "/mini-chat/v1/models".into(), None));
    routes.push(("GET", "/mini-chat/v1/models/gpt-4.1".into(), None));
    routes.push(("GET", "/mini-chat/v1/quota/status".into(), None));
    for (mode, status) in [(AuthzMode::Deny, 403), (AuthzMode::Fail, 503)] {
        env.set_authz(mode);
        for (m, uri, body) in &routes {
            let r = env.call(&user_a(), m, uri, body.clone()).await;
            assert_eq!(r.status, status, "{m} {uri}: {}", r.text());
            if status == 403 {
                assert!(r.json()["context"].to_string().contains("AUTHZ_DENIED"), "{m} {uri}: {}", r.text());
            } else {
                assert_eq!(r.headers["retry-after"], "5", "{m} {uri}");
            }
        }
    }
    // fail closed: nothing was written and the provider was never called again
    env.set_authz(AuthzMode::Allow);
    assert_eq!(env.proxy.chat_requests().len(), 1);
    assert_eq!(env.count("SELECT COUNT(*) FROM chats").await, 1);
}

#[tokio::test]
async fn pdp_receives_the_documented_actions() {
    let env = TestEnv::new().await;
    let f = fixture(&env).await;
    env.authz.calls.lock().unwrap().clear();
    env.get(&format!("/mini-chat/v1/chats/{}/messages", f.chat)).await;
    env.get(&format!("/mini-chat/v1/chats/{}/turns/{}", f.chat, f.rid)).await;
    env.get("/mini-chat/v1/quota/status").await;
    env.get("/mini-chat/v1/models").await;
    let calls = env.authz.calls.lock().unwrap().clone();
    assert_eq!(calls, vec!["list_messages", "read_turn", "read", "list"]);
}
