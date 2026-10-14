//! A turn that enables web search, over the real router, SQLite, the real quota service, the real
//! provider task and a mock OAGW: the request body `web_search` equal to `{"enabled": true}` is
//! accepted, the quota preflight applies the kill switch and the daily web search quota only for a
//! request that enables web search (the quota only on a model that supports it), and the stream of
//! such a turn ends with `done` or with the terminal `error`.
//!
//! Slice S3-completion of `cpt-cf-mini-chat-feature-message-streaming`: flow
//! `cpt-cf-mini-chat-flow-message-streaming-web-search-turn` (`inst-web-request`,
//! `inst-web-send-flow`, `inst-web-return`); definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-tools-web-search`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::test_support::chats::assert_problem;
use crate::test_support::provider::{
    MockProvider, Reply, completed, delta, happy_steps, web_done, web_start,
};
use crate::test_support::quota::{row, seed_rows};
use crate::test_support::send::{
    SendRig, SendRigOptions, WEB_GUARD, catalog_of, names, parse_sse, prem_model, send_rig_with,
    std_model, turn_rows, with_web_search,
};

const DAY: &str = "2026-02-28";

fn web(enabled: bool) -> Value {
    json!({ "content": "what is new?", "web_search": { "enabled": enabled } })
}

/// A rig whose standard model supports web search or not, with an optional kill switch and an
/// exhausted daily web search quota.
async fn rig_for(
    supports: bool,
    kill_switch: bool,
    exhausted: bool,
    upstream: &std::sync::Arc<MockProvider>,
) -> SendRig {
    let mut snapshot = catalog_of(vec![
        prem_model(&json!({})),
        with_web_search(&std_model(&json!({})), supports),
    ]);
    snapshot.kill_switches.disable_web_search = kill_switch;
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_snapshot(snapshot)
            .with_upstream(upstream.clone()),
    )
    .await;
    if exhausted {
        seed_rows(&rig.quota.db, &[row("daily", DAY, "total").web(75)]).await;
    }
    rig
}

fn has_web_tool(body: &Value) -> bool {
    body["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|tool| tool["type"] == "web_search"))
}

// ── inst-web-request ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_request_with_web_search_enabled_true_is_accepted_and_reaches_the_provider() {
    let upstream = MockProvider::answering(Reply::Sse(happy_steps(&["hi"], (3, 2))));
    let rig = rig_for(true, false, false, &upstream).await;
    let chat = rig.std_chat().await;
    let reply = rig.stream(chat.id, &web(true)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert!(reply.content_type.starts_with("text/event-stream"));
    assert_eq!(
        names(&parse_sse(&reply.text)).last(),
        Some(&"done"),
        "{}",
        reply.text
    );
    let call = upstream.only_call();
    assert!(has_web_tool(&call.body), "{}", call.body);
    assert!(
        call.body["instructions"]
            .as_str()
            .unwrap()
            .ends_with(WEB_GUARD),
        "the guard ends the instructions"
    );
    assert!(turn_rows(&rig.quota.db, chat.id).await[0].web_search_enabled);
}

#[tokio::test]
async fn an_absent_null_or_false_web_search_sends_no_web_tool_and_stores_false() {
    for body in [
        json!({ "content": "q" }),
        json!({ "content": "q", "web_search": null }),
        json!({ "content": "q", "web_search": { "enabled": false } }),
    ] {
        let upstream = MockProvider::answering(Reply::Sse(happy_steps(&["hi"], (3, 2))));
        let rig = rig_for(true, false, false, &upstream).await;
        let chat = rig.std_chat().await;
        let reply = rig.stream(chat.id, &body).await;
        assert_eq!(reply.status, StatusCode::OK, "{body}: {}", reply.text);
        assert!(!has_web_tool(&upstream.only_call().body), "{body}");
        assert!(!turn_rows(&rig.quota.db, chat.id).await[0].web_search_enabled);
    }
}

// ── inst-web-send-flow ───────────────────────────────────────────────────────

#[tokio::test]
async fn the_kill_switch_and_the_daily_quota_apply_only_to_a_request_that_enables_web_search() {
    // both active: a request without web search is untouched by either
    for body in [json!({ "content": "q" }), web(false)] {
        let upstream = MockProvider::answering(Reply::Sse(happy_steps(&["hi"], (3, 2))));
        let rig = rig_for(true, true, true, &upstream).await;
        let chat = rig.std_chat().await;
        let reply = rig.stream(chat.id, &body).await;
        assert_eq!(reply.status, StatusCode::OK, "{body}: {}", reply.text);
        assert_eq!(upstream.call_count(), 1);
    }
    // the kill switch rejects a web search request before any stream
    let upstream = MockProvider::new();
    let rig = rig_for(true, true, false, &upstream).await;
    let chat = rig.std_chat().await;
    let reply = rig.stream(chat.id, &web(true)).await;
    let problem = assert_problem(&reply, 400, "failed_precondition");
    assert_eq!(problem["context"]["violations"][0]["subject"], "web_search");
    assert_eq!(
        problem["context"]["violations"][0]["type"],
        "FEATURE_DISABLED"
    );
    assert_eq!(upstream.call_count(), 0);
    assert!(turn_rows(&rig.quota.db, chat.id).await.is_empty());
    // the exhausted daily quota rejects a web search request on a supporting model
    let upstream = MockProvider::new();
    let rig = rig_for(true, false, true, &upstream).await;
    let chat = rig.std_chat().await;
    let reply = rig.stream(chat.id, &web(true)).await;
    let problem = assert_problem(&reply, 429, "resource_exhausted");
    assert_eq!(problem["context"]["violations"][0]["subject"], "web_search");
    assert_eq!(upstream.call_count(), 0);
    assert!(turn_rows(&rig.quota.db, chat.id).await.is_empty());
}

#[tokio::test]
async fn the_daily_quota_is_not_checked_for_a_model_without_web_search_support() {
    let upstream = MockProvider::answering(Reply::Sse(happy_steps(&["plain"], (3, 2))));
    let rig = rig_for(false, false, true, &upstream).await;
    let chat = rig.std_chat().await;
    let reply = rig.stream(chat.id, &web(true)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    assert_eq!(names(&parse_sse(&reply.text)).last(), Some(&"done"));
    let call = upstream.only_call();
    assert!(!has_web_tool(&call.body), "{}", call.body);
    assert!(
        !call.body["instructions"]
            .as_str()
            .unwrap()
            .contains(WEB_GUARD)
    );
    // the requested flag is stored whether or not the tool was sent
    assert!(turn_rows(&rig.quota.db, chat.id).await[0].web_search_enabled);
}

#[tokio::test]
async fn the_daily_quota_below_its_limit_admits_a_web_search_request() {
    let upstream = MockProvider::answering(Reply::Sse(happy_steps(&["ok"], (3, 2))));
    let rig = rig_for(true, false, false, &upstream).await;
    seed_rows(&rig.quota.db, &[row("daily", DAY, "total").web(74)]).await;
    let chat = rig.std_chat().await;
    let reply = rig.stream(chat.id, &web(true)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
}

// ── inst-web-return ──────────────────────────────────────────────────────────

#[tokio::test]
async fn a_web_search_stream_ends_with_done_after_its_tool_events() {
    let steps = vec![
        web_start(),
        web_done(),
        delta("answer"),
        completed("answer", (10, 4)),
    ];
    let upstream = MockProvider::answering(Reply::Sse(steps));
    let rig = rig_for(true, false, false, &upstream).await;
    let chat = rig.std_chat().await;
    let reply = rig.stream(chat.id, &web(true)).await;
    let frames = parse_sse(&reply.text);
    assert_eq!(
        names(&frames),
        vec!["stream_started", "tool", "tool", "delta", "done"],
        "{}",
        reply.text
    );
    assert_eq!(frames.iter().filter(|f| f.event == "error").count(), 0);
}

#[tokio::test]
async fn a_web_search_stream_ends_with_the_terminal_error_and_nothing_after_it() {
    // more searches than `quota.web_search_max_calls_per_message`
    let upstream = MockProvider::answering(Reply::Sse(vec![
        web_start(),
        web_done(),
        web_start(),
        web_done(),
        web_start(),
        completed("x", (1, 1)),
    ]));
    let rig = rig_for(true, false, false, &upstream).await;
    let chat = rig.std_chat().await;
    let reply = rig.stream(chat.id, &web(true)).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text);
    let frames = parse_sse(&reply.text);
    let last = frames.last().unwrap();
    assert_eq!(last.event, "error");
    assert_eq!(last.data["code"], "web_search_calls_exceeded");
    assert_eq!(frames.iter().filter(|f| f.event == "done").count(), 0);
    assert_eq!(
        frames
            .iter()
            .filter(|f| f.event == "error" || f.event == "done")
            .count(),
        1
    );

    // a provider failure ends the web turn the same way
    let upstream =
        MockProvider::answering(Reply::http(500, &json!({ "error": { "message": "x" } })));
    let rig = rig_for(true, false, false, &upstream).await;
    let chat = rig.std_chat().await;
    let reply = rig.stream(chat.id, &web(true)).await;
    let frames = parse_sse(&reply.text);
    assert_eq!(names(&frames).last(), Some(&"error"), "{}", reply.text);
    assert_eq!(frames.last().unwrap().data["code"], "provider_error");
}
