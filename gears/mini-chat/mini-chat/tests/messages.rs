//! T045: message listing, response contract and message counting.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;

#[tokio::test]
async fn chronological_listing_and_contract_fields() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let img = h.upload_ok(chat, "pic.png", "image/png", &png(16, 8)).await;
    h.provider.push(Script::ok("one"));
    h.send_body(chat, json!({"content": "first", "attachment_ids": [img]}))
        .await;
    h.provider.push(Script::ok("two"));
    h.send(chat, "second").await;

    let msgs = h.messages(chat).await;
    assert_eq!(msgs.len(), 4);
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, vec!["user", "assistant", "user", "assistant"]);
    let contents: Vec<&str> = msgs
        .iter()
        .map(|m| m["content"].as_str().unwrap())
        .collect();
    assert_eq!(contents, vec!["first", "one", "second", "two"]);
    for m in &msgs {
        assert!(m["id"].is_string() && m["request_id"].is_string() && m["created_at"].is_string());
        assert!(
            m["attachments"].is_array(),
            "attachments always present: {m}"
        );
        assert!(
            m.as_object().unwrap().contains_key("my_reaction"),
            "my_reaction always present: {m}"
        );
        assert!(m["my_reaction"].is_null());
    }
    // user messages carry no model / tokens
    assert!(msgs[0].get("model").is_none());
    assert!(msgs[0].get("input_tokens").is_none() && msgs[0].get("output_tokens").is_none());
    assert_eq!(msgs[1]["model"], "prem");
    // attachment summary on the user message, thumbnail for ready images
    let a = &msgs[0]["attachments"][0];
    assert_eq!(a["attachment_id"], img.to_string());
    assert_eq!(a["kind"], "image");
    assert_eq!(a["filename"], "pic.png");
    assert_eq!(a["status"], "ready");
    assert_eq!(a["img_thumbnail"]["content_type"], "image/webp");
    assert!(a["img_thumbnail"]["width"].as_i64().unwrap() > 0);
    assert_eq!(msgs[2]["attachments"], json!([]));
    // same request id within a turn
    assert_eq!(msgs[0]["request_id"], msgs[1]["request_id"]);
    assert_ne!(msgs[0]["request_id"], msgs[2]["request_id"]);

    let (_, chat_v) = h.get(&format!("/mini-chat/v1/chats/{chat}")).await;
    assert_eq!(chat_v["message_count"], 4);
}

#[tokio::test]
async fn filter_orderby_pagination() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    for i in 0..3 {
        h.provider.push(Script::ok(&format!("a{i}")));
        h.send(chat, &format!("u{i}")).await;
    }
    let base = format!("/mini-chat/v1/chats/{chat}/messages");
    let (s, v) = h
        .get(&format!("{base}?$filter=role%20eq%20'assistant'"))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert!(items.iter().all(|m| m["role"] == "assistant"));

    let (s, v) = h.get(&format!("{base}?$orderby=created_at%20desc")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["items"][0]["content"], "a2");

    let id = v["items"][1]["id"].as_str().unwrap().to_owned();
    let (s, v) = h.get(&format!("{base}?$filter=id%20eq%20{id}")).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["items"].as_array().unwrap().len(), 1);

    let mut seen = Vec::new();
    let mut url = format!("{base}?limit=2");
    loop {
        let (s, v) = h.get(&url).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        seen.extend(
            v["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["content"].as_str().unwrap().to_owned()),
        );
        match v["page_info"]["next_cursor"].as_str() {
            Some(c) => url = format!("{base}?limit=2&cursor={c}"),
            None => break,
        }
    }
    assert_eq!(seen, vec!["u0", "a0", "u1", "a1", "u2", "a2"]);

    let (s, _) = h.get(&format!("{base}?limit=0")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = h.get(&format!("{base}?$filter=content%20eq%20'x'")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = h.get(&format!("{base}?cursor=zzz")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, v) = h.get(&format!("{base}?limit=500")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["page_info"]["limit"], 100);
}

#[tokio::test]
async fn message_count_tracks_failures_and_deletions() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.send(chat, "ok").await;
    h.provider.push(Script::Failed {
        parts: vec![],
        message: "x".into(),
        usage: None,
    });
    let r = h.send(chat, "fails").await;
    let failed_rid = r.request_id();
    let (_, v) = h.get(&format!("/mini-chat/v1/chats/{chat}")).await;
    assert_eq!(v["message_count"], 3, "failed turn keeps the user message");
    let (s, _, _) = h
        .req(
            &h.ctx(),
            "DELETE",
            &format!("/mini-chat/v1/chats/{chat}/turns/{failed_rid}"),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, v) = h.get(&format!("/mini-chat/v1/chats/{chat}")).await;
    assert_eq!(v["message_count"], 2);
    assert_eq!(h.messages(chat).await.len(), 2);
}
