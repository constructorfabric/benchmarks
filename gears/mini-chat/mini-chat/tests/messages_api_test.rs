//! Messages list: contract (`request_id`, attachments, `my_reaction`, optional
//! fields), ordering, `OData` filter/order/pagination, counts across turns.

mod common;

use common::*;
use http::StatusCode;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn message_contract_and_chronological_order() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let s1 = h.send(&a, chat, "first question").await;
    assert_eq!(s1.names().last(), Some(&"done"), "{}", s1.raw);
    let s2 = h.send(&a, chat, "second question").await;
    assert_eq!(s2.names().last(), Some(&"done"));

    let items = h.messages(&a, chat).await;
    assert_eq!(items.len(), 4);
    let roles: Vec<&str> = items.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, vec!["user", "assistant", "user", "assistant"]);
    assert_eq!(items[0]["content"], "first question");
    assert_eq!(items[1]["content"], "Hello world");
    // user and assistant of one turn share the request id
    assert_eq!(items[0]["request_id"], items[1]["request_id"]);
    assert_eq!(items[0]["request_id"], s1.request_id().to_string());
    assert_ne!(items[0]["request_id"], items[2]["request_id"]);
    assert_eq!(items[1]["id"], s1.message_id().to_string(), "stream_started.message_id is the assistant message id");
    for m in &items {
        assert!(m["attachments"].is_array(), "attachments always present");
        assert!(m.get("my_reaction").is_some(), "my_reaction always present");
        assert!(m["my_reaction"].is_null());
        assert!(m.get("created_at").is_some());
    }
    // optional fields: model and tokens on assistant messages only
    assert!(items[0].get("model").is_none());
    assert!(items[0].get("input_tokens").is_none());
    assert_eq!(items[1]["model"], "gpt-premium");
    assert_eq!(items[1]["input_tokens"], 10);
    assert_eq!(items[1]["output_tokens"], 5);
    // chronological
    let ts: Vec<&str> = items.iter().map(|m| m["created_at"].as_str().unwrap()).collect();
    let mut sorted = ts.clone();
    sorted.sort_unstable();
    assert_eq!(ts, sorted);

    let chat_detail = h.get(&format!("/mini-chat/v1/chats/{chat}"), &a).await;
    assert_eq!(chat_detail.body["message_count"], 4);
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_filter_order_and_paginate() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    for i in 0..3 {
        let s = h.send(&a, chat, &format!("q{i}")).await;
        assert_eq!(s.names().last(), Some(&"done"));
    }
    let base = format!("/mini-chat/v1/chats/{chat}/messages");
    let r = h.get(&format!("{base}?$filter=role%20eq%20'user'"), &a).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let items = r.body["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert!(items.iter().all(|m| m["role"] == "user"));

    let r = h.get(&format!("{base}?$orderby=created_at%20desc"), &a).await;
    assert_eq!(r.body["items"][0]["role"], "assistant");
    assert_eq!(r.body["items"][1]["content"], "q2");

    let id = items[1]["id"].as_str().unwrap();
    let r = h.get(&format!("{base}?$filter=id%20eq%20{id}"), &a).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert_eq!(r.body["items"].as_array().unwrap().len(), 1);
    assert_eq!(r.body["items"][0]["content"], "q1");

    // Pagination across all 6 messages, 4 per page, no duplicates.
    let p1 = h.get(&format!("{base}?limit=4"), &a).await;
    assert_eq!(p1.body["items"].as_array().unwrap().len(), 4);
    let c = p1.body["page_info"]["next_cursor"].as_str().unwrap();
    let p2 = h.get(&format!("{base}?limit=4&cursor={c}"), &a).await;
    assert_eq!(p2.status, StatusCode::OK, "{}", p2.text);
    let p2_items = p2.body["items"].as_array().unwrap();
    assert_eq!(p2_items.len(), 2);
    assert!(p2.body["page_info"]["next_cursor"].is_null());
    let mut ids: Vec<String> = p1.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .chain(p2_items)
        .map(|m| m["id"].as_str().unwrap().to_owned())
        .collect();
    ids.dedup();
    assert_eq!(ids.len(), 6);

    // Malformed input
    for (q, reason) in [
        ("$filter=nope%20eq%201", "INVALID_FILTER"),
        ("$orderby=content%20asc", "INVALID_ORDERBY_FIELD"),
        ("limit=0", "INVALID_LIMIT"),
        ("cursor=@@@", "INVALID_CURSOR"),
    ] {
        let r = h.get(&format!("{base}?{q}"), &a).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{q}");
        assert_eq!(r.body["context"]["resource_type"], "gts.cf.core.odata.query.v1~");
        assert_eq!(r.body["context"]["field_violations"][0]["reason"], reason, "{q}: {}", r.text);
    }
    // cursor + $orderby
    let r = h
        .get(&format!("{base}?cursor={c}&$orderby=created_at%20asc"), &a)
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_of_unknown_or_foreign_chat_are_404() {
    let h = Harness::new().await;
    let chat = h.create_chat(&user_a()).await;
    for c in [user_a2(), user_b()] {
        let r = h.get(&format!("/mini-chat/v1/chats/{chat}/messages"), &c).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        assert_eq!(r.body["context"]["resource_type"], "gts.cf.core.mini_chat.chat.v1~");
    }
    let r = h
        .get(&format!("/mini-chat/v1/chats/{}/messages", uuid::Uuid::new_v4()), &user_a())
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn message_attachments_and_reaction_enrichment() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let up = h.upload(&a, chat, "notes.txt", "text/plain", b"some notes").await;
    assert_eq!(up.status, StatusCode::CREATED, "{}", up.text);
    let att = up.body["id"].as_str().unwrap().to_owned();
    let s = h
        .send_body(&a, chat, json!({"content": "summarize", "attachment_ids": [att]}))
        .await;
    assert_eq!(s.names().last(), Some(&"done"), "{}", s.raw);
    let msg = s.message_id();
    let r = h
        .req(
            "PUT",
            &format!("/mini-chat/v1/chats/{chat}/messages/{msg}/reaction"),
            &a,
            Some(json!({"reaction": "dislike"})),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let items = h.messages(&a, chat).await;
    let user = &items[0];
    assert_eq!(user["attachments"].as_array().unwrap().len(), 1);
    let summary = &user["attachments"][0];
    assert_eq!(summary["attachment_id"], att.as_str());
    assert_eq!(summary["kind"], "document");
    assert_eq!(summary["filename"], "notes.txt");
    assert_eq!(summary["status"], "ready");
    assert!(summary.get("img_thumbnail").is_none());
    assert_eq!(items[1]["my_reaction"], "dislike");
    // Another user of the tenant has no access to the chat at all.
    let r = h.get(&format!("/mini-chat/v1/chats/{chat}/messages"), &user_a2()).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}
