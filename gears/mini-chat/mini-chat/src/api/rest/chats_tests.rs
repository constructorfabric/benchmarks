//! Router tests: chat CRUD, listing, validation (acceptance: Chat CRUD,
//! Principles "model immutable", Authorization).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::body::Body;
use axum::http::Request;
use serde_json::json;

use crate::test_support::{AuthzMode, TENANT_A, TENANT_B, TestEnv, USER_A2, USER_B, ctx, user_a};

#[tokio::test]
async fn create_uses_default_model_and_returns_location() {
    let env = TestEnv::new().await;
    let r = env.call(&user_a(), "POST", "/mini-chat/v1/chats", Some(json!({}))).await;
    assert_eq!(r.status, 201);
    let v = r.json();
    let id = v["id"].as_str().unwrap();
    assert_eq!(r.headers["location"], format!("/mini-chat/v1/chats/{id}"));
    assert_eq!(v["model"], "gpt-4.1");
    assert_eq!(v["message_count"], 0);
    assert_eq!(v["is_temporary"], false);
    assert!(v.get("title").is_none(), "untitled chat must omit title: {v}");
    assert!(v.get("user_id").is_none());
    let rows = env.count(&format!("SELECT COUNT(*) FROM chats WHERE id = x'{}'", id.replace('-', ""))).await;
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn create_validates_title_and_model() {
    let env = TestEnv::new().await;
    let v = env.create_chat(&user_a(), json!({"title": "  Plan  ", "model": "gpt-4.1-mini"})).await;
    assert_eq!(v["title"], "Plan");
    assert_eq!(v["model"], "gpt-4.1-mini");
    for title in [json!(""), json!("   "), json!("x".repeat(256))] {
        let r = env.call(&user_a(), "POST", "/mini-chat/v1/chats", Some(json!({"title": title}))).await;
        r.assert_problem(400, "INVALID_TITLE");
    }
    let ok255 = env.call(&user_a(), "POST", "/mini-chat/v1/chats", Some(json!({"title": "\u{e9}".repeat(255)}))).await;
    assert_eq!(ok255.status, 201);
    for model in ["old-model", "nope"] {
        let r = env.call(&user_a(), "POST", "/mini-chat/v1/chats", Some(json!({"model": model}))).await;
        r.assert_problem(400, "INVALID_MODEL");
        assert_eq!(r.json()["context"]["field_violations"][0]["field"], "model");
    }
    // title is validated before authorization
    env.set_authz(AuthzMode::Deny);
    let r = env.call(&user_a(), "POST", "/mini-chat/v1/chats", Some(json!({"title": ""}))).await;
    assert_eq!(r.status, 400);
}

#[tokio::test]
async fn get_update_delete_lifecycle_and_model_immutability() {
    let env = TestEnv::new().await;
    let id = env.chat(None).await;
    let r = env.get(&format!("/mini-chat/v1/chats/{id}")).await;
    assert_eq!(r.status, 200);
    let created = r.json();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let r = env
        .call(&user_a(), "PATCH", &format!("/mini-chat/v1/chats/{id}"), Some(json!({"title": " Renamed ", "model": "gpt-4.1-mini"})))
        .await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(v["title"], "Renamed");
    assert_eq!(v["model"], "gpt-4.1", "model must stay locked");
    assert_ne!(v["updated_at"], created["updated_at"]);
    let r = env.call(&user_a(), "PATCH", &format!("/mini-chat/v1/chats/{id}"), Some(json!({"title": "  "}))).await;
    r.assert_problem(400, "INVALID_TITLE");
    let r = env.call(&user_a(), "PATCH", &format!("/mini-chat/v1/chats/{id}"), Some(json!({}))).await;
    assert_eq!(r.status, 422);
    let r = env.call(&user_a(), "PATCH", &format!("/mini-chat/v1/chats/{id}"), Some(json!({"title": null}))).await;
    assert_eq!(r.status, 422);
    let r = env.call(&user_a(), "DELETE", &format!("/mini-chat/v1/chats/{id}"), None).await;
    assert_eq!(r.status, 204);
    assert_eq!(env.get(&format!("/mini-chat/v1/chats/{id}")).await.status, 404);
    let r = env.call(&user_a(), "DELETE", &format!("/mini-chat/v1/chats/{id}"), None).await;
    r.assert_problem(404, "gts.cf.core.mini_chat.chat.v1~");
    let deleted = env
        .count(&format!("SELECT COUNT(*) FROM chats WHERE id = x'{}' AND deleted_at IS NOT NULL", id.replace('-', "")))
        .await;
    assert_eq!(deleted, 1, "soft delete keeps the row");
}

#[tokio::test]
async fn foreign_and_cross_tenant_access_is_404() {
    let env = TestEnv::new().await;
    let id = env.chat(None).await;
    for other in [ctx(TENANT_A, USER_A2), ctx(TENANT_B, USER_B), ctx(TENANT_B, crate::test_support::USER_A)] {
        for (m, uri, body) in [
            ("GET", format!("/mini-chat/v1/chats/{id}"), None),
            ("PATCH", format!("/mini-chat/v1/chats/{id}"), Some(json!({"title": "x"}))),
            ("DELETE", format!("/mini-chat/v1/chats/{id}"), None),
            ("GET", format!("/mini-chat/v1/chats/{id}/messages"), None),
        ] {
            let r = env.call(&other, m, &uri, body).await;
            r.assert_problem(404, "gts.cf.core.mini_chat.chat.v1~");
        }
        let r = env.call(&other, "GET", "/mini-chat/v1/chats", None).await;
        assert_eq!(r.json()["items"].as_array().unwrap().len(), 0);
    }
    assert_eq!(TENANT_A.to_string(), "00000000-df51-5b42-9538-d2b56b7ee953");
}

#[tokio::test]
async fn pdp_denial_and_failure() {
    let env = TestEnv::new().await;
    let id = env.chat(None).await;
    env.set_authz(AuthzMode::Deny);
    let r = env.get(&format!("/mini-chat/v1/chats/{id}")).await;
    r.assert_problem(403, "AUTHZ_DENIED");
    env.set_authz(AuthzMode::Fail);
    let r = env.get(&format!("/mini-chat/v1/chats/{id}")).await;
    assert_eq!(r.status, 503);
    assert_eq!(r.headers["retry-after"], "5");
}

#[tokio::test]
async fn request_shape_errors() {
    let env = TestEnv::new().await;
    let r = env.get("/mini-chat/v1/chats/not-a-uuid").await;
    r.assert_problem(400, "invalid_path_params");
    let mut req = Request::builder()
        .method("POST")
        .uri("/mini-chat/v1/chats")
        .header("content-type", "application/json")
        .body(Body::from("{bad json"))
        .unwrap();
    req.extensions_mut().insert(user_a());
    env.send(req).await.assert_problem(400, "json_syntax_error");
    let mut req = Request::builder()
        .method("POST")
        .uri("/mini-chat/v1/chats")
        .body(Body::from("{}"))
        .unwrap();
    req.extensions_mut().insert(user_a());
    assert_eq!(env.send(req).await.status, 415);
    let r = env.call(&user_a(), "POST", "/mini-chat/v1/chats", Some(json!({"title": 5}))).await;
    r.assert_problem(422, "invalid_json_body");
}

#[tokio::test]
#[allow(clippy::many_single_char_names)] // reason: short throwaway response/chat bindings in a scenario test
async fn list_orders_by_activity_and_paginates() {
    let env = TestEnv::new().await;
    let a = env.create_chat(&user_a(), json!({"title": "alpha"})).await["id"].as_str().unwrap().to_owned();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let b = env.create_chat(&user_a(), json!({"title": "beta"})).await["id"].as_str().unwrap().to_owned();
    let r = env.get("/mini-chat/v1/chats").await;
    let ids: Vec<String> = r.json()["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap().to_owned()).collect();
    assert_eq!(ids, vec![b.clone(), a.clone()]);
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let s = env.send_msg(&a, "hi").await;
    assert_eq!(s.status, 200);
    let r = env.get("/mini-chat/v1/chats").await;
    let items = r.json()["items"].as_array().unwrap().clone();
    assert_eq!(items[0]["id"], a.as_str(), "most recent activity first");
    assert_eq!(items[0]["message_count"], 2);
    // filter / orderby
    let r = env.get("/mini-chat/v1/chats?$filter=title%20eq%20'beta'").await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["items"].as_array().unwrap().len(), 1);
    let r = env.get("/mini-chat/v1/chats?$orderby=title%20asc").await;
    assert_eq!(r.json()["items"][0]["title"], "alpha");
    // pagination
    let r = env.get("/mini-chat/v1/chats?limit=1").await;
    let v = r.json();
    assert_eq!(v["items"].as_array().unwrap().len(), 1);
    assert_eq!(v["page_info"]["limit"], 1);
    let next = v["page_info"]["next_cursor"].as_str().unwrap().to_owned();
    let r = env.get(&format!("/mini-chat/v1/chats?limit=1&cursor={next}")).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["items"][0]["id"], b.as_str());
    let r = env.get("/mini-chat/v1/chats?limit=1000").await;
    assert_eq!(r.json()["page_info"]["limit"], 100);
    // malformed input
    for (q, reason) in [
        ("limit=0", "INVALID_LIMIT"),
        ("$filter=bogus%20eq%201", "INVALID_FILTER"),
        ("$filter=title%20eq", "INVALID_FILTER"),
        ("$orderby=bogus%20asc", "INVALID_ORDERBY_FIELD"),
        ("cursor=not-a-cursor", "INVALID_CURSOR"),
        ("$skip=1", "UNSUPPORTED_QUERY_PARAM"),
    ] {
        let r = env.get(&format!("/mini-chat/v1/chats?{q}")).await;
        r.assert_problem(400, reason);
        assert_eq!(r.json()["context"]["resource_type"], "gts.cf.core.odata.query.v1~", "{q}");
    }
}
