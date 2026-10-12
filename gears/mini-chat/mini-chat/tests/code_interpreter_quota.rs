//! Code interpreter quota enforcement at preflight (deferred S4 finding F-002) and the per-message
//! call limit with a real spreadsheet attachment.
//!
//! Source of truth: DESIGN "Code Interpreter Configuration" (`quota.code_interpreter_daily_quota`,
//! default 50, `quota.code_interpreter_max_calls_per_message`, default 10), "Code Interpreter
//! Quota Enforcement (P1 Determinism Rules)" (kill switch check first and the daily check skipped;
//! daily check only when the tool is included - ready XLSX attachments, model support, kill switch
//! off; `daily_usage >= quota` rejects with HTTP 429 `resource_exhausted`, scope `code_interpreter`;
//! the per-turn limit finalizes the turn as `failed` with `code_interpreter_calls_exceeded`), the
//! REST error table (429 `violations[{subject: code_interpreter, description: quota_exceeded}]`),
//! 3.7 `quota_usage.code_interpreter_calls` (bucket `total` only) and 3.9 rule 8 (retry / edit run
//! the same preflight before they commit).

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

mod attachment_support;
mod chat_support;
mod common;
mod quota_support;
mod stream_support;

use attachment_support::{
    AttRow, AttachmentHarness, AttachmentOptions, LimitsSpec, Opts, ProviderReply, STANDARD,
    UsageSeed, assert_exhausted, code_interpreter_pieces, eventually, quota_models,
};
use serde_json::json;
use uuid::Uuid;

fn quota(daily: u32, per_message: u32) -> serde_json::Value {
    json!({"quota": {
        "code_interpreter_daily_quota": daily,
        "code_interpreter_max_calls_per_message": per_message,
    }})
}

fn runs(n: usize) -> Opts {
    let pieces: Vec<_> = (0..n)
        .flat_map(|i| code_interpreter_pieces(&format!("run {i}")))
        .collect();
    Opts::default().usage(900, 100).insert_after(1, pieces)
}

/// A chat with a ready spreadsheet (so the tool is included).
async fn chat_with_sheet(h: &AttachmentHarness) -> Uuid {
    let chat = h.seed_chat().await;
    h.xlsx(chat, "sales.xlsx").await;
    chat
}

async fn calls_today(h: &AttachmentHarness) -> i32 {
    h.bucket("total", "daily")
        .await
        .map_or(0, |row| row.code_interpreter_calls)
}

// ═════════════════════════════════════════════════════════════════════════════
// the daily quota at preflight
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn an_exhausted_daily_quota_is_429_code_interpreter_before_anything_happens() {
    let h = AttachmentHarness::with_config(quota(3, 10)).await;
    let chat = chat_with_sheet(&h).await;
    h.seed_code_interpreter_calls(3).await;

    let before = h.row_counts().await;
    let usage_before = h.usage_rows().await.len();
    let resp = h
        .reject(chat, json!({"content": "analyse the sheet"}))
        .await;
    let problem = assert_exhausted(&resp, "code_interpreter");
    assert_eq!(
        problem["context"]["violations"][0]["description"],
        json!("quota_exceeded")
    );
    assert!(
        !resp.raw.contains("sales.xlsx") && !resp.raw.contains("file-s6fake"),
        "{}",
        resp.raw
    );
    assert_eq!(
        h.row_counts().await,
        before,
        "no turn, message, link or reserve"
    );
    assert_eq!(h.usage_rows().await.len(), usage_before);
    h.oagw.assert_no_provider_call();
}

#[tokio::test]
async fn the_limit_is_reached_at_the_quota_not_above_it() {
    let h = AttachmentHarness::with_config(quota(3, 10)).await;
    let chat = chat_with_sheet(&h).await;

    h.seed_code_interpreter_calls(2).await;
    let sent = h.say(chat, "one call below the quota").await;
    assert_eq!(sent.terminal().event, "done", "{:?}", sent.names());

    h.seed_code_interpreter_calls(1).await;
    assert_eq!(calls_today(&h).await, 3);
    let resp = h.reject(chat, json!({"content": "at the quota"})).await;
    assert_exhausted(&resp, "code_interpreter");
}

#[tokio::test]
async fn the_default_daily_quota_is_50() {
    let h = AttachmentHarness::new().await;
    let chat = chat_with_sheet(&h).await;
    h.seed_code_interpreter_calls(49).await;
    assert_eq!(h.say(chat, "49 used").await.terminal().event, "done");
    h.seed_code_interpreter_calls(1).await;
    let resp = h.reject(chat, json!({"content": "50 used"})).await;
    assert_exhausted(&resp, "code_interpreter");
}

#[tokio::test]
async fn the_check_only_applies_when_the_tool_is_included() {
    // no ready spreadsheet: an uploading, failed, deleted one and a document do not include the tool
    let h = AttachmentHarness::with_config(quota(1, 10)).await;
    h.seed_code_interpreter_calls(100).await;
    let chat = h.seed_chat().await;
    h.seed(AttRow::xlsx(chat, "a.xlsx").status("uploaded"))
        .await;
    h.seed(AttRow::xlsx(chat, "b.xlsx").status("failed")).await;
    h.seed(AttRow::xlsx(chat, "c.xlsx").deleted()).await;
    h.doc(chat, "d.pdf").await;
    let sent = h.say(chat, "no tool, no quota check").await;
    assert_eq!(sent.terminal().event, "done", "{:?}", sent.names());
    assert!(h.oagw.last_request().tool("code_interpreter").is_none());

    // a spreadsheet in another chat does not include it here either
    let other = h.seed_chat().await;
    h.xlsx(other, "e.xlsx").await;
    assert_eq!(h.say(chat, "still no tool").await.terminal().event, "done");
}

#[tokio::test]
async fn the_kill_switch_skips_the_check_and_omits_the_tool() {
    let h = AttachmentHarness::with_config(quota(1, 10)).await;
    let chat = chat_with_sheet(&h).await;
    h.seed_code_interpreter_calls(100).await;
    h.policy
        .set_kill_switches(json!({"disable_code_interpreter": true}));

    let sent = h.say(chat, "analyse").await;
    assert_eq!(sent.terminal().event, "done", "{:?}", sent.names());
    assert!(h.oagw.last_request().tool("code_interpreter").is_none());
}

#[tokio::test]
async fn a_model_without_code_interpreter_skips_the_check() {
    let mut models = quota_models();
    models[1]["general_config"]["tool_support"]["code_interpreter"] = json!(false);
    let h = AttachmentHarness::with(AttachmentOptions {
        models,
        config: quota(1, 10),
        ..Default::default()
    })
    .await;
    let chat = chat_with_sheet(&h).await;
    h.seed_code_interpreter_calls(100).await;
    // the premium model (selected) still has the tool: rejected
    let resp = h.reject(chat, json!({"content": "q"})).await;
    assert_exhausted(&resp, "code_interpreter");

    // the effective model of the turn is the standard one, which has no code interpreter
    h.policy
        .set_kill_switches(json!({"force_standard_tier": true}));
    let sent = h.say(chat, "q").await;
    assert_eq!(sent.terminal().event, "done", "{:?}", sent.names());
    assert_eq!(
        h.oagw.last_request().model(),
        stream_support::provider_model(STANDARD)
    );
    assert!(h.oagw.last_request().tool("code_interpreter").is_none());
}

#[tokio::test]
async fn only_the_users_daily_total_bucket_of_today_counts() {
    let h = AttachmentHarness::with_config(quota(3, 10)).await;
    let chat = chat_with_sheet(&h).await;
    // another user's usage, yesterday's usage and the monthly bucket do not count
    h.seed_usage(
        UsageSeed::total("daily")
            .user(chat_support::USER_A2, common::TENANT_A)
            .code_interpreter_calls(50),
    )
    .await;
    h.seed_usage(UsageSeed::total("daily").stale().code_interpreter_calls(50))
        .await;
    h.seed_usage(UsageSeed::total("monthly").code_interpreter_calls(50))
        .await;
    let sent = h.say(chat, "analyse").await;
    assert_eq!(sent.terminal().event, "done", "{:?}", sent.names());
}

#[tokio::test]
async fn token_exhaustion_is_reported_before_the_code_interpreter_quota() {
    let h = AttachmentHarness::with(AttachmentOptions {
        config: quota(1, 10),
        limits: LimitsSpec::new(10, 10, 10, 10),
        ..Default::default()
    })
    .await;
    let chat = chat_with_sheet(&h).await;
    h.seed_code_interpreter_calls(100).await;
    let resp = h.reject(chat, json!({"content": "q"})).await;
    assert_exhausted(&resp, "tokens");
}

// ═════════════════════════════════════════════════════════════════════════════
// accounting up to the quota, end to end
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn completed_calls_are_counted_until_the_next_message_is_refused() {
    let h = AttachmentHarness::with_config(quota(3, 10)).await;
    let chat = chat_with_sheet(&h).await;

    for turn in 1..=3 {
        h.oagw.push(ProviderReply::text_with(&["a", "b"], &runs(1)));
        let sent = h.say(chat, "analyse").await;
        assert_eq!(
            sent.terminal().event,
            "done",
            "turn {turn}: {:?}",
            sent.names()
        );
        let tools: Vec<_> = sent
            .of("tool")
            .iter()
            .map(|e| e.data["name"].clone())
            .collect();
        assert!(tools.contains(&json!("code_interpreter")), "{tools:?}");
        eventually("the call is settled", || async {
            calls_today(&h).await == turn
        })
        .await;
    }
    let resp = h.reject(chat, json!({"content": "a fourth"})).await;
    assert_exhausted(&resp, "code_interpreter");
    let monthly = h.bucket("total", "monthly").await.unwrap();
    assert_eq!(
        monthly.code_interpreter_calls, 3,
        "counted in the total bucket of both periods"
    );
    assert_eq!(
        h.bucket("tier:premium", "daily")
            .await
            .map_or(0, |r| r.code_interpreter_calls),
        0,
        "never in the tier bucket"
    );
}

#[tokio::test]
async fn a_turn_over_the_per_message_limit_fails_with_code_interpreter_calls_exceeded() {
    let h = AttachmentHarness::with_config(quota(50, 2)).await;
    let chat = chat_with_sheet(&h).await;
    h.oagw.push(ProviderReply::text_with(&["a", "b"], &runs(3)));

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
    eventually("the completed calls are settled", || async {
        calls_today(&h).await == 2
    })
    .await;
}

#[tokio::test]
async fn runs_up_to_the_per_message_limit_complete() {
    let h = AttachmentHarness::with_config(quota(50, 2)).await;
    let chat = chat_with_sheet(&h).await;
    h.oagw.push(ProviderReply::text_with(&["a", "b"], &runs(2)));
    let sent = h.say(chat, "run the analysis").await;
    assert_eq!(sent.terminal().event, "done", "{:?}", sent.names());
    eventually("settled", || async { calls_today(&h).await == 2 }).await;
}

// ═════════════════════════════════════════════════════════════════════════════
// retry and edit run the same preflight before they commit
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn retry_and_edit_are_429_before_the_commit_when_the_daily_quota_is_exhausted() {
    let h = AttachmentHarness::with_config(quota(3, 10)).await;
    let chat = chat_with_sheet(&h).await;
    let first = h.say(chat, "analyse").await;
    assert_eq!(first.terminal().event, "done");
    h.seed_code_interpreter_calls(3).await;

    let before = h.row_counts().await;
    let retry = h.reject_retry(chat, first.request_id()).await;
    assert_exhausted(&retry, "code_interpreter");
    let edit = h
        .reject_edit(chat, first.request_id(), "analyse again")
        .await;
    assert_exhausted(&edit, "code_interpreter");

    let turn = h.turn_of(first.request_id()).await.unwrap();
    assert!(
        turn.deleted_at.is_none() && turn.replaced_by_request_id.is_none(),
        "the old turn stays"
    );
    assert_eq!(h.row_counts().await, before, "nothing was committed");
    assert_eq!(
        h.oagw.request_count(),
        1,
        "no provider call for the rejected mutations"
    );

    // below the quota the retry goes through
    let h2 = AttachmentHarness::with_config(quota(3, 10)).await;
    let chat2 = chat_with_sheet(&h2).await;
    let first2 = h2.say(chat2, "analyse").await;
    let retried = h2.retry(chat2, first2.request_id()).await;
    assert_eq!(retried.terminal().event, "done", "{:?}", retried.names());
}
