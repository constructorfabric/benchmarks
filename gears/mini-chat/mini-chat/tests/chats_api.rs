//! US1 — chat CRUD, `OData` listing and isolation (T026–T028; AC: chats, authorization, error mapping).

mod common;

use axum::http::{Method, StatusCode};
use common::*;
use serde_json::json;

#[tokio::test]
async fn create_chat_uses_default_model_and_returns_location() {
    let env = TestEnv::start().await;
    let r = env.call("a1", Method::POST, "/chats", Some(json!({"title": "Hello"}))).await;
    assert_eq!(r.status(), StatusCode::CREATED);
    let loc = r.headers().get("location").unwrap().to_str().unwrap().to_owned();
    let body: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(r.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(loc, format!("/mini-chat/v1/chats/{}", body["id"].as_str().unwrap()));
    assert_eq!(body["model"], "gpt-4.1");
    assert_eq!(body["title"], "Hello");
    assert_eq!(body["message_count"], 0);
    assert_eq!(body["is_temporary"], false);
    assert!(body["created_at"].as_str().unwrap().ends_with('Z'));
}

#[tokio::test]
async fn create_chat_with_explicit_and_invalid_models() {
    let env = TestEnv::start().await;
    let (s, v) = env.json("a1", Method::POST, "/chats", Some(json!({"model": "gpt-4.1-mini"}))).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(v["model"], "gpt-4.1-mini");
    assert!(v.get("title").is_none() || v["title"].is_null());

    for bad in ["nope", "disabled-model"] {
        let (s, v) = env.json("a1", Method::POST, "/chats", Some(json!({"model": bad}))).await;
        assert_problem(s, &v, 400, "invalid_argument");
        assert_eq!(violation_reason(&v).as_deref(), Some("INVALID_MODEL"));
    }
}

#[tokio::test]
async fn title_validation() {
    let env = TestEnv::start().await;
    for bad in [json!(""), json!("   "), json!("x".repeat(256))] {
        let (s, v) = env.json("a1", Method::POST, "/chats", Some(json!({"title": bad}))).await;
        assert_problem(s, &v, 400, "invalid_argument");
        assert_eq!(violation_reason(&v).as_deref(), Some("INVALID_TITLE"), "{v}");
    }
    let (s, _) = env.json("a1", Method::POST, "/chats", Some(json!({"title": "x".repeat(255)}))).await;
    assert_eq!(s, StatusCode::CREATED);
}

#[tokio::test]
async fn malformed_json_bodies_map_to_canonical_errors() {
    let env = TestEnv::start().await;
    let r = env
        .raw(
            "a1",
            axum::http::Request::builder().method(Method::POST).uri("/mini-chat/v1/chats").header("content-type", "application/json"),
            axum::body::Body::from("{not json"),
        )
        .await;
    assert!(r.status().is_client_error());
    let (s, v) = env.json("a1", Method::GET, "/chats/not-a-uuid", None).await;
    assert_problem(s, &v, 400, "invalid_argument");
}

#[tokio::test]
async fn get_rename_delete_lifecycle() {
    let env = TestEnv::start().await;
    let id = env.create_chat("a1", json!({"title": "one"})).await;
    let (s, v) = env.json("a1", Method::GET, &format!("/chats/{id}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["title"], "one");

    let (s, v) = env.json("a1", Method::PATCH, &format!("/chats/{id}"), Some(json!({"title": "two"}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["title"], "two");
    let (s, v) = env.json("a1", Method::PATCH, &format!("/chats/{id}"), Some(json!({"title": " "}))).await;
    assert_problem(s, &v, 400, "invalid_argument");

    let (s, _) = env.json("a1", Method::DELETE, &format!("/chats/{id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, v) = env.json("a1", Method::GET, &format!("/chats/{id}"), None).await;
    assert_problem(s, &v, 404, "not_found");
    assert_eq!(v["context"]["resource_type"], "gts.cf.core.mini_chat.chat.v1~");
    let (s, _) = env.json("a1", Method::DELETE, &format!("/chats/{id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // The chat row is soft-deleted and a chat-cleanup outbox message exists.
    assert_eq!(env.count(&format!("SELECT COUNT(*) FROM chats WHERE id = {} AND deleted_at IS NOT NULL", blob(id))).await, 1);
}

#[tokio::test]
async fn list_orders_by_updated_at_desc_and_paginates() {
    let env = TestEnv::start().await;
    let mut ids = Vec::new();
    for i in 0..5 {
        ids.push(env.create_chat("a1", json!({"title": format!("chat {i}")})).await);
    }
    let (s, v) = env.json("a1", Method::GET, "/chats", None).await;
    assert_eq!(s, StatusCode::OK);
    let got: Vec<String> = v["items"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap().to_owned()).collect();
    let want: Vec<String> = ids.iter().rev().map(ToString::to_string).collect();
    assert_eq!(got, want);
    assert_eq!(v["page_info"]["limit"], 20);

    // Cursor paging with limit=2.
    let (_, p1) = env.json("a1", Method::GET, "/chats?limit=2", None).await;
    assert_eq!(p1["items"].as_array().unwrap().len(), 2);
    let cursor = p1["page_info"]["next_cursor"].as_str().unwrap().to_owned();
    let (_, p2) = env.json("a1", Method::GET, &format!("/chats?limit=2&cursor={cursor}"), None).await;
    assert_eq!(p2["items"][0]["id"], want[2].as_str());

    // limit is clamped to 100.
    let (s, v) = env.json("a1", Method::GET, "/chats?limit=1000", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["page_info"]["limit"], 100);
}

#[tokio::test]
async fn list_supports_filter_and_orderby() {
    let env = TestEnv::start().await;
    env.create_chat("a1", json!({"title": "banana"})).await;
    env.create_chat("a1", json!({"title": "apple pie"})).await;
    env.create_chat("a1", json!({"title": "cherry"})).await;
    let (s, v) = env.json("a1", Method::GET, "/chats?$filter=contains(title,%27apple%27)", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["items"].as_array().unwrap().len(), 1);
    assert_eq!(v["items"][0]["title"], "apple pie");
    let (s, v) = env.json("a1", Method::GET, "/chats?$orderby=title%20asc", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let titles: Vec<&str> = v["items"].as_array().unwrap().iter().map(|c| c["title"].as_str().unwrap()).collect();
    assert_eq!(titles, vec!["apple pie", "banana", "cherry"]);
    let (s, v) = env.json("a1", Method::GET, "/chats?$filter=bogus%20eq%201", None).await;
    assert_problem(s, &v, 400, "invalid_argument");
}

#[tokio::test]
async fn chats_are_isolated_per_owner_and_tenant() {
    let env = TestEnv::start().await;
    let id = env.create_chat("a1", json!({"title": "private"})).await;
    for who in ["a2", "b"] {
        let (s, v) = env.json(who, Method::GET, &format!("/chats/{id}"), None).await;
        assert_problem(s, &v, 404, "not_found");
        let (s, _) = env.json(who, Method::PATCH, &format!("/chats/{id}"), Some(json!({"title": "x"}))).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _) = env.json(who, Method::DELETE, &format!("/chats/{id}"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _) = env.json(who, Method::GET, &format!("/chats/{id}/messages"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let r = env.stream(who, id, json!({"content": "hi"})).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        let (_, list) = env.json(who, Method::GET, "/chats", None).await;
        assert!(list["items"].as_array().unwrap().is_empty());
    }
    assert!(env.mock.responses_requests().is_empty());
}

#[tokio::test]
async fn pdp_deny_and_failure_map_to_403_and_503() {
    let env = TestEnv::start().await;
    let id = env.create_chat("a1", json!({})).await;
    env.pdp.set(PdpMode::Deny);
    let (s, v) = env.json("a1", Method::GET, &format!("/chats/{id}"), None).await;
    assert_problem(s, &v, 403, "permission_denied");
    let (s, _) = env.json("a1", Method::POST, "/chats", Some(json!({}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    env.pdp.set(PdpMode::Fail);
    let r = env.call("a1", Method::GET, &format!("/chats/{id}"), None).await;
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(r.headers().get("retry-after").unwrap(), "5");
}

#[tokio::test]
async fn timestamp_filters_use_exact_comparison_semantics() {
    let env = TestEnv::start().await;
    let chat = env.create_chat("a1", json!({})).await;
    env.create_chat("a1", json!({})).await;
    let (_, v) = env.json("a1", Method::GET, &format!("/chats/{chat}"), None).await;
    let ts = v["updated_at"].as_str().unwrap().to_owned();
    let count = |op: &'static str| {
        let env = &env;
        let ts = ts.clone();
        async move {
            let (s, l) = env.json("a1", Method::GET, &format!("/chats?$filter=updated_at%20{op}%20{ts}"), None).await;
            assert_eq!(s, StatusCode::OK, "{l}");
            l["items"].as_array().unwrap().len()
        }
    };
    assert_eq!(count("eq").await, 1);
    assert_eq!(count("ne").await, 1);
    assert_eq!(count("gt").await, 1);
    assert_eq!(count("ge").await, 2);
    assert_eq!(count("lt").await, 0);
    assert_eq!(count("le").await, 1);
    // Messages: created_at of the first message.
    env.stream("a1", chat, json!({"content": "hi"})).await;
    let (_, m) = env.json("a1", Method::GET, &format!("/chats/{chat}/messages"), None).await;
    let first = m["items"][0]["created_at"].as_str().unwrap().to_owned();
    let (s, l) = env.json("a1", Method::GET, &format!("/chats/{chat}/messages?$filter=created_at%20eq%20{first}"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(l["items"].as_array().unwrap().len(), 1);
    let (_, l) = env.json("a1", Method::GET, &format!("/chats/{chat}/messages?$filter=created_at%20gt%20{first}"), None).await;
    assert_eq!(l["items"].as_array().unwrap().len(), 1);
}
