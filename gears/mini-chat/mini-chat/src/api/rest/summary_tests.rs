//! End-to-end thread summary: trigger at finalization, outbox handler, summary
//! row + compressed messages, system usage event, summary preamble in the next
//! request, empty-summary retry (acceptance: Context Assembly, Cleanup &
//! Recovery "thread summary generation, failure/retry").

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::json;

use crate::test_support::{EnvOptions, TestEnv, json_resp};

fn summary_env() -> EnvOptions {
    EnvOptions {
        config: json!({"thread_summary_worker": {"summary_model_id": "gpt-4.1-mini"}}),
        ..Default::default()
    }
}

#[tokio::test]
async fn long_chat_gets_summarized_and_summary_is_used() {
    let env = TestEnv::with(summary_env()).await;
    let chat = env.chat(Some("tiny-ctx")).await;
    let long = "c".repeat(2_400);
    for i in 0..4 {
        let r = env.send_msg(&chat, &format!("marker{i}x {long}")).await;
        assert_eq!(r.event_names().last().unwrap(), "done", "{}", r.text());
    }
    for _ in 0..200 {
        if env.count("SELECT COUNT(*) FROM thread_summaries").await == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(env.count("SELECT COUNT(*) FROM thread_summaries").await, 1);
    let text = env.sql_rows("SELECT summary_text FROM thread_summaries").await[0]
        .try_get_by_index::<String>(0)
        .unwrap();
    assert_eq!(text, "Summary of chat");
    assert!(env.count("SELECT COUNT(*) FROM messages WHERE is_compressed = 1").await > 0);
    // the summary request: non-streaming, summary model, system task metadata
    let sreq = env
        .proxy
        .chat_requests()
        .into_iter()
        .map(|r| r.json())
        .find(|b| b["stream"] == json!(false))
        .expect("summary request");
    assert_eq!(sreq["model"], "gpt-4.1-mini");
    assert_eq!(sreq["metadata"]["request_type"], "summary");
    assert_eq!(sreq["metadata"]["feature"], "none");
    // system usage event, not charged to the user
    env.eventually("system usage", |e| e.usage_events().iter().any(|u| u.requester_type == "system")).await;
    let u = env.usage_events().into_iter().find(|u| u.requester_type == "system").unwrap();
    assert!(u.user_id.is_none());
    assert_eq!(u.system_task_type.as_deref(), Some("thread_summary_update"));
    assert_eq!(u.billing_outcome, "system_task");
    assert!(u.turn_id.is_none());
    // the next request carries the preamble + summary and no compressed messages
    let r = env.send_msg(&chat, "short follow-up").await;
    assert_eq!(r.event_names().last().unwrap(), "done");
    let stored = env.count("SELECT token_estimate FROM thread_summaries").await;
    assert_eq!(stored, 20, "output_tokens of the summary call");
    let started = r.event("stream_started").unwrap();
    assert_eq!(started["thread_summary_applied"], json!({"token_estimate": stored}));
    let last = env.proxy.chat_requests().last().unwrap().json();
    let input = last["input"].as_array().unwrap();
    assert_eq!(input[0]["role"], "user", "summary is a user-role message");
    let first = input[0]["content"].as_str().unwrap();
    assert!(first.starts_with(crate::domain::context::SUMMARY_PREAMBLE), "{first}");
    assert!(first.ends_with("Summary of chat"));
    let compressed: Vec<String> = env
        .sql_rows("SELECT content FROM messages WHERE is_compressed = 1 AND role = 'user'")
        .await
        .iter()
        .map(|r| r.try_get_by_index::<String>(0).unwrap())
        .collect();
    assert!(!compressed.is_empty());
    let sent = last["input"].to_string();
    for c in &compressed {
        assert!(!sent.contains(&c[..8]), "compressed message {} was sent", &c[..8]);
    }
    // system tasks never create chat_turns rows
    let turns = env.count("SELECT COUNT(*) FROM chat_turns").await;
    assert_eq!(turns, 5);
}

#[tokio::test]
async fn empty_summary_is_retried_then_dead_lettered() {
    let env = TestEnv::with(EnvOptions {
        config: json!({"thread_summary_worker": {"summary_model_id": "gpt-4.1-mini", "max_attempts": 2}}),
        ..Default::default()
    })
    .await;
    env.proxy.respond(|r| {
        (r.uri.contains("/responses") && r.json()["stream"] == json!(false)).then(|| {
            json_resp(200, &json!({"output": [{"type": "message", "content": [{"type": "output_text", "text": "   "}]}], "usage": {"input_tokens": 5, "output_tokens": 0}}))
        })
    });
    let chat = env.chat(Some("tiny-ctx")).await;
    let long = "d".repeat(2_400);
    for _ in 0..4 {
        env.send_msg(&chat, &long).await;
    }
    for _ in 0..300 {
        let n = env
            .proxy
            .chat_requests()
            .iter()
            .filter(|r| r.json()["stream"] == json!(false))
            .count();
        if n >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    env.settle().await;
    assert_eq!(env.count("SELECT COUNT(*) FROM thread_summaries").await, 0, "no empty summary stored");
    assert_eq!(env.count("SELECT COUNT(*) FROM messages WHERE is_compressed = 1").await, 0);
    let attempts = env
        .proxy
        .chat_requests()
        .iter()
        .filter(|r| r.json()["stream"] == json!(false))
        .count();
    assert!(attempts >= 2, "retried: {attempts}");
}
