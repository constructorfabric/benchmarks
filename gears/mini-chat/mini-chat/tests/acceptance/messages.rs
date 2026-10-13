//! Messages API.

use serde_json::json;
use uuid::Uuid;

use crate::common::*;

/// List messages with filtering, ordering and pagination.
#[tokio::test]
async fn list_messages_filter_order_pagination() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    for i in 0..3 {
        assert_eq!(h.send_message(U1, chat, json!({"content": format!("q{i}")})).await.status, 200);
    }
    let all = h.call(U1, "GET", &format!("/chats/{chat}/messages"), None).await.json();
    let items = all["items"].as_array().unwrap();
    assert_eq!(items.len(), 6);
    assert_eq!(items[0]["content"], "q0");
    let users = h.call(U1, "GET", &format!("/chats/{chat}/messages?$filter=role%20eq%20'user'"), None).await.json();
    assert_eq!(users["items"].as_array().unwrap().len(), 3);
    let desc = h.call(U1, "GET", &format!("/chats/{chat}/messages?$orderby=created_at%20desc"), None).await.json();
    assert_eq!(desc["items"][0]["role"], "assistant");
    assert_eq!(desc["items"][5]["content"], "q0");
    let p1 = h.call(U1, "GET", &format!("/chats/{chat}/messages?limit=4"), None).await.json();
    assert_eq!(p1["items"].as_array().unwrap().len(), 4);
    let c = p1["page_info"]["next_cursor"].as_str().unwrap();
    let p2 = h.call(U1, "GET", &format!("/chats/{chat}/messages?limit=4&cursor={c}"), None).await.json();
    assert_eq!(p2["items"].as_array().unwrap().len(), 2);
    assert_eq!(p2["items"][1]["content"], "Hello world");
    let id = items[3]["id"].as_str().unwrap();
    let one = h.call(U1, "GET", &format!("/chats/{chat}/messages?$filter=id%20eq%20{id}"), None).await.json();
    assert_eq!(one["items"].as_array().unwrap().len(), 1);
    for q in ["limit=0", "$filter=bogus%20eq%201", "cursor=xyz", "$orderby=title"] {
        let r = h.call(U1, "GET", &format!("/chats/{chat}/messages?{q}"), None).await;
        assert_eq!(r.status, 400, "{q}");
        assert_eq!(r.json()["context"]["resource_type"], "gts.cf.core.odata.query.v1~");
    }
    assert_eq!(
        h.call(U1, "GET", &format!("/chats/{}/messages", Uuid::new_v4()), None).await.status,
        404
    );
}

/// Message response contract: identity, attachments and reaction always present.
#[tokio::test]
async fn message_response_contract() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    let up = h.upload(U1, chat, "a.txt", "text/plain", b"text").await;
    let att = up.json()["id"].as_str().unwrap().to_owned();
    let rid = Uuid::new_v4();
    h.provider.push(completed(&["Answer"], 12, 7));
    let r = h.send_message(U1, chat, json!({"content": "q", "request_id": rid, "attachment_ids": [att]})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    let items = h.call(U1, "GET", &format!("/chats/{chat}/messages"), None).await.json()["items"].clone();
    let (user, asst) = (&items[0], &items[1]);
    for m in [user, asst] {
        assert!(m["id"].is_string());
        assert_eq!(m["request_id"], rid.to_string(), "user and assistant share the request id");
        assert!(m["attachments"].is_array());
        assert!(m.as_object().unwrap().contains_key("my_reaction"));
        assert!(m["my_reaction"].is_null());
        assert!(m["created_at"].is_string());
    }
    assert_eq!(user["role"], "user");
    assert!(user.get("model").is_none() && user.get("input_tokens").is_none());
    assert_eq!(user["attachments"][0]["attachment_id"], att);
    assert_eq!(user["attachments"][0]["kind"], "document");
    assert_eq!(user["attachments"][0]["filename"], "a.txt");
    assert_eq!(user["attachments"][0]["status"], "ready");
    assert!(user["attachments"][0].get("img_thumbnail").is_none());
    assert_eq!(asst["role"], "assistant");
    assert_eq!(asst["attachments"], json!([]));
    assert_eq!(asst["model"], "premium-1");
    assert_eq!(asst["input_tokens"], 12);
    assert_eq!(asst["output_tokens"], 7);
    let text = h.call(U1, "GET", &format!("/chats/{chat}/messages"), None).await.text();
    assert!(!text.contains("file-") && !text.contains("resp_"), "no provider ids in messages");
    // Reaction shows up as my_reaction.
    let aid = asst["id"].as_str().unwrap();
    h.call(U1, "PUT", &format!("/chats/{chat}/messages/{aid}/reaction"), Some(json!({"reaction": "dislike"}))).await;
    let items = h.call(U1, "GET", &format!("/chats/{chat}/messages"), None).await.json()["items"].clone();
    assert_eq!(items[1]["my_reaction"], "dislike");
}

/// Message count and chronological ordering are tracked across turns.
#[tokio::test]
async fn message_count_and_chronology() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    for i in 0..2 {
        h.send_message(U1, chat, json!({"content": format!("m{i}")})).await;
    }
    let c = h.call(U1, "GET", &format!("/chats/{chat}"), None).await.json();
    assert_eq!(c["message_count"], 4);
    let items = h.call(U1, "GET", &format!("/chats/{chat}/messages"), None).await.json()["items"].clone();
    let roles: Vec<&str> = items.as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, vec!["user", "assistant", "user", "assistant"]);
    let times: Vec<&str> = items.as_array().unwrap().iter().map(|m| m["created_at"].as_str().unwrap()).collect();
    let mut sorted = times.clone();
    sorted.sort_unstable();
    assert_eq!(times, sorted);
    // Deleting the last turn removes its messages from the count.
    let rid = items[2]["request_id"].as_str().unwrap();
    assert_eq!(h.call(U1, "DELETE", &format!("/chats/{chat}/turns/{rid}"), None).await.status, 204);
    let c = h.call(U1, "GET", &format!("/chats/{chat}"), None).await.json();
    assert_eq!(c["message_count"], 2);
    let list = h.call(U1, "GET", "/chats", None).await.json();
    assert_eq!(list["items"][0]["message_count"], 2);
}
