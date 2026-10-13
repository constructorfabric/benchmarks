//! T064: web search tool exposure, guard, kill switch, quotas, accounting.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use axum::http::StatusCode;
use common::*;
use mini_chat::config::DEFAULT_WEB_SEARCH_GUARD;
use serde_json::{Value, json};
use uuid::Uuid;

fn ws_events(n: usize) -> Vec<(String, Value)> {
    let mut v = Vec::new();
    for _ in 0..n {
        v.push((
            "response.web_search_call.searching".to_owned(),
            json!({"type": "response.web_search_call.searching"}),
        ));
        v.push((
            "response.web_search_call.completed".to_owned(),
            json!({"type": "response.web_search_call.completed"}),
        ));
    }
    v
}

fn ws_tool(req: &Value) -> Option<&Value> {
    req["tools"]
        .as_array()
        .and_then(|t| t.iter().find(|t| t["type"] == "web_search"))
}

#[tokio::test]
async fn tool_and_guard_only_when_enabled_and_supported() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.send(chat, "no search").await;
    let req = h.provider.chat_requests().pop().unwrap();
    assert!(ws_tool(&req).is_none());
    assert!(
        !req["instructions"]
            .as_str()
            .unwrap()
            .contains(DEFAULT_WEB_SEARCH_GUARD)
    );

    h.send_body(
        chat,
        json!({"content": "search", "web_search": {"enabled": true}}),
    )
    .await;
    let req = h.provider.chat_requests().pop().unwrap();
    assert_eq!(ws_tool(&req).unwrap()["search_context_size"], "low");
    assert!(
        req["instructions"]
            .as_str()
            .unwrap()
            .contains(DEFAULT_WEB_SEARCH_GUARD)
    );
    assert_eq!(req["metadata"]["feature"], "web_search");

    h.send_body(
        chat,
        json!({"content": "search", "web_search": {"enabled": false}}),
    )
    .await;
    assert!(ws_tool(&h.provider.chat_requests().pop().unwrap()).is_none());

    // model without web_search support: request accepted, tool not sent
    let std_chat = h.create_chat_as(&h.ctx(), json!({"model": "std"})).await;
    let r = h
        .send_body(
            std_chat,
            json!({"content": "search", "web_search": {"enabled": true}}),
        )
        .await;
    assert_eq!(r.names().last(), Some(&"done"));
    assert!(ws_tool(&h.provider.chat_requests().pop().unwrap()).is_none());
}

#[tokio::test]
async fn kill_switch_rejects_enabled_requests() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    let mut p = default_policy();
    p["kill_switches"] = json!({"disable_web_search": true});
    h.set_policy(p);
    let r = h
        .send_body(
            chat,
            json!({"content": "s", "web_search": {"enabled": true}}),
        )
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_violation(&r.error, "web_search");
    assert!(h.provider.chat_requests().is_empty());
    let r = h.send(chat, "plain").await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn calls_are_reported_and_accounted() {
    let h = Harness::new().await;
    let chat = h.create_chat().await;
    h.provider.push(Script::Ok {
        parts: vec!["r".into()],
        usage: (10, 10),
        before_done: ws_events(1),
        output: None,
    });
    let rid = Uuid::new_v4();
    let r = h
        .send_body(
            chat,
            json!({"content": "s", "request_id": rid, "web_search": {"enabled": true}}),
        )
        .await;
    let tools: Vec<&Value> = r
        .events
        .iter()
        .filter(|(n, _)| n == "tool")
        .map(|(_, v)| v)
        .collect();
    assert_eq!(tools.len(), 2);
    assert!(tools.iter().all(|t| t["name"] == "web_search"));
    let t = db::turns(&h, chat).await;
    assert_eq!(t[0].web_search_completed_count, 1);
    assert!(t[0].web_search_enabled);
    let rows = db::quota_rows(&h).await;
    assert_eq!(db::total_daily(&rows).web_search_calls, 1);
    assert!(h.eventually(|| h.published_for(rid).len() == 1).await);
    assert_eq!(h.published_for(rid)[0].web_search_calls, 1);
}

#[tokio::test]
async fn daily_quota_is_enforced() {
    let mut cfg = default_config();
    cfg["quota"] = json!({"web_search_daily_quota": 1});
    let h = Harness::with(Opts {
        config: cfg,
        ..Opts::default()
    })
    .await;
    let chat = h.create_chat().await;
    h.provider.push(Script::Ok {
        parts: vec!["r".into()],
        usage: (10, 10),
        before_done: ws_events(1),
        output: None,
    });
    h.send_body(
        chat,
        json!({"content": "s", "web_search": {"enabled": true}}),
    )
    .await;
    let calls = h.provider.chat_requests().len();
    let r = h
        .send_body(
            chat,
            json!({"content": "s", "web_search": {"enabled": true}}),
        )
        .await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "{r:?}");
    assert_violation(&r.error, "web_search");
    assert_eq!(h.provider.chat_requests().len(), calls);
    // without web search the user can still chat
    let r = h.send(chat, "plain").await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn per_message_limit_fails_the_turn() {
    let mut cfg = default_config();
    cfg["quota"] = json!({"web_search_max_calls_per_message": 1});
    let h = Harness::with(Opts {
        config: cfg,
        ..Opts::default()
    })
    .await;
    let chat = h.create_chat().await;
    h.provider.push(Script::Ok {
        parts: vec!["r".into()],
        usage: (10, 10),
        before_done: ws_events(2),
        output: None,
    });
    let rid = Uuid::new_v4();
    let r = h
        .send_body(
            chat,
            json!({"content": "s", "request_id": rid, "web_search": {"enabled": true}}),
        )
        .await;
    assert_eq!(r.names().last(), Some(&"error"), "{:?}", r.names());
    assert_eq!(
        r.first("error").unwrap()["code"],
        "web_search_calls_exceeded"
    );
    let t = h.wait_turn_terminal(chat).await;
    assert_eq!(t[0].state, "failed");
    assert_eq!(
        t[0].error_code.as_deref(),
        Some("web_search_calls_exceeded")
    );
    assert!(h.eventually(|| h.published_for(rid).len() == 1).await);
    let ev = &h.published_for(rid)[0];
    assert_eq!(ev.billing_outcome, "failed");
    assert_eq!(ev.settlement_method, "estimated");
}
