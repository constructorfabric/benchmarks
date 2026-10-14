//! The real provider task against the mock OAGW, read through its relay channel: the request
//! that reaches OAGW, the translated and accounted events, the terminal event gated on the
//! committed finalization, every provider failure mapped and sanitized, tool limits, cancellation
//! and hard cancel, backpressure and no buffering, durable progress, the other adapter kinds and
//! the first-terminal-wins rule.
//!
//! Slice S2-provider-path of `cpt-cf-mini-chat-feature-message-streaming`: flows `send-message`
//! (provider, relay and finalize parts), `disconnect` and `web-search-turn`; algorithms
//! `provider-call`, `responses-request`, `adapter-kinds`, `event-translation`, `tool-accounting`,
//! `citations`, `finalization`, `terminal-emission`, `error-sanitization` and `tool-assembly`
//! (`inst-tools-max-calls`); states `turn-lifecycle`; definitions of done
//! `cpt-cf-mini-chat-dod-message-streaming-provider-adapters`,
//! `cpt-cf-mini-chat-dod-message-streaming-tools-web-search`,
//! `cpt-cf-mini-chat-dod-message-streaming-error-sanitization`,
//! `cpt-cf-mini-chat-dod-message-streaming-cancellation`,
//! `cpt-cf-mini-chat-dod-message-streaming-finalization` and
//! `cpt-cf-mini-chat-dod-message-streaming-turn-lifecycle`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use time::Duration as TimeDuration;

use crate::domain::error::StreamErrorCode;
use crate::domain::streaming::{
    CitationSource, Delta, DeltaKind, Done, RelayEvent, ToolPhase, TurnOutcome, TurnTarget,
};
use crate::infra::provider_task::{ProviderTaskSettings, S2sContext};
use crate::infra::storage::entity::chat_turn;
use crate::infra::storage::repo_impl::ChatRecord;
use crate::test_support::chats::{fail_outbox_inserts, message_rows};
use crate::test_support::provider::{
    Gate, GatewayFailure, MockProvider, RESPONSE_ID, Reply, Step, ci_done, ci_start, completed,
    completed_parts, delta, error_event, failed, fs_done, fs_start, function_call_done,
    happy_steps, incomplete, numbered_deltas, numbered_text, output_part, url_annotation, web_done,
    web_start,
};
use crate::test_support::quota::{
    FixedClock, STANDARD, all_quota_rows, default_now, row, seed_rows,
};
use crate::test_support::raw_exec;
use crate::test_support::send::{
    ANTHROPIC_MODEL, CC_MODEL, SendRig, SendRigOptions, VLLM_MODEL, multi_catalog, multi_providers,
    provider_s2s, rig_task_settings, send_rig, send_rig_with, stream_rig, turn_rows,
};

struct Harness {
    rig: SendRig,
    chat: ChatRecord,
    upstream: Arc<MockProvider>,
}

async fn harness(reply: Reply) -> Harness {
    let upstream = MockProvider::answering(reply);
    let rig = stream_rig(upstream.clone()).await;
    let chat = rig.std_chat().await;
    Harness {
        rig,
        chat,
        upstream,
    }
}

async fn turn_of(rig: &SendRig, chat: &ChatRecord) -> chat_turn::Model {
    turn_rows(&rig.quota.db, chat.id).await.remove(0)
}

fn text_of(events: &[RelayEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            RelayEvent::Delta(Delta {
                kind: DeltaKind::Text,
                content,
            }) => Some(content.as_str()),
            _ => None,
        })
        .collect()
}

fn done_of(events: &[RelayEvent]) -> &Done {
    match events.last() {
        Some(RelayEvent::Done(done)) => done,
        other => panic!("expected a trailing done, got {other:?} in {events:?}"),
    }
}

fn error_of(events: &[RelayEvent]) -> (&StreamErrorCode, &str) {
    match events.last() {
        Some(RelayEvent::Error(error)) => (&error.code, error.message.as_str()),
        other => panic!("expected a trailing error, got {other:?} in {events:?}"),
    }
}

fn is_terminal(event: &RelayEvent) -> bool {
    matches!(event, RelayEvent::Done(_) | RelayEvent::Error(_))
}

async fn reserved(rig: &SendRig) -> i64 {
    all_quota_rows(&rig.quota.db)
        .await
        .iter()
        .map(|row| row.reserved)
        .sum()
}

fn question() -> Value {
    json!({ "content": "what is rust?" })
}

// ── the normal stream ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_normal_stream_relays_the_deltas_then_done_only_after_the_commit() {
    let h = harness(Reply::Sse(happy_steps(&["Hel", "lo"], (120, 30)))).await;
    let mut turn = h.rig.start_turn(&h.chat, &question()).await;
    let mut events = Vec::new();
    while let Some(event) = turn.next().await {
        if is_terminal(&event) {
            // the terminal event is held back until the finalization committed
            assert_eq!(turn_of(&h.rig, &h.chat).await.state, "completed");
            assert_eq!(message_rows(&h.rig.quota.db, h.chat.id).await.len(), 2);
        }
        events.push(event);
    }
    assert_eq!(events.len(), 3, "{events:?}");
    assert_eq!(
        events[0],
        RelayEvent::Delta(Delta {
            kind: DeltaKind::Text,
            content: "Hel".to_owned()
        })
    );
    assert_eq!(
        events[1],
        RelayEvent::Delta(Delta {
            kind: DeltaKind::Text,
            content: "lo".to_owned()
        })
    );
    let done = done_of(&events);
    assert_eq!((done.input_tokens, done.output_tokens), (120, 30));
    assert_eq!(done.effective_model, turn.plan.decision.effective_model);
    assert_eq!(done.selected_model, turn.plan.selected_model);
    assert_eq!(done.downgrade_from, None);
    assert_eq!(done.downgrade_reason, None);

    let row = turn_of(&h.rig, &h.chat).await;
    assert_eq!(row.error_code, None);
    assert_eq!(
        row.assistant_message_id,
        Some(turn.plan.assistant_message_id)
    );
    // the provider response id is kept internally only
    assert_eq!(row.provider_response_id.as_deref(), Some(RESPONSE_ID));
    let messages = message_rows(&h.rig.quota.db, h.chat.id).await;
    let assistant = messages.iter().find(|m| m.role == "assistant").unwrap();
    assert_eq!(assistant.content, "Hello");
    assert_eq!((assistant.input_tokens, assistant.output_tokens), (120, 30));
    assert_eq!(assistant.provider_response_id.as_deref(), Some(RESPONSE_ID));
    assert_eq!(reserved(&h.rig).await, 0);
    let usage = h.rig.events().await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].settlement_method, "actual");
    assert_eq!(h.rig.audit_events().await.len(), 1);
}

#[tokio::test]
async fn done_carries_the_quota_warnings_read_after_the_settlement() {
    let h = harness(Reply::Sse(happy_steps(&["ok"], (120, 30)))).await;
    // 83 percent of the daily total is spent: the turn leaves it in the warning zone
    seed_rows(
        &h.rig.quota.db,
        &[row("daily", "2026-02-28", "total").spent(50_000_000)],
    )
    .await;
    let mut turn = h.rig.start_turn(&h.chat, &question()).await;
    let events = turn.drain().await;
    let warnings = done_of(&events)
        .quota_warnings
        .as_ref()
        .expect("warnings on the CAS-winning completed turn");
    assert!(
        warnings
            .iter()
            .any(|w| w.warning && format!("{:?}", w.period) == "Daily"),
        "{warnings:?}"
    );
}

#[tokio::test]
async fn the_provider_request_goes_through_oagw_under_the_s2s_context() {
    let h = harness(Reply::Sse(happy_steps(&["x"], (1, 1)))).await;
    let mut turn = h.rig.start_turn(&h.chat, &question()).await;
    turn.drain().await;
    let call = h.upstream.only_call();
    assert_eq!(call.method, "POST");
    assert_eq!(call.uri, "/azure.example.com/v1/responses");
    // the gear's S2S context, never the caller's identity
    let s2s = provider_s2s();
    assert_eq!(call.subject, s2s.subject_id());
    assert_eq!(call.tenant, s2s.subject_tenant_id());
    assert_ne!(call.subject, turn.plan.user_id);
    assert_ne!(call.tenant, turn.plan.tenant_id);
    assert_eq!(call.header("content-type"), Some("application/json"));
    assert_eq!(call.header("accept"), Some("text/event-stream"));
    // OAGW injects the credentials; the gear sends none
    for credential in [
        "authorization",
        "api-key",
        "x-api-key",
        "proxy-authorization",
    ] {
        assert_eq!(call.header(credential), None, "{credential}");
    }
    assert_eq!(call.body["model"], "provider-standard-1");
    assert_eq!(call.body["stream"], true);
    assert_eq!(call.body["store"], false);
    assert_eq!(
        call.body["max_output_tokens"],
        turn.plan.decision.max_output_tokens_applied
    );
    assert_eq!(
        call.body["input"],
        json!([{ "role": "user", "content": "what is rust?" }])
    );
    // the identity travels only in `user` and `metadata`
    assert_eq!(
        call.body["metadata"]["user_id"],
        turn.plan.user_id.to_string()
    );
    assert_eq!(
        call.body["metadata"]["tenant_id"],
        turn.plan.tenant_id.to_string()
    );
    assert_eq!(call.body["metadata"]["chat_id"], h.chat.id.to_string());
    assert_eq!(call.body["user"].as_str().unwrap().len(), 64);
}

#[tokio::test]
async fn the_api_version_query_is_kept_and_a_host_port_alias_is_used() {
    use crate::config::MiniChatConfig;
    use crate::domain::provider_registry::ProviderRegistry;
    let config = MiniChatConfig::from_value(json!({
        "client_credentials": { "client_id": "mini-chat", "client_secret": "dev-secret" },
        "providers": { "azure_openai": {
            "kind": "openai_responses", "host": "127.0.0.1", "port": 8081, "use_http": true,
            "upstream_alias": "127.0.0.1:8081", "storage_kind": "openai",
            "api_path": "/openai/deployments/{model}/responses?api-version=2025-03-01-preview"
        } }
    }))
    .expect("valid config");
    let upstream = MockProvider::answering(Reply::Sse(happy_steps(&["x"], (1, 1))));
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream.clone())
            .with_providers(Arc::new(ProviderRegistry::build(&config.providers))),
    )
    .await;
    let chat = rig.std_chat().await;
    let mut turn = rig.start_turn(&chat, &question()).await;
    turn.drain().await;
    assert_eq!(
        upstream.only_call().uri,
        "/127.0.0.1:8081/openai/deployments/provider-standard-1/responses?api-version=2025-03-01-preview"
    );
}

#[tokio::test]
async fn a_completion_without_deltas_persists_an_empty_assistant_message() {
    let h = harness(Reply::Sse(vec![completed("", (10, 0))])).await;
    let mut turn = h.rig.start_turn(&h.chat, &question()).await;
    let events = turn.drain().await;
    assert_eq!(events.len(), 1, "{events:?}");
    done_of(&events);
    assert_eq!(turn_of(&h.rig, &h.chat).await.state, "completed");
    let messages = message_rows(&h.rig.quota.db, h.chat.id).await;
    assert_eq!(
        messages
            .iter()
            .find(|m| m.role == "assistant")
            .unwrap()
            .content,
        ""
    );
}

#[tokio::test]
async fn an_incomplete_response_completes_the_turn_and_sends_no_citations() {
    let steps = vec![delta("cut"), incomplete("max_output_tokens", (10, 500))];
    let h = harness(Reply::Sse(steps)).await;
    let mut turn = h.rig.start_turn(&h.chat, &question()).await;
    let events = turn.drain().await;
    assert!(!events.iter().any(|e| matches!(e, RelayEvent::Citations(_))));
    let done = done_of(&events);
    assert_eq!((done.input_tokens, done.output_tokens), (10, 500));
    let row = turn_of(&h.rig, &h.chat).await;
    assert_eq!(row.state, "completed");
    // the incomplete reason is never stored as an error code
    assert_eq!(row.error_code, None);
    let messages = message_rows(&h.rig.quota.db, h.chat.id).await;
    assert_eq!(
        messages
            .iter()
            .find(|m| m.role == "assistant")
            .unwrap()
            .content,
        "cut"
    );
    assert_eq!(h.rig.events().await[0].settlement_method, "actual");
}

// ── tools and citations ──────────────────────────────────────────────────────

#[tokio::test]
async fn a_web_search_turn_reports_the_tool_events_the_citations_and_the_counters() {
    let text = "Growth was strong in 2025 according to analysts.";
    let steps = vec![
        web_start(),
        web_done(),
        delta(text),
        completed_parts(
            &[output_part(
                text,
                &[url_annotation(
                    "https://example.com/m",
                    "Market Analysis",
                    7,
                    18,
                )],
            )],
            (200, 50),
        ),
    ];
    let h = harness(Reply::Sse(steps)).await;
    let mut turn = h
        .rig
        .start_turn(
            &h.chat,
            &json!({ "content": "market?", "web_search": { "enabled": true } }),
        )
        .await;
    let events = turn.drain().await;
    // tool start and done, the delta, one citations event, then done
    assert_eq!(events.len(), 5, "{events:?}");
    let RelayEvent::Tool(start) = &events[0] else {
        panic!("{events:?}");
    };
    assert_eq!(
        (start.phase, start.name.as_str()),
        (ToolPhase::Start, "web_search")
    );
    assert_eq!(start.details, json!({}));
    let RelayEvent::Tool(done) = &events[1] else {
        panic!("{events:?}");
    };
    assert_eq!(
        (done.phase, done.name.as_str()),
        (ToolPhase::Done, "web_search")
    );
    let RelayEvent::Citations(citations) = &events[3] else {
        panic!("{events:?}");
    };
    assert_eq!(citations.items.len(), 1);
    let item = &citations.items[0];
    assert_eq!(item.source, CitationSource::Web);
    assert_eq!(item.url.as_deref(), Some("https://example.com/m"));
    assert_eq!(item.title, "Market Analysis");
    assert_eq!(item.snippet, "was strong ");
    assert_eq!(item.span.map(|s| (s.start, s.end)), Some((7, 18)));
    done_of(&events);

    // the request carried the tool, the guard and the requested flag
    let call = h.upstream.only_call();
    assert_eq!(call.body["tools"][0]["type"], "web_search");
    assert!(
        call.body["instructions"]
            .as_str()
            .unwrap()
            .contains("WEB-GUARD")
    );
    // the completed call is counted durably and settled into the daily counter
    let row = turn_of(&h.rig, &h.chat).await;
    assert!(row.web_search_enabled);
    assert_eq!(row.web_search_completed_count, 1);
    let usage = h.rig.events().await;
    assert_eq!(usage[0].web_search_calls, 1);
    let daily: i32 = all_quota_rows(&h.rig.quota.db)
        .await
        .iter()
        .filter(|r| r.bucket == "total" && r.period == "daily")
        .map(|r| r.web_search_calls)
        .sum();
    assert_eq!(daily, 1);
}

#[tokio::test]
async fn the_third_web_search_start_breaches_the_limit_and_fails_the_turn_estimated() {
    let steps = vec![
        web_start(),
        web_done(),
        web_start(),
        web_done(),
        web_start(),
        delta("never relayed"),
        completed("never relayed", (999, 999)),
    ];
    let h = harness(Reply::Sse(steps)).await;
    let mut turn = h
        .rig
        .start_turn(
            &h.chat,
            &json!({ "content": "search", "web_search": { "enabled": true } }),
        )
        .await;
    let events = turn.drain().await;
    // the breaching start is not relayed; the stream ends with the error
    let tools = events
        .iter()
        .filter(|e| matches!(e, RelayEvent::Tool(_)))
        .count();
    assert_eq!(tools, 4, "{events:?}");
    assert_eq!(text_of(&events), "");
    let (code, message) = error_of(&events);
    assert_eq!(*code, StreamErrorCode::WebSearchCallsExceeded);
    assert!(!message.is_empty());
    // the provider stream was cancelled before its usage arrived
    h.upstream.wait_for_aborted(1).await;

    let row = turn_of(&h.rig, &h.chat).await;
    assert_eq!(row.state, "failed");
    assert_eq!(row.error_code.as_deref(), Some("web_search_calls_exceeded"));
    assert_eq!(row.assistant_message_id, None);
    assert_eq!(row.web_search_completed_count, 2);
    // no usage: estimated settlement, no refund of the surcharge
    let usage = h.rig.events().await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].settlement_method, "estimated");
    assert_eq!(usage[0].usage, None);
    assert_eq!(usage[0].web_search_calls, 2);
    assert_eq!(message_rows(&h.rig.quota.db, h.chat.id).await.len(), 1);
}

#[tokio::test]
async fn the_web_search_limit_comes_from_the_configuration() {
    let upstream = MockProvider::answering(Reply::Sse(vec![
        web_start(),
        web_done(),
        web_start(),
        completed("x", (1, 1)),
    ]));
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream.clone())
            .with_task_settings(ProviderTaskSettings {
                web_search_max_calls_per_message: 1,
                ..rig_task_settings()
            }),
    )
    .await;
    let chat = rig.std_chat().await;
    let mut turn = rig
        .start_turn(
            &chat,
            &json!({ "content": "s", "web_search": { "enabled": true } }),
        )
        .await;
    let events = turn.drain().await;
    assert_eq!(
        *error_of(&events).0,
        StreamErrorCode::WebSearchCallsExceeded
    );
}

#[tokio::test]
async fn the_code_interpreter_has_its_own_limit_and_reports_its_output() {
    let steps = vec![
        ci_start(),
        ci_done("42"),
        ci_start(),
        ci_done("43"),
        completed("x", (1, 1)),
    ];
    let upstream = MockProvider::answering(Reply::Sse(steps));
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream.clone())
            .with_task_settings(ProviderTaskSettings {
                code_interpreter_max_calls_per_message: 1,
                ..rig_task_settings()
            }),
    )
    .await;
    let chat = rig.std_chat().await;
    let mut turn = rig.start_turn(&chat, &question()).await;
    let events = turn.drain().await;
    let RelayEvent::Tool(first) = &events[0] else {
        panic!("{events:?}");
    };
    assert_eq!(first.name, "code_interpreter");
    let RelayEvent::Tool(done) = &events[1] else {
        panic!("{events:?}");
    };
    assert_eq!(
        (done.phase, done.details.clone()),
        (ToolPhase::Done, json!({ "output": "42" }))
    );
    assert_eq!(
        *error_of(&events).0,
        StreamErrorCode::CodeInterpreterCallsExceeded
    );
    let row = turn_of(&rig, &chat).await;
    assert_eq!(
        row.error_code.as_deref(),
        Some("code_interpreter_calls_exceeded")
    );
    assert_eq!(row.code_interpreter_completed_count, 1);
}

#[tokio::test]
async fn file_search_calls_have_no_mid_turn_limit() {
    let mut steps = Vec::new();
    for _ in 0..5 {
        steps.push(fs_start());
        steps.push(fs_done());
    }
    steps.push(completed("answer", (5, 5)));
    let h = harness(Reply::Sse(steps)).await;
    let mut turn = h.rig.start_turn(&h.chat, &question()).await;
    let events = turn.drain().await;
    done_of(&events);
    let tools = events
        .iter()
        .filter(|e| matches!(e, RelayEvent::Tool(_)))
        .count();
    assert_eq!(tools, 10);
    assert_eq!(
        turn_of(&h.rig, &h.chat).await.file_search_completed_count,
        5
    );
    assert_eq!(h.rig.events().await[0].file_search_calls, 5);
}

#[tokio::test]
async fn a_function_tool_use_without_knowledge_search_fails_the_turn_as_unexpected() {
    let steps = vec![
        delta("partial"),
        function_call_done("search_knowledge"),
        completed("x", (7, 3)),
    ];
    let h = harness(Reply::Sse(steps)).await;
    let mut turn = h.rig.start_turn(&h.chat, &question()).await;
    let events = turn.drain().await;
    assert_eq!(*error_of(&events).0, StreamErrorCode::UnexpectedToolUse);
    let row = turn_of(&h.rig, &h.chat).await;
    assert_eq!(row.state, "failed");
    assert_eq!(row.error_code.as_deref(), Some("unexpected_tool_use"));
    assert_eq!(row.assistant_message_id, None);
    assert_eq!(message_rows(&h.rig.quota.db, h.chat.id).await.len(), 1);
    assert_eq!(h.rig.events().await.len(), 1);
}

// ── provider failures ────────────────────────────────────────────────────────

async fn failure_of(reply: Reply) -> (Vec<RelayEvent>, chat_turn::Model, Harness) {
    let h = harness(reply).await;
    let mut turn = h.rig.start_turn(&h.chat, &question()).await;
    let events = turn.drain().await;
    let row = turn_of(&h.rig, &h.chat).await;
    (events, row, h)
}

fn assert_failed_turn(row: &chat_turn::Model, code: &str) {
    assert_eq!(row.state, "failed");
    assert_eq!(row.error_code.as_deref(), Some(code));
    assert_eq!(row.assistant_message_id, None);
    assert!(row.completed_at.is_some());
}

#[tokio::test]
async fn an_http_500_with_provider_identifiers_is_a_sanitized_provider_error() {
    let body = json!({ "error": {
        "message": "The server had an error. Please include the request ID req_abc123 in your message. \
                    See https://help.openai.com/x and key sk-proj-****abcd, bearer Bearer tok_123abc.",
        "type": "server_error" } });
    let (events, row, h) = failure_of(Reply::http(500, &body)).await;
    let (code, message) = error_of(&events);
    assert_eq!(*code, StreamErrorCode::ProviderError);
    for leaked in ["req_abc123", "https://", "sk-proj", "abcd", "tok_123abc"] {
        assert!(!message.contains(leaked), "`{leaked}` in {message}");
        assert!(
            !row.error_detail
                .clone()
                .unwrap_or_default()
                .contains(leaked)
        );
    }
    assert!(message.contains("[provider_id]"), "{message}");
    assert_failed_turn(&row, "provider_error");
    // the stored detail is the sanitized text as well
    assert_eq!(row.error_detail.as_deref(), Some(message));
    // no partial work: reserve settled estimated, one usage event, one audit event
    assert_eq!(reserved(&h.rig).await, 0);
    let usage = h.rig.events().await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].settlement_method, "estimated");
    assert_eq!(h.rig.audit_events().await.len(), 1);
}

#[tokio::test]
async fn an_http_429_is_rate_limited_with_the_retry_after_seconds() {
    let body = json!({ "error": { "message":
        "Rate limit reached for gpt in organization org-XYZ on requests per min.",
        "type": "requests" } });
    let (events, row, _h) =
        failure_of(Reply::http_with(429, &[("retry-after", "30")], &body)).await;
    let (code, message) = error_of(&events);
    assert_eq!(*code, StreamErrorCode::RateLimited);
    assert!(message.contains("30"), "{message}");
    assert!(!message.contains("org-XYZ"), "{message}");
    assert_failed_turn(&row, "rate_limited");
}

#[tokio::test]
async fn an_http_401_uses_the_generic_message_and_never_provider_text() {
    let body = json!({ "error": { "message": "Incorrect API key provided: sk-proj-****abcd.",
                                    "type": "invalid_request_error", "code": "invalid_api_key" } });
    let (events, row, _h) = failure_of(Reply::http(401, &body)).await;
    let (code, message) = error_of(&events);
    assert_eq!(*code, StreamErrorCode::ProviderError);
    for leaked in ["sk-", "abcd", "Incorrect"] {
        assert!(!message.contains(leaked), "`{leaked}` in {message}");
        assert!(
            !row.error_detail
                .clone()
                .unwrap_or_default()
                .contains(leaked)
        );
    }
    assert_failed_turn(&row, "provider_error");
}

#[tokio::test]
async fn gateway_timeouts_are_provider_timeout() {
    let (events, row, _h) = failure_of(Reply::Fail(GatewayFailure::Timeout)).await;
    assert_eq!(*error_of(&events).0, StreamErrorCode::ProviderTimeout);
    assert_failed_turn(&row, "provider_timeout");

    let problem = json!({ "type": "gts.cf.core.errors.err.v1~cf.core.err.deadline_exceeded.v1~",
                           "title": "Deadline exceeded", "status": 504, "detail": "upstream timed out" });
    let (events, row, _h) = failure_of(Reply::gateway_problem(504, &problem)).await;
    assert_eq!(*error_of(&events).0, StreamErrorCode::ProviderTimeout);
    assert_failed_turn(&row, "provider_timeout");
}

#[tokio::test]
async fn a_provider_504_with_its_own_body_and_other_gateway_errors_are_provider_error() {
    let own = json!({ "error": { "message": "upstream request timeout" } });
    let (events, row, _h) = failure_of(Reply::http(504, &own)).await;
    assert_eq!(*error_of(&events).0, StreamErrorCode::ProviderError);
    assert_failed_turn(&row, "provider_error");
    for failure in [GatewayFailure::Unavailable, GatewayFailure::Internal] {
        let (events, row, _h) = failure_of(Reply::Fail(failure)).await;
        assert_eq!(
            *error_of(&events).0,
            StreamErrorCode::ProviderError,
            "{failure:?}"
        );
        assert_failed_turn(&row, "provider_error");
    }
}

#[tokio::test]
async fn a_failed_event_with_usage_settles_actual_and_without_usage_estimated() {
    let with_usage = vec![
        delta("partial"),
        failed("server_error", "Boom resp_0aB1c2D3e4F5", Some((80, 12))),
    ];
    let (events, row, h) = failure_of(Reply::Sse(with_usage)).await;
    let (code, message) = error_of(&events);
    assert_eq!(*code, StreamErrorCode::ProviderError);
    assert!(message.contains("Boom"), "{message}");
    assert!(!message.contains("resp_0aB1c2D3e4F5"), "{message}");
    assert_failed_turn(&row, "provider_error");
    // partial text of a failed turn is discarded
    assert_eq!(message_rows(&h.rig.quota.db, h.chat.id).await.len(), 1);
    let usage = h.rig.events().await;
    assert_eq!(usage[0].settlement_method, "actual");
    assert_eq!(
        usage[0].usage.map(|u| (u.input_tokens, u.output_tokens)),
        Some((80, 12))
    );

    let without = vec![failed("server_error", "Boom", None)];
    let (_events, _row, h) = failure_of(Reply::Sse(without)).await;
    assert_eq!(h.rig.events().await[0].settlement_method, "estimated");
}

#[tokio::test]
async fn an_error_event_is_a_provider_error() {
    let (events, row, _h) = failure_of(Reply::Sse(vec![
        delta("x"),
        error_event("overloaded", "The model is overloaded"),
    ]))
    .await;
    let (code, message) = error_of(&events);
    assert_eq!(*code, StreamErrorCode::ProviderError);
    assert!(message.contains("overloaded"), "{message}");
    assert_failed_turn(&row, "provider_error");
}

#[tokio::test]
async fn a_stream_that_ends_without_a_terminal_event_is_a_provider_error() {
    let (events, row, h) = failure_of(Reply::Sse(vec![delta("half an answer")])).await;
    assert_eq!(*error_of(&events).0, StreamErrorCode::ProviderError);
    assert_failed_turn(&row, "provider_error");
    assert_eq!(message_rows(&h.rig.quota.db, h.chat.id).await.len(), 1);
}

#[tokio::test]
async fn a_broken_provider_stream_is_a_provider_error() {
    let (events, row, _h) = failure_of(Reply::Sse(vec![delta("x"), Step::Break])).await;
    assert_eq!(*error_of(&events).0, StreamErrorCode::ProviderError);
    assert_failed_turn(&row, "provider_error");
}

#[tokio::test]
async fn an_idle_provider_stream_times_out_as_provider_timeout_and_is_dropped() {
    let upstream = MockProvider::answering(Reply::Sse(vec![delta("x"), Step::Hang]));
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream.clone())
            .with_task_settings(ProviderTaskSettings {
                idle_timeout: Duration::from_millis(150),
                ..rig_task_settings()
            }),
    )
    .await;
    let chat = rig.std_chat().await;
    let mut turn = rig.start_turn(&chat, &question()).await;
    let events = turn.drain().await;
    assert_eq!(*error_of(&events).0, StreamErrorCode::ProviderTimeout);
    assert_failed_turn(&turn_of(&rig, &chat).await, "provider_timeout");
    upstream.wait_for_aborted(1).await;
}

// ── the other adapter kinds ──────────────────────────────────────────────────

fn cc_chunk(delta: &Value, finish: Option<&str>) -> Step {
    Step::unnamed(
        &json!({ "id": "chatcmpl-AbC123def", "object": "chat.completion.chunk",
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }] }),
    )
}

async fn multi_rig(upstream: Arc<MockProvider>, model: &str) -> (SendRig, ChatRecord) {
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_snapshot(multi_catalog())
            .with_providers(multi_providers())
            .with_upstream(upstream),
    )
    .await;
    let chat = rig.quota.chat_a1(model).await;
    (rig, chat)
}

#[tokio::test]
async fn chat_completions_streams_through_its_adapter() {
    let steps = vec![
        cc_chunk(&json!({ "role": "assistant", "content": "" }), None),
        cc_chunk(&json!({ "content": "Hel" }), None),
        cc_chunk(&json!({ "content": "lo" }), None),
        cc_chunk(&json!({}), Some("stop")),
        Step::unnamed(&json!({ "id": "chatcmpl-AbC123def", "choices": [],
            "usage": { "prompt_tokens": 11, "completion_tokens": 7 } })),
        Step::Raw("data: [DONE]\n\n".to_owned()),
    ];
    let upstream = MockProvider::answering(Reply::Sse(steps));
    let (rig, chat) = multi_rig(upstream.clone(), CC_MODEL).await;
    let mut turn = rig.start_turn(&chat, &question()).await;
    let events = turn.drain().await;
    assert_eq!(text_of(&events), "Hello");
    let done = done_of(&events);
    assert_eq!((done.input_tokens, done.output_tokens), (11, 7));
    let call = upstream.only_call();
    assert_eq!(call.uri, "/cc.example.com/v1/chat/completions");
    assert_eq!(
        call.body["stream_options"],
        json!({ "include_usage": true })
    );
    assert_eq!(call.body["model"], format!("provider-{CC_MODEL}"));
    assert!(call.body.get("input").is_none());
    assert_eq!(turn_of(&rig, &chat).await.state, "completed");
}

#[tokio::test]
async fn a_chat_completions_length_stop_completes_the_turn_as_incomplete() {
    let steps = vec![
        cc_chunk(&json!({ "content": "cut" }), None),
        cc_chunk(&json!({}), Some("length")),
        Step::unnamed(
            &json!({ "choices": [], "usage": { "prompt_tokens": 4, "completion_tokens": 9 } }),
        ),
        Step::Raw("data: [DONE]\n\n".to_owned()),
    ];
    let upstream = MockProvider::answering(Reply::Sse(steps));
    let (rig, chat) = multi_rig(upstream, CC_MODEL).await;
    let mut turn = rig.start_turn(&chat, &question()).await;
    let events = turn.drain().await;
    done_of(&events);
    let row = turn_of(&rig, &chat).await;
    assert_eq!(
        (row.state.as_str(), row.error_code.clone()),
        ("completed", None)
    );
}

#[tokio::test]
async fn vllm_drops_the_tools_and_streams_think_blocks_as_reasoning() {
    let steps = vec![
        delta("<think>"),
        delta("pondering"),
        delta("</think>"),
        delta("Answer"),
        completed("<think>pondering</think>Answer", (9, 4)),
    ];
    let upstream = MockProvider::answering(Reply::Sse(steps));
    let (rig, chat) = multi_rig(upstream.clone(), VLLM_MODEL).await;
    let mut turn = rig
        .start_turn(
            &chat,
            &json!({ "content": "q", "web_search": { "enabled": true } }),
        )
        .await;
    let events = turn.drain().await;
    let reasoning: String = events
        .iter()
        .filter_map(|event| match event {
            RelayEvent::Delta(Delta {
                kind: DeltaKind::Reasoning,
                content,
            }) => Some(content.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning, "pondering");
    assert_eq!(text_of(&events), "Answer");
    let call = upstream.only_call();
    for absent in ["tools", "metadata", "max_tool_calls", "include"] {
        assert!(
            call.body.get(absent).is_none(),
            "`{absent}` in {}",
            call.body
        );
    }
    assert!(
        call.body["instructions"]
            .as_str()
            .unwrap()
            .contains("WEB-GUARD")
    );
}

#[tokio::test]
async fn anthropic_streams_text_and_maps_its_usage() {
    let named = |name: &str, data: Value| Step::named(name, &data);
    let steps = vec![
        named(
            "message_start",
            json!({ "type": "message_start", "message": { "id": "msg_01AbC",
            "role": "assistant", "usage": { "input_tokens": 25, "output_tokens": 1 } } }),
        ),
        named(
            "content_block_start",
            json!({ "type": "content_block_start", "index": 0,
            "content_block": { "type": "text", "text": "" } }),
        ),
        named(
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": 0,
            "delta": { "type": "text_delta", "text": "Hello" } }),
        ),
        named(
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 0 }),
        ),
        named(
            "message_delta",
            json!({ "type": "message_delta",
            "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 15 } }),
        ),
        named("message_stop", json!({ "type": "message_stop" })),
    ];
    let upstream = MockProvider::answering(Reply::Sse(steps));
    let (rig, chat) = multi_rig(upstream.clone(), ANTHROPIC_MODEL).await;
    let mut turn = rig.start_turn(&chat, &question()).await;
    let events = turn.drain().await;
    assert_eq!(text_of(&events), "Hello");
    let done = done_of(&events);
    assert_eq!((done.input_tokens, done.output_tokens), (25, 15));
    let call = upstream.only_call();
    assert_eq!(call.uri, "/anthropic.example.com/v1/messages");
    assert_eq!(
        call.body["max_tokens"],
        turn.plan.decision.max_output_tokens_applied
    );
    assert!(call.body["metadata"]["user_id"].is_string());
    assert!(call.body.get("input").is_none());
}

// ── wire robustness ──────────────────────────────────────────────────────────

#[tokio::test]
async fn events_split_across_chunks_joined_in_one_chunk_commented_or_unnamed_are_all_read() {
    let delta_json =
        |text: &str| json!({ "type": "response.output_text.delta", "delta": text }).to_string();
    let first = format!(
        "event: response.output_text.delta\ndata: {}\n\n",
        delta_json("AB")
    );
    let (head, tail) = first.split_at(first.len().div_ceil(2));
    let two = format!(
        "event: response.output_text.delta\ndata: {}\n\nevent: response.output_text.delta\ndata: {}\n\n",
        delta_json("C"),
        delta_json("D")
    );
    let steps = vec![
        Step::Raw(head.to_owned()),
        Step::Raw(tail.to_owned()),
        Step::Raw(two),
        Step::Raw(": keep-alive comment\n\n".to_owned()),
        // no `event:` line: the name is the `type` of the data
        Step::unnamed(&json!({ "type": "response.output_text.delta", "delta": "E" })),
        completed("ABCDE", (3, 5)),
    ];
    let h = harness(Reply::Sse(steps)).await;
    let mut turn = h.rig.start_turn(&h.chat, &question()).await;
    let events = turn.drain().await;
    assert_eq!(text_of(&events), "ABCDE");
    done_of(&events);
}

// ── no buffering and bounded channels ────────────────────────────────────────

#[tokio::test]
async fn a_slow_provider_is_relayed_event_by_event_as_it_delivers() {
    let steps = vec![
        Step::Delay(Duration::from_millis(40)),
        delta("a"),
        Step::Delay(Duration::from_millis(40)),
        delta("b"),
        Step::Delay(Duration::from_millis(40)),
        completed("ab", (1, 1)),
    ];
    let h = harness(Reply::Sse(steps)).await;
    let mut turn = h.rig.start_turn(&h.chat, &question()).await;
    let started = std::time::Instant::now();
    let first = turn.next().await.expect("the first delta");
    assert!(matches!(first, RelayEvent::Delta(_)));
    // the first delta is relayed while the provider still has delays to go
    assert!(started.elapsed() < Duration::from_millis(1500));
    let rest = turn.drain().await;
    assert_eq!(text_of(&rest), "b");
    done_of(&rest);
}

#[tokio::test]
async fn a_delta_is_relayed_before_the_provider_sends_the_next_event() {
    let gate = Gate::new();
    let steps = vec![
        delta("first"),
        Step::Wait(gate.clone()),
        delta("second"),
        completed("firstsecond", (1, 2)),
    ];
    let h = harness(Reply::Sse(steps)).await;
    let mut turn = h.rig.start_turn(&h.chat, &question()).await;
    // the provider is paused: the first delta must already be here
    let first = turn.next().await.expect("the first delta");
    assert_eq!(
        first,
        RelayEvent::Delta(Delta {
            kind: DeltaKind::Text,
            content: "first".to_owned()
        })
    );
    assert_eq!(turn_of(&h.rig, &h.chat).await.state, "running");
    gate.open();
    let rest = turn.drain().await;
    assert_eq!(text_of(&rest), "second");
    done_of(&rest);
}

#[tokio::test]
async fn a_slow_consumer_blocks_the_producer_instead_of_growing_memory() {
    let mut steps = numbered_deltas(60);
    steps.push(completed("x", (1, 1)));
    let upstream = MockProvider::answering(Reply::Sse(steps));
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream.clone())
            .with_task_settings(ProviderTaskSettings {
                channel_capacity: 2,
                ..rig_task_settings()
            }),
    )
    .await;
    let chat = rig.std_chat().await;
    let mut turn = rig.start_turn(&chat, &question()).await;
    // nobody reads: the producer stalls once the bounded channel is full
    tokio::time::sleep(Duration::from_millis(300)).await;
    let pulled = upstream.delivered_events();
    assert!(
        pulled < 20,
        "the producer ran ahead of the consumer: {pulled}"
    );
    // reading resumes the producer and nothing is lost or reordered
    let events = turn.drain().await;
    assert_eq!(text_of(&events), numbered_text(60));
    done_of(&events);
}

// ── durable progress ─────────────────────────────────────────────────────────

#[tokio::test]
async fn events_write_last_progress_at_when_30_seconds_passed_since_the_last_write() {
    let clock = FixedClock::at(default_now());
    let gate = Gate::new();
    let upstream = MockProvider::answering(Reply::Sse(vec![
        Step::Wait(gate.clone()),
        delta("late"),
        completed("late", (1, 1)),
    ]));
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream)
            .with_clock(clock.clone()),
    )
    .await;
    let chat = rig.std_chat().await;
    let mut turn = rig.start_turn(&chat, &question()).await;
    let later = default_now() + TimeDuration::seconds(100);
    clock.set(later);
    gate.open();
    turn.drain().await;
    assert_eq!(turn_of(&rig, &chat).await.last_progress_at, Some(later));
}

#[tokio::test]
async fn no_progress_is_written_within_30_seconds_of_the_last_write() {
    let clock = FixedClock::at(default_now());
    let gate = Gate::new();
    let upstream = MockProvider::answering(Reply::Sse(vec![
        Step::Wait(gate.clone()),
        delta("soon"),
        completed("soon", (1, 1)),
    ]));
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream)
            .with_clock(clock.clone()),
    )
    .await;
    let chat = rig.std_chat().await;
    let mut turn = rig.start_turn(&chat, &question()).await;
    let before = turn_of(&rig, &chat).await.last_progress_at;
    clock.set(default_now() + TimeDuration::seconds(10));
    gate.open();
    turn.drain().await;
    assert_eq!(turn_of(&rig, &chat).await.last_progress_at, before);
}

/// An event the translation ignores (`response.in_progress`).
fn ignored_event() -> Step {
    Step::named(
        "response.in_progress",
        &json!({ "type": "response.in_progress", "response": { "status": "in_progress" } }),
    )
}

#[tokio::test]
async fn ignored_provider_events_keep_a_live_stream_from_the_orphan_watchdog() {
    let timeout = Duration::from_secs(300);
    let clock = FixedClock::at(default_now());
    let (first, second, last) = (Gate::new(), Gate::new(), Gate::new());
    let upstream = MockProvider::answering(Reply::Sse(vec![
        Step::Wait(first.clone()),
        ignored_event(),
        Step::Wait(second.clone()),
        ignored_event(),
        Step::Wait(last.clone()),
        delta("alive"),
        completed("alive", (1, 1)),
    ]));
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(upstream)
            .with_clock(clock.clone()),
    )
    .await;
    let chat = rig.std_chat().await;
    let mut turn = rig.start_turn(&chat, &question()).await;

    // only ignored events arrive, over more than the watchdog timeout in total
    for (gate, seconds) in [(&first, 200), (&second, 400)] {
        let at = default_now() + TimeDuration::seconds(seconds);
        clock.set(at);
        gate.open();
        let mut written = false;
        for _ in 0..200 {
            if turn_of(&rig, &chat).await.last_progress_at == Some(at) {
                written = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(written, "an ignored event did not refresh the progress");
    }

    // 600 s after the start, 200 s after the last event: the turn is live
    clock.set(default_now() + TimeDuration::seconds(600));
    let report = rig.watchdog(timeout).scan_once().await.unwrap();
    assert_eq!(report.finalized, 0, "{report:?}");
    assert_eq!(turn_of(&rig, &chat).await.state, "running");

    // the stream later completes normally
    last.open();
    let events = turn.drain().await;
    assert_eq!(text_of(&events), "alive");
    done_of(&events);
    assert_eq!(turn_of(&rig, &chat).await.state, "completed");
}

#[tokio::test]
async fn progress_is_refreshed_while_a_slow_client_blocks_the_relay() {
    let clock = FixedClock::at(default_now());
    let mut steps = numbered_deltas(6);
    steps.push(completed("x", (1, 1)));
    let rig = send_rig_with(
        SendRigOptions::new()
            .with_upstream(MockProvider::answering(Reply::Sse(steps)))
            .with_clock(clock.clone())
            .with_task_settings(ProviderTaskSettings {
                channel_capacity: 1,
                progress_interval: Duration::from_millis(20),
                ..rig_task_settings()
            }),
    )
    .await;
    let chat = rig.std_chat().await;
    let mut turn = rig.start_turn(&chat, &question()).await;
    // nobody reads: the producer waits on the full channel; the clock moves on meanwhile
    let at = default_now() + TimeDuration::seconds(100);
    clock.set(at);
    let mut written = false;
    for _ in 0..200 {
        if turn_of(&rig, &chat).await.last_progress_at == Some(at) {
            written = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(written, "the blocked relay did not refresh the progress");
    let events = turn.drain().await;
    assert_eq!(text_of(&events), numbered_text(6));
}

// ── cancellation ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_cancelled_token_hard_cancels_the_provider_and_persists_the_partial_text() {
    let gate = Gate::new();
    let steps = vec![
        delta("part"),
        Step::Wait(gate),
        delta("late"),
        completed("partlate", (9, 9)),
    ];
    let h = harness(Reply::Sse(steps)).await;
    let mut turn = h.rig.start_turn(&h.chat, &question()).await;
    let first = turn.next().await.expect("the first delta");
    assert!(matches!(first, RelayEvent::Delta(_)));
    turn.cancel.cancel();
    // nothing more is sent: no `error`, no `done`
    let rest = turn.drain().await;
    assert!(rest.is_empty(), "{rest:?}");
    // the outbound request was hard-cancelled
    h.upstream.wait_for_aborted(1).await;
    assert_eq!(h.upstream.finished_streams(), 0);

    let row = h.rig.wait_terminal(h.chat.id).await;
    assert_eq!(row.state, "cancelled");
    assert_eq!(row.error_code, None);
    let messages = message_rows(&h.rig.quota.db, h.chat.id).await;
    let assistant = messages
        .iter()
        .find(|m| m.role == "assistant")
        .expect("the partial message");
    assert_eq!(assistant.content, "part");
    assert_eq!(row.assistant_message_id, Some(assistant.id));
    assert_eq!(reserved(&h.rig).await, 0);
    let usage = h.rig.events().await;
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].terminal_state, "cancelled");
    assert_eq!(usage[0].billing_outcome, "aborted");
    assert_eq!(usage[0].settlement_method, "estimated");
    assert_eq!(h.rig.audit_events().await.len(), 1);
}

#[tokio::test]
async fn a_cancellation_before_any_text_persists_no_message() {
    let gate = Gate::new();
    let h = harness(Reply::Sse(vec![
        Step::Wait(gate),
        delta("never"),
        completed("never", (1, 1)),
    ]))
    .await;
    let mut turn = h.rig.start_turn(&h.chat, &question()).await;
    h.upstream.wait_for_calls(1).await;
    turn.cancel.cancel();
    assert!(turn.drain().await.is_empty());
    h.upstream.wait_for_aborted(1).await;
    let row = h.rig.wait_terminal(h.chat.id).await;
    assert_eq!(row.state, "cancelled");
    assert_eq!(row.assistant_message_id, None);
    assert_eq!(message_rows(&h.rig.quota.db, h.chat.id).await.len(), 1);
}

#[tokio::test]
async fn a_failed_channel_send_is_the_same_disconnect() {
    let gate = Gate::new();
    let steps = vec![
        delta("first"),
        Step::Wait(gate.clone()),
        delta("second"),
        delta("third"),
        completed("firstsecondthird", (9, 9)),
    ];
    let h = harness(Reply::Sse(steps)).await;
    let mut turn = h.rig.start_turn(&h.chat, &question()).await;
    assert!(matches!(turn.next().await, Some(RelayEvent::Delta(_))));
    // the relay is gone: the next send fails
    let crate::test_support::send::RunningTurn { events, .. } = turn;
    drop(events);
    gate.open();
    let row = h.rig.wait_terminal(h.chat.id).await;
    assert_eq!(row.state, "cancelled");
    h.upstream.wait_for_aborted(1).await;
    assert_eq!(reserved(&h.rig).await, 0);
    let messages = message_rows(&h.rig.quota.db, h.chat.id).await;
    if let Some(assistant) = messages.iter().find(|m| m.role == "assistant") {
        assert!(
            assistant.content.starts_with("first"),
            "{}",
            assistant.content
        );
    }
}

#[tokio::test]
async fn a_token_cancelled_before_the_request_sends_nothing_to_oagw() {
    let upstream = MockProvider::new();
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let plan = rig.plan_for(&chat, "hi").await;
    let task = rig.task_over(
        upstream.clone(),
        S2sContext::bound(provider_s2s()),
        rig_task_settings(),
    );
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let mut receiver = crate::domain::streaming::ProviderTaskPort::start(&task, plan, cancel)
        .await
        .unwrap();
    assert!(receiver.recv().await.is_none());
    assert_eq!(upstream.call_count(), 0, "a request was sent");
    let row = rig.wait_terminal(chat.id).await;
    assert_eq!(row.state, "cancelled");
    assert_eq!(row.assistant_message_id, None);
}

#[tokio::test]
async fn a_task_without_the_s2s_context_cannot_start_and_sends_nothing() {
    let upstream = MockProvider::new();
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let plan = rig.plan_for(&chat, "hi").await;
    let task = rig.task_over(upstream.clone(), S2sContext::unbound(), rig_task_settings());
    let started = crate::domain::streaming::ProviderTaskPort::start(
        &task,
        plan,
        tokio_util::sync::CancellationToken::new(),
    )
    .await;
    assert!(started.is_err());
    assert_eq!(upstream.call_count(), 0);
}

// ── the terminal emission guard ──────────────────────────────────────────────

async fn gated_start(
    rig: &SendRig,
    chat: &ChatRecord,
    upstream: &Arc<MockProvider>,
) -> tokio::sync::mpsc::Receiver<RelayEvent> {
    let plan = rig.plan_for(chat, "hi").await;
    let task = rig.task_over(
        upstream.clone(),
        S2sContext::bound(provider_s2s()),
        rig_task_settings(),
    );
    crate::domain::streaming::ProviderTaskPort::start(
        &task,
        plan,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap()
}

async fn drain(receiver: &mut tokio::sync::mpsc::Receiver<RelayEvent>) -> Vec<RelayEvent> {
    let mut events = Vec::new();
    let read = async {
        while let Some(event) = receiver.recv().await {
            events.push(event);
        }
    };
    tokio::time::timeout(Duration::from_secs(10), read)
        .await
        .expect("channel closes");
    events
}

#[tokio::test]
async fn a_failed_finalization_of_a_completed_stream_sends_finalization_failed_not_done() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    fail_outbox_inserts(&rig.quota.db).await;
    let upstream = MockProvider::answering(Reply::Sse(happy_steps(&["answer"], (5, 5))));
    let mut receiver = gated_start(&rig, &chat, &upstream).await;
    let events = drain(&mut receiver).await;
    assert!(
        !events.iter().any(|e| matches!(e, RelayEvent::Done(_))),
        "{events:?}"
    );
    let (code, message) = error_of(&events);
    assert_eq!(*code, StreamErrorCode::FinalizationFailed);
    assert!(!message.is_empty());
    // the turn stays running until the orphan watchdog finalizes it
    let row = turn_of(&rig, &chat).await;
    assert_eq!(row.state, "running");
    assert_eq!(row.assistant_message_id, None);
    assert_eq!(message_rows(&rig.quota.db, chat.id).await.len(), 1);
}

#[tokio::test]
async fn a_failed_finalization_on_an_error_path_still_sends_the_original_code() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    fail_outbox_inserts(&rig.quota.db).await;
    let upstream =
        MockProvider::answering(Reply::http(500, &json!({ "error": { "message": "boom" } })));
    let mut receiver = gated_start(&rig, &chat, &upstream).await;
    let events = drain(&mut receiver).await;
    assert_eq!(*error_of(&events).0, StreamErrorCode::ProviderError);
    assert_eq!(turn_of(&rig, &chat).await.state, "running");
}

#[tokio::test]
async fn a_message_persistence_failure_sends_that_error_and_fails_the_turn() {
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    raw_exec(
        &rig.quota.db.raw,
        "CREATE TRIGGER fail_assistant_insert BEFORE INSERT ON messages \
         WHEN NEW.role = 'assistant' BEGIN SELECT RAISE(ABORT, 'forced insert failure'); END",
    )
    .await
    .unwrap();
    let upstream = MockProvider::answering(Reply::Sse(happy_steps(&["answer"], (5, 5))));
    let mut receiver = gated_start(&rig, &chat, &upstream).await;
    let events = drain(&mut receiver).await;
    assert!(
        !events.iter().any(|e| matches!(e, RelayEvent::Done(_))),
        "{events:?}"
    );
    assert_eq!(
        *error_of(&events).0,
        StreamErrorCode::MessagePersistenceFailed
    );
    let row = turn_of(&rig, &chat).await;
    assert_failed_turn(&row, "message_persistence_failed");
}

#[tokio::test]
async fn a_finalizer_that_won_first_makes_the_provider_terminal_a_no_op() {
    let gate = Gate::new();
    let rig = send_rig().await;
    let chat = rig.std_chat().await;
    let upstream = MockProvider::answering(Reply::Sse(vec![
        delta("part"),
        Step::Wait(gate.clone()),
        completed("part", (5, 5)),
    ]));
    let mut receiver = gated_start(&rig, &chat, &upstream).await;
    assert!(matches!(receiver.recv().await, Some(RelayEvent::Delta(_))));
    // another finalizer (the orphan watchdog) wins the CAS first
    let running = turn_of(&rig, &chat).await;
    let plan_target = TurnTarget {
        tenant_id: running.tenant_id,
        turn_id: running.id,
        selected_model: STANDARD.to_owned(),
        period_starts: crate::domain::quota::PeriodStarts::of(running.started_at),
    };
    rig.finalizer
        .finalize(
            &plan_target,
            TurnOutcome::Failed {
                code: StreamErrorCode::ProviderError,
            },
        )
        .await
        .unwrap();
    gate.open();
    // the loser emits nothing (the relay synthesizes `stream_interrupted`)
    let rest = drain(&mut receiver).await;
    assert!(rest.is_empty(), "{rest:?}");
    let row = turn_of(&rig, &chat).await;
    assert_eq!(row.state, "failed");
    assert_eq!(row.assistant_message_id, None);
    assert_eq!(message_rows(&rig.quota.db, chat.id).await.len(), 1);
    assert_eq!(rig.events().await.len(), 1);
    assert_eq!(rig.audit_events().await.len(), 1);
}

// ── deferred scope stays unimplemented ───────────────────────────────────────

#[tokio::test]
async fn no_assistant_row_exists_while_the_stream_runs_and_no_completion_signal_is_stored() {
    let gate = Gate::new();
    let steps = vec![
        delta("part"),
        Step::Wait(gate.clone()),
        incomplete("max_output_tokens", (1, 2)),
    ];
    let h = harness(Reply::Sse(steps)).await;
    let mut turn = h.rig.start_turn(&h.chat, &question()).await;
    assert!(matches!(turn.next().await, Some(RelayEvent::Delta(_))));
    // no incremental partial-delta persistence: only the user message exists until the terminal
    assert_eq!(message_rows(&h.rig.quota.db, h.chat.id).await.len(), 1);
    gate.open();
    turn.drain().await;
    // `completion_signal` is not implemented: no payload carries it
    let usage = serde_json::to_string(&h.rig.events().await).unwrap();
    let audit = h
        .rig
        .audit_payloads()
        .await
        .iter()
        .map(Value::to_string)
        .collect::<String>();
    for payload in [usage, audit] {
        assert!(!payload.contains("completion_signal"), "{payload}");
        assert!(!payload.contains("max_output_tokens"), "{payload}");
    }
}

#[tokio::test]
async fn an_overshooting_stream_is_never_aborted_by_the_quota() {
    // no mid-stream quota abort: a huge actual usage still completes and settles
    let upstream = MockProvider::answering(Reply::Sse(vec![
        delta("a"),
        delta("b"),
        completed("ab", (5_000_000, 5_000_000)),
    ]));
    let rig = send_rig_with(SendRigOptions::new().with_upstream(upstream)).await;
    let chat = rig.std_chat().await;
    let mut turn = rig.start_turn(&chat, &question()).await;
    let events = turn.drain().await;
    assert_eq!(text_of(&events), "ab");
    let done = done_of(&events);
    assert_eq!(done.input_tokens, 5_000_000);
    assert_eq!(turn_of(&rig, &chat).await.state, "completed");
    assert_eq!(rig.events().await[0].settlement_method, "actual");
}

#[tokio::test]
async fn anthropic_web_search_produces_tool_events_but_no_citations() {
    let named = |name: &str, data: Value| Step::named(name, &data);
    let steps = vec![
        named(
            "message_start",
            json!({ "type": "message_start", "message": { "id": "msg_01AbC",
            "usage": { "input_tokens": 10, "output_tokens": 1 } } }),
        ),
        named(
            "content_block_start",
            json!({ "type": "content_block_start", "index": 0,
            "content_block": { "type": "server_tool_use", "id": "srvtoolu_01", "name": "web_search",
                                "input": {} } }),
        ),
        named(
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 0 }),
        ),
        named(
            "content_block_start",
            json!({ "type": "content_block_start", "index": 1,
            "content_block": { "type": "web_search_tool_result", "tool_use_id": "srvtoolu_01",
                "content": [{ "type": "web_search_result", "url": "https://example.com/a",
                               "title": "A" }] } }),
        ),
        named(
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 1 }),
        ),
        named(
            "content_block_start",
            json!({ "type": "content_block_start", "index": 2,
            "content_block": { "type": "text", "text": "" } }),
        ),
        named(
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": 2,
            "delta": { "type": "text_delta", "text": "See A." } }),
        ),
        named(
            "content_block_delta",
            json!({ "type": "content_block_delta", "index": 2,
            "delta": { "type": "citations_delta", "citation": { "type": "web_search_result_location",
                "url": "https://example.com/a", "title": "A", "cited_text": "x" } } }),
        ),
        named(
            "content_block_stop",
            json!({ "type": "content_block_stop", "index": 2 }),
        ),
        named(
            "message_delta",
            json!({ "type": "message_delta",
            "delta": { "stop_reason": "end_turn" }, "usage": { "output_tokens": 15 } }),
        ),
        named("message_stop", json!({ "type": "message_stop" })),
    ];
    let upstream = MockProvider::answering(Reply::Sse(steps));
    let (rig, chat) = multi_rig(upstream, ANTHROPIC_MODEL).await;
    let mut turn = rig
        .start_turn(
            &chat,
            &json!({ "content": "q", "web_search": { "enabled": true } }),
        )
        .await;
    let events = turn.drain().await;
    assert!(
        !events.iter().any(|e| matches!(e, RelayEvent::Citations(_))),
        "{events:?}"
    );
    let tool_names: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            RelayEvent::Tool(tool) => Some(tool.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tool_names, vec!["web_search", "web_search"]);
    assert_eq!(text_of(&events), "See A.");
    done_of(&events);
    assert_eq!(turn_of(&rig, &chat).await.web_search_completed_count, 1);
}

#[tokio::test]
async fn a_huge_error_body_is_read_bounded_and_never_reaches_the_client() {
    let body = format!("<html><body>{}</body></html>", "x".repeat(3 * 1024 * 1024));
    let (events, row, _h) = failure_of(Reply::Http {
        status: 502,
        headers: vec![("content-type".to_owned(), "text/html".to_owned())],
        body,
        from_gateway: false,
    })
    .await;
    let (code, message) = error_of(&events);
    assert_eq!(*code, StreamErrorCode::ProviderError);
    assert!(message.len() < 500, "{}", message.len());
    assert!(row.error_detail.unwrap_or_default().len() < 500);
}

#[tokio::test]
async fn images_reach_the_provider_by_their_provider_file_id() {
    let upstream = MockProvider::answering(Reply::Sse(happy_steps(&["ok"], (1, 1))));
    let rig = stream_rig(upstream.clone()).await;
    let chat = rig.prem_chat().await;
    let one = crate::test_support::send::seed_att(
        &rig.quota.db,
        &chat,
        crate::test_support::send::AttachmentSeed::ready("image"),
    )
    .await;
    let mut turn = rig
        .start_turn(
            &chat,
            &json!({ "content": "look", "attachment_ids": [one.to_string()] }),
        )
        .await;
    let events = turn.drain().await;
    let rendered = format!("{events:?}");
    let call = upstream.only_call();
    let parts = call.body["input"].as_array().unwrap().last().unwrap()["content"].clone();
    assert_eq!(parts[0], json!({ "type": "input_text", "text": "look" }));
    assert_eq!(parts[1]["type"], "input_image");
    let file_id = parts[1]["file_id"].as_str().unwrap().to_owned();
    assert!(file_id.starts_with("file-"), "{file_id}");
    // the internal provider file id never reaches the client
    assert!(!rendered.contains(&file_id), "{rendered}");
}

// ── message content is never logged ──────────────────────────────────────────

#[tracing_test::traced_test]
#[tokio::test]
async fn the_message_content_and_the_request_body_are_never_logged() {
    let secret = "TOP-SECRET-CONTENT-42";
    let h = harness(Reply::Sse(happy_steps(&[secret], (1, 1)))).await;
    let mut turn = h
        .rig
        .start_turn(
            &h.chat,
            &json!({ "content": format!("my {secret} question") }),
        )
        .await;
    turn.drain().await;
    let failing = harness(Reply::http(500, &json!({ "error": { "message": "boom" } }))).await;
    let mut turn = failing
        .rig
        .start_turn(
            &failing.chat,
            &json!({ "content": format!("my {secret} question") }),
        )
        .await;
    turn.drain().await;
    assert!(!logs_contain(secret), "the message content was logged");
}
