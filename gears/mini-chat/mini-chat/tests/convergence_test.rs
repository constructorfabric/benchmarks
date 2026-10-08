//! Convergence coverage: code-interpreter tool exposure and limits, the
//! per-model upload size cap, `$select` handling, quota periods without a
//! limit, and thread-summary dead-lettering.

#![allow(clippy::many_single_char_names, clippy::unwrap_used, clippy::expect_used, clippy::non_ascii_literal)]

mod common;

use common::*;
use http::StatusCode;
use serde_json::json;
use toolkit_db::outbox::{DeadLetterFilter, DeadLetterScope};

const XLSX: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

fn ci_start() -> String {
    frame("response.code_interpreter_call.in_progress", json!({"item_id": "ci_1"}))
}

fn ci_done() -> String {
    frame(
        "response.output_item.done",
        json!({"item": {"type": "code_interpreter_call", "outputs": [{"type": "logs", "logs": "42"}]}}),
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn ready_xlsx_exposes_code_interpreter_with_file_ids() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let up = h.upload(&a, chat, "data.xlsx", XLSX, b"PK\x03\x04").await;
    assert_eq!(up.status, StatusCode::CREATED, "{}", up.text);
    let file_id = h
        .rows(&format!("SELECT provider_file_id FROM attachments WHERE chat_id = {}", blob(chat)))
        .await[0][0]
        .clone()
        .unwrap();
    h.gw.push(Reply::Sse(vec![created_frame(), ci_start(), ci_done(), delta_frame("The sum is 42."), completed_frame(5, 5)]));
    let s = h.send(&a, chat, "sum column A").await;
    assert_eq!(s.names(), vec!["stream_started", "tool", "tool", "delta", "done"], "{}", s.raw);
    let tools: Vec<_> = s.events.iter().filter(|(n, _)| n == "tool").map(|(_, v)| v.clone()).collect();
    assert_eq!(tools[0]["name"], "code_interpreter");
    assert_eq!(tools[1]["phase"], "done");
    assert_eq!(tools[1]["details"]["output"], "42");
    let call = h.gw.chat_calls().pop().unwrap().json();
    let ci = call["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["type"] == "code_interpreter")
        .unwrap_or_else(|| panic!("no code_interpreter tool: {call}"))
        .clone();
    assert_eq!(ci["container"]["file_ids"], json!([file_id]));
    assert_eq!(call["include"], json!(["code_interpreter_call.outputs"]));
    assert!(!call.to_string().contains("\"file_search\""), "xlsx is not indexed for file_search");
    let row = h
        .rows(&format!(
            "SELECT CAST(code_interpreter_completed_count AS TEXT) FROM chat_turns WHERE request_id = {}",
            blob(s.request_id())
        ))
        .await;
    assert_eq!(row[0][0].as_deref(), Some("1"));
    // the kill switch removes the tool
    h.policy.update(|s| s.kill_switches.disable_code_interpreter = true);
    h.send(&a, chat, "again").await;
    let call = h.gw.chat_calls().pop().unwrap().json();
    assert!(!call.to_string().contains("code_interpreter\""), "{call}");
}

#[tokio::test(flavor = "multi_thread")]
async fn code_interpreter_daily_quota_and_per_turn_limit() {
    let h = Harness::with(Options {
        config: json!({"quota": {"code_interpreter_daily_quota": 1, "code_interpreter_max_calls_per_message": 1}}),
        ..Options::default()
    })
    .await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    h.upload(&a, chat, "data.xlsx", XLSX, b"PK\x03\x04").await;
    // two calls in one turn exceed the per-turn limit
    h.gw.push(Reply::Sse(vec![created_frame(), ci_start(), ci_done(), ci_start(), completed_frame(1, 1)]));
    let s = h.send(&a, chat, "loop").await;
    assert_eq!(s.names().last(), Some(&"error"), "{}", s.raw);
    assert_eq!(s.first("error").unwrap()["code"], "code_interpreter_calls_exceeded");
    assert_eq!(h.turn_row(s.request_id()).await[1].as_deref(), Some("code_interpreter_calls_exceeded"));
    // the completed call counts toward the daily quota → the next turn is rejected before the stream
    let s = h.send(&a, chat, "again").await;
    assert_eq!(s.status, StatusCode::TOO_MANY_REQUESTS, "{}", s.raw);
    assert_eq!(s.problem["context"]["violations"][0]["subject"], "code_interpreter");
    assert_eq!(s.problem["context"]["violations"][0]["description"], "quota_exceeded");
}

#[tokio::test(flavor = "multi_thread")]
async fn model_max_file_size_caps_uploads() {
    let h = Harness::with(Options {
        catalog: vec![model(
            "tiny-files",
            "standard",
            json!({"preference": {"is_default": true},
                   "general_config": {"max_file_size_mb": 1, "tool_support": {"file_search": true}}}),
        )],
        ..Options::default()
    })
    .await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    // gear limit is 25 MiB, the model allows 1 MiB
    let r = h.upload(&a, chat, "big.txt", "text/plain", &vec![b'x'; 1024 * 1024 + 1]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.text);
    assert_eq!(r.body["context"]["field_violations"][0]["reason"], "FILE_TOO_LARGE");
    let r = h.upload(&a, chat, "ok.txt", "text/plain", &vec![b'x'; 1024 * 1024]).await;
    assert_eq!(r.status, StatusCode::CREATED);
}

#[tokio::test(flavor = "multi_thread")]
async fn select_is_accepted_and_ignored() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    h.send(&a, chat, "hi").await;
    let r = h.get("/mini-chat/v1/chats?$select=id", &a).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    let item = &r.body["items"][0];
    assert!(item.get("model").is_some() && item.get("message_count").is_some(), "full item: {item}");
    let r = h.get(&format!("/mini-chat/v1/chats/{chat}/messages?$select=id,role"), &a).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.text);
    assert!(r.body["items"][0].get("content").is_some());
    // malformed $select is still validated by the extractor
    let r = h.get("/mini-chat/v1/chats?$select=", &a).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.text);
    assert_eq!(r.body["context"]["field_violations"][0]["reason"], "INVALID_SELECT");
}

#[tokio::test(flavor = "multi_thread")]
async fn periods_without_limit_are_omitted() {
    let h = Harness::new().await;
    let a = user_a();
    // total monthly has no limit → omitted from the status
    h.policy.set_limits((100_000_000, 0), (50_000_000, 500_000_000));
    let r = h.get("/mini-chat/v1/quota/status", &a).await;
    assert_eq!(r.status, StatusCode::OK);
    let tiers = r.body["tiers"].as_array().unwrap();
    let total = tiers.iter().find(|t| t["tier"] == "total").expect("total tier");
    let periods: Vec<&str> = total["periods"].as_array().unwrap().iter().map(|p| p["period"].as_str().unwrap()).collect();
    assert_eq!(periods, vec!["daily"]);

    // premium periods without limits are omitted from the status and from done.quota_warnings
    h.policy.set_limits((100_000_000, 1_000_000_000), (0, 0));
    let r = h.get("/mini-chat/v1/quota/status", &a).await;
    let tiers = r.body["tiers"].as_array().unwrap();
    if let Some(p) = tiers.iter().find(|t| t["tier"] == "premium") {
        assert!(p["periods"].as_array().unwrap().is_empty(), "{p}");
    }
    let chat = h.create_chat_with(&a, json!({"model": "gpt-standard"})).await;
    let chat = uuid::Uuid::parse_str(chat.body["id"].as_str().unwrap()).unwrap();
    let s = h.send(&a, chat, "hi").await;
    let done = s.first("done").unwrap_or_else(|| panic!("{}", s.raw));
    let warnings = done["quota_warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 2, "{done}");
    assert!(warnings.iter().all(|w| w["tier"] == "total"));
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_summary_is_dead_lettered_after_max_attempts() {
    let h = Harness::with(Options {
        catalog: vec![
            model(
                "chat-model",
                "standard",
                json!({"context_window": 3000, "max_output_tokens": 1000, "max_input_tokens": 0,
                       "preference": {"is_default": true}}),
            ),
            model("summarizer", "standard", json!({})),
        ],
        config: json!({"thread_summary_worker": {"summary_model_id": "summarizer", "compression_threshold_pct": 10,
                                                 "max_attempts": 2}}),
        ..Options::default()
    })
    .await;
    for _ in 0..10 {
        h.gw.push_summary(Reply::Status(500, json!({"error": {"message": "down"}})));
    }
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let long = "words ".repeat(150);
    h.send(&a, chat, &format!("first {long}")).await;
    h.send(&a, chat, &format!("second {long}")).await;
    let outbox = std::sync::Arc::clone(h.outbox.as_ref().unwrap().outbox());
    let conn = h.svc.db().conn().unwrap();
    let dead = h
        .eventually("summary dead-lettered", || async {
            let list = outbox
                .dead_letter_list(&conn, &DeadLetterFilter::from_scope(DeadLetterScope::default().queue("mini-chat.thread_summary")))
                .await
                .unwrap();
            (!list.is_empty()).then_some(list)
        })
        .await;
    assert_eq!(dead.len(), 1);
    assert_eq!(h.gw.summary_calls().len(), 2, "max_attempts calls, then rejected");
    assert_eq!(h.scalar(&format!("SELECT COUNT(*) FROM thread_summaries WHERE chat_id = {}", blob(chat))).await, 0);
    // the chat keeps working with the uncompressed history
    let s = h.send(&a, chat, "third").await;
    assert_eq!(s.names().last(), Some(&"done"));
}
