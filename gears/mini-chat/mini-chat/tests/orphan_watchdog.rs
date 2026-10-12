//! Orphan turn watchdog (slice S5): a `running` turn without progress for `orphan_watchdog.
//! timeout_secs` is finalized `failed` / `orphan_timeout` by a CAS that re-checks the stale-
//! progress predicate, settled by the estimated formula against the periods and the policy version
//! it was reserved under, with exactly one usage event and one audit event; fresh, progressing,
//! terminal and soft-deleted turns are untouched; a finalizer that comes late loses the CAS.
//!
//! Source of truth: DESIGN "Turn Lifecycle, Crash Recovery and Orphan Handling" ("Orphan Turn
//! Watchdog": condition, `COALESCE(last_progress_at, started_at)`, at most 100 candidates per
//! scan, the CAS `UPDATE`, rows_affected = 0 skips everything, billing outcome `aborted` /
//! `estimated`, "Watchdog Invariants"), B.9.1 (`orphan_watchdog.enabled` / `timeout_secs` /
//! `scan_interval_secs`), 3.7 `chat_turns` ("Preflight columns on retry/edit": NULL reserve
//! columns skip settlement but still enqueue the events), 3.2 "Quota period boundary invariant"
//! (the watchdog derives the periods from `started_at`), 5.4.4 / 5.7 "FinalizeTurn Invariant"
//! (orphan finalization), 5.8 (`orphan_timeout` -> `ABORTED` / estimated), 5.9, ADR-0010
//! (application clock), 2 "Effective model" (on the orphan path `selected_model` is the persisted
//! `effective_model`, audit `quota_decision` is `"unknown"`).

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
mod mutation_support;
mod quota_support;
mod stream_support;

use std::time::Duration;

use mini_chat::infra::db::timestamp::to_db;
use mini_chat_sdk::{AuditEvent, BillingOutcome, RequesterType, SettlementMethod, TerminalState};
use mutation_support::{
    ALL_OPS, MutationHarness, MutationOptions, Op, Opts, PREMIUM, PREMIUM_IN, PREMIUM_OUT,
    ProviderReply, TENANT_A, TENANT_B, TurnRowSeed, USER_A, USER_B, assert_conflict,
    assert_turn_not_found, ctx_a, dedupe_key, eventually, from_db, quota_models, ref_credits,
    ref_estimate, ts, web_search_pieces,
};
use serde_json::json;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const Q: &str = "hello";

/// The estimated settlement of a premium turn whose message is `text` (DESIGN 5.8: the input
/// estimate plus the minimal generation floor of 50 tokens).
fn estimated(text: &str) -> i64 {
    ref_credits(ref_estimate(text), 50, PREMIUM_IN, PREMIUM_OUT)
}

/// The instant a sweep must run at to find a turn started at `started_at` stale: one second past
/// its timeout.
fn stale_at(h: &MutationHarness, started_at: OffsetDateTime) -> OffsetDateTime {
    started_at + h.timeout() + time::Duration::seconds(1)
}

async fn started_of(h: &MutationHarness, rid: Uuid) -> OffsetDateTime {
    from_db(h.turn(rid).await.started_at)
}

// ═════════════════════════════════════════════════════════════════════════════
// the finalization
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_stalled_turn_past_the_timeout_is_finalized_failed_with_orphan_timeout() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let (_live, rid) = h.stall(chat, Q).await;
    let now = stale_at(&h, started_of(&h, rid).await);
    assert!(
        h.amounts("total", "daily").await.1 > 0,
        "the reserve is held"
    );

    assert_eq!(h.sweep(now).await, 1);

    let turn = h.turn(rid).await;
    assert_eq!(turn.state, "failed");
    assert_eq!(turn.error_code.as_deref(), Some("orphan_timeout"));
    assert_eq!(
        turn.completed_at,
        Some(to_db(now)),
        "completed_at is the application clock"
    );
    assert_eq!(turn.updated_at, Some(to_db(now)));
    assert_eq!(turn.assistant_message_id, None);
    assert!(turn.deleted_at.is_none());
    h.oagw.release();
}

#[tokio::test]
async fn the_orphan_turn_reports_error_orphan_timeout_through_the_turn_status_api() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let (_live, rid) = h.stall(chat, Q).await;
    h.sweep(stale_at(&h, started_of(&h, rid).await)).await;

    let status = h.turn_status(chat, rid, &ctx_a()).await;

    assert_eq!(status.status.as_u16(), 200, "{}", status.raw);
    assert_eq!(status.json()["state"], json!("error"));
    assert_eq!(status.json()["error_code"], json!("orphan_timeout"));
    assert!(status.json().get("assistant_message_id").is_none());
    h.oagw.release();
}

#[tokio::test]
async fn an_orphan_stores_no_assistant_message_and_keeps_the_user_message() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let (_live, rid) = h.stall(chat, Q).await;
    h.sweep(stale_at(&h, started_of(&h, rid).await)).await;

    let rows = h.messages_in(chat).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].role, "user");
    assert!(rows[0].deleted_at.is_none());
    h.oagw.release();
}

#[tokio::test]
async fn an_orphan_is_settled_by_the_estimated_formula_and_the_reserve_returns_to_zero() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let (_live, rid) = h.stall(chat, Q).await;
    let reserve = h.turn(rid).await.reserved_credits_micro.unwrap();
    for period in ["daily", "monthly"] {
        assert_eq!(
            h.amounts("total", period).await,
            (0, reserve),
            "{period}: held"
        );
        assert_eq!(h.amounts("tier:premium", period).await, (0, reserve));
    }
    assert_eq!(estimated(Q), 1_089, "credits(113, 50) on the premium model");

    h.sweep(stale_at(&h, started_of(&h, rid).await)).await;

    for period in ["daily", "monthly"] {
        let total = h.bucket("total", period).await.unwrap();
        assert_eq!(
            (
                total.spent_credits_micro,
                total.reserved_credits_micro,
                total.calls
            ),
            (estimated(Q), 0, 1),
            "total/{period}"
        );
        assert_eq!(
            (total.input_tokens, total.output_tokens),
            (0, 0),
            "an estimated settlement adds no token counts"
        );
        let tier = h.bucket("tier:premium", period).await.unwrap();
        assert_eq!(
            (
                tier.spent_credits_micro,
                tier.reserved_credits_micro,
                tier.calls
            ),
            (estimated(Q), 0, 1),
            "tier:premium/{period}"
        );
    }
    h.assert_nothing_reserved().await;
    h.oagw.release();
}

#[tokio::test]
async fn an_orphan_publishes_one_usage_event_aborted_estimated() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let (_live, rid) = h.stall(chat, Q).await;
    let turn = h.turn(rid).await;
    let now = stale_at(&h, from_db(turn.started_at));

    h.sweep(now).await;

    let events = h.policy.wait_published(1).await;
    h.policy.assert_published_stays(1).await;
    let event = &events[0];
    assert_eq!(event.tenant_id, TENANT_A);
    assert_eq!(event.user_id, Some(USER_A));
    assert_eq!(event.chat_id, chat);
    assert_eq!(event.turn_id, Some(turn.id));
    assert_eq!(event.request_id, rid);
    assert_eq!(event.effective_model, PREMIUM);
    assert_eq!(
        event.selected_model, PREMIUM,
        "the persisted effective model stands in"
    );
    assert_eq!(event.terminal_state, TerminalState::Failed);
    assert_eq!(
        event.billing_outcome,
        BillingOutcome::Aborted,
        "not `failed`"
    );
    assert_eq!(event.settlement_method, SettlementMethod::Estimated);
    assert!(event.usage.is_none(), "no provider usage");
    assert_eq!(event.actual_credits_micro, estimated(Q));
    assert_eq!(event.policy_version_applied, 1);
    assert_eq!(
        (
            event.web_search_calls,
            event.code_interpreter_calls,
            event.file_search_calls
        ),
        (0, 0, 0)
    );
    assert_eq!(event.requester_type, RequesterType::User);
    assert_eq!(event.dedupe_key, dedupe_key(TENANT_A, turn.id, rid));
    assert_eq!(event.timestamp, now);
    h.oagw.release();
}

#[tokio::test]
async fn an_orphan_publishes_a_turn_failed_audit_event_with_an_unknown_quota_decision() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let (_live, rid) = h.stall(chat, Q).await;
    let now = stale_at(&h, started_of(&h, rid).await);

    h.sweep(now).await;

    let events = h.audit.wait_events(1).await;
    h.audit.assert_events_stay(1).await;
    let AuditEvent::TurnFailed(event) = &events[0] else {
        panic!("an orphan is a turn_failed event, got {:?}", events[0]);
    };
    assert_eq!(event.tenant_id, TENANT_A);
    assert_eq!(event.user_id, USER_A);
    assert_eq!(event.chat_id, chat);
    assert_eq!(event.request_id, rid);
    assert_eq!(event.effective_model, PREMIUM);
    assert_eq!(event.selected_model, PREMIUM);
    assert!(event.usage.is_none());
    assert_eq!(event.error_code.as_deref(), Some("orphan_timeout"));
    assert_eq!(event.quota_decision, "unknown");
    assert_eq!((event.web_search_calls, event.file_search_calls), (0, 0));
    assert_eq!(event.timestamp, now);
    h.oagw.release();
}

#[tokio::test]
async fn the_estimate_uses_the_policy_version_the_turn_was_planned_under() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let (_live, rid) = h.stall(chat, Q).await;
    // a newer policy version changes the premium multipliers
    let mut cheaper = quota_models();
    cheaper[0]["input_tokens_credit_multiplier_micro"] = json!(1_000_000);
    cheaper[0]["output_tokens_credit_multiplier_micro"] = json!(1_000_000);
    h.policy.publish_version(2, &cheaper);
    let asked_before = h.policy.snapshot_requests.lock().unwrap().len();

    h.sweep(stale_at(&h, started_of(&h, rid).await)).await;

    let asked: Vec<u64> = h.policy.snapshot_requests.lock().unwrap()[asked_before..].to_vec();
    assert!(
        asked.contains(&1),
        "the settlement loads the snapshot of version 1: {asked:?}"
    );
    assert!(!asked.contains(&2), "never the current one: {asked:?}");
    assert_eq!(h.amounts("total", "daily").await, (estimated(Q), 0));
    let events = h.policy.wait_published(1).await;
    assert_eq!(events[0].policy_version_applied, 1);
    assert_eq!(events[0].actual_credits_micro, estimated(Q));
    h.oagw.release();
}

#[tokio::test]
async fn the_quota_periods_are_derived_from_started_at_not_from_the_clock() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let (_live, rid) = h.stall(chat, Q).await;
    // the turn started on 2025-03-15 (its reserve lives in that day and month)
    let started = time::macros::datetime!(2025-03-15 12:00:00 UTC);
    h.set_liveness(rid, started, Some(started)).await;
    h.relocate_quota_rows(started).await;
    let reserve = h.turn(rid).await.reserved_credits_micro.unwrap();
    let held: Vec<_> = h
        .usage_rows()
        .await
        .iter()
        .map(|r| r.reserved_credits_micro)
        .collect();
    assert_eq!(held, vec![reserve; 4]);

    h.sweep(started + h.timeout() + time::Duration::seconds(1))
        .await;

    let rows = h.usage_rows().await;
    assert_eq!(
        rows.len(),
        4,
        "no row was created for the current period: {rows:?}"
    );
    for row in &rows {
        assert_eq!(
            row.reserved_credits_micro, 0,
            "{}/{}",
            row.bucket, row.period_type
        );
        assert_eq!(row.spent_credits_micro, estimated(Q));
        assert_eq!(row.calls, 1);
        let expected_start = if row.period_type == "monthly" { 1 } else { 15 };
        assert_eq!(
            row.period_start.to_string(),
            format!("2025-03-{expected_start:02}")
        );
    }
    h.oagw.release();
}

#[tokio::test]
async fn an_orphan_reports_the_tool_calls_it_completed() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    h.oagw.push(ProviderReply::text_with(
        &["partial"],
        &Opts::default()
            .insert_after(0, web_search_pieces())
            .pause_after(1),
    ));
    let rid = Uuid::new_v4();
    let mut live = h
        .open(
            chat,
            &ctx_a(),
            &json!({"content": "search it", "request_id": rid, "web_search": {"enabled": true}}),
        )
        .await
        .live();
    live.expect("stream_started").await;
    h.oagw.wait_paused(1).await;
    eventually(
        "the completed web search is counted on the turn",
        || async { h.turn(rid).await.web_search_completed_count == 1 },
    )
    .await;

    h.sweep(stale_at(&h, started_of(&h, rid).await)).await;

    let events = h.policy.wait_published(1).await;
    assert_eq!(events[0].web_search_calls, 1);
    let audit = h.audit.wait_events(1).await;
    let AuditEvent::TurnFailed(event) = &audit[0] else {
        panic!("{:?}", audit[0])
    };
    assert_eq!(event.web_search_calls, 1);
    assert_eq!(
        h.bucket("total", "daily").await.unwrap().web_search_calls,
        1
    );
    h.oagw.release();
}

// ═════════════════════════════════════════════════════════════════════════════
// which turns are orphans
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_turn_within_the_timeout_is_untouched() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let (mut live, rid) = h.hold_running(chat, Q).await;
    let started = started_of(&h, rid).await;
    let reserve = h.amounts("total", "daily").await.1;
    assert!(reserve > 0);

    assert_eq!(
        h.sweep(started + h.timeout() - time::Duration::seconds(1))
            .await,
        0
    );

    assert_eq!(h.turn(rid).await.state, "running");
    assert_eq!(
        h.amounts("total", "daily").await,
        (0, reserve),
        "the reserve stays held"
    );
    h.audit.assert_events_stay(0).await;
    h.policy.assert_published_stays(0).await;

    // the turn then finishes normally
    h.oagw.release();
    let rest = live.drain().await;
    assert_eq!(rest.last().unwrap().event, "done");
    assert_eq!(h.turn(rid).await.state, "completed");
}

#[tokio::test]
async fn the_cutoff_is_inclusive_and_comes_from_the_configured_timeout() {
    for timeout_secs in [90_i64, 300, 3600] {
        let h = MutationHarness::with_config(
            json!({"orphan_watchdog": {"timeout_secs": timeout_secs}}),
        )
        .await;
        let chat = h.seed_chat().await;
        let last_progress = ts(100_000);
        let rid = h
            .insert_turn(TurnRowSeed::running(chat).bare().started(last_progress))
            .await;
        let timeout = time::Duration::seconds(timeout_secs);

        // one microsecond fresher than the cutoff
        let almost = last_progress + timeout - time::Duration::microseconds(1);
        assert_eq!(h.sweep(almost).await, 0, "timeout {timeout_secs}: not yet");
        assert_eq!(h.turn(rid).await.state, "running");

        // `last_progress_at <= now - timeout`
        assert_eq!(
            h.sweep(last_progress + timeout).await,
            1,
            "timeout {timeout_secs}: at the cutoff"
        );
        assert_eq!(h.turn(rid).await.state, "failed");
    }
}

#[tokio::test]
async fn progress_decides_not_the_age_of_the_turn() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let now = ts(200_000);
    // started two hours ago, but made progress half a timeout ago
    let rid = h
        .insert_turn(
            TurnRowSeed::running(chat)
                .bare()
                .started(now - time::Duration::hours(2))
                .progress(Some(now - h.timeout() / 2)),
        )
        .await;

    assert_eq!(
        h.sweep(now).await,
        0,
        "a long-running turn with recent progress is no orphan"
    );
    assert_eq!(h.turn(rid).await.state, "running");

    assert_eq!(
        h.sweep(now + h.timeout()).await,
        1,
        "the progress went stale"
    );
    assert_eq!(h.turn(rid).await.state, "failed");
}

#[tokio::test]
async fn a_missing_progress_timestamp_falls_back_to_started_at() {
    let h = MutationHarness::new().await;
    let now = ts(300_000);
    let stale_chat = h.seed_chat().await;
    let stale = h
        .insert_turn(
            TurnRowSeed::running(stale_chat)
                .bare()
                .started(now - h.timeout() - time::Duration::seconds(5))
                .progress(None),
        )
        .await;
    let fresh_chat = h.seed_chat().await;
    let fresh = h
        .insert_turn(
            TurnRowSeed::running(fresh_chat)
                .bare()
                .started(now - time::Duration::seconds(5))
                .progress(None),
        )
        .await;

    assert_eq!(h.sweep(now).await, 1);

    assert_eq!(h.turn(stale).await.state, "failed");
    assert_eq!(h.turn(fresh).await.state, "running");
}

#[tokio::test]
async fn terminal_and_soft_deleted_turns_are_never_finalized() {
    let h = MutationHarness::new().await;
    let long_ago = ts(10);
    for state in ["completed", "failed", "cancelled"] {
        let chat = h.seed_chat().await;
        h.insert_turn(TurnRowSeed::terminal(chat, state).started(long_ago))
            .await;
    }
    // a soft-deleted turn that still says `running` (never created by the API, but the predicate
    // `deleted_at IS NULL` is part of the CAS)
    let chat = h.seed_chat().await;
    h.insert_turn(TurnRowSeed::running(chat).started(long_ago).deleted(ts(20)))
        .await;
    let before = h.snapshot().await;

    assert_eq!(h.sweep(OffsetDateTime::now_utc()).await, 0);

    assert_eq!(h.snapshot().await, before);
    h.audit.assert_events_stay(0).await;
    h.policy.assert_published_stays(0).await;
}

#[tokio::test]
async fn a_second_sweep_changes_nothing_and_emits_nothing() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let (_live, rid) = h.stall(chat, Q).await;
    let now = stale_at(&h, started_of(&h, rid).await);
    assert_eq!(h.sweep(now).await, 1);
    h.policy.wait_published(1).await;
    h.audit.wait_events(1).await;
    let after_first = h.snapshot().await;

    assert_eq!(h.sweep(now + time::Duration::seconds(60)).await, 0);

    assert_eq!(h.snapshot().await, after_first, "no second settlement");
    h.policy.assert_published_stays(1).await;
    h.audit.assert_events_stay(1).await;
    h.oagw.release();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_sweeps_finalize_a_turn_exactly_once() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let (_live, rid) = h.stall(chat, Q).await;
    let now = stale_at(&h, started_of(&h, rid).await);

    let (a, b, c) = tokio::join!(h.sweep(now), h.sweep(now), h.sweep(now));

    assert_eq!(a + b + c, 1, "one CAS winner among the sweeps: {a} {b} {c}");
    assert_eq!(
        h.amounts("total", "daily").await,
        (estimated(Q), 0),
        "settled once"
    );
    h.policy.wait_published(1).await;
    h.policy.assert_published_stays(1).await;
    h.audit.wait_events(1).await;
    h.audit.assert_events_stay(1).await;
    h.oagw.release();
}

#[tokio::test]
async fn a_scan_takes_at_most_100_candidates() {
    let h = MutationHarness::new().await;
    let mut rids = Vec::new();
    for _ in 0..105 {
        let chat = h.seed_chat().await;
        rids.push(h.insert_turn(TurnRowSeed::running(chat).bare()).await);
    }
    let now = OffsetDateTime::now_utc();

    assert_eq!(h.sweep(now).await, 100, "the first scan is bounded");
    assert_eq!(
        h.sweep(now).await,
        5,
        "the rest is picked up by the next scan"
    );
    assert_eq!(h.sweep(now).await, 0);

    let failed = h
        .all_turns()
        .await
        .iter()
        .filter(|t| t.state == "failed" && t.error_code.as_deref() == Some("orphan_timeout"))
        .count();
    assert_eq!(failed, 105);
    h.policy.wait_published(105).await;
    h.policy.assert_published_stays(105).await;
}

#[tokio::test]
async fn the_watchdog_serves_every_tenant() {
    let h = MutationHarness::new().await;
    let chat_a = h.seed_chat().await;
    let chat_b = h.seed_chat_of(USER_B, TENANT_B, PREMIUM).await;
    let a = h.insert_turn(TurnRowSeed::running(chat_a).bare()).await;
    let b = h
        .insert_turn(TurnRowSeed::running(chat_b).bare().of(TENANT_B, USER_B))
        .await;

    assert_eq!(h.sweep(OffsetDateTime::now_utc()).await, 2);

    assert_eq!(h.turn(a).await.state, "failed");
    assert_eq!(h.turn(b).await.state, "failed");
    let events = h.policy.wait_published(2).await;
    let tenant_of = |rid: Uuid| {
        events
            .iter()
            .find(|e| e.request_id == rid)
            .unwrap()
            .tenant_id
    };
    assert_eq!(tenant_of(a), TENANT_A);
    assert_eq!(tenant_of(b), TENANT_B);
}

// ═════════════════════════════════════════════════════════════════════════════
// turns without a reserve
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_turn_without_reserve_columns_skips_the_quota_rows_but_still_emits_the_events() {
    // a retry/edit turn left `running` before its preflight fields were written
    let h = MutationHarness::new().await;
    // another user's live reserve must stay untouched
    let live_chat = h.seed_chat().await;
    let (mut live, live_rid) = h.hold_running(live_chat, Q).await;
    let quota_before = h.snapshot().await.usage;
    assert!(!quota_before.is_empty());
    let chat = h.seed_chat().await;
    let bare = h.insert_turn(TurnRowSeed::running(chat).bare()).await;
    let bare_turn = h.turn(bare).await;
    assert!(bare_turn.reserve_tokens.is_none() && bare_turn.effective_model.is_none());

    // the real turn is fresh, the bare one dates from 2025
    assert_eq!(h.sweep(OffsetDateTime::now_utc()).await, 1);

    assert_eq!(
        h.turn(bare).await.error_code.as_deref(),
        Some("orphan_timeout")
    );
    assert_eq!(h.turn(live_rid).await.state, "running");
    assert_eq!(
        h.snapshot().await.usage,
        quota_before,
        "no quota row changed"
    );

    let events = h.policy.wait_published(1).await;
    let event = &events[0];
    assert_eq!(event.request_id, bare);
    assert_eq!(event.turn_id, Some(bare_turn.id));
    assert_eq!(event.billing_outcome, BillingOutcome::Aborted);
    assert_eq!(event.settlement_method, SettlementMethod::Estimated);
    assert_eq!(event.actual_credits_micro, 0);
    assert!(event.usage.is_none());
    assert_eq!(event.effective_model, "");
    assert_eq!(event.selected_model, "");
    assert_eq!(event.policy_version_applied, 0);
    assert_eq!(event.dedupe_key, dedupe_key(TENANT_A, bare_turn.id, bare));
    let audit = h.audit.wait_events(1).await;
    let AuditEvent::TurnFailed(audit_event) = &audit[0] else {
        panic!("{:?}", audit[0])
    };
    assert_eq!(audit_event.request_id, bare);
    assert_eq!(audit_event.error_code.as_deref(), Some("orphan_timeout"));

    h.oagw.release();
    live.drain().await;
}

// ═════════════════════════════════════════════════════════════════════════════
// after the sweep
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_chat_is_blocked_until_the_sweep_and_accepts_a_message_afterwards() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let stuck = h.insert_turn(TurnRowSeed::running(chat)).await;

    let blocked = h.reject_send(chat, json!({"content": "q"})).await;
    assert_conflict(&blocked, "turn_already_running");

    assert_eq!(h.sweep(OffsetDateTime::now_utc()).await, 1);

    let sent = h.say(chat, "now it works").await;
    assert_eq!(sent.terminal().event, "done");
    assert_eq!(h.turn(stuck).await.state, "failed");
    assert_conflict(
        &h.reject_send(chat, json!({"content": "q", "request_id": stuck}))
            .await,
        "request_id_conflict",
    );
}

#[tokio::test]
async fn an_orphaned_turn_can_be_retried_edited_or_deleted_once_it_is_the_latest() {
    for op in ALL_OPS {
        let h = MutationHarness::new().await;
        let chat = h.seed_chat().await;
        let (_live, rid) = h.stall(chat, Q).await;
        h.sweep(stale_at(&h, started_of(&h, rid).await)).await;
        // while it ran, the turn was a running target (400); now it is a failed one
        h.oagw.release();

        match op {
            Op::Retry => {
                let sent = h.retried(chat, rid).await;
                assert_eq!(sent.terminal().event, "done");
                assert_eq!(
                    h.turn(rid).await.replaced_by_request_id,
                    Some(sent.request_id())
                );
            }
            Op::Edit => {
                let sent = h.edited(chat, rid, "reworded").await;
                assert_eq!(sent.terminal().event, "done");
            }
            Op::Delete => {
                assert_eq!(h.delete(chat, rid).await.status.as_u16(), 204);
                assert_turn_not_found(&h.turn_status(chat, rid, &ctx_a()).await);
            }
        }
        assert_eq!(
            h.turn(rid).await.error_code.as_deref(),
            Some("orphan_timeout")
        );
    }
}

// ═════════════════════════════════════════════════════════════════════════════
// the race with the stream's own finalizer
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn a_stream_that_finishes_after_the_sweep_loses_the_cas_and_is_interrupted() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let (mut live, rid) = h.stall(chat, Q).await;
    h.sweep(stale_at(&h, started_of(&h, rid).await)).await;
    let settled = h.snapshot().await.usage;
    h.policy.wait_published(1).await;
    h.audit.wait_events(1).await;

    // the provider recovers and completes the answer
    h.oagw.release();
    let rest = live.drain().await;

    let last = rest.last().expect("a terminal event");
    assert_eq!(last.event, "error");
    assert_eq!(last.data["code"], json!("stream_interrupted"));
    assert!(rest.iter().all(|e| e.event != "done"), "{rest:?}");
    let turn = h.turn(rid).await;
    assert_eq!(
        (turn.state.as_str(), turn.error_code.as_deref()),
        ("failed", Some("orphan_timeout"))
    );
    assert_eq!(turn.assistant_message_id, None);
    assert!(
        h.messages_in(chat)
            .await
            .iter()
            .all(|m| m.role != "assistant"),
        "the loser persists no assistant message"
    );
    assert_eq!(h.snapshot().await.usage, settled, "no double settlement");
    h.policy.assert_published_stays(1).await;
    h.audit.assert_events_stay(1).await;
}

#[tokio::test]
async fn a_turn_the_stream_finalized_first_is_not_an_orphan_however_stale_it_looks() {
    let h = MutationHarness::new().await;
    let chat = h.seed_chat().await;
    let sent = h.say(chat, Q).await;
    let rid = sent.request_id();
    h.policy.wait_published(1).await;
    h.audit.wait_events(1).await;
    // wind the clock back so the finished turn looks stale
    h.set_liveness(rid, ts(10), Some(ts(10))).await;
    let before = h.snapshot().await;

    assert_eq!(h.sweep(OffsetDateTime::now_utc()).await, 0);

    assert_eq!(h.snapshot().await, before);
    h.policy.assert_published_stays(1).await;
    h.audit.assert_events_stay(1).await;
}

// ═════════════════════════════════════════════════════════════════════════════
// atomicity with the settlement and the events
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn an_orphan_finalization_whose_audit_event_cannot_be_enqueued_leaves_the_turn_running() {
    let h = MutationHarness::with(MutationOptions {
        register_audit_queue: false,
        ..Default::default()
    })
    .await;
    let chat = h.seed_chat().await;
    let (_live, rid) = h.stall(chat, Q).await;
    let reserve = h.amounts("total", "daily").await.1;
    let before = h.snapshot().await;

    // the transaction rolls back as a whole (CAS, settlement, events); whether the sweep reports
    // that as an error or as a skipped turn is not pinned
    let outcome = h
        .watchdog
        .sweep_at(stale_at(&h, started_of(&h, rid).await))
        .await;
    drop(outcome);

    assert_eq!(h.turn(rid).await.state, "running");
    assert_eq!(h.amounts("total", "daily").await, (0, reserve));
    assert_eq!(h.snapshot().await, before);
    h.policy.assert_published_stays(0).await;
    h.oagw.release();
}

// ═════════════════════════════════════════════════════════════════════════════
// the periodic task (`orphan_watchdog.enabled`, `scan_interval_secs`)
// ═════════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn the_periodic_task_finalizes_stale_turns_scan_after_scan_until_it_is_cancelled() {
    let h = MutationHarness::with_config(
        json!({"orphan_watchdog": {"enabled": true, "timeout_secs": 90, "scan_interval_secs": 1}}),
    )
    .await;
    let first_chat = h.seed_chat().await;
    let first = h.insert_turn(TurnRowSeed::running(first_chat).bare()).await;
    let cancel = CancellationToken::new();
    let task = tokio::spawn(h.watchdog.clone().run(cancel.clone()));

    eventually("the first stale turn is finalized", || async {
        h.turn(first).await.state == "failed"
    })
    .await;
    // a turn that becomes stale later is found by a later scan
    let second_chat = h.seed_chat().await;
    let second = h
        .insert_turn(TurnRowSeed::running(second_chat).bare())
        .await;
    eventually("the second stale turn is finalized", || async {
        h.turn(second).await.state == "failed"
    })
    .await;
    h.policy.wait_published(2).await;
    h.policy.assert_published_stays(2).await;

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the task stops when cancelled")
        .expect("the task does not panic");
}

#[tokio::test]
async fn a_disabled_watchdog_does_not_scan() {
    let h = MutationHarness::with_config(
        json!({"orphan_watchdog": {"enabled": false, "timeout_secs": 90, "scan_interval_secs": 1}}),
    )
    .await;
    let chat = h.seed_chat().await;
    let stuck = h.insert_turn(TurnRowSeed::running(chat).bare()).await;
    let cancel = CancellationToken::new();
    let task = tokio::spawn(h.watchdog.clone().run(cancel.clone()));

    tokio::time::sleep(Duration::from_millis(3_500)).await;

    assert_eq!(
        h.turn(stuck).await.state,
        "running",
        "nothing scanned in three intervals"
    );
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("the task ends")
        .expect("the task does not panic");
}
