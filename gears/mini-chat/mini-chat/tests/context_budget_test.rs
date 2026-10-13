//! Context assembly through the HTTP surface: system prompt, recent history
//! limit, whole-turn truncation within the token budget, mandatory items,
//! tool availability and guard instructions.
#![allow(clippy::unwrap_used)]

mod common;

use common::*;
use http::StatusCode;
use serde_json::{Value, json};

fn input_texts(call: &Value) -> Vec<String> {
    call["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let c = &m["content"];
            c.as_str().map_or_else(|| c.to_string(), ToOwned::to_owned)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn recent_messages_limit_applies() {
    let h = Harness::with(Options {
        config: json!({"context": {"recent_messages_limit": 2}}),
        ..Options::default()
    })
    .await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    for i in 0..3 {
        h.send(&a, chat, &format!("question {i}")).await;
    }
    let call = h.gw.chat_calls().pop().unwrap().json();
    let texts = input_texts(&call);
    assert_eq!(texts.len(), 3, "2 recent + current: {texts:?}");
    assert!(texts[0].contains("question 1"));
    assert!(texts[2].contains("question 2"));
}

#[tokio::test(flavor = "multi_thread")]
async fn oldest_whole_turns_are_dropped_to_fit_budget() {
    // input budget = context_window - max_output = 1600 - 1000 = 600 tokens
    let h = Harness::with(Options {
        catalog: vec![model(
            "small",
            "standard",
            json!({"context_window": 1600, "max_output_tokens": 1000, "max_input_tokens": 0,
                   "preference": {"is_default": true}, "system_prompt": "SYS"}),
        )],
        ..Options::default()
    })
    .await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let big = "y".repeat(600); // ~165 tokens each with margin
    for i in 0..4 {
        h.gw.push(text_reply(&[&format!("answer {i} {big}")], 1, 1));
        let s = h.send(&a, chat, &format!("q{i} {big}")).await;
        assert_eq!(s.names().last(), Some(&"done"), "{}", s.raw);
    }
    let call = h.gw.chat_calls().pop().unwrap().json();
    let input = call["input"].as_array().unwrap();
    let texts = input_texts(&call);
    // newest turns kept, oldest dropped, never an answer without its question
    assert!(texts.last().unwrap().contains("q3"));
    assert_eq!(input[0]["role"], "user", "history starts with a question: {texts:?}");
    assert!(!texts.iter().any(|t| t.contains("q0")), "oldest turn dropped: {texts:?}");
    assert!(texts.len() < 7);
    assert_eq!(call["instructions"], "SYS");
}

#[tokio::test(flavor = "multi_thread")]
async fn mandatory_context_over_budget_is_rejected() {
    let h = Harness::with(Options {
        catalog: vec![model(
            "small",
            "standard",
            json!({"context_window": 1200, "max_output_tokens": 1000, "max_input_tokens": 0,
                   "preference": {"is_default": true}}),
        )],
        ..Options::default()
    })
    .await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    let s = h.send(&a, chat, &"z".repeat(2000)).await;
    assert_eq!(s.status, StatusCode::BAD_REQUEST, "{}", s.raw);
    assert_eq!(s.problem["title"], "Out of Range");
    assert_eq!(s.problem["context"]["field_violations"][0]["reason"], "CONTEXT_BUDGET_EXCEEDED");
    assert!(h.gw.chat_calls().is_empty());
    assert_eq!(h.scalar("SELECT COUNT(*) FROM chat_turns").await, 0, "no turn on preflight rejection");
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_and_guidance_follow_chat_state() {
    let h = Harness::new().await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    // No documents, no web search → no tools.
    h.send(&a, chat, "plain").await;
    let call = h.gw.chat_calls().pop().unwrap().json();
    assert!(call.get("tools").is_none_or(|t| t.as_array().unwrap().is_empty()));
    let guards = h.svc.config().context.clone();
    assert!(!call["instructions"].as_str().unwrap().contains(&guards.file_search_guard));

    // Web search requested → web_search tool + guard.
    h.send_body(&a, chat, json!({"content": "news?", "web_search": {"enabled": true}})).await;
    let call = h.gw.chat_calls().pop().unwrap().json();
    let tools = call["tools"].as_array().unwrap();
    assert!(tools.iter().any(|t| t["type"] == "web_search"), "{call}");
    assert!(call["instructions"].as_str().unwrap().contains(&guards.web_search_guard));
    assert_eq!(call["metadata"]["feature"], "web_search");

    // A ready document in the chat → file_search over the chat vector store + guard.
    let up = h.upload(&a, chat, "doc.pdf", "application/pdf", b"%PDF-1.4 x").await;
    assert_eq!(up.status, StatusCode::CREATED, "{}", up.text);
    h.send(&a, chat, "what does the doc say?").await;
    let call = h.gw.chat_calls().pop().unwrap().json();
    let tools = call["tools"].as_array().unwrap();
    let fs = tools.iter().find(|t| t["type"] == "file_search").expect("file_search tool");
    let vs = h
        .rows(&format!("SELECT vector_store_id FROM chat_vector_stores WHERE chat_id = {}", blob(chat)))
        .await;
    assert_eq!(fs["vector_store_ids"], json!([vs[0][0].clone().unwrap()]));
    assert_eq!(fs["max_num_results"], 5);
    assert!(call["instructions"].as_str().unwrap().contains(&guards.file_search_guard));
    assert_eq!(call["metadata"]["feature"], "file_search");

    // file_search kill switch removes the tool
    h.policy.update(|s| s.kill_switches.disable_file_search = true);
    h.send(&a, chat, "again").await;
    let call = h.gw.chat_calls().pop().unwrap().json();
    assert!(!call.to_string().contains("file_search\""), "{call}");
}

#[tokio::test(flavor = "multi_thread")]
async fn model_without_tool_support_gets_no_tools() {
    let h = Harness::with(Options {
        catalog: vec![model(
            "plain",
            "standard",
            json!({"preference": {"is_default": true},
                   "general_config": {"tool_support": {"web_search": false, "file_search": false, "code_interpreter": false}}}),
        )],
        ..Options::default()
    })
    .await;
    let a = user_a();
    let chat = h.create_chat(&a).await;
    h.upload(&a, chat, "doc.pdf", "application/pdf", b"%PDF-1.4 x").await;
    let s = h.send_body(&a, chat, json!({"content": "q", "web_search": {"enabled": true}})).await;
    assert_eq!(s.names().last(), Some(&"done"), "{}", s.raw);
    let call = h.gw.chat_calls().pop().unwrap().json();
    assert!(call.get("tools").is_none_or(|t| t.as_array().unwrap().is_empty()), "{call}");
}
