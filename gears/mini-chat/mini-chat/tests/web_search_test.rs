//! Web search: tool events, citations, per-turn call limit, daily quota,
//! kill switch, accounting; file-search citations map to attachment ids.

mod common;

use common::*;
use http::StatusCode;
use serde_json::{Value, json};

fn ws_start() -> String {
    frame("response.web_search_call.searching", json!({"item_id": "ws_1"}))
}
fn ws_done() -> String {
    frame("response.web_search_call.completed", json!({"item_id": "ws_1"}))
}

#[tokio::test(flavor = "multi_thread")]
async fn web_search_tool_events_citations_and_accounting() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let text = "Rust 1.0 was released in 2015.";
    h.gw.push(Reply::Sse(vec![
        created_frame(),
        ws_start(),
        ws_done(),
        delta_frame(text),
        frame(
            "response.output_text.annotation.added",
            json!({"output_index": 0, "content_index": 0, "annotation": {"type": "url_citation",
                "url": "https://blog.rust-lang.org/2015/05/15/Rust-1.0.html", "title": "Announcing Rust 1.0",
                "start_index": 0, "end_index": 8}}),
        ),
        completed_frame(50, 10),
    ]));
    let s = h
        .send_body(&a, chat, json!({"content": "when was rust 1.0?", "web_search": {"enabled": true}}))
        .await;
    assert_eq!(s.status, StatusCode::OK);
    let names = s.names();
    assert_eq!(names, vec!["stream_started", "tool", "tool", "delta", "citations", "done"], "{}", s.raw);
    let tools: Vec<&Value> = s.events.iter().filter(|(n, _)| n == "tool").map(|(_, v)| v).collect();
    assert_eq!(tools[0]["phase"], "start");
    assert_eq!(tools[0]["name"], "web_search");
    assert!(tools[0]["details"].is_object());
    assert_eq!(tools[1]["phase"], "done");
    let cit = &s.first("citations").unwrap()["items"][0];
    assert_eq!(cit["source"], "web");
    assert_eq!(cit["url"], "https://blog.rust-lang.org/2015/05/15/Rust-1.0.html");
    assert_eq!(cit["title"], "Announcing Rust 1.0");
    assert_eq!(cit["snippet"], "Rust 1.0");
    assert_eq!(cit["span"], json!({"start": 0, "end": 8}));

    let call = h.gw.chat_calls().pop().unwrap().json();
    let tools = call["tools"].as_array().unwrap();
    let ws = tools.iter().find(|t| t["type"] == "web_search").unwrap();
    assert_eq!(ws["search_context_size"], "low");
    assert_eq!(call["max_tool_calls"], 2);

    let row = h
        .rows(&format!(
            "SELECT CAST(web_search_completed_count AS TEXT) FROM chat_turns WHERE request_id = {}",
            blob(s.request_id())
        ))
        .await;
    assert_eq!(row[0][0].as_deref(), Some("1"));
    let ws_calls = h
        .rows(&format!(
            "SELECT CAST(web_search_calls AS TEXT) FROM quota_usage WHERE user_id = {} AND bucket = 'total' AND period_type = 'daily'",
            blob(USER_A)
        ))
        .await;
    assert_eq!(ws_calls[0][0].as_deref(), Some("1"));
    h.drain_outbox().await;
    let ev = h.policy.published.lock().clone();
    assert_eq!(ev[0].web_search_calls, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn per_turn_web_search_limit_fails_turn() {
    let h = Harness::with(Options {
        config: json!({"quota": {"web_search_max_calls_per_message": 1}}),
        ..Options::default()
    })
    .await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    h.gw.push(Reply::Sse(vec![
        created_frame(),
        ws_start(),
        ws_done(),
        frame("response.web_search_call.searching", json!({"item_id": "ws_2"})),
        delta_frame("never"),
        completed_frame(1, 1),
    ]));
    let s = h
        .send_body(&a, chat, json!({"content": "search a lot", "web_search": {"enabled": true}}))
        .await;
    assert_eq!(s.names().last(), Some(&"error"), "{}", s.raw);
    assert_eq!(s.first("error").unwrap()["code"], "web_search_calls_exceeded");
    let t = h.turn_row(s.request_id()).await;
    assert_eq!(t[0].as_deref(), Some("failed"));
    assert_eq!(t[1].as_deref(), Some("web_search_calls_exceeded"));
}

#[tokio::test(flavor = "multi_thread")]
async fn daily_web_search_quota_rejects_before_stream() {
    let h = Harness::with(Options {
        config: json!({"quota": {"web_search_daily_quota": 1}}),
        ..Options::default()
    })
    .await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    h.gw.push(Reply::Sse(vec![created_frame(), ws_start(), ws_done(), delta_frame("a"), completed_frame(1, 1)]));
    let s = h.send_body(&a, chat, json!({"content": "one", "web_search": {"enabled": true}})).await;
    assert_eq!(s.names().last(), Some(&"done"));
    let s = h.send_body(&a, chat, json!({"content": "two", "web_search": {"enabled": true}})).await;
    assert_eq!(s.status, StatusCode::TOO_MANY_REQUESTS, "{}", s.raw);
    assert_eq!(s.problem["context"]["violations"][0]["subject"], "web_search");
    assert_eq!(s.problem["context"]["violations"][0]["description"], "quota_exceeded");
    // without web search the turn is allowed
    let s = h.send(&a, chat, "three").await;
    assert_eq!(s.names().last(), Some(&"done"));
}

#[tokio::test(flavor = "multi_thread")]
async fn web_search_kill_switch_rejects() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    h.policy.update(|s| s.kill_switches.disable_web_search = true);
    let s = h.send_body(&a, chat, json!({"content": "q", "web_search": {"enabled": true}})).await;
    assert_eq!(s.status, StatusCode::BAD_REQUEST);
    assert_eq!(s.problem["title"], "Failed Precondition");
    assert_eq!(s.problem["context"]["violations"][0]["subject"], "web_search");
    assert_eq!(s.problem["context"]["violations"][0]["type"], "FEATURE_DISABLED");
    assert!(h.gw.chat_calls().is_empty());
    let s = h.send_body(&a, chat, json!({"content": "q", "web_search": {"enabled": false}})).await;
    assert_eq!(s.names().last(), Some(&"done"));
}

#[tokio::test(flavor = "multi_thread")]
async fn file_search_citations_map_to_attachment_ids() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let up = h.upload(&a, chat, "manual.pdf", "application/pdf", b"%PDF").await;
    let att = up.body["id"].as_str().unwrap().to_owned();
    let file_id = h
        .rows(&format!("SELECT provider_file_id FROM attachments WHERE chat_id = {}", blob(chat)))
        .await[0][0]
        .clone()
        .unwrap();
    h.gw.push(Reply::Sse(vec![
        created_frame(),
        frame("response.file_search_call.searching", json!({})),
        frame("response.file_search_call.completed", json!({"results": [{}, {}]})),
        delta_frame("See the manual."),
        frame(
            "response.output_text.annotation.added",
            json!({"output_index": 0, "content_index": 0,
                   "annotation": {"type": "file_citation", "file_id": file_id, "filename": "x.pdf", "index": 3}}),
        ),
        completed_frame(5, 5),
    ]));
    let s = h.send(&a, chat, "what does the manual say?").await;
    assert_eq!(s.names(), vec!["stream_started", "tool", "tool", "delta", "citations", "done"], "{}", s.raw);
    let cit = &s.first("citations").unwrap()["items"][0];
    assert_eq!(cit["source"], "file");
    assert_eq!(cit["attachment_id"], att.as_str());
    assert_eq!(cit["title"], "manual.pdf");
    assert!(!s.raw.contains(&file_id), "provider file id never exposed");
    let tool_done = s.events.iter().filter(|(n, _)| n == "tool").nth(1).unwrap();
    assert_eq!(tool_done.1["name"], "file_search");
    assert_eq!(tool_done.1["details"]["files_searched"], 2);
}
