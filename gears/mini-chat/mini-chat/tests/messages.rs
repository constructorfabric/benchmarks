//! Messages API (list contract, filtering, ordering, pagination) and reactions.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use toolkit_security::SecurityContext;
use serde_json::{Value, json};

async fn chat_with_turns(h: &Harness, u: &SecurityContext, n: usize) -> uuid::Uuid {
    let chat = h.create_chat(u, json!({"model": "std"})).await;
    for i in 0..n {
        h.provider.push(ok_reply());
        let rid = uuid::Uuid::new_v4();
        let r = h.send(u, chat, json!({"content": format!("q{i}"), "request_id": rid})).await;
        assert_eq!(r.status, 200, "{}", r.text);
        wait_terminal(h, rid).await;
    }
    chat
}

fn items(v: &Value) -> Vec<Value> {
    v["items"].as_array().unwrap().clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn message_list_contract_order_filter_and_pagination() {
    let h = Harness::new().await;
    let u = user();
    let chat = chat_with_turns(&h, &u, 3).await;
    let base = format!("/mini-chat/v1/chats/{chat}/messages");

    let all = items(&h.call(&u, "GET", &base, None).await.json());
    assert_eq!(all.len(), 6);
    let roles: Vec<_> = all.iter().map(|m| m["role"].as_str().unwrap().to_owned()).collect();
    assert_eq!(roles, ["user", "assistant", "user", "assistant", "user", "assistant"], "chronological");
    assert_eq!(all[0]["content"], "q0");
    for pair in all.chunks(2) {
        assert_eq!(pair[0]["request_id"], pair[1]["request_id"], "turn messages share request_id");
    }
    for m in &all {
        for k in ["id", "request_id", "role", "content", "attachments", "my_reaction", "created_at"] {
            assert!(m.get(k).is_some(), "{k} always present: {m}");
        }
        assert!(m["attachments"].is_array());
        assert!(m["my_reaction"].is_null());
        assert!(m.get("provider_response_id").is_none() && m.get("tenant_id").is_none());
    }
    assert_eq!(all[1]["content"], "Hello world");
    assert_eq!(all[1]["input_tokens"], 100);

    // filter by role
    let r = h.call(&u, "GET", &format!("{base}?$filter=role%20eq%20'assistant'"), None).await;
    assert_eq!(r.status, 200, "{}", r.text);
    let a = items(&r.json());
    assert_eq!(a.len(), 3);
    assert!(a.iter().all(|m| m["role"] == "assistant"));

    // explicit descending order
    let r = h.call(&u, "GET", &format!("{base}?$orderby=created_at%20desc"), None).await;
    assert_eq!(items(&r.json())[0]["id"], all[5]["id"]);

    // cursor pagination walks the whole list without duplicates
    let mut seen = Vec::new();
    let mut url = format!("{base}?limit=4");
    loop {
        let p = h.call(&u, "GET", &url, None).await.json();
        seen.extend(items(&p).into_iter().map(|m| m["id"].clone()));
        match p["page_info"]["next_cursor"].as_str() {
            Some(c) => url = format!("{base}?limit=4&cursor={c}"),
            None => break,
        }
    }
    assert_eq!(seen, all.iter().map(|m| m["id"].clone()).collect::<Vec<_>>());

    for q in ["limit=0", "cursor=garbage", "$filter=content%20eq%20'x'", "$orderby=bogus%20asc"] {
        let r = h.call(&u, "GET", &format!("{base}?{q}"), None).await;
        assert_eq!(r.status, 400, "{q}: {}", r.text);
    }

    // message count tracks turns
    let c = h.call(&u, "GET", &format!("/mini-chat/v1/chats/{chat}"), None).await.json();
    assert_eq!(c["message_count"], 6);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn reactions_on_assistant_messages_only_and_idempotent() {
    let h = Harness::new().await;
    let u = user();
    let chat = chat_with_turns(&h, &u, 1).await;
    let base = format!("/mini-chat/v1/chats/{chat}/messages");
    let all = items(&h.call(&u, "GET", &base, None).await.json());
    let (user_msg, asst) = (all[0]["id"].as_str().unwrap().to_owned(), all[1]["id"].as_str().unwrap().to_owned());
    let uri = format!("{base}/{asst}/reaction");

    let r = h.call(&u, "PUT", &uri, Some(json!({"reaction": "like"}))).await;
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(r.json()["reaction"], "like");
    assert_eq!(r.json()["message_id"], asst.as_str());
    let first_at = r.json()["created_at"].clone();
    // idempotent re-set
    let r = h.call(&u, "PUT", &uri, Some(json!({"reaction": "like"}))).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["created_at"], first_at);
    assert_eq!(h.scalar_i64("SELECT count(*) FROM message_reactions").await, 1);
    let msgs = items(&h.call(&u, "GET", &base, None).await.json());
    assert_eq!(msgs[1]["my_reaction"], "like");
    // switch to dislike replaces the row
    let r = h.call(&u, "PUT", &uri, Some(json!({"reaction": "dislike"}))).await;
    assert_eq!(r.json()["reaction"], "dislike");
    assert_eq!(h.scalar_i64("SELECT count(*) FROM message_reactions").await, 1);

    // invalid value / user message target
    let r = h.call(&u, "PUT", &uri, Some(json!({"reaction": "love"}))).await;
    assert_eq!(r.status, 400);
    assert_eq!(reason(&r.json()), "INVALID_REACTION");
    let r = h.call(&u, "PUT", &format!("{base}/{user_msg}/reaction"), Some(json!({"reaction": "like"}))).await;
    assert_eq!(r.status, 400, "{}", r.text);
    let r = h.call(&u, "DELETE", &format!("{base}/{user_msg}/reaction"), None).await;
    assert_eq!(r.status, 400);
    // unknown message
    let r = h.call(&u, "PUT", &format!("{base}/{}/reaction", uuid::Uuid::new_v4()), Some(json!({"reaction": "like"}))).await;
    assert_eq!(r.status, 404);

    // remove is idempotent
    assert_eq!(h.call(&u, "DELETE", &uri, None).await.status, 204);
    assert_eq!(h.call(&u, "DELETE", &uri, None).await.status, 204);
    assert_eq!(h.scalar_i64("SELECT count(*) FROM message_reactions").await, 0);
    let msgs = items(&h.call(&u, "GET", &base, None).await.json());
    assert!(msgs[1]["my_reaction"].is_null());
    h.shutdown().await;
}
