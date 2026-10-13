//! Web search under quota: tool inclusion and guard, the per-message call limit enforced mid-turn,
//! the daily quota checked at preflight, call accounting in `quota_usage` / usage / audit events,
//! the kill switch, and the per-message code interpreter limit.
//!
//! Source of truth: DESIGN 4 "Web Search Configuration" and "Web Search Quota Enforcement (P1
//! Determinism Rules)", "Code Interpreter Configuration" / "Code Interpreter Quota Enforcement",
//! 2.2 kill switches, 3.7 `quota_usage` (`web_search_calls` / `code_interpreter_calls` on bucket
//! `total` only, added on actual and estimated settlements), 5.5.6 (web search surcharge), 5.8 /
//! 5.9 (`web_search_calls_exceeded` is a post-provider-start failure settled estimated), REST
//! error table (429 `resource_exhausted` subject `web_search`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

mod chat_support;
mod common;
mod quota_support;
mod stream_support;

use chat_support::assert_precondition;
use mini_chat_sdk::{AuditEvent, BillingOutcome, SettlementMethod, TerminalState};
use quota_support::{
    LimitsSpec, Opts, PREMIUM_IN, PREMIUM_OUT, ProviderReply, QuotaHarness, QuotaOptions, STANDARD,
    UsageSeed, assert_exhausted, assert_problem, ctx_a, file_search_pieces, ref_credits,
    web_annotation, web_search_pieces,
};
use serde_json::json;
use stream_support::{WEB_GUARD, code_interpreter_pieces};
use uuid::Uuid;

const MSG: &str = "latest news";

fn roomy() -> LimitsSpec {
    LimitsSpec::new(1_000_000_000, 10_000_000_000, 500_000_000, 5_000_000_000)
}

/// `n` complete web searches after the first delta.
fn searches(n: usize) -> Opts {
    let pieces: Vec<_> = (0..n).flat_map(|_| web_search_pieces()).collect();
    Opts::default().usage(900, 100).insert_after(1, pieces)
}

fn premium_credits(input: u64, output: u64) -> i64 {
    ref_credits(input, output, PREMIUM_IN, PREMIUM_OUT)
}

/// The estimated charge of a premium web search turn of `MSG`: 114 input tokens (the message) +
/// 500 (the web search surcharge) plus the 50 token floor.
fn estimated_web_turn() -> i64 {
    // "latest news" = 11 bytes -> ceil((3 + 100) * 110 / 100) = 114 input tokens
    premium_credits(114 + 500, 50)
}

// ═════════════════════════════════════════════════════════════════════════════
// tool inclusion, guard, relay
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_tool_and_its_guard_are_sent_when_enabled_and_supported() {
    let h = QuotaHarness::with_limits(roomy()).await;
    let chat = h.seed_chat().await;
    h.oagw
        .push(ProviderReply::text_with(&["a", "b"], &searches(1)));

    let sent = h.say_web(chat, MSG).await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");

    let request = h.oagw.last_request();
    let tool = request
        .tool("web_search")
        .expect("the web_search tool is sent");
    assert_eq!(
        tool["search_context_size"],
        json!("medium"),
        "the catalog context size"
    );
    assert!(
        request.system_text().contains(WEB_GUARD),
        "the guard is appended to the system prompt: {}",
        request.system_text()
    );
    // the provider's tool events are relayed
    let tools: Vec<_> = sent.of("tool").iter().map(|e| e.data.clone()).collect();
    assert!(
        tools
            .iter()
            .any(|t| t["name"] == json!("web_search") && t["phase"] == json!("start")),
        "{tools:?}"
    );
    assert!(
        tools
            .iter()
            .any(|t| t["name"] == json!("web_search") && t["phase"] == json!("done")),
        "{tools:?}"
    );
}

#[tokio::test]
async fn no_tool_no_guard_and_no_daily_check_without_the_request_flag_or_model_support() {
    let h = QuotaHarness::with_limits(roomy()).await;
    h.seed_usage(UsageSeed::total("daily").web_search_calls(10_000))
        .await;

    // flag absent on the premium model: no tool, no guard, never rejected by the daily quota
    let chat = h.seed_chat().await;
    let sent = h.say(chat, MSG).await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");
    let request = h.oagw.last_request();
    assert!(request.tool("web_search").is_none());
    assert!(!request.system_text().contains(WEB_GUARD));

    // flag set on a model without web search support (standard): proceeds without the tool
    let chat = h.seed_chat_on(STANDARD).await;
    let sent = h.say_web(chat, MSG).await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");
    let request = h.oagw.last_request();
    assert!(request.tool("web_search").is_none());
    assert!(!request.system_text().contains(WEB_GUARD));
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert!(turn.web_search_enabled, "the request flag is still stored");
}

#[tokio::test]
async fn a_downgrade_to_a_model_without_web_search_drops_the_tool_the_guard_and_the_quota_check() {
    let h = QuotaHarness::with_limits(roomy()).await;
    h.policy
        .set_kill_switches(json!({"force_standard_tier": true}));
    // the daily web search quota is exhausted, but the effective model sends no tool
    h.seed_usage(UsageSeed::total("daily").web_search_calls(10_000))
        .await;
    let chat = h.seed_chat().await;

    let sent = h.say_web(chat, MSG).await;

    assert_eq!(sent.names_no_ping().last().unwrap(), "done");
    assert_eq!(sent.done()["effective_model"], json!(STANDARD));
    let request = h.oagw.last_request();
    assert!(request.tool("web_search").is_none());
    assert!(!request.system_text().contains(WEB_GUARD));
    // and no surcharge was reserved for the tool that is not sent
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    let (_, _, _, credits) = quota_support::ref_reserve(
        &quota_support::find_model(&quota_support::quota_models(), STANDARD),
        32_768,
        MSG,
        0,
        0,
    );
    assert_eq!(turn.reserved_credits_micro, Some(credits));
}

#[tokio::test]
async fn the_kill_switch_rejects_a_web_search_request_with_feature_disabled() {
    let h = QuotaHarness::with_limits(roomy()).await;
    h.policy
        .set_kill_switches(json!({"disable_web_search": true}));
    let chat = h.seed_chat().await;
    let before = h.row_counts().await;

    let resp = h
        .reject(
            chat,
            json!({"content": MSG, "web_search": {"enabled": true}}),
        )
        .await;
    let problem = assert_problem(&resp, 400, "failed_precondition");
    assert_precondition(&problem, "web_search", "FEATURE_DISABLED");
    h.oagw.assert_no_provider_call();
    assert_eq!(h.row_counts().await, before);

    // requests that do not use web search are unaffected
    let sent = h.say(chat, "plain").await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");
}

#[tokio::test]
async fn web_citations_reach_the_client_before_done() {
    let h = QuotaHarness::with_limits(roomy()).await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text_with(
        &["See the source."],
        &searches(1).annotated(vec![web_annotation(
            "https://example.test/a",
            "Example A",
            4,
            14,
        )]),
    ));
    let sent = h.say_web(chat, MSG).await;
    let names = sent.names_no_ping();
    assert_eq!(names[names.len() - 2], "citations", "{names:?}");
    let items = sent.of("citations")[0].data["items"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(items[0]["source"], json!("web"));
    assert_eq!(items[0]["url"], json!("https://example.test/a"));
    assert!(sent.done().get("quota_warnings").is_some());
}

// ═════════════════════════════════════════════════════════════════════════════
// per-message limit: web_search_calls_exceeded
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_third_search_start_fails_the_turn_with_web_search_calls_exceeded() {
    let h = QuotaHarness::with_limits(roomy()).await; // quota.web_search_max_calls_per_message = 2
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.oagw
        .push(ProviderReply::text_with(&["a", "b"], &searches(3)));

    let sent = h
        .send(
            chat,
            &json!({"content": MSG, "request_id": rid, "web_search": {"enabled": true}}),
        )
        .await;

    assert_eq!(sent.names_no_ping().last().unwrap(), "error");
    assert!(sent.of("done").is_empty());
    assert_eq!(sent.error()["code"], json!("web_search_calls_exceeded"));
    let starts = sent
        .of("tool")
        .iter()
        .filter(|e| e.data["name"] == json!("web_search") && e.data["phase"] == json!("start"))
        .count();
    assert!(
        (2..=3).contains(&starts),
        "two allowed starts, at most the offending one: {starts}"
    );

    // the provider stream was cancelled (hard cancel) and the turn failed
    h.oagw.wait_dropped_early(1).await;
    let turn = h.turn_of(rid).await.unwrap();
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("web_search_calls_exceeded")
    );
    assert_eq!(turn.web_search_completed_count, 2, "completed calls only");
    assert!(
        h.chat
            .all_messages()
            .await
            .iter()
            .all(|m| m.role != "assistant"),
        "a failed turn stores no answer"
    );
    let status = h.turn_status(chat, rid, &ctx_a()).await.json();
    assert_eq!(status["state"], json!("error"));
    assert_eq!(status["error_code"], json!("web_search_calls_exceeded"));
}

#[tokio::test]
async fn the_limit_breach_settles_estimated_with_no_refund_of_the_surcharge() {
    let h = QuotaHarness::with_limits(roomy()).await;
    let chat = h.seed_chat().await;
    h.oagw
        .push(ProviderReply::text_with(&["a", "b"], &searches(3)));
    h.say_web(chat, MSG).await;

    // the stream is cancelled before usage arrives: estimated = 614 input tokens (message +
    // web surcharge, never refunded) + the 50 token floor
    for period in ["daily", "monthly"] {
        let total = h.bucket("total", period).await.unwrap();
        assert_eq!(total.spent_credits_micro, estimated_web_turn(), "{period}");
        assert_eq!(total.reserved_credits_micro, 0);
        assert_eq!(total.calls, 1);
        assert_eq!((total.input_tokens, total.output_tokens), (0, 0));
        assert_eq!(
            total.web_search_calls, 2,
            "added on estimated settlements: completed calls"
        );
    }

    let event = h.policy.wait_published(1).await.remove(0);
    assert_eq!(event.terminal_state, TerminalState::Failed);
    assert_eq!(event.billing_outcome, BillingOutcome::Failed);
    assert_eq!(event.settlement_method, SettlementMethod::Estimated);
    assert_eq!(event.usage, None);
    assert_eq!(event.actual_credits_micro, estimated_web_turn());
    assert_eq!(event.web_search_calls, 2);

    let audit = h.audit.wait_events(1).await;
    let AuditEvent::TurnFailed(e) = &audit[0] else {
        panic!("expected turn_failed: {:?}", audit[0])
    };
    assert_eq!(e.error_code.as_deref(), Some("web_search_calls_exceeded"));
    assert_eq!(e.web_search_calls, 2);
}

#[tokio::test]
async fn exactly_the_limit_is_allowed_and_the_limit_is_configurable() {
    // two searches with the default limit of 2: fine
    let h = QuotaHarness::with_limits(roomy()).await;
    let chat = h.seed_chat().await;
    h.oagw
        .push(ProviderReply::text_with(&["a", "b"], &searches(2)));
    let sent = h.say_web(chat, MSG).await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");

    // the counter is per turn: another turn may search twice again
    h.oagw
        .push(ProviderReply::text_with(&["a", "b"], &searches(2)));
    let sent = h.say_web(chat, "again").await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");

    // limit 1: the second search start breaches it
    let h = QuotaHarness::with(QuotaOptions {
        config: json!({"quota": {"web_search_max_calls_per_message": 1}}),
        limits: roomy(),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    h.oagw
        .push(ProviderReply::text_with(&["a", "b"], &searches(2)));
    let sent = h.say_web(chat, MSG).await;
    assert_eq!(sent.error()["code"], json!("web_search_calls_exceeded"));
    assert_eq!(
        h.turn_of(sent.request_id())
            .await
            .unwrap()
            .web_search_completed_count,
        1
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// daily quota: 429 resource_exhausted, subject web_search
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn an_exhausted_daily_web_search_quota_is_a_429_before_any_provider_call() {
    let h = QuotaHarness::with(QuotaOptions {
        config: json!({"quota": {"web_search_daily_quota": 3}}),
        limits: roomy(),
        ..Default::default()
    })
    .await;
    h.seed_usage(UsageSeed::total("daily").web_search_calls(3))
        .await;
    let chat = h.seed_chat().await;
    let before = h.row_counts().await;

    let resp = h
        .reject(
            chat,
            json!({"content": MSG, "web_search": {"enabled": true}}),
        )
        .await;

    assert_exhausted(&resp, "web_search");
    h.oagw.assert_no_provider_call();
    assert_eq!(h.row_counts().await, before, "no turn, message or reserve");
    h.policy.assert_published_stays(0).await;

    // a request that does not use web search is never rejected by this quota
    let sent = h.say(chat, "no search").await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");
}

#[tokio::test]
async fn one_call_below_the_daily_quota_is_admitted_and_the_quota_then_closes() {
    let h = QuotaHarness::with(QuotaOptions {
        config: json!({"quota": {"web_search_daily_quota": 3}}),
        limits: roomy(),
        ..Default::default()
    })
    .await;
    h.seed_usage(UsageSeed::total("daily").web_search_calls(2))
        .await;
    let chat = h.seed_chat().await;
    h.oagw
        .push(ProviderReply::text_with(&["a", "b"], &searches(1)));
    let sent = h.say_web(chat, MSG).await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");
    assert_eq!(
        h.bucket("total", "daily").await.unwrap().web_search_calls,
        3
    );

    let resp = h
        .reject(
            chat,
            json!({"content": "again", "web_search": {"enabled": true}}),
        )
        .await;
    assert_exhausted(&resp, "web_search");
}

#[tokio::test]
async fn only_the_daily_total_row_is_read_by_the_daily_web_search_check() {
    let h = QuotaHarness::with(QuotaOptions {
        config: json!({"quota": {"web_search_daily_quota": 3}}),
        limits: roomy(),
        ..Default::default()
    })
    .await;
    // the monthly row and yesterday's row do not count against today
    h.seed_usage(UsageSeed::total("monthly").web_search_calls(500))
        .await;
    h.seed_usage(UsageSeed::total("daily").web_search_calls(500).stale())
        .await;
    let chat = h.seed_chat().await;
    let sent = h.say_web(chat, MSG).await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");
}

#[tokio::test]
async fn token_exhaustion_is_reported_before_the_web_search_scope() {
    let h = QuotaHarness::with(QuotaOptions {
        config: json!({"quota": {"web_search_daily_quota": 3}}),
        limits: LimitsSpec::new(1_000_000, 10_000_000, 500_000, 5_000_000),
        ..Default::default()
    })
    .await;
    h.seed_usage(
        UsageSeed::total("daily")
            .spent(1_000_000)
            .web_search_calls(3),
    )
    .await;
    let chat = h.seed_chat().await;
    let resp = h
        .reject(
            chat,
            json!({"content": MSG, "web_search": {"enabled": true}}),
        )
        .await;
    assert_exhausted(&resp, "tokens");
}

// ═════════════════════════════════════════════════════════════════════════════
// accounting
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn completed_searches_are_counted_in_the_total_bucket_the_usage_and_the_audit_event() {
    let h = QuotaHarness::with_limits(roomy()).await;
    let chat = h.seed_chat().await;
    h.oagw
        .push(ProviderReply::text_with(&["a", "b"], &searches(2)));
    let sent = h.say_web(chat, MSG).await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");

    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert_eq!(turn.web_search_completed_count, 2);
    for period in ["daily", "monthly"] {
        let total = h.bucket("total", period).await.unwrap();
        assert_eq!(total.web_search_calls, 2, "total/{period}");
        let premium = h.bucket("tier:premium", period).await.unwrap();
        assert_eq!(
            premium.web_search_calls, 0,
            "tool counters live in bucket total only"
        );
        assert_eq!(total.code_interpreter_calls, 0);
        assert_eq!(total.file_search_calls, 0);
    }
    // the reserve held 114 + 500 estimated input tokens (web search surcharge); 900 / 100 settled
    assert_eq!(
        h.bucket("total", "daily")
            .await
            .unwrap()
            .spent_credits_micro,
        premium_credits(900, 100)
    );

    let event = h.policy.wait_published(1).await.remove(0);
    assert_eq!(event.web_search_calls, 2);
    assert_eq!(
        (event.code_interpreter_calls, event.file_search_calls),
        (0, 0)
    );
    let audit = h.audit.wait_events(1).await;
    let AuditEvent::TurnCompleted(e) = &audit[0] else {
        panic!("expected turn_completed: {:?}", audit[0])
    };
    assert_eq!(e.web_search_calls, 2);
}

#[tokio::test]
async fn searches_of_a_cancelled_turn_are_counted_too() {
    let h = QuotaHarness::with_limits(roomy()).await;
    let chat = h.seed_chat().await;
    let rid = Uuid::new_v4();
    h.oagw.push(ProviderReply::text_with(
        &["a", "b"],
        &searches(1).insert_after(2, vec![stream_support::Piece::Pause]),
    ));
    let mut live = h
        .open(
            chat,
            &ctx_a(),
            &json!({"content": MSG, "request_id": rid, "web_search": {"enabled": true}}),
        )
        .await
        .live();
    live.expect("stream_started").await;
    live.next_non_ping().await;
    h.oagw.wait_paused(1).await;
    drop(live);
    h.wait_turn_state(rid, "cancelled").await;

    assert_eq!(
        h.bucket("total", "daily").await.unwrap().web_search_calls,
        1
    );
    let event = h.policy.wait_published(1).await.remove(0);
    assert_eq!(event.billing_outcome, BillingOutcome::Aborted);
    assert_eq!(event.web_search_calls, 1);
}

#[tokio::test]
async fn a_turn_without_searches_adds_no_search_calls() {
    let h = QuotaHarness::with_limits(roomy()).await;
    let chat = h.seed_chat().await;
    h.say_web(chat, MSG).await; // the tool is available, the provider did not use it
    assert_eq!(
        h.bucket("total", "daily").await.unwrap().web_search_calls,
        0
    );
    let event = h.policy.wait_published(1).await.remove(0);
    assert_eq!(event.web_search_calls, 0);
}

#[tokio::test]
async fn the_daily_quota_follows_the_accounted_calls_end_to_end() {
    let h = QuotaHarness::with(QuotaOptions {
        config: json!({"quota": {"web_search_daily_quota": 2, "web_search_max_calls_per_message": 2}}),
        limits: roomy(),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    h.oagw
        .push(ProviderReply::text_with(&["a", "b"], &searches(2)));
    let sent = h.say_web(chat, MSG).await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");

    let resp = h
        .reject(
            chat,
            json!({"content": "again", "web_search": {"enabled": true}}),
        )
        .await;
    assert_exhausted(&resp, "web_search");
}

#[tokio::test]
async fn file_search_calls_are_reported_but_never_accounted_in_quota_usage() {
    let h = QuotaHarness::with_limits(roomy()).await;
    let chat = h.seed_chat().await;
    h.seed_document_with_store(chat).await;
    h.oagw.push(ProviderReply::text_with(
        &["a", "b"],
        &Opts::default()
            .usage(100, 10)
            .insert_after(1, file_search_pieces(Some(2))),
    ));
    let sent = h.say(chat, "what does the document say").await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");

    let event = h.policy.wait_published(1).await.remove(0);
    assert_eq!(event.file_search_calls, 1);
    for period in ["daily", "monthly"] {
        assert_eq!(
            h.bucket("total", period).await.unwrap().file_search_calls,
            0,
            "quota_usage.file_search_calls is reserved and not populated (ADR-0007)"
        );
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// code interpreter: per-message limit (the daily check needs attachments of slice S6)
// ═════════════════════════════════════════════════════════════════════════════

fn interpreter_runs(n: usize) -> Opts {
    let pieces: Vec<_> = (0..n)
        .flat_map(|i| code_interpreter_pieces(&format!("run {i}")))
        .collect();
    Opts::default().usage(900, 100).insert_after(1, pieces)
}

#[tokio::test]
async fn the_code_interpreter_per_message_limit_fails_the_turn_like_the_web_search_one() {
    let h = QuotaHarness::with(QuotaOptions {
        config: json!({"quota": {"code_interpreter_max_calls_per_message": 2}}),
        limits: roomy(),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    h.oagw
        .push(ProviderReply::text_with(&["a", "b"], &interpreter_runs(3)));

    let sent = h.say(chat, "run the analysis").await;

    assert_eq!(
        sent.error()["code"],
        json!("code_interpreter_calls_exceeded")
    );
    h.oagw.wait_dropped_early(1).await;
    let turn = h.turn_of(sent.request_id()).await.unwrap();
    assert_eq!(turn.state, "failed");
    assert_eq!(
        turn.error_code.as_deref(),
        Some("code_interpreter_calls_exceeded")
    );
    assert_eq!(turn.code_interpreter_completed_count, 2);

    // failed after the provider started, no usage: estimated; the calls that completed are counted
    let event = h.policy.wait_published(1).await.remove(0);
    assert_eq!(event.billing_outcome, BillingOutcome::Failed);
    assert_eq!(event.settlement_method, SettlementMethod::Estimated);
    assert_eq!(event.code_interpreter_calls, 2);
    for period in ["daily", "monthly"] {
        assert_eq!(
            h.bucket("total", period)
                .await
                .unwrap()
                .code_interpreter_calls,
            2
        );
    }
}

#[tokio::test]
async fn code_interpreter_runs_up_to_the_limit_complete_and_are_accounted() {
    let h = QuotaHarness::with(QuotaOptions {
        config: json!({"quota": {"code_interpreter_max_calls_per_message": 2}}),
        limits: roomy(),
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    h.oagw
        .push(ProviderReply::text_with(&["a", "b"], &interpreter_runs(2)));
    let sent = h.say(chat, "run the analysis").await;
    assert_eq!(sent.names_no_ping().last().unwrap(), "done");
    assert_eq!(
        h.bucket("total", "daily")
            .await
            .unwrap()
            .code_interpreter_calls,
        2
    );
    let event = h.policy.wait_published(1).await.remove(0);
    assert_eq!(event.code_interpreter_calls, 2);
    assert_eq!(
        h.bucket("tier:premium", "daily")
            .await
            .unwrap()
            .code_interpreter_calls,
        0
    );
}
