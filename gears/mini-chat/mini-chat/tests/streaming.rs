//! US2: send message + SSE stream.
//!
//! AC: Streaming Send Message, SSE Event Contract, Settlement & Finalization, Context Assembly,
//! Principles (context budget, no buffering, quota before provider), Error Mapping & Sanitization.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::similar_names, clippy::many_single_char_names)]

mod common;
use std::time::{Duration, Instant};

use common::*;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use toolkit_db::secure::secure_insert;
use toolkit_security::AccessScope;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn happy_path_event_order_and_persistence() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let before = h.send(ALICE, "GET", &format!("/chats/{chat}"), None).await.json();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let rid = Uuid::new_v4();
    let (r, ev) = h.stream(ALICE, chat, json!({"content": "Hello there", "request_id": rid})).await;
    assert_eq!(r.status, 200, "{}", r.text());
    assert!(r.headers["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    assert_eq!(r.headers["cache-control"], "no-cache");

    let n = names(&ev);
    assert_eq!(n.first(), Some(&"stream_started"));
    assert_eq!(n.last(), Some(&"done"));
    assert!(n.iter().filter(|e| **e == "delta").count() >= 2, "{n:?}");
    let started = find(&ev, "stream_started");
    assert_eq!(started["request_id"], json!(rid.to_string()), "request id echoed");
    assert_eq!(started["is_new_turn"], json!(true));
    let text: String = ev.iter().filter(|(n, _)| n == "delta").map(|(_, d)| {
        assert_eq!(d["type"], json!("text"));
        d["content"].as_str().unwrap().to_owned()
    }).collect();
    assert_eq!(text, "Hello from the fake provider");

    let done = find(&ev, "done");
    assert_eq!(done["usage"], json!({"input_tokens": 42, "output_tokens": 7}));
    assert_eq!(done["effective_model"], json!("standard-m"));
    assert_eq!(done["selected_model"], json!("standard-m"));
    assert_eq!(done["quota_decision"], json!("allow"));
    assert!(done.get("downgrade_from").is_none());
    let done_s = done.to_string();
    assert!(!done_s.contains("prov-"), "no provider model id leak: {done_s}");
    assert!(!done_s.contains("resp_"), "no provider response id leak: {done_s}");

    // DB: messages, turn, quota.
    let msgs = h.messages(chat).await;
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, "user");
    assert_eq!(msgs[0].content, "Hello there");
    assert_eq!(msgs[1].role, "assistant");
    assert_eq!(msgs[1].content, "Hello from the fake provider");
    assert_eq!(msgs[1].id.to_string(), started["message_id"].as_str().unwrap());
    assert_eq!(msgs[0].request_id, Some(rid));
    assert_eq!(msgs[1].request_id, Some(rid));
    let turns = h.turns(chat).await;
    assert_eq!(turns.len(), 1);
    let t = &turns[0];
    assert_eq!(t.state, "completed");
    assert_eq!(t.request_id, rid);
    assert_eq!(t.assistant_message_id, Some(msgs[1].id));
    assert_eq!(t.provider_response_id.as_deref(), Some("resp_abc123"));
    assert_eq!(t.effective_model.as_deref(), Some("standard-m"));
    assert!(t.completed_at.is_some());
    // standard-m: in 1x, out 2x → ceil(42*1e6/1e6) + ceil(7*2e6/1e6) = 42 + 14
    let expected = mini_chat::domain::credits::credits_micro(42, 7, 1_000_000, 2_000_000).unwrap();
    let row = h.quota_row(ALICE, "total", "daily").await.expect("daily total row");
    assert_eq!(row.spent_credits_micro, expected);
    assert_eq!(row.reserved_credits_micro, 0, "reserve released after settlement");
    assert_eq!(row.input_tokens, 42);
    assert_eq!(row.output_tokens, 7);
    assert_eq!(row.calls, 1);
    assert!(h.quota_row(ALICE, "tier:premium", "daily").await.is_none_or(|r| r.spent_credits_micro == 0));

    // Chat activity bumps updated_at.
    let after = h.send(ALICE, "GET", &format!("/chats/{chat}"), None).await.json();
    assert_ne!(before["updated_at"], after["updated_at"]);
    assert_eq!(after["message_count"], json!(2));

    // Usage published exactly once; audit event emitted.
    h.eventually("usage event", || h.usage_events().len() == 1).await;
    let u = &h.usage_events()[0];
    assert_eq!(u.request_id, rid);
    assert_eq!(u.billing_outcome, "completed");
    assert_eq!(u.settlement_method, "actual");
    assert_eq!(u.terminal_state, "completed");
    assert_eq!(u.actual_credits_micro, expected);
    assert_eq!(u.user_id, Some(ALICE.user));
    h.eventually("audit event", || !h.audit_events().is_empty()).await;
    assert_eq!(h.audit_events()[0].event_type(), "turn_completed");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.usage_events().len(), 1, "published exactly once");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn request_id_is_generated_when_omitted() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, None).await;
    let ev = h.say(ALICE, chat, "hi").await;
    let rid = find(&ev, "stream_started")["request_id"].as_str().unwrap().to_owned();
    assert!(Uuid::parse_str(&rid).is_ok());
    assert_eq!(h.turns(chat).await[0].request_id.to_string(), rid);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_request_body() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.say(ALICE, chat, "first question").await;
    h.say(ALICE, chat, "second question").await;
    let reqs = h.provider.chat_requests();
    assert_eq!(reqs.len(), 2);
    let b = &reqs[1];
    assert_eq!(b["model"], json!("prov-standard-m"));
    assert_eq!(b["stream"], json!(true));
    assert!(b["instructions"].as_str().unwrap().starts_with("System prompt of standard-m."));
    assert_eq!(b["temperature"], json!(0.5), "api_params forwarded");
    assert!(b["max_output_tokens"].as_u64().unwrap() <= 1000);
    let user = b["user"].as_str().unwrap();
    assert_eq!(user.len(), 64);
    assert!(user.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(b["metadata"]["chat_id"], json!(chat.to_string()));
    assert_eq!(b["metadata"]["tenant_id"], json!(ALICE.tenant.to_string()));
    assert_eq!(b["metadata"]["user_id"], json!(ALICE.user.to_string()));
    assert_eq!(b["metadata"]["request_type"], json!("chat"));
    // history (user, assistant) + current
    let input = b["input"].as_array().unwrap();
    assert_eq!(input.len(), 3, "{input:?}");
    let texts: Vec<String> = input.iter().map(ToString::to_string).collect();
    assert!(texts[0].contains("first question"));
    assert_eq!(input[0]["role"], json!("user"));
    assert_eq!(input[1]["role"], json!("assistant"));
    assert!(texts[1].contains("Hello from the fake provider"));
    assert!(texts[2].contains("second question"));
    assert!(b.get("tools").is_none_or(|t| t.as_array().is_some_and(Vec::is_empty)), "no tools without gates");
    // Request went through the provider alias route.
    let rec = h.provider.recorded();
    assert!(rec[0].path.ends_with("/v1/responses"), "{}", rec[0].path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preflight_validation_never_calls_provider() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let cases = vec![
        (json!({"content": ""}), 400, "EMPTY_CONTENT"),
        (json!({"content": "   "}), 400, "EMPTY_CONTENT"),
        (json!({"content": "x", "attachment_ids": [Uuid::new_v4()]}), 400, ""),
    ];
    for (body, status, reason) in cases {
        let r = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(body.clone())).await;
        let p = r.problem(status);
        if !reason.is_empty() {
            assert_eq!(fv_reason(&p), reason, "{body}");
        }
    }
    // Duplicate attachment ids / too many ids are rejected too.
    let a = Uuid::new_v4();
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "x", "attachment_ids": [a, a]}))).await;
    r.problem(400);
    assert!(h.provider.chat_requests().is_empty());
    assert!(h.turns(chat).await.is_empty());
    assert!(h.messages(chat).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_too_long_and_context_budget_exceeded() {
    let mut cat = catalog();
    cat.as_array_mut().unwrap().push(model("bigprompt-m", "Standard", true, false, json!({
        "context_window": 900, "max_output_tokens": 100, "max_input_tokens": 600,
        "system_prompt": "s".repeat(2400),
        "estimation_budgets": {"bytes_per_token_conservative": 4, "fixed_overhead_tokens": 10, "safety_margin_pct": 0,
            "image_token_budget": 100, "tool_surcharge_tokens": 10, "web_search_surcharge_tokens": 10, "code_interpreter_surcharge_tokens": 10}
    })));
    let h = Harness::with(Opts { policy: policy_cfg(cat, json!({})), ..Default::default() }).await;
    let chat = h.chat(ALICE, Some("tiny-m")).await;
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "x".repeat(2600)}))).await;
    assert_eq!(fv_reason(&r.problem(400)), "INPUT_TOO_LONG");

    let chat2 = h.chat(ALICE, Some("bigprompt-m")).await;
    let r = h.send(ALICE, "POST", &format!("/chats/{chat2}/messages:stream"), Some(json!({"content": "hello"}))).await;
    assert_eq!(fv_reason(&r.problem(400)), "CONTEXT_BUDGET_EXCEEDED");
    assert!(h.provider.chat_requests().is_empty(), "no provider call");
    assert!(h.quota_rows(ALICE).await.iter().all(|r| r.reserved_credits_micro == 0 && r.spent_credits_micro == 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn history_is_truncated_deterministically_within_budget() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("tiny-m")).await;
    for i in 0..3 {
        h.say(ALICE, chat, &format!("{i}{}", "m".repeat(999))).await;
    }
    let reqs = h.provider.chat_requests();
    let last = reqs.last().unwrap()["input"].as_array().unwrap().clone();
    // u0 does not fit; the orphan assistant answer a0 is dropped as well → [u1, a1, u2]
    assert_eq!(last.len(), 3, "{last:?}");
    assert_eq!(last[0]["role"], json!("user"));
    assert!(last[0].to_string().contains("1mmm"));
    assert_eq!(last[1]["role"], json!("assistant"));
    assert!(last[2].to_string().contains("2mmm"));
    // Identical state → identical assembly.
    let turns = h.turns(chat).await;
    assert!(turns.iter().all(|t| t.state == "completed"), "{turns:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thread_summary_is_injected_with_preamble() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.say(ALICE, chat, "old question").await;
    let msgs = h.messages(chat).await;
    let frontier = msgs.last().unwrap();
    let ts = time::OffsetDateTime::now_utc();
    let am = ent::thread_summaries::ActiveModel {
        id: sea_orm::Set(Uuid::new_v4()),
        tenant_id: sea_orm::Set(ALICE.tenant),
        chat_id: sea_orm::Set(chat),
        summary_text: sea_orm::Set("The user asked an old question.".into()),
        summarized_up_to_created_at: sea_orm::Set(frontier.created_at),
        summarized_up_to_message_id: sea_orm::Set(frontier.id),
        token_estimate: sea_orm::Set(12),
        created_at: sea_orm::Set(ts),
        updated_at: sea_orm::Set(ts),
    };
    secure_insert::<ent::thread_summaries::Entity>(am, &AccessScope::allow_all(), &h.db.conn().unwrap()).await.unwrap();
    let ev = h.say(ALICE, chat, "new question").await;
    let started = find(&ev, "stream_started");
    assert!(started["thread_summary_applied"].is_object(), "{started}");
    let req = h.provider.chat_requests().pop().unwrap();
    let input = req["input"].as_array().unwrap();
    assert_eq!(input.len(), 2, "summary + current message only: {input:?}");
    let first = input[0].to_string();
    assert!(first.contains("The user asked an old question."));
    assert!(first.contains(mini_chat::domain::context::SUMMARY_PREAMBLE.split('\n').next().unwrap()));
    assert!(!req.to_string().contains("old question\""), "summarized messages are not resent verbatim");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_http_error_is_terminal_sanitized_error_event() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.provider.push(Reply::Status {
        status: 400,
        body: json!({"error": {"message": "Bad file file-abc123def456ghi789jkl at https://api.openai.com/v1/files with key sk-abcdefghijklmnopqrstuvwx"}}),
        retry_after: None,
    });
    let ev = h.say(ALICE, chat, "hi").await;
    assert_eq!(names(&ev), vec!["stream_started", "error"]);
    let err = find(&ev, "error");
    assert_eq!(err["code"], json!("provider_error"));
    let msg = err["message"].as_str().unwrap();
    assert!(!msg.contains("file-abc123"), "{msg}");
    assert!(!msg.contains("api.openai.com"), "{msg}");
    assert!(!msg.contains("sk-abcdef"), "{msg}");
    let t = &h.turns(chat).await[0];
    assert_eq!(t.state, "failed");
    assert_eq!(t.error_code.as_deref(), Some("provider_error"));
    assert!(t.assistant_message_id.is_none(), "no assistant message on failure");
    let row = h.quota_row(ALICE, "total", "daily").await.unwrap();
    assert_eq!(row.reserved_credits_micro, 0);
    assert!(row.spent_credits_micro > 0, "estimated settlement charged");
    h.eventually("usage", || h.usage_events().len() == 1).await;
    assert_eq!(h.usage_events()[0].billing_outcome, "failed");
    assert_eq!(h.usage_events()[0].settlement_method, "estimated");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_rate_limit_and_timeout_codes() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.provider.push(Reply::Status { status: 429, body: json!({"error": {"message": "slow down"}}), retry_after: Some(7) });
    let ev = h.say(ALICE, chat, "a").await;
    assert_eq!(find(&ev, "error")["code"], json!("rate_limited"));
    h.provider.push(Reply::Gateway(mini_chat::api::rest::error::ChatResource::deadline_exceeded("upstream timeout").create()));
    let ev = h.say(ALICE, chat, "b").await;
    assert_eq!(find(&ev, "error")["code"], json!("provider_timeout"));
    // The gateway's own 504 deadline_exceeded Problem is a timeout ...
    h.provider.push(Reply::Status {
        status: 504,
        body: json!({"type": "gts://gts.cf.core.errors.err.v1~cf.core.err.deadline_exceeded.v1~", "status": 504, "title": "Deadline Exceeded"}),
        retry_after: None,
    });
    let ev = h.say(ALICE, chat, "c").await;
    assert_eq!(find(&ev, "error")["code"], json!("provider_timeout"));
    // ... while a provider's own 504 with its JSON error body is provider_error.
    h.provider.push(Reply::Status { status: 504, body: json!({"error": {"message": "upstream timeout"}}), retry_after: None });
    let ev = h.say(ALICE, chat, "c2").await;
    assert_eq!(find(&ev, "error")["code"], json!("provider_error"));
    h.provider.push(Reply::Status { status: 500, body: json!({"error": {"message": "boom"}}), retry_after: None });
    let ev = h.say(ALICE, chat, "d").await;
    assert_eq!(find(&ev, "error")["code"], json!("provider_error"));
    assert!(h.turns(chat).await.iter().all(|t| t.state == "failed"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_event_mid_stream_uses_reported_usage() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.provider.push(Reply::Sse {
        events: vec![
            ("response.created".into(), json!({"type": "response.created"})),
            ("response.output_text.delta".into(), json!({"type": "response.output_text.delta", "delta": "partial"})),
            ("response.failed".into(), json!({"type": "response.failed", "response": {"error": {"message": "server overloaded"},
                "usage": {"input_tokens": 10, "output_tokens": 3}}})),
        ],
        delay: Duration::ZERO,
    });
    let ev = h.say(ALICE, chat, "x").await;
    assert_eq!(names(&ev), vec!["stream_started", "delta", "error"]);
    let t = &h.turns(chat).await[0];
    assert_eq!(t.state, "failed");
    h.eventually("usage", || h.usage_events().len() == 1).await;
    let u = &h.usage_events()[0];
    assert_eq!(u.settlement_method, "actual");
    assert_eq!(u.actual_credits_micro, mini_chat::domain::credits::credits_micro(10, 3, 1_000_000, 2_000_000).unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn incomplete_response_still_completes() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.provider.push(Reply::Sse {
        events: vec![
            ("response.output_text.delta".into(), json!({"type": "response.output_text.delta", "delta": "cut"})),
            ("response.incomplete".into(), json!({"type": "response.incomplete", "response": {"id": "resp_x",
                "incomplete_details": {"reason": "max_output_tokens"}, "usage": {"input_tokens": 5, "output_tokens": 100}}})),
        ],
        delay: Duration::ZERO,
    });
    let ev = h.say(ALICE, chat, "x").await;
    assert_eq!(names(&ev).last(), Some(&"done"));
    assert_eq!(h.turns(chat).await[0].state, "completed");
    assert_eq!(h.messages(chat).await[1].content, "cut");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn provider_stream_ending_without_terminal_event_fails() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.provider.push(Reply::Sse {
        events: vec![("response.output_text.delta".into(), json!({"type": "response.output_text.delta", "delta": "x"}))],
        delay: Duration::ZERO,
    });
    let ev = h.say(ALICE, chat, "x").await;
    assert_eq!(find(&ev, "error")["code"], json!("provider_error"));
    assert_eq!(h.turns(chat).await[0].state, "failed");
}

/// Read SSE frames from a live response until `pred` matches; returns the collected text.
async fn read_until(body: &mut axum::body::Body, pred: impl Fn(&str) -> bool) -> String {
    let mut acc = String::new();
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame.unwrap().into_data() {
            acc.push_str(&String::from_utf8_lossy(&data));
            if pred(&acc) {
                break;
            }
        }
    }
    acc
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deltas_are_relayed_without_buffering() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.provider.push(Reply::slow("one two three four", Duration::from_millis(250)));
    let began = Instant::now();
    let resp = h.open(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), json!({"content": "x"})).await;
    assert_eq!(resp.status(), 200);
    let mut body = resp.into_body();
    let got = read_until(&mut body, |s| s.contains("event: delta")).await;
    let first_delta = began.elapsed();
    assert!(got.contains("one"), "{got}");
    // The first delta reached the client while the provider is still streaming.
    assert_eq!(h.turns(chat).await[0].state, "running");
    let rest = read_until(&mut body, |s| s.contains("event: done")).await;
    let done_at = began.elapsed();
    // Remaining 3 deltas + completion are 250 ms apart each: no buffering until the end.
    assert!(done_at.checked_sub(first_delta).unwrap() >= Duration::from_millis(700), "first={first_delta:?} done={done_at:?}");
    assert!(rest.contains("event: done"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ping_is_sent_before_first_content() {
    let mut opts = Opts::default();
    opts.cfg.streaming.sse_ping_interval_seconds = 1;
    let h = Harness::with(opts).await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.provider.push(Reply::Sse {
        events: vec![
            ("response.created".into(), json!({"type": "response.created"})),
            ("response.output_text.delta".into(), json!({"type": "response.output_text.delta", "delta": "late"})),
            ("response.completed".into(), json!({"type": "response.completed", "response": {"id": "r", "usage": {"input_tokens": 1, "output_tokens": 1}}})),
        ],
        delay: Duration::from_millis(1300),
    });
    let ev = h.say(ALICE, chat, "x").await;
    let n = names(&ev);
    let first_delta = n.iter().position(|e| *e == "delta").unwrap();
    let first_ping = n.iter().position(|e| *e == "ping").expect("ping");
    assert!(first_ping > 0 && first_ping < first_delta, "{n:?}");
    assert!(n[first_delta..].iter().all(|e| *e != "ping"), "no ping after content: {n:?}");
    assert_eq!(find(&ev, "ping"), &json!({}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_disconnect_cancels_with_partial_text_and_estimated_settlement() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.provider.push(Reply::slow("partial answer that never ends because the client leaves", Duration::from_millis(150)));
    let resp = h.open(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), json!({"content": "x"})).await;
    let mut body = resp.into_body();
    let got = read_until(&mut body, |s| s.contains("event: delta")).await;
    assert!(got.contains("partial"));
    drop(body);
    for _ in 0..100 {
        if h.turns(chat).await[0].state != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let t = &h.turns(chat).await[0];
    assert_eq!(t.state, "cancelled");
    let msgs = h.messages(chat).await;
    let assistant = msgs.iter().find(|m| m.role == "assistant").expect("partial assistant message persisted");
    assert!(assistant.content.starts_with("partial"), "{}", assistant.content);
    assert!(!assistant.content.contains("leaves"));
    h.eventually("usage", || h.usage_events().len() == 1).await;
    let u = &h.usage_events()[0];
    assert_eq!(u.billing_outcome, "aborted");
    assert_eq!(u.settlement_method, "estimated");
    assert_eq!(u.terminal_state, "cancelled");
    // DESIGN: a cancelled turn emits `turn_failed` (there is no `turn_cancelled`).
    h.eventually("audit", || !h.audit_events().is_empty()).await;
    assert_eq!(h.audit_events()[0].event_type(), "turn_failed");
    let row = h.quota_row(ALICE, "total", "daily").await.unwrap();
    assert_eq!(row.reserved_credits_micro, 0);
    assert_eq!(row.spent_credits_micro, u.actual_credits_micro);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn disconnect_before_any_content_leaves_null_content() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.provider.push(Reply::slow("never seen", Duration::from_millis(400)));
    let resp = h.open(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), json!({"content": "x"})).await;
    let mut body = resp.into_body();
    read_until(&mut body, |s| s.contains("event: stream_started")).await;
    drop(body);
    for _ in 0..100 {
        if h.turns(chat).await[0].state != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(h.turns(chat).await[0].state, "cancelled");
    let msgs = h.messages(chat).await;
    assert!(msgs.iter().filter(|m| m.role == "assistant").all(|m| m.content.is_empty()), "no partial content");
    // Message list stays consistent (only non-empty / user messages are listed with full contract).
    let r = h.send(ALICE, "GET", &format!("/chats/{chat}/messages"), None).await;
    assert_eq!(r.status, 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quota_is_checked_before_provider_call() {
    let h = Harness::with(Opts {
        policy: policy_cfg(catalog(), json!({"default_standard_limits": {"limit_daily_credits_micro": 100, "limit_monthly_credits_micro": 100},
            "default_premium_limits": {"limit_daily_credits_micro": 100, "limit_monthly_credits_micro": 100}})),
        ..Default::default()
    })
    .await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    h.seed_spent(ALICE, "total", 100).await;
    let r = h.send(ALICE, "POST", &format!("/chats/{chat}/messages:stream"), Some(json!({"content": "x"}))).await;
    let p = r.problem(429);
    assert_eq!(p["context"]["violations"][0]["description"], json!("quota_exceeded"), "{p}");
    assert!(h.provider.chat_requests().is_empty());
    assert!(h.turns(chat).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sse_payloads_never_contain_internal_ids() {
    let h = Harness::new().await;
    let chat = h.chat(ALICE, Some("standard-m")).await;
    let ev = h.say(ALICE, chat, "x").await;
    for (name, data) in &ev {
        let s: String = Value::to_string(data);
        assert!(!s.contains("resp_abc123"), "{name}: {s}");
        assert!(!s.contains("tenant_id"), "{name}: {s}");
    }
}
