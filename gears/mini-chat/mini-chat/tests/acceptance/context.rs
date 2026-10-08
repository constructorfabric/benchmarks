//! Context assembly.

use serde_json::json;

use crate::common::*;

fn texts(req: &serde_json::Value) -> Vec<(String, String)> {
    req["input"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["role"].as_str().unwrap().to_owned(), m["content"][0]["text"].as_str().unwrap_or_default().to_owned()))
        .collect()
}

/// System prompt, thread summary and recent history are assembled deterministically within budget.
#[tokio::test]
async fn system_prompt_summary_history_assembled_within_budget() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, Some("standard-1")).await;
    h.provider.push(completed(&["A1"], 1, 1));
    h.send_message(U1, chat, json!({"content": "Q1"})).await;
    h.provider.push(completed(&["A2"], 1, 1));
    h.send_message(U1, chat, json!({"content": "Q2"})).await;
    h.send_message(U1, chat, json!({"content": "Q3"})).await;
    let req = h.provider.chat_requests().pop().unwrap();
    assert!(req["instructions"].as_str().unwrap().starts_with("You are standard-1."));
    assert_eq!(
        texts(&req),
        vec![
            ("user".into(), "Q1".into()),
            ("assistant".into(), "A1".into()),
            ("user".into(), "Q2".into()),
            ("assistant".into(), "A2".into()),
            ("user".into(), "Q3".into())
        ]
    );
    assert_eq!(req["temperature"], 0.5, "catalog api params applied");
    assert!(req.get("previous_response_id").is_none(), "history is sent explicitly");
    // A failed turn keeps its user message in history (no assistant reply);
    // deleted turns are excluded (DESIGN "Recent messages query").
    h.provider.push(Script::Http(500, json!({"error": {"message": "x"}}), vec![]));
    h.send_message(U1, chat, json!({"content": "QFAIL"})).await;
    h.send_message(U1, chat, json!({"content": "Q5"})).await;
    let req = h.provider.chat_requests().pop().unwrap();
    let t = texts(&req);
    let i = t.iter().position(|(_, x)| x == "QFAIL").unwrap();
    assert_eq!(t[i + 1], ("user".to_owned(), "Q5".to_owned()));
    // Deterministic: the same state assembles the same request.
    let rid = h.turns(chat).await.last().unwrap().request_id;
    h.call(U1, "POST", &format!("/chats/{chat}/turns/{rid}/retry"), None).await;
    let again = h.provider.chat_requests().pop().unwrap();
    assert_eq!(again["input"], req["input"]);
    assert_eq!(again["instructions"], req["instructions"]);

    // With a stored summary: preamble + summary first, summarized messages omitted.
    let h = Harness::with(Options {
        config: json!({"thread_summary_worker": {"summary_model_id": "standard-1"}}),
        ..Options::default()
    })
    .await;
    let chat = h.create_chat(U1, Some("tiny")).await;
    let big = "w".repeat(3000);
    for i in 0..4 {
        h.send_message(U1, chat, json!({"content": format!("turn{i} {big}")})).await;
    }
    // Truncation triggered a summary task; wait for it.
    let mut summary = None;
    for _ in 0..200 {
        summary = h.summary(chat).await;
        if summary.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let summary = summary.expect("thread summary created");
    assert_eq!(summary.summary_text, "Summary text");
    let r = h.send_message(U1, chat, json!({"content": "after summary"})).await;
    let started = r.event("stream_started").unwrap();
    assert!(started["thread_summary_applied"]["token_estimate"].as_i64().unwrap() > 0, "{started}");
    let req = h.provider.chat_requests().pop().unwrap();
    let t = texts(&req);
    assert_eq!(t[0].0, "user");
    assert!(t[0].1.starts_with("This conversation has earlier messages that have been summarized."));
    assert!(t[0].1.ends_with("Summary text"));
    assert_eq!(t.last().unwrap().1, "after summary");
    assert!(!t.iter().any(|(_, x)| x.starts_with("turn0")), "summarized messages omitted");
}

/// Tool availability and guidance are reflected in the assembled request.
#[tokio::test]
async fn tool_availability_and_guidance_reflected() {
    let h = Harness::new().await;
    let chat = h.create_chat(U1, None).await;
    h.send_message(U1, chat, json!({"content": "plain"})).await;
    let req = h.provider.chat_requests().pop().unwrap();
    assert!(req.get("tools").is_none() || req["tools"].as_array().unwrap().is_empty(), "{req}");
    assert_eq!(req["instructions"], "You are premium-1.");

    h.upload(U1, chat, "a.pdf", "application/pdf", b"%PDF").await;
    h.send_message(U1, chat, json!({"content": "docs", "web_search": {"enabled": true}})).await;
    let req = h.provider.chat_requests().pop().unwrap();
    let kinds: Vec<&str> = req["tools"].as_array().unwrap().iter().map(|t| t["type"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"file_search") && kinds.contains(&"web_search"), "{kinds:?}");
    let ins = req["instructions"].as_str().unwrap();
    assert!(ins.starts_with("You are premium-1."));
    assert!(ins.contains(mini_chat::config::DEFAULT_FILE_SEARCH_GUARD));
    assert!(ins.contains(mini_chat::config::DEFAULT_WEB_SEARCH_GUARD));

    // A model without tool support gets no tools and no guards.
    let nv = h.create_chat(U1, Some("novision")).await;
    h.upload(U1, nv, "b.pdf", "application/pdf", b"%PDF").await;
    h.send_message(U1, nv, json!({"content": "x", "web_search": {"enabled": true}})).await;
    let req = h.provider.chat_requests().pop().unwrap();
    assert!(req.get("tools").is_none() || req["tools"].as_array().unwrap().is_empty(), "{req}");
    assert_eq!(req["instructions"], "You are novision.");
    // Custom guards from config are used.
    let h = Harness::with(Options { config: json!({"context": {"web_search_guard": "CUSTOM GUARD"}}), ..Options::default() }).await;
    let chat = h.create_chat(U1, None).await;
    h.send_message(U1, chat, json!({"content": "x", "web_search": {"enabled": true}})).await;
    assert!(h.provider.chat_requests().pop().unwrap()["instructions"].as_str().unwrap().contains("CUSTOM GUARD"));
}
