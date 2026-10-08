#![allow(clippy::unwrap_used, clippy::expect_used)]

//! `GET /chats/{id}/messages` (D§3.3 "List Messages", S§5.1).

mod common;

use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use common::*;

fn ids() -> (Uuid, Uuid) {
    (Uuid::new_v4(), Uuid::new_v4())
}

/// Run one completed turn (`"Hello world"`, usage 10 / 5) with `content`.
async fn turn(app: &TestApp, client: &UserClient<'_>, chat: Uuid, content: &str) -> Uuid {
    push_hello(app);
    let rid = Uuid::new_v4();
    let resp = client
        .post_json(
            &stream_path(chat),
            &json!({"content": content, "request_id": rid}),
        )
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    assert_eq!(resp.sse_events().last().unwrap().0, "done");
    rid
}

fn items(page: &Value) -> Vec<Value> {
    page["items"].as_array().unwrap().clone()
}

#[tokio::test]
async fn list_messages_chronological_with_required_fields() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    let rid = turn(&app, &client, chat, "Question?").await;

    let resp = client.get(&messages_path(chat)).await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let page = resp.json();
    let list = items(&page);
    assert_eq!(list.len(), 2, "{page}");
    let (u, a) = (&list[0], &list[1]);
    assert_eq!(u["role"], "user");
    assert_eq!(u["content"], "Question?");
    assert_eq!(a["role"], "assistant");
    assert_eq!(a["content"], "Hello world");
    for m in [u, a] {
        assert_eq!(m["request_id"], rid.to_string());
        assert_eq!(m["attachments"], json!([]));
        assert!(m.as_object().unwrap().contains_key("my_reaction"), "{m}");
        assert_eq!(m["my_reaction"], Value::Null);
        assert!(m["id"].as_str().unwrap().parse::<Uuid>().is_ok());
        assert!(m["created_at"].is_string());
    }
    for key in ["model", "input_tokens", "output_tokens"] {
        assert!(u.get(key).is_none(), "user message has `{key}`: {u}");
    }
    assert_eq!(a["model"], "s1");
    assert_eq!(a["input_tokens"], 10);
    assert_eq!(a["output_tokens"], 5);
    assert_eq!(page["page_info"]["limit"], 20);
}

#[tokio::test]
async fn messages_filter_orderby_pagination() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    turn(&app, &client, chat, "first").await;
    app.clock.advance(time::Duration::seconds(5));
    turn(&app, &client, chat, "second").await;
    let base = messages_path(chat);

    let all = items(&client.get(&base).await.json());
    let contents: Vec<&str> = all.iter().map(|m| m["content"].as_str().unwrap()).collect();
    assert_eq!(contents, ["first", "Hello world", "second", "Hello world"]);

    let resp = client
        .get(&format!("{base}?$filter=role%20eq%20'assistant'"))
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let assistants = items(&resp.json());
    assert_eq!(assistants.len(), 2);
    assert!(assistants.iter().all(|m| m["role"] == "assistant"));

    let resp = client
        .get(&format!("{base}?$orderby=created_at%20desc"))
        .await;
    assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
    let desc = items(&resp.json());
    assert_eq!(desc[0]["id"], all[3]["id"]);
    assert_eq!(desc[3]["id"], all[0]["id"]);

    // limit=1 + cursor walks every message in order.
    let mut seen = Vec::new();
    let mut url = format!("{base}?limit=1");
    for _ in 0..10 {
        let resp = client.get(&url).await;
        assert_eq!(resp.status, StatusCode::OK, "{}", resp.text());
        let page = resp.json();
        let list = items(&page);
        assert!(list.len() <= 1);
        seen.extend(list.iter().map(|m| m["id"].clone()));
        match page["page_info"]["next_cursor"].as_str() {
            Some(c) => url = format!("{base}?limit=1&cursor={c}"),
            None => break,
        }
    }
    let expected: Vec<Value> = all.iter().map(|m| m["id"].clone()).collect();
    assert_eq!(seen, expected);

    let resp = client.get(&format!("{base}?limit=0")).await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
    let resp = client
        .get(&format!("{base}?$filter=content%20eq%20'x'"))
        .await;
    assert_eq!(resp.status, StatusCode::BAD_REQUEST, "{}", resp.text());
    assert_eq!(
        resp.json()["context"]["resource_type"],
        "gts.cf.core.odata.query.v1~"
    );
}

#[tokio::test]
async fn message_count_tracks_turns() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    turn(&app, &client, chat, "one").await;
    turn(&app, &client, chat, "two").await;

    let detail = client.get(&chat_path(chat)).await.json();
    assert_eq!(detail["message_count"], 4);
}

#[tokio::test]
async fn foreign_chat_messages_404() {
    let app = TestApp::builder().build().await;
    let (user, tenant) = ids();
    let client = app.as_user(user, tenant);
    let chat = create_chat(&client, "s1").await;
    turn(&app, &client, chat, "mine").await;

    let other = app.as_user(Uuid::new_v4(), tenant);
    let resp = other.get(&messages_path(chat)).await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND, "{}", resp.text());
    assert_eq!(
        resp.json()["context"]["resource_type"],
        "gts.cf.core.mini_chat.chat.v1~"
    );
    let resp = client.get(&messages_path(Uuid::new_v4())).await;
    assert_eq!(resp.status, StatusCode::NOT_FOUND);
}
