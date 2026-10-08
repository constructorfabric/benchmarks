//! Tenant/owner isolation on every resource, PDP denial (403) and PDP
//! failure (503 + Retry-After), PEP request shape.

mod common;

use common::*;
use http::StatusCode;
use serde_json::json;
use uuid::Uuid;

/// Every chat-scoped route a foreign caller might try.
fn chat_routes(chat: Uuid, msg: Uuid, rid: Uuid, att: Uuid) -> Vec<(&'static str, String, Option<serde_json::Value>)> {
    let c = format!("/mini-chat/v1/chats/{chat}");
    vec![
        ("GET", c.clone(), None),
        ("PATCH", c.clone(), Some(json!({"title": "hijack"}))),
        ("DELETE", c.clone(), None),
        ("GET", format!("{c}/messages"), None),
        ("POST", format!("{c}/messages:stream"), Some(json!({"content": "hi"}))),
        ("GET", format!("{c}/turns/{rid}"), None),
        ("POST", format!("{c}/turns/{rid}/retry"), None),
        ("PATCH", format!("{c}/turns/{rid}"), Some(json!({"content": "x"}))),
        ("DELETE", format!("{c}/turns/{rid}"), None),
        ("GET", format!("{c}/attachments/{att}"), None),
        ("DELETE", format!("{c}/attachments/{att}"), None),
        ("PUT", format!("{c}/messages/{msg}/reaction"), Some(json!({"reaction": "like"}))),
        ("DELETE", format!("{c}/messages/{msg}/reaction"), None),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn foreign_user_and_tenant_get_404_everywhere() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let up = h.upload(&a, chat, "a.txt", "text/plain", b"text").await;
    let att = Uuid::parse_str(up.body["id"].as_str().unwrap()).unwrap();
    let s = h.send(&a, chat, "hi").await;
    let (rid, msg) = (s.request_id(), s.message_id());
    let calls = h.gw.chat_calls().len();

    for intruder in [user_a2(), user_b()] {
        for (m, uri, body) in chat_routes(chat, msg, rid, att) {
            let r = h.req(m, &uri, &intruder, body).await;
            assert_eq!(r.status, StatusCode::NOT_FOUND, "{m} {uri}: {}", r.text);
            assert_eq!(r.body["context"]["resource_type"], "gts.cf.core.mini_chat.chat.v1~", "{m} {uri}");
        }
        let up = h.upload(&intruder, chat, "b.txt", "text/plain", b"x").await;
        assert_eq!(up.status, StatusCode::NOT_FOUND);
        // listing does not reveal the chat
        let l = h.get("/mini-chat/v1/chats", &intruder).await;
        assert!(l.body["items"].as_array().unwrap().is_empty());
    }
    // nothing changed for the owner
    let g = h.get(&format!("/mini-chat/v1/chats/{chat}"), &a).await;
    assert_eq!(g.status, StatusCode::OK);
    assert!(g.body.get("title").is_none());
    assert_eq!(h.gw.chat_calls().len(), calls);
    assert_eq!(h.messages(&a, chat).await.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn rows_carry_tenant_and_owner() {
    let h = Harness::new().await;
    let chat_a = h.create_chat(&user_a()).await;
    let chat_b = h.create_chat(&user_b()).await;
    let rows = h
        .rows("SELECT hex(id), hex(tenant_id), hex(user_id) FROM chats ORDER BY created_at")
        .await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0].as_deref().unwrap().to_lowercase(), chat_a.simple().to_string());
    assert_eq!(rows[0][1].as_deref().unwrap().to_lowercase(), TENANT_A.simple().to_string());
    assert_eq!(rows[0][2].as_deref().unwrap().to_lowercase(), USER_A.simple().to_string());
    assert_eq!(rows[1][0].as_deref().unwrap().to_lowercase(), chat_b.simple().to_string());
    assert_eq!(rows[1][1].as_deref().unwrap().to_lowercase(), TENANT_B.simple().to_string());
    let la = h.get("/mini-chat/v1/chats", &user_a()).await;
    assert_eq!(la.body["items"].as_array().unwrap().len(), 1);
    assert_eq!(la.body["items"][0]["id"], chat_a.to_string());
}

#[tokio::test(flavor = "multi_thread")]
async fn pdp_denial_is_403_and_failure_is_503() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let routes = [
        ("GET", "/mini-chat/v1/chats".to_owned(), None),
        ("POST", "/mini-chat/v1/chats".to_owned(), Some(json!({}))),
        ("GET", format!("/mini-chat/v1/chats/{chat}"), None),
        ("GET", "/mini-chat/v1/models".to_owned(), None),
        ("GET", "/mini-chat/v1/quota/status".to_owned(), None),
        ("POST", format!("/mini-chat/v1/chats/{chat}/messages:stream"), Some(json!({"content": "hi"}))),
    ];
    h.set_pdp(PDP_DENY);
    for (m, uri, body) in routes.clone() {
        let r = h.req(m, &uri, &a, body).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{m} {uri}: {}", r.text);
        assert_eq!(r.body["context"]["reason"], "AUTHZ_DENIED");
        assert_eq!(r.body["type"], "gts://gts.cf.core.errors.err.v1~cf.core.err.permission_denied.v1~");
    }
    h.set_pdp(PDP_FAIL);
    for (m, uri, body) in routes {
        let r = h.req(m, &uri, &a, body).await;
        assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{m} {uri}: {}", r.text);
        assert_eq!(r.headers["retry-after"], "5");
        assert_eq!(r.body["context"]["retry_after_seconds"], 5);
        assert!(!r.text.contains("pdp down"), "cause is only logged");
    }
    assert!(h.gw.chat_calls().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn nil_tenant_is_denied() {
    let h = Harness::new().await;
    let nil = ctx(Uuid::new_v4(), Uuid::nil());
    let r = h.get("/mini-chat/v1/chats", &nil).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(r.body["context"]["reason"], "AUTHZ_DENIED");
}

#[tokio::test(flavor = "multi_thread")]
async fn pep_sends_resource_type_and_owner_properties() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    h.get(&format!("/mini-chat/v1/chats/{chat}"), &a).await;
    let reqs = h.pdp.requests.lock().clone();
    let read = reqs.last().unwrap();
    assert_eq!(read.resource.resource_type, mini_chat_sdk::CHAT_AUTHZ_RESOURCE_TYPE);
    assert_eq!(read.resource.id, Some(chat));
    assert_eq!(read.subject.id, USER_A);
    assert!(read.context.require_constraints);
    assert!(read
        .context
        .supported_properties
        .iter()
        .any(|p| p == "owner_tenant_id"));
}
