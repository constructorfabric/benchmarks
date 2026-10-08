//! Streaming flows: SSE events, persistence and settlement, provider
//! requests, errors, cancellation, idempotency and the parallel-turn guard.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::many_single_char_names
)]

mod common;

use std::time::Duration;

use common::*;
use mini_chat::domain::error::{DisabledFeature, DomainError, QuotaScope};
use mini_chat::domain::service::stream::{SendRequest, StreamEvent, StreamStart};
use serde_json::json;
use uuid::Uuid;

fn req(content: &str) -> SendRequest {
    SendRequest {
        content: content.to_owned(),
        request_id: None,
        attachment_ids: Vec::new(),
        web_search: false,
    }
}

fn started(ev: &[StreamEvent]) -> (Uuid, Uuid, bool) {
    match &ev[0] {
        StreamEvent::Started {
            request_id,
            message_id,
            is_new_turn,
            ..
        } => (*request_id, *message_id, *is_new_turn),
        other => panic!("first event is {other:?}"),
    }
}

async fn turn(h: &Harness, request_id: Uuid) -> serde_json::Value {
    h.rows(&format!(
        "select * from chat_turns where hex(request_id) = upper('{}')",
        uhex(request_id)
    ))
    .await
    .remove(0)
}

#[tokio::test]
async fn completed_turn_persists_and_settles() {
    let h = Harness::new().await;
    let a = ctx(TENANT_A, USER_A1);
    let chat = h
        .svc
        .create_chat(&a, None, Some("std".into()))
        .await
        .unwrap()
        .chat;
    h.gw.push(Reply::Events(text_events("Answer", 1000, 100)));
    let rid = Uuid::new_v4();
    let mut r = req("question");
    r.request_id = Some(rid);
    let ev = Harness::collect(h.svc.send_message(&a, chat.id, r).await.unwrap()).await;
    let (request_id, message_id, new_turn) = started(&ev);
    assert_eq!(request_id, rid);
    assert!(new_turn);
    assert!(
        matches!(&ev[1], StreamEvent::Delta { content, reasoning: false } if content == "Answer")
    );
    let StreamEvent::Done(done) = ev.last().unwrap() else {
        panic!("done expected")
    };
    assert_eq!((done.input_tokens, done.output_tokens), (1000, 100));
    assert_eq!(done.effective_model, "std");
    assert!(!done.downgrade);
    // provider request
    let body = h.gw.requests_to("/responses")[0].json();
    assert_eq!(body["model"], "std");
    assert_eq!(body["instructions"], "You are std.");
    assert_eq!(body["input"][0]["content"][0]["text"], "question");
    assert_eq!(
        body["user"],
        format!("{}{}", TENANT_A.simple(), USER_A1.simple())
    );
    // DB
    let t = turn(&h, rid).await;
    assert_eq!(t["state"], "completed");
    assert_eq!(t["assistant_message_id"], uhex(message_id));
    assert_eq!(t["effective_model"], "std");
    let q = h
        .rows("select * from quota_usage where bucket = 'total' order by period_type")
        .await;
    assert_eq!(q.len(), 2);
    for row in &q {
        assert_eq!(row["spent_credits_micro"], 1300);
        assert_eq!(row["reserved_credits_micro"], 0);
        assert_eq!(row["calls"], 1);
    }
    // usage published exactly once with the turn identity
    let published = h.wait_published(1).await;
    assert_eq!(published.len(), 1);
    let e = &published[0];
    assert_eq!(e.billing_outcome, "completed");
    assert_eq!(e.settlement_method, "actual");
    assert_eq!(e.actual_credits_micro, 1300);
    assert_eq!(e.request_id, rid);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.policy.published.lock().len(), 1);
    h.stop().await;
}

#[tokio::test]
async fn replay_and_request_id_conflicts() {
    let h = Harness::new().await;
    let a = ctx(TENANT_A, USER_A1);
    let chat = h
        .svc
        .create_chat(&a, None, Some("std".into()))
        .await
        .unwrap()
        .chat;
    let rid = Uuid::new_v4();
    let mut r = req("q");
    r.request_id = Some(rid);
    let first = Harness::collect(h.svc.send_message(&a, chat.id, r.clone()).await.unwrap()).await;
    let calls = h.gw.requests_to("/responses").len();
    let quota_before = h.rows("select * from quota_usage").await;
    let start = h.svc.send_message(&a, chat.id, r).await.unwrap();
    assert!(matches!(start, StreamStart::Replay(_)));
    let ev = Harness::collect(start).await;
    assert_eq!(ev.len(), 3);
    let (_, mid, new_turn) = started(&ev);
    assert!(!new_turn);
    assert_eq!(mid, started(&first).1);
    assert_eq!(h.gw.requests_to("/responses").len(), calls);
    assert_eq!(h.rows("select * from quota_usage").await, quota_before);
    // failed turn: conflict
    h.gw.push(Reply::Status(500, json!({"error": {"message": "x"}})));
    let rid2 = Uuid::new_v4();
    let mut r2 = req("q2");
    r2.request_id = Some(rid2);
    let ev = Harness::collect(h.svc.send_message(&a, chat.id, r2.clone()).await.unwrap()).await;
    assert!(matches!(ev.last(), Some(StreamEvent::Error { code, .. }) if code == "provider_error"));
    assert!(matches!(
        h.svc.send_message(&a, chat.id, r2).await.err().unwrap(),
        DomainError::RequestIdConflict { .. }
    ));
    h.stop().await;
}

#[tokio::test]
async fn parallel_turn_guard_and_cancellation() {
    let h = Harness::new().await;
    let a = ctx(TENANT_A, USER_A1);
    let chat = h
        .svc
        .create_chat(&a, None, Some("std".into()))
        .await
        .unwrap()
        .chat;
    h.gw.push(Reply::Hang);
    let StreamStart::Live(mut live) = h.svc.send_message(&a, chat.id, req("slow")).await.unwrap()
    else {
        panic!("live expected")
    };
    let ev = live.events.recv().await.unwrap();
    let rid = match ev {
        StreamEvent::Started { request_id, .. } => request_id,
        other => panic!("{other:?}"),
    };
    assert!(matches!(
        h.svc
            .send_message(&a, chat.id, req("parallel"))
            .await
            .err()
            .unwrap(),
        DomainError::TurnAlreadyRunning
    ));
    let mut same = req("same");
    same.request_id = Some(rid);
    assert!(matches!(
        h.svc.send_message(&a, chat.id, same).await.err().unwrap(),
        DomainError::RequestIdConflict { .. }
    ));
    // wait until the provider call is in flight, then disconnect
    for _ in 0..100 {
        if !h.gw.requests_to("/responses").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    live.cancel.cancel();
    drop(live);
    let mut state = String::new();
    for _ in 0..100 {
        state = turn(&h, rid).await["state"].as_str().unwrap().to_owned();
        if state != "running" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(state, "cancelled");
    let published = h.wait_published(1).await;
    assert_eq!(published[0].billing_outcome, "aborted");
    assert_eq!(published[0].settlement_method, "estimated");
    // the chat accepts a new turn
    let ev = Harness::collect(h.svc.send_message(&a, chat.id, req("again")).await.unwrap()).await;
    assert!(matches!(ev.last(), Some(StreamEvent::Done(_))));
    h.stop().await;
}

#[tokio::test]
async fn provider_errors_are_sanitized_and_failed() {
    let h = Harness::new().await;
    let a = ctx(TENANT_A, USER_A1);
    let chat = h
        .svc
        .create_chat(&a, None, Some("std".into()))
        .await
        .unwrap()
        .chat;
    h.gw.push(Reply::Events(vec![json!({"type": "response.failed", "response": {"error": {
        "code": "server_error", "message": "failed resp_0123456789 for file-abcdefghijklmnop see https://x.example/a"}}})]));
    let ev = Harness::collect(h.svc.send_message(&a, chat.id, req("x")).await.unwrap()).await;
    let StreamEvent::Error { code, message } = ev.last().unwrap() else {
        panic!("error expected")
    };
    assert_eq!(code, "provider_error");
    assert!(
        !message.contains("resp_0123")
            && !message.contains("file-abc")
            && !message.contains("https://")
    );
    let t = turn(&h, started(&ev).0).await;
    assert_eq!(t["state"], "failed");
    assert_eq!(t["error_code"], "provider_error");
    h.gw.push(Reply::Status(429, json!({"error": {"message": "slow"}})));
    let ev = Harness::collect(h.svc.send_message(&a, chat.id, req("y")).await.unwrap()).await;
    assert!(matches!(ev.last(), Some(StreamEvent::Error { code, .. }) if code == "rate_limited"));
    h.stop().await;
}

#[tokio::test]
async fn tools_citations_and_web_search_limits() {
    let h = Harness::new().await;
    let a = ctx(TENANT_A, USER_A1);
    let chat = h
        .svc
        .create_chat(&a, None, Some("std".into()))
        .await
        .unwrap()
        .chat;
    h.gw.push(Reply::Events(vec![
        json!({"type": "response.web_search_call.in_progress", "item_id": "w1"}),
        json!({"type": "response.web_search_call.completed", "item_id": "w1"}),
        json!({"type": "response.output_text.delta", "item_id": "m", "delta": "See site"}),
        json!({"type": "response.output_text.annotation.added", "item_id": "m",
               "annotation": {"type": "url_citation", "url": "https://e.com", "title": "E", "start_index": 4, "end_index": 8}}),
        json!({"type": "response.completed", "response": {"usage": {"input_tokens": 1, "output_tokens": 1}}}),
    ]));
    let mut r = req("search");
    r.web_search = true;
    let ev = Harness::collect(h.svc.send_message(&a, chat.id, r.clone()).await.unwrap()).await;
    assert!(matches!(&ev[1], StreamEvent::Tool { done: false, name, .. } if name == "web_search"));
    assert!(matches!(&ev[2], StreamEvent::Tool { done: true, name, .. } if name == "web_search"));
    let StreamEvent::Citations(c) = &ev[ev.len() - 2] else {
        panic!("citations expected before done")
    };
    assert!(c[0].web && c[0].snippet == "site" && c[0].span == Some((4, 8)));
    let body = h.gw.requests_to("/responses")[0].json();
    assert!(
        body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["type"] == "web_search")
    );
    assert!(
        body["instructions"]
            .as_str()
            .unwrap()
            .contains("web_search")
    );
    // per-turn web search limit
    h.gw.push(Reply::Events(
        (0..3)
            .map(|i| json!({"type": "response.web_search_call.in_progress", "item_id": format!("w{i}")}))
            .collect(),
    ));
    let ev = Harness::collect(h.svc.send_message(&a, chat.id, r).await.unwrap()).await;
    assert!(
        matches!(ev.last(), Some(StreamEvent::Error { code, .. }) if code == "web_search_calls_exceeded")
    );
    h.stop().await;
}

#[tokio::test]
async fn preflight_rejections_leave_nothing_behind() {
    let h = Harness::new().await;
    let a = ctx(TENANT_A, USER_A1);
    let chat = h
        .svc
        .create_chat(&a, None, Some("std".into()))
        .await
        .unwrap()
        .chat;
    assert!(matches!(
        h.svc
            .send_message(&a, chat.id, req("  "))
            .await
            .err()
            .unwrap(),
        DomainError::EmptyContent
    ));
    let id = Uuid::new_v4();
    let mut r = req("x");
    r.attachment_ids = vec![id, id];
    assert!(matches!(
        h.svc.send_message(&a, chat.id, r).await.err().unwrap(),
        DomainError::InvalidAttachment { .. }
    ));
    let mut r = req("x");
    r.attachment_ids = vec![Uuid::new_v4()];
    assert!(matches!(
        h.svc.send_message(&a, chat.id, r).await.err().unwrap(),
        DomainError::InvalidAttachment { .. }
    ));
    assert_eq!(h.rows("select count(*) n from messages").await[0]["n"], 0);
    assert_eq!(h.rows("select count(*) n from chat_turns").await[0]["n"], 0);
    let q = h
        .rows("select coalesce(sum(reserved_credits_micro), 0) s from quota_usage")
        .await;
    assert_eq!(q[0]["s"], 0);
    assert!(h.gw.requests_to("/responses").is_empty());
    h.stop().await;
}

#[tokio::test]
async fn kill_switches_and_quota_exhaustion() {
    let h = Harness::new().await;
    let a = ctx(TENANT_A, USER_A1);
    let chat = h.svc.create_chat(&a, None, None).await.unwrap().chat;
    h.policy.snapshot.lock().kill_switches.disable_web_search = true;
    let mut r = req("x");
    r.web_search = true;
    assert!(matches!(
        h.svc.send_message(&a, chat.id, r).await.err().unwrap(),
        DomainError::FeatureDisabled {
            feature: DisabledFeature::WebSearch
        }
    ));
    h.policy.snapshot.lock().kill_switches.disable_web_search = false;
    // premium exhausted -> downgrade
    h.policy.limits.lock().1.limit_daily_credits_micro = 1;
    let ev = Harness::collect(h.svc.send_message(&a, chat.id, req("x")).await.unwrap()).await;
    let StreamEvent::Done(d) = ev.last().unwrap() else {
        panic!("done expected")
    };
    assert!(d.downgrade);
    assert_eq!(d.effective_model, "std");
    assert_eq!(d.downgrade_from.as_deref(), Some("prem"));
    assert_eq!(
        d.downgrade_reason.as_deref(),
        Some("premium_quota_exhausted")
    );
    // all exhausted -> 429 tokens, no provider call
    h.policy.limits.lock().0.limit_daily_credits_micro = 1;
    let calls = h.gw.requests_to("/responses").len();
    assert!(matches!(
        h.svc
            .send_message(&a, chat.id, req("x"))
            .await
            .err()
            .unwrap(),
        DomainError::QuotaExceeded {
            scope: QuotaScope::Tokens
        }
    ));
    assert_eq!(h.gw.requests_to("/responses").len(), calls);
    h.stop().await;
}

#[tokio::test]
async fn context_budget_and_input_limit() {
    let h = Harness::new().await;
    {
        let mut s = h.policy.snapshot.lock();
        let mut tiny = model("tiny", "standard", true, false);
        tiny.context_window = 2600;
        tiny.max_output_tokens = 1024;
        tiny.max_input_tokens = 1500;
        s.model_catalog.push(tiny);
    }
    let a = ctx(TENANT_A, USER_A1);
    let chat = h
        .svc
        .create_chat(&a, None, Some("tiny".into()))
        .await
        .unwrap()
        .chat;
    assert!(matches!(
        h.svc
            .send_message(&a, chat.id, req(&"x".repeat(8000)))
            .await
            .err()
            .unwrap(),
        DomainError::InputTooLong { .. }
    ));
    assert!(matches!(
        h.svc
            .send_message(&a, chat.id, req(&"x".repeat(5000)))
            .await
            .err()
            .unwrap(),
        DomainError::ContextBudgetExceeded
    ));
    // history is truncated by whole turns within the budget
    for i in 0..4 {
        h.gw.push(Reply::Events(text_events(&"A".repeat(1500), 1, 1)));
        Harness::collect(
            h.svc
                .send_message(&a, chat.id, req(&format!("{i}").repeat(1500)))
                .await
                .unwrap(),
        )
        .await;
    }
    let body = h.gw.requests_to("/responses").last().unwrap().json();
    let input = body["input"].as_array().unwrap();
    assert!(input.len() < 8, "older turns dropped");
    assert_eq!(
        input[0]["role"], "user",
        "history never starts with an answer"
    );
    h.stop().await;
}
