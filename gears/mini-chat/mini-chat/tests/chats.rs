//! US1: chat CRUD, chat list (`OData`), messages list and message contract.
//!
//! AC: Chat CRUD (lifecycle, list filtering/ordering/pagination, activity ordering),
//! Messages API (list, response contract, count + chronology), Principles (model immutable).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::many_single_char_names)]

mod common;
use common::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_chat_default_and_explicit_model() {
    let h = Harness::new().await;
    let r = h.send(ALICE, "POST", "/chats", Some(json!({}))).await;
    assert_eq!(r.status, 201, "{}", r.text());
    let body = r.json();
    assert_eq!(body["model"], json!("premium-m"), "default model is the catalog default");
    assert_eq!(body["message_count"], json!(0));
    assert_eq!(body["is_temporary"], json!(false));
    assert!(body.get("title").is_none(), "null title is omitted");
    let id = body["id"].as_str().unwrap();
    let loc = r.headers.get("location").expect("Location header").to_str().unwrap();
    assert!(loc.ends_with(&format!("/mini-chat/v1/chats/{id}")), "{loc}");

    let r = h.send(ALICE, "POST", "/chats", Some(json!({"model": "standard-m", "title": "  Hello  "}))).await;
    assert_eq!(r.status, 201);
    assert_eq!(r.json()["model"], json!("standard-m"));
    assert_eq!(r.json()["title"], json!("Hello"), "title is trimmed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn create_chat_rejects_invalid_or_disabled_model() {
    let h = Harness::new().await;
    for m in ["nope", "disabled-m"] {
        let r = h.send(ALICE, "POST", "/chats", Some(json!({"model": m}))).await;
        let p = r.problem(400);
        assert_eq!(fv_reason(&p), "INVALID_MODEL");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn title_validation() {
    let h = Harness::new().await;
    for t in ["", "   ", &"x".repeat(256)] {
        let r = h.send(ALICE, "POST", "/chats", Some(json!({"title": t}))).await;
        assert_eq!(fv_reason(&r.problem(400)), "INVALID_TITLE");
    }
    let r = h.send(ALICE, "POST", "/chats", Some(json!({"title": "x".repeat(255)}))).await;
    assert_eq!(r.status, 201);
    let chat = h.chat(ALICE, None).await;
    let r = h.send(ALICE, "PATCH", &format!("/chats/{chat}"), Some(json!({"title": "  "}))).await;
    assert_eq!(fv_reason(&r.problem(400)), "INVALID_TITLE");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_rename_delete_lifecycle() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let r = h.send(ALICE, "GET", &format!("/chats/{chat}"), None).await;
    assert_eq!(r.status, 200);
    let before = r.json();

    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let r = h.send(ALICE, "PATCH", &format!("/chats/{chat}"), Some(json!({"title": "Renamed"}))).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let after = r.json();
    assert_eq!(after["title"], json!("Renamed"));
    assert_ne!(after["updated_at"], before["updated_at"]);
    assert_eq!(after["created_at"], before["created_at"]);

    let r = h.send(ALICE, "DELETE", &format!("/chats/{chat}"), None).await;
    assert_eq!(r.status, 204);
    assert!(r.body.is_empty());
    let r = h.send(ALICE, "GET", &format!("/chats/{chat}"), None).await;
    r.problem(404);
    let r = h.send(ALICE, "DELETE", &format!("/chats/{chat}"), None).await;
    r.problem(404);
    let r = h.send(ALICE, "PATCH", &format!("/chats/{chat}"), Some(json!({"title": "x"}))).await;
    r.problem(404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn model_is_immutable_via_patch() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let r = h.send(ALICE, "PATCH", &format!("/chats/{chat}"), Some(json!({"title": "t", "model": "premium-m"}))).await;
    // Either the unknown field is rejected or ignored; the model never changes.
    assert!(r.status == 200 || r.status == 400 || r.status == 422, "{}", r.text());
    let r = h.send(ALICE, "GET", &format!("/chats/{chat}"), None).await;
    assert_eq!(r.json()["model"], json!("standard-m"));
    // A turn on the chat keeps using the chat model.
    h.say(ALICE, chat, "hi").await;
    let req = h.provider.chat_requests().pop().unwrap();
    assert_eq!(req["model"], json!("prov-standard-m"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_chats_default_order_is_most_recent_activity() {
    let h = Harness::new().await;
    let a = h.chat(ALICE, Some("standard-m")).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let b = h.chat(ALICE, Some("standard-m")).await;
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    let c = h.chat(ALICE, Some("standard-m")).await;
    let ids = |v: serde_json::Value| -> Vec<String> {
        v["items"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap().to_owned()).collect()
    };
    let r = h.send(ALICE, "GET", "/chats", None).await;
    assert_eq!(r.status, 200);
    assert_eq!(ids(r.json()), vec![c.to_string(), b.to_string(), a.to_string()]);

    // Activity on the oldest chat moves it to the top.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    h.say(ALICE, a, "bump").await;
    let r = h.send(ALICE, "GET", "/chats", None).await;
    let v = r.json();
    assert_eq!(ids(v.clone())[0], a.to_string());
    assert_eq!(v["items"][0]["message_count"], json!(2));

    // Other users never see these chats.
    let r = h.send(BOB, "GET", "/chats", None).await;
    assert_eq!(r.json()["items"].as_array().unwrap().len(), 0);
    let r = h.send(CAROL, "GET", "/chats", None).await;
    assert_eq!(r.json()["items"].as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_chats_pagination_filter_orderby() {
    let h = Harness::new().await;
    for i in 0..5 {
        let r = h.send(ALICE, "POST", "/chats", Some(json!({"title": format!("chat {i}"), "model": "standard-m"}))).await;
        assert_eq!(r.status, 201);
        tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    }
    // limit + cursor
    let r = h.send(ALICE, "GET", "/chats?limit=2", None).await;
    assert_eq!(r.status, 200);
    let p1 = r.json();
    assert_eq!(p1["items"].as_array().unwrap().len(), 2);
    assert_eq!(p1["page_info"]["limit"], json!(2));
    let cursor = p1["page_info"]["next_cursor"].as_str().expect("next_cursor").to_owned();
    let r = h.send(ALICE, "GET", &format!("/chats?limit=2&cursor={cursor}"), None).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let p2 = r.json();
    assert_eq!(p2["items"].as_array().unwrap().len(), 2);
    assert_ne!(p1["items"][0]["id"], p2["items"][0]["id"]);

    // limit clamp to 100
    let r = h.send(ALICE, "GET", "/chats?limit=1000", None).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["page_info"]["limit"], json!(100));

    // filter
    let r = h.send(ALICE, "GET", "/chats?$filter=title%20eq%20'chat%203'", None).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let items = r.json()["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["title"], json!("chat 3"));

    // orderby title asc
    let r = h.send(ALICE, "GET", "/chats?$orderby=title%20asc", None).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let titles: Vec<String> = r.json()["items"].as_array().unwrap().iter().map(|c| c["title"].as_str().unwrap().to_owned()).collect();
    let mut sorted = titles.clone();
    sorted.sort();
    assert_eq!(titles, sorted);

    // $select is accepted
    let r = h.send(ALICE, "GET", "/chats?$select=id,title", None).await;
    assert_eq!(r.status, 200, "{}", r.text());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_chats_rejects_malformed_odata() {
    let h = Harness::new().await;
    h.chat(ALICE, None).await;
    for q in [
        "/chats?limit=0",
        "/chats?limit=abc",
        "/chats?cursor=not-a-cursor",
        "/chats?$filter=unknown_field%20eq%201",
        "/chats?$filter=title%20eq",
        "/chats?$orderby=nope%20asc",
    ] {
        let r = h.send(ALICE, "GET", q, None).await;
        assert_eq!(r.status, 400, "{q}: {}", r.text());
        assert_eq!(r.json()["status"], json!(400));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn messages_list_contract_and_chronology() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.say(ALICE, chat, "first").await;
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
    h.say(ALICE, chat, "second").await;

    let r = h.send(ALICE, "GET", &format!("/chats/{chat}/messages"), None).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let items = r.json()["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 4);
    let roles: Vec<&str> = items.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant"]);
    assert_eq!(items[0]["content"], json!("first"));
    assert_eq!(items[2]["content"], json!("second"));
    // user and assistant of one turn share the request_id
    assert_eq!(items[0]["request_id"], items[1]["request_id"]);
    assert_ne!(items[0]["request_id"], items[2]["request_id"]);
    for m in &items {
        assert!(m["id"].is_string());
        assert!(m["attachments"].is_array(), "attachments always present");
        assert!(m.get("my_reaction").is_some(), "my_reaction always present");
        assert!(m["my_reaction"].is_null());
        assert!(m["created_at"].is_string());
    }
    // assistant messages carry model and token counts
    assert_eq!(items[1]["model"], json!("standard-m"));
    assert_eq!(items[1]["input_tokens"], json!(42));
    assert_eq!(items[1]["output_tokens"], json!(7));
    let created: Vec<&str> = items.iter().map(|m| m["created_at"].as_str().unwrap()).collect();
    let mut sorted = created.clone();
    sorted.sort_unstable();
    assert_eq!(created, sorted, "chronological");

    // message_count on the chat
    let r = h.send(ALICE, "GET", &format!("/chats/{chat}"), None).await;
    assert_eq!(r.json()["message_count"], json!(4));

    // ordering, filter, pagination
    let r = h.send(ALICE, "GET", &format!("/chats/{chat}/messages?$orderby=created_at%20desc&limit=1"), None).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(v["items"].as_array().unwrap().len(), 1);
    assert_eq!(v["items"][0]["role"], json!("assistant"));
    assert_eq!(v["items"][0]["request_id"], items[3]["request_id"]);
    assert!(v["page_info"]["next_cursor"].is_string());

    let r = h.send(ALICE, "GET", &format!("/chats/{chat}/messages?$filter=role%20eq%20'user'"), None).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["items"].as_array().unwrap().len(), 2);

    let r = h.send(ALICE, "GET", &format!("/chats/{chat}/messages?$filter=bogus%20eq%201"), None).await;
    r.problem(400);
    let r = h.send(ALICE, "GET", &format!("/chats/{chat}/messages?limit=0"), None).await;
    r.problem(400);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn messages_of_deleted_or_missing_chat_are_404() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, None).await;
    let r = h.send(ALICE, "GET", &format!("/chats/{}/messages", uuid::Uuid::new_v4()), None).await;
    r.problem(404);
    h.send(ALICE, "DELETE", &format!("/chats/{chat}"), None).await;
    let r = h.send(ALICE, "GET", &format!("/chats/{chat}/messages"), None).await;
    r.problem(404);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_model_removed_from_catalog_is_invalid_model_on_send_and_upload() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("tiny-m")).await;
    let remaining: Vec<serde_json::Value> = catalog().as_array().unwrap().iter().filter(|m| m["id"] != json!("tiny-m")).cloned().collect();
    h.policy.set(&policy_cfg(json!(remaining), json!({})));
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "hi"}))).await;
    assert_eq!(fv_reason(&r.problem(400)), "INVALID_MODEL");
    let r = h.upload(ALICE, chat, "a.txt", "text/plain", b"x").await;
    assert_eq!(fv_reason(&r.problem(400)), "INVALID_MODEL");
    assert!(h.provider.chat_requests().is_empty());
    assert_eq!(h.provider.count("POST", "/files"), 0);
    // The chat itself is still readable.
    assert_eq!(h.send(ALICE, "GET", &format!("/chats/{chat}"), None).await.json()["model"], json!("tiny-m"));
}
