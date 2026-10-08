//! Chat CRUD.

use serde_json::json;

use crate::common::*;

/// Create / get / list / update / delete lifecycle, with model and title validation.
#[tokio::test]
async fn chat_lifecycle_with_validation() {
    let h = Harness::new().await;
    // Default model: first enabled is_default entry.
    let r = h.call(U1, "POST", "/chats", Some(json!({}))).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let created = r.json();
    let id = created["id"].as_str().unwrap().to_owned();
    assert_eq!(created["model"], "premium-1");
    assert_eq!(created["is_temporary"], false);
    assert_eq!(created["message_count"], 0);
    assert!(created.get("title").is_none(), "absent title is omitted");
    assert_eq!(r.headers.get("location").unwrap().to_str().unwrap(), format!("/mini-chat/v1/chats/{id}"));
    assert!(created.get("user_id").is_none() && created.get("tenant_id").is_none());

    // Explicit model and trimmed title.
    let r = h.call(U1, "POST", "/chats", Some(json!({"title": "  Q3  ", "model": "standard-1"}))).await;
    assert_eq!(r.status, 201);
    assert_eq!(r.json()["title"], "Q3");
    assert_eq!(r.json()["model"], "standard-1");

    // Invalid / disabled model.
    for m in ["nope", "disabled-1"] {
        let r = h.call(U1, "POST", "/chats", Some(json!({"model": m}))).await;
        assert_eq!(r.status, 400);
        assert_eq!(r.reason(), "INVALID_MODEL");
        assert_eq!(r.json()["context"]["field_violations"][0]["field"], "model");
    }
    // Invalid titles.
    for t in ["   ", "", &"x".repeat(256)] {
        let r = h.call(U1, "POST", "/chats", Some(json!({"title": t}))).await;
        assert_eq!(r.status, 400, "title {t:?}");
        assert_eq!(r.reason(), "INVALID_TITLE");
        let r = h.call(U1, "PATCH", &format!("/chats/{id}"), Some(json!({"title": t}))).await;
        assert_eq!(r.status, 400);
        assert_eq!(r.reason(), "INVALID_TITLE");
    }
    assert_eq!(h.call(U1, "POST", "/chats", Some(json!({"title": "x".repeat(255)}))).await.status, 201);
    // PATCH body schema errors come from the JSON extractor.
    let r = h.call(U1, "PATCH", &format!("/chats/{id}"), Some(json!({"title": null}))).await;
    assert_eq!(r.status, 422);
    assert_eq!(r.reason(), "invalid_json_body");
    let r = h.call(U1, "PATCH", &format!("/chats/{id}"), Some(json!({}))).await;
    assert_eq!(r.status, 422);

    // Get and update.
    let g = h.call(U1, "GET", &format!("/chats/{id}"), None).await;
    assert_eq!(g.status, 200);
    let before = g.json()["updated_at"].as_str().unwrap().to_owned();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let u = h.call(U1, "PATCH", &format!("/chats/{id}"), Some(json!({"title": " Renamed "}))).await;
    assert_eq!(u.status, 200);
    assert_eq!(u.json()["title"], "Renamed");
    assert_ne!(u.json()["updated_at"].as_str().unwrap(), before);

    // Delete: 204, then 404 for get and second delete.
    assert_eq!(h.call(U1, "DELETE", &format!("/chats/{id}"), None).await.status, 204);
    assert_eq!(h.call(U1, "GET", &format!("/chats/{id}"), None).await.status, 404);
    let r = h.call(U1, "DELETE", &format!("/chats/{id}"), None).await;
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["context"]["resource_type"], "gts.cf.core.mini_chat.chat.v1~");
    let list = h.call(U1, "GET", "/chats", None).await.json();
    assert!(list["items"].as_array().unwrap().iter().all(|c| c["id"] != id.as_str()));
}

/// Listing supports filtering, ordering and pagination; malformed input is rejected.
#[tokio::test]
async fn chat_list_filter_order_pagination() {
    let h = Harness::new().await;
    for t in ["alpha", "beta", "gamma"] {
        h.call(U1, "POST", "/chats", Some(json!({"title": t}))).await;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let all = h.call(U1, "GET", "/chats", None).await.json();
    let titles: Vec<&str> = all["items"].as_array().unwrap().iter().map(|c| c["title"].as_str().unwrap()).collect();
    assert_eq!(titles, vec!["gamma", "beta", "alpha"], "default order updated_at desc");
    assert_eq!(all["page_info"]["limit"], 20);

    let p1 = h.call(U1, "GET", "/chats?limit=2", None).await.json();
    assert_eq!(p1["items"].as_array().unwrap().len(), 2);
    let cursor = p1["page_info"]["next_cursor"].as_str().unwrap();
    assert!(p1["page_info"]["prev_cursor"].is_null());
    let p2 = h.call(U1, "GET", &format!("/chats?limit=2&cursor={cursor}"), None).await.json();
    assert_eq!(p2["items"].as_array().unwrap().len(), 1);
    assert_eq!(p2["items"][0]["title"], "alpha");
    assert!(p2["page_info"]["next_cursor"].is_null());

    let f = h.call(U1, "GET", "/chats?$filter=title%20eq%20'beta'", None).await.json();
    assert_eq!(f["items"].as_array().unwrap().len(), 1);
    let f = h.call(U1, "GET", "/chats?$filter=contains(title,'mm')", None).await.json();
    assert_eq!(f["items"][0]["title"], "gamma");
    let o = h.call(U1, "GET", "/chats?$orderby=title%20asc", None).await.json();
    assert_eq!(o["items"][0]["title"], "alpha");

    assert_eq!(h.call(U1, "GET", "/chats?limit=500", None).await.json()["page_info"]["limit"], 100);
    for (q, reason) in [
        ("limit=0", "INVALID_LIMIT"),
        ("$filter=bogus%20eq%201", "INVALID_FILTER"),
        ("$filter=(((", "INVALID_FILTER"),
        ("cursor=garbage", "INVALID_CURSOR"),
        ("$orderby=nope", "INVALID_ORDERBY_FIELD"),
        ("$skip=1", "UNSUPPORTED_QUERY_PARAM"),
    ] {
        let r = h.call(U1, "GET", &format!("/chats?{q}"), None).await;
        assert_eq!(r.status, 400, "{q}: {}", r.text());
        assert_eq!(r.reason(), reason, "{q}");
        assert_eq!(r.json()["context"]["resource_type"], "gts.cf.core.odata.query.v1~");
    }
}

/// Chat ordering reflects most recent activity.
#[tokio::test]
async fn chat_order_reflects_activity() {
    let h = Harness::new().await;
    let older = h.create_chat(U1, None).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let newer = h.create_chat(U1, None).await;
    let first = |v: serde_json::Value| v["items"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(first(h.call(U1, "GET", "/chats", None).await.json()), newer.to_string());
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    assert_eq!(h.send_message(U1, older, json!({"content": "bump"})).await.status, 200);
    assert_eq!(first(h.call(U1, "GET", "/chats", None).await.json()), older.to_string());
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    h.call(U1, "PATCH", &format!("/chats/{newer}"), Some(json!({"title": "renamed"}))).await;
    assert_eq!(first(h.call(U1, "GET", "/chats", None).await.json()), newer.to_string());
}
