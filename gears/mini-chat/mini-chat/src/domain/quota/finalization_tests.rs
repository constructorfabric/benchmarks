//! Finalization settlement tests over in-memory `SQLite` and the shared outbox pipeline: the
//! CAS-guarded exactly-once settlement per terminal outcome, the bucket arithmetic per
//! settlement method, the overshoot rule, the tool counters, the usage message of the
//! settlement transaction, the rollback of the whole transaction, the policy version binding,
//! the turn without a reserve, the period boundary on the stream and the orphan path, and the
//! metrics of the result.
//!
//! Slice S1-all: flow `cpt-cf-mini-chat-flow-quota-and-billing-turn-settlement`, algorithms
//! `cpt-cf-mini-chat-algo-quota-and-billing-finalization-settlement`,
//! `cpt-cf-mini-chat-algo-quota-and-billing-settlement-computation`,
//! `cpt-cf-mini-chat-algo-quota-and-billing-policy-binding` (settlement snapshot) and
//! `cpt-cf-mini-chat-algo-quota-and-billing-usage-event`, states
//! `cpt-cf-mini-chat-state-quota-and-billing-turn-billing` and
//! `cpt-cf-mini-chat-state-quota-and-billing-turn-reserve`, definitions of done
//! `cpt-cf-mini-chat-dod-quota-and-billing-cas-exactly-once`,
//! `cpt-cf-mini-chat-dod-quota-and-billing-settlement-outcomes`,
//! `cpt-cf-mini-chat-dod-quota-and-billing-policy-version`,
//! `cpt-cf-mini-chat-dod-quota-and-billing-usage-event` and
//! `cpt-cf-mini-chat-dod-quota-and-billing-period-buckets`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::{ModelTier, UsageTokens};
use tracing_test::traced_test;
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::error::{DomainError, EnqueueSite};
use crate::domain::quota::{
    BillingOutcome, FinalizeOutcome, FinalizeRequest, Finalizer, PeriodStarts, PeriodType,
    Settlement, SettlementMethod, TerminalState, ToolCallCounts, TurnTerminalUpdate,
};
use crate::test_support::chats::{fail_outbox_inserts, outbox_body_count};
use crate::test_support::quota::{
    BookedTurn, FailingUsageQueue, FixedClock, MetricCall, PREMIUM, QuotaGateway, QuotaRig,
    QuotaRigOptions, ReserveCols, STANDARD, TurnSeed, USAGE_PAYLOAD_TYPE, UsageQueueKind,
    all_quota_rows, catalog_v, date, default_now, model, partition_for, plain_facts, premium_model,
    quota_rig, quota_rig_with, quota_row, row, seed_rows, seed_turn, spent_of, standard_model,
    touch_turn, turn_row, usage, usage_payloads, usage_rows, utc, utc_ms,
};
use crate::test_support::{TENANT_A, TENANT_B, USER_A1};

const DAY: &str = "2026-02-28";
const MONTH: &str = "2026-02-01";

// ── request builders ─────────────────────────────────────────────────────────

fn update(state: TerminalState, code: Option<&str>) -> TurnTerminalUpdate {
    TurnTerminalUpdate {
        state,
        error_code: code.map(ToOwned::to_owned),
        error_detail: None,
        assistant_message_id: None,
        completed_at: utc(2026, 2, 28, 15, 31, 0),
    }
}

fn request(
    booked: &BookedTurn,
    finalizer: Finalizer,
    update: TurnTerminalUpdate,
    reported: Option<UsageTokens>,
    tools: ToolCallCounts,
) -> FinalizeRequest {
    FinalizeRequest {
        tenant_id: booked.identity.tenant_id,
        turn_id: booked.turn_id,
        update,
        finalizer,
        usage: reported,
        tool_counts: tools,
    }
}

fn stream_of(booked: &BookedTurn, selected: &str) -> Finalizer {
    Finalizer::Stream {
        selected_model: selected.to_owned(),
        period_starts: booked.decision.period_starts,
    }
}

fn orphan_finalizer() -> Finalizer {
    Finalizer::Orphan {
        stale_cutoff: utc(2026, 2, 28, 15, 40, 0),
    }
}

fn completed(
    booked: &BookedTurn,
    selected: &str,
    reported: Option<UsageTokens>,
) -> FinalizeRequest {
    request(
        booked,
        stream_of(booked, selected),
        update(TerminalState::Completed, None),
        reported,
        ToolCallCounts::default(),
    )
}

fn settled(outcome: FinalizeOutcome) -> Box<Settlement> {
    match outcome {
        FinalizeOutcome::Settled(settlement) => settlement,
        other => panic!("expected a settlement, got {other:?}"),
    }
}

async fn finalize_ok(rig: &QuotaRig, request: &FinalizeRequest) -> Box<Settlement> {
    settled(rig.finalize(request).await.expect("finalizes"))
}

fn assert_no_reserve(rows: &[crate::test_support::quota::QuotaRowView]) {
    for row in rows {
        assert!(
            row.reserved == 0,
            "a terminal transaction leaves no reserve: {row:?}"
        );
    }
}

/// The DESIGN 5.10 consumption of the `total` bucket before the standard turn.
async fn seed_total_consumption(rig: &QuotaRig) {
    seed_rows(
        &rig.db,
        &[
            row("daily", DAY, "total").spent(25_000_000),
            row("monthly", MONTH, "total").spent(240_000_000),
        ],
    )
    .await;
}

async fn booked_standard(rig: &QuotaRig) -> BookedTurn {
    let chat = rig.chat_a1(STANDARD).await;
    rig.book(&chat, STANDARD, plain_facts()).await
}

async fn booked_premium(rig: &QuotaRig) -> BookedTurn {
    let chat = rig.chat_a1(PREMIUM).await;
    rig.book(&chat, PREMIUM, plain_facts()).await
}

// ── a completed turn: the DESIGN 5.10 numbers ────────────────────────────────

#[tokio::test]
async fn a_completed_standard_turn_moves_the_reserve_to_spent_in_the_total_bucket_only() {
    let rig = quota_rig().await;
    seed_total_consumption(&rig).await;
    let booked = booked_standard(&rig).await;
    assert_eq!(booked.decision.reserved_credits_micro, 1_500_000);

    let settlement = finalize_ok(&rig, &completed(&booked, STANDARD, Some(usage(900, 300)))).await;
    let result = &settlement.result;
    assert_eq!(result.billing_outcome, BillingOutcome::Completed);
    assert_eq!(result.settlement_method, SettlementMethod::Actual);
    assert_eq!(result.committed_credits_micro, 1_200_000);
    assert_eq!(result.tier, Some(ModelTier::Standard));

    let daily = quota_row(&rig.db, "daily", DAY, "total").await.unwrap();
    assert_eq!(daily.spent, 26_200_000);
    assert_eq!(daily.reserved, 0);
    assert_eq!(daily.calls, 1);
    assert_eq!(daily.input_tokens, 900);
    assert_eq!(daily.output_tokens, 300);
    let monthly = quota_row(&rig.db, "monthly", MONTH, "total").await.unwrap();
    assert_eq!(monthly.spent, 241_200_000);
    assert_eq!(monthly.reserved, 0);
    assert_eq!(monthly.calls, 1);
    assert_eq!(monthly.input_tokens, 900);
    assert_eq!(monthly.output_tokens, 300);
    assert!(
        quota_row(&rig.db, "daily", DAY, "tier:premium")
            .await
            .is_none(),
        "a standard turn does not touch the premium bucket"
    );

    let turn = turn_row(&rig.db, booked.turn_id).await;
    assert_eq!(turn.state, "completed");
    assert_eq!(turn.error_code, None);
    assert!(turn.completed_at.is_some());
    assert_no_reserve(&all_quota_rows(&rig.db).await);
}

#[tokio::test]
async fn a_completed_premium_turn_settles_both_buckets_and_counts_tokens_in_the_total_row_only() {
    let rig = quota_rig().await;
    let booked = booked_premium(&rig).await;
    assert_eq!(booked.decision.reserved_credits_micro, 3_750_000);

    let settlement = finalize_ok(&rig, &completed(&booked, PREMIUM, Some(usage(800, 400)))).await;
    assert_eq!(settlement.result.committed_credits_micro, 3_000_000);
    assert_eq!(settlement.result.tier, Some(ModelTier::Premium));

    for (period, start) in [("daily", DAY), ("monthly", MONTH)] {
        let total = quota_row(&rig.db, period, start, "total").await.unwrap();
        assert_eq!(
            (total.spent, total.reserved, total.calls),
            (3_000_000, 0, 1),
            "{period}"
        );
        assert_eq!(
            (total.input_tokens, total.output_tokens),
            (800, 400),
            "{period}"
        );
        let premium = quota_row(&rig.db, period, start, "tier:premium")
            .await
            .unwrap();
        assert_eq!(
            (premium.spent, premium.reserved, premium.calls),
            (3_000_000, 0, 1),
            "{period}"
        );
        assert_eq!(
            (premium.input_tokens, premium.output_tokens),
            (0, 0),
            "{period}"
        );
        assert_eq!(
            (premium.web_search_calls, premium.code_interpreter_calls),
            (0, 0)
        );
    }
}

#[tokio::test]
async fn the_calls_counter_rises_by_one_per_settlement() {
    let rig = quota_rig().await;
    let first = booked_standard(&rig).await;
    finalize_ok(&rig, &completed(&first, STANDARD, Some(usage(10, 10)))).await;
    let second = booked_standard(&rig).await;
    finalize_ok(&rig, &completed(&second, STANDARD, Some(usage(10, 10)))).await;
    assert_eq!(
        quota_row(&rig.db, "daily", DAY, "total")
            .await
            .unwrap()
            .calls,
        2
    );
}

#[tokio::test]
async fn a_completed_turn_without_usage_settles_zero_credits_and_releases_the_reserve() {
    let rig = quota_rig().await;
    let booked = booked_standard(&rig).await;
    let settlement = finalize_ok(&rig, &completed(&booked, STANDARD, None)).await;
    assert_eq!(
        settlement.result.settlement_method,
        SettlementMethod::Actual
    );
    assert_eq!(settlement.result.committed_credits_micro, 0);
    let daily = quota_row(&rig.db, "daily", DAY, "total").await.unwrap();
    assert_eq!((daily.spent, daily.reserved, daily.calls), (0, 0, 1));
}

// ── exactly once ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_second_finalizer_of_the_same_turn_is_a_complete_no_op() {
    let rig = quota_rig().await;
    let booked = booked_standard(&rig).await;
    finalize_ok(&rig, &completed(&booked, STANDARD, Some(usage(900, 300)))).await;
    let rows = all_quota_rows(&rig.db).await;
    let turn = turn_row(&rig.db, booked.turn_id).await;
    let events = rig.events().await;
    assert_eq!(events.len(), 1);

    // the same trigger again, a disconnect and the orphan watchdog all lose the CAS
    let late = [
        completed(&booked, STANDARD, Some(usage(900, 300))),
        request(
            &booked,
            stream_of(&booked, STANDARD),
            update(TerminalState::Cancelled, None),
            None,
            ToolCallCounts::default(),
        ),
        request(
            &booked,
            orphan_finalizer(),
            update(TerminalState::Failed, Some("orphan_timeout")),
            None,
            ToolCallCounts::default(),
        ),
    ];
    for finalizer in &late {
        let outcome = rig.finalize(finalizer).await.unwrap();
        assert!(
            matches!(outcome, FinalizeOutcome::AlreadyFinalized),
            "{outcome:?}"
        );
    }
    assert_eq!(all_quota_rows(&rig.db).await, rows);
    assert_eq!(turn_row(&rig.db, booked.turn_id).await, turn);
    assert_eq!(rig.events().await.len(), 1, "no second usage message");
}

#[tokio::test]
async fn a_turn_of_another_tenant_or_an_unknown_turn_is_an_already_finalized_no_op() {
    let rig = quota_rig().await;
    let booked = booked_standard(&rig).await;
    let rows = all_quota_rows(&rig.db).await;
    let mut foreign = completed(&booked, STANDARD, Some(usage(900, 300)));
    foreign.tenant_id = TENANT_B;
    assert!(matches!(
        rig.finalize(&foreign).await.unwrap(),
        FinalizeOutcome::AlreadyFinalized
    ));
    let mut unknown = completed(&booked, STANDARD, Some(usage(900, 300)));
    unknown.turn_id = Uuid::new_v4();
    assert!(matches!(
        rig.finalize(&unknown).await.unwrap(),
        FinalizeOutcome::AlreadyFinalized
    ));
    assert_eq!(turn_row(&rig.db, booked.turn_id).await.state, "running");
    assert_eq!(all_quota_rows(&rig.db).await, rows);
    assert!(rig.events().await.is_empty());
}

// ── estimated outcomes ───────────────────────────────────────────────────────

#[tokio::test]
async fn a_cancelled_turn_settles_the_estimated_formula_and_adds_the_tool_counts_but_no_tokens() {
    let rig = quota_rig().await;
    let booked = booked_standard(&rig).await;
    let tools = ToolCallCounts {
        web_search: 1,
        code_interpreter: 2,
        file_search: 3,
    };
    let finalize = request(
        &booked,
        stream_of(&booked, STANDARD),
        update(TerminalState::Cancelled, None),
        Some(usage(900, 300)),
        tools,
    );
    let settlement = finalize_ok(&rig, &finalize).await;
    let result = &settlement.result;
    assert_eq!(result.billing_outcome, BillingOutcome::Aborted);
    assert_eq!(result.settlement_method, SettlementMethod::Estimated);
    // estimated input 1000 + the persisted floor of 50 output tokens
    assert_eq!(result.committed_credits_micro, 1_050_000);
    assert_eq!(result.charged_tokens, 1050);

    for (period, start) in [("daily", DAY), ("monthly", MONTH)] {
        let total = quota_row(&rig.db, period, start, "total").await.unwrap();
        assert_eq!(
            (total.spent, total.reserved, total.calls),
            (1_050_000, 0, 1),
            "{period}"
        );
        assert_eq!(
            (total.input_tokens, total.output_tokens),
            (0, 0),
            "{period}: no tokens on estimated"
        );
        assert_eq!(
            (total.web_search_calls, total.code_interpreter_calls),
            (1, 2),
            "{period}"
        );
        assert_eq!(
            total.file_search_calls, 0,
            "{period}: file_search_calls is never populated"
        );
    }
    let turn = turn_row(&rig.db, booked.turn_id).await;
    assert_eq!(turn.state, "cancelled");

    let event = rig.events().await.remove(0);
    assert_eq!(event.terminal_state, "cancelled");
    assert_eq!(event.billing_outcome, "aborted");
    assert_eq!(event.settlement_method, "estimated");
    assert_eq!(event.usage, None);
    assert_eq!(event.actual_credits_micro, 1_050_000);
    assert_eq!(
        event.file_search_calls, 3,
        "the event reports it, the quota row does not"
    );
    assert_no_reserve(&all_quota_rows(&rig.db).await);
}

#[tokio::test]
async fn a_failed_turn_settles_actual_with_known_usage_and_estimated_without() {
    for (reported, method, credits) in [
        (Some(usage(100, 50)), SettlementMethod::Actual, 150_000),
        (None, SettlementMethod::Estimated, 1_050_000),
        (Some(usage(0, 0)), SettlementMethod::Estimated, 1_050_000),
    ] {
        let rig = quota_rig().await;
        let booked = booked_standard(&rig).await;
        let finalize = request(
            &booked,
            stream_of(&booked, STANDARD),
            update(TerminalState::Failed, Some("provider_error")),
            reported,
            ToolCallCounts::default(),
        );
        let settlement = finalize_ok(&rig, &finalize).await;
        assert_eq!(settlement.result.billing_outcome, BillingOutcome::Failed);
        assert_eq!(settlement.result.settlement_method, method, "{reported:?}");
        assert_eq!(
            settlement.result.committed_credits_micro, credits,
            "{reported:?}"
        );
        let turn = turn_row(&rig.db, booked.turn_id).await;
        assert_eq!(turn.state, "failed");
        assert_eq!(turn.error_code.as_deref(), Some("provider_error"));
        assert_no_reserve(&all_quota_rows(&rig.db).await);
    }
}

#[tokio::test]
async fn an_unknown_error_code_settles_failed_and_estimated() {
    let rig = quota_rig().await;
    let booked = booked_standard(&rig).await;
    let finalize = request(
        &booked,
        stream_of(&booked, STANDARD),
        update(TerminalState::Failed, Some("brand_new_code")),
        Some(usage(100, 50)),
        ToolCallCounts::default(),
    );
    let settlement = finalize_ok(&rig, &finalize).await;
    assert_eq!(settlement.result.billing_outcome, BillingOutcome::Failed);
    assert_eq!(
        settlement.result.settlement_method,
        SettlementMethod::Estimated
    );
}

// ── released ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_pre_provider_failure_releases_the_whole_reserve_at_zero_charge() {
    let rig = quota_rig().await;
    let booked = booked_standard(&rig).await;
    let tools = ToolCallCounts {
        web_search: 4,
        code_interpreter: 4,
        file_search: 4,
    };
    let finalize = request(
        &booked,
        stream_of(&booked, STANDARD),
        update(TerminalState::Failed, Some("turn_setup_failed")),
        Some(usage(50, 50)),
        tools,
    );
    let settlement = finalize_ok(&rig, &finalize).await;
    assert_eq!(
        settlement.result.settlement_method,
        SettlementMethod::Released
    );
    assert_eq!(settlement.result.committed_credits_micro, 0);

    for (period, start) in [("daily", DAY), ("monthly", MONTH)] {
        let total = quota_row(&rig.db, period, start, "total").await.unwrap();
        assert_eq!(
            (total.spent, total.reserved, total.calls),
            (0, 0, 1),
            "{period}: calls rises on every method"
        );
        assert_eq!(
            (total.input_tokens, total.output_tokens),
            (0, 0),
            "{period}"
        );
        assert_eq!(
            (total.web_search_calls, total.code_interpreter_calls),
            (0, 0),
            "{period}"
        );
    }
    let event = rig.events().await.remove(0);
    assert_eq!(event.settlement_method, "released");
    assert_eq!(event.billing_outcome, "failed");
    let reported = event.usage.expect("zero usage object");
    assert_eq!((reported.input_tokens, reported.output_tokens), (0, 0));
    assert_eq!(event.actual_credits_micro, 0);
}

#[tokio::test]
async fn a_premium_turn_released_updates_the_calls_of_both_buckets() {
    let rig = quota_rig().await;
    let booked = booked_premium(&rig).await;
    let finalize = request(
        &booked,
        stream_of(&booked, PREMIUM),
        update(TerminalState::Failed, Some("context_length_exceeded")),
        None,
        ToolCallCounts::default(),
    );
    finalize_ok(&rig, &finalize).await;
    for bucket in ["total", "tier:premium"] {
        let row = quota_row(&rig.db, "daily", DAY, bucket).await.unwrap();
        assert_eq!((row.spent, row.reserved, row.calls), (0, 0, 1), "{bucket}");
    }
}

// ── no billing outcome ───────────────────────────────────────────────────────

#[tokio::test]
async fn quota_exceeded_finalizes_the_turn_without_a_settlement_or_a_usage_event() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    // a retry or edit turn whose reserve re-check failed: no reserve columns exist
    let turn = TurnSeed::running(&chat, utc(2026, 2, 28, 15, 0, 0));
    let turn_id = seed_turn(&rig.db, &turn).await;
    seed_rows(&rig.db, &[row("daily", DAY, "total").spent(5)]).await;
    let before = all_quota_rows(&rig.db).await;

    let finalize = FinalizeRequest {
        tenant_id: TENANT_A,
        turn_id,
        update: update(TerminalState::Failed, Some("quota_exceeded")),
        finalizer: Finalizer::Stream {
            selected_model: STANDARD.to_owned(),
            period_starts: PeriodStarts::of(default_now()),
        },
        usage: None,
        tool_counts: ToolCallCounts::default(),
    };
    let outcome = rig.finalize(&finalize).await.unwrap();
    assert!(
        matches!(outcome, FinalizeOutcome::NoSettlement),
        "{outcome:?}"
    );
    let row = turn_row(&rig.db, turn_id).await;
    assert_eq!(row.state, "failed");
    assert_eq!(row.error_code.as_deref(), Some("quota_exceeded"));
    assert_eq!(all_quota_rows(&rig.db).await, before);
    assert!(rig.events().await.is_empty());
}

// ── the orphan path ──────────────────────────────────────────────────────────

#[tokio::test]
async fn the_orphan_watchdog_settles_estimated_and_reports_the_effective_model_as_selected() {
    let rig = quota_rig().await;
    // the DESIGN example: premium selected, downgraded to standard
    seed_rows(
        &rig.db,
        &[
            row("daily", DAY, "tier:premium").spent(20_000_000),
            row("monthly", MONTH, "tier:premium").spent(200_000_000),
            row("daily", DAY, "total").spent(25_000_000),
            row("monthly", MONTH, "total").spent(240_000_000),
        ],
    )
    .await;
    let chat = rig.chat_a1(PREMIUM).await;
    let booked = rig.book(&chat, PREMIUM, plain_facts()).await;
    assert_eq!(booked.decision.effective_model, STANDARD);

    let finalize = request(
        &booked,
        orphan_finalizer(),
        update(TerminalState::Failed, Some("orphan_timeout")),
        None,
        ToolCallCounts::default(),
    );
    let settlement = finalize_ok(&rig, &finalize).await;
    assert_eq!(settlement.result.billing_outcome, BillingOutcome::Aborted);
    assert_eq!(
        settlement.result.settlement_method,
        SettlementMethod::Estimated
    );
    let event = rig.events().await.remove(0);
    assert_eq!(event.terminal_state, "failed");
    assert_eq!(event.billing_outcome, "aborted");
    assert_eq!(
        event.selected_model, STANDARD,
        "the effective model on the orphan path"
    );
    assert_eq!(event.effective_model, STANDARD);
    assert_eq!(
        turn_row(&rig.db, booked.turn_id)
            .await
            .error_code
            .as_deref(),
        Some("orphan_timeout")
    );
    assert_no_reserve(&all_quota_rows(&rig.db).await);
}

#[tokio::test]
async fn the_stream_path_reports_the_model_the_chat_selected() {
    let rig = quota_rig().await;
    seed_rows(
        &rig.db,
        &[
            row("daily", DAY, "tier:premium").spent(20_000_000),
            row("daily", DAY, "total").spent(25_000_000),
        ],
    )
    .await;
    let chat = rig.chat_a1(PREMIUM).await;
    let booked = rig.book(&chat, PREMIUM, plain_facts()).await;
    finalize_ok(&rig, &completed(&booked, PREMIUM, Some(usage(900, 300)))).await;
    let event = rig.events().await.remove(0);
    assert_eq!(event.selected_model, PREMIUM);
    assert_eq!(event.effective_model, STANDARD);
}

#[tokio::test]
async fn the_orphan_cas_also_requires_a_stale_turn_that_is_not_soft_deleted() {
    let rig = quota_rig().await;
    let booked = booked_standard(&rig).await;
    let rows = all_quota_rows(&rig.db).await;
    let orphan = request(
        &booked,
        orphan_finalizer(),
        update(TerminalState::Failed, Some("orphan_timeout")),
        None,
        ToolCallCounts::default(),
    );

    // progress after the cutoff: the turn is alive
    touch_turn(
        &rig.db,
        booked.turn_id,
        Some(utc(2026, 2, 28, 15, 45, 0)),
        None,
    )
    .await;
    assert!(matches!(
        rig.finalize(&orphan).await.unwrap(),
        FinalizeOutcome::AlreadyFinalized
    ));
    assert_eq!(turn_row(&rig.db, booked.turn_id).await.state, "running");

    // a soft-deleted turn is skipped
    touch_turn(
        &rig.db,
        booked.turn_id,
        None,
        Some(utc(2026, 2, 28, 15, 20, 0)),
    )
    .await;
    assert!(matches!(
        rig.finalize(&orphan).await.unwrap(),
        FinalizeOutcome::AlreadyFinalized
    ));

    // progress before the cutoff: stale, finalizable
    touch_turn(
        &rig.db,
        booked.turn_id,
        Some(utc(2026, 2, 28, 15, 35, 0)),
        None,
    )
    .await;
    assert_eq!(all_quota_rows(&rig.db).await, rows);
    assert!(matches!(
        rig.finalize(&orphan).await.unwrap(),
        FinalizeOutcome::Settled(_)
    ));
}

// ── overshoot ────────────────────────────────────────────────────────────────

/// A turn on the `quarter` model (0.25 credit per 1,000 tokens) holding the DESIGN overshoot
/// reserve: 10,000 tokens and 2,500,000 credits, booked in both `total` rows.
async fn overshoot_rig() -> (QuotaRig, BookedLike) {
    let gateway = QuotaGateway::serving(
        catalog_v(
            3,
            vec![
                model("quarter", ModelTier::Standard, 250_000_000),
                premium_model(),
            ],
        ),
        (60_000_000, 600_000_000),
        (22_000_000, 300_000_000),
    );
    let rig = quota_rig_with(QuotaRigOptions {
        gateway,
        ..QuotaRigOptions::new()
    })
    .await;
    seed_rows(
        &rig.db,
        &[
            row("daily", DAY, "total").reserved(2_500_000),
            row("monthly", MONTH, "total").reserved(2_500_000),
        ],
    )
    .await;
    let chat = rig.chat_a1("quarter").await;
    let turn = TurnSeed::running(&chat, default_now()).reserved(ReserveCols {
        policy_version: 3,
        effective_model: "quarter".to_owned(),
        reserve_tokens: 10_000,
        max_output_tokens: 500,
        reserved_credits_micro: 2_500_000,
        floor: 50,
    });
    let turn_id = seed_turn(&rig.db, &turn).await;
    (
        rig,
        BookedLike {
            turn_id,
            tenant_id: TENANT_A,
        },
    )
}

struct BookedLike {
    turn_id: Uuid,
    tenant_id: Uuid,
}

fn overshoot_request(
    turn: &BookedLike,
    state: TerminalState,
    code: Option<&str>,
    reported: UsageTokens,
) -> FinalizeRequest {
    FinalizeRequest {
        tenant_id: turn.tenant_id,
        turn_id: turn.turn_id,
        update: update(state, code),
        finalizer: Finalizer::Stream {
            selected_model: "quarter".to_owned(),
            period_starts: PeriodStarts::of(default_now()),
        },
        usage: Some(reported),
        tool_counts: ToolCallCounts::default(),
    }
}

#[tokio::test]
async fn an_overshoot_above_the_tolerance_commits_the_reserve_keeps_the_turn_completed_and_the_tokens_uncapped()
 {
    let (rig, turn) = overshoot_rig().await;
    // the DESIGN example: 11,000 input and 500 output tokens against a reserve of 10,000
    let settlement = finalize_ok(
        &rig,
        &overshoot_request(&turn, TerminalState::Completed, None, usage(11_000, 500)),
    )
    .await;
    assert!(settlement.result.overshoot);
    assert!(settlement.result.overshoot_capped);
    assert_eq!(settlement.result.committed_credits_micro, 2_500_000);

    let daily = quota_row(&rig.db, "daily", DAY, "total").await.unwrap();
    assert_eq!((daily.spent, daily.reserved), (2_500_000, 0));
    assert_eq!((daily.input_tokens, daily.output_tokens), (11_000, 500));
    assert_eq!(turn_row(&rig.db, turn.turn_id).await.state, "completed");

    let event = rig.events().await.remove(0);
    assert_eq!(
        event.actual_credits_micro, 2_500_000,
        "the committed amount, despite the name"
    );
    assert_eq!(event.usage.unwrap().input_tokens, 11_000);
    let payload = usage_payloads(&rig.db, &rig.usage_queue()).await.remove(0);
    assert!(payload.get("committed_credits_micro").is_none());
    assert!(payload.get("overshoot_capped").is_none());
}

#[tokio::test]
async fn an_overshoot_within_the_tolerance_commits_the_actual_credits() {
    let (rig, turn) = overshoot_rig().await;
    let settlement = finalize_ok(
        &rig,
        &overshoot_request(&turn, TerminalState::Completed, None, usage(10_000, 500)),
    )
    .await;
    assert!(settlement.result.overshoot);
    assert!(!settlement.result.overshoot_capped);
    assert_eq!(settlement.result.committed_credits_micro, 2_625_000);
    assert_eq!(spent_of(&rig.db, "daily", DAY, "total").await, 2_625_000);
}

#[tokio::test]
async fn a_failed_turn_with_known_usage_follows_the_same_overshoot_rule() {
    let (rig, turn) = overshoot_rig().await;
    let settlement = finalize_ok(
        &rig,
        &overshoot_request(
            &turn,
            TerminalState::Failed,
            Some("provider_error"),
            usage(11_000, 500),
        ),
    )
    .await;
    assert_eq!(
        settlement.result.settlement_method,
        SettlementMethod::Actual
    );
    assert!(settlement.result.overshoot_capped);
    assert_eq!(spent_of(&rig.db, "daily", DAY, "total").await, 2_500_000);
}

#[tokio::test]
async fn the_overshoot_metric_is_recorded_per_period_in_both_branches_and_not_without_overshoot() {
    for (reported, expect_overshoot) in [
        (usage(11_000, 500), true),
        (usage(10_000, 500), true),
        (usage(5_000, 500), false),
    ] {
        let (rig, turn) = overshoot_rig().await;
        let settlement = finalize_ok(
            &rig,
            &overshoot_request(&turn, TerminalState::Completed, None, reported),
        )
        .await;
        settlement.result.record_after_commit(rig.metrics.as_ref());
        let calls = rig.metrics.calls();
        let overshoots: Vec<&MetricCall> = calls
            .iter()
            .filter(|call| matches!(call, MetricCall::Overshoot(_)))
            .collect();
        if expect_overshoot {
            assert_eq!(overshoots.len(), 2, "{calls:?}");
            assert!(calls.contains(&MetricCall::Overshoot(PeriodType::Daily)));
            assert!(calls.contains(&MetricCall::Overshoot(PeriodType::Monthly)));
        } else {
            assert!(overshoots.is_empty(), "{calls:?}");
        }
    }
}

// ── tool counters ────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_actual_settlement_adds_the_tool_counts_to_the_total_row_only() {
    let rig = quota_rig().await;
    let booked = booked_premium(&rig).await;
    let tools = ToolCallCounts {
        web_search: 2,
        code_interpreter: 3,
        file_search: 4,
    };
    let finalize = request(
        &booked,
        stream_of(&booked, PREMIUM),
        update(TerminalState::Completed, None),
        Some(usage(100, 100)),
        tools,
    );
    finalize_ok(&rig, &finalize).await;
    for (period, start) in [("daily", DAY), ("monthly", MONTH)] {
        let total = quota_row(&rig.db, period, start, "total").await.unwrap();
        assert_eq!(
            (total.web_search_calls, total.code_interpreter_calls),
            (2, 3),
            "{period}"
        );
        assert_eq!(total.file_search_calls, 0, "{period}");
        let premium = quota_row(&rig.db, period, start, "tier:premium")
            .await
            .unwrap();
        assert_eq!(
            (premium.web_search_calls, premium.code_interpreter_calls),
            (0, 0),
            "{period}"
        );
    }
    let event = rig.events().await.remove(0);
    assert_eq!(
        (
            event.web_search_calls,
            event.code_interpreter_calls,
            event.file_search_calls
        ),
        (2, 3, 4)
    );
}

#[tokio::test]
async fn the_reserved_columns_of_the_deferred_scope_are_never_populated() {
    let rig = quota_rig().await;
    let booked = booked_premium(&rig).await;
    let tools = ToolCallCounts {
        web_search: 1,
        code_interpreter: 1,
        file_search: 1,
    };
    let finalize = request(
        &booked,
        stream_of(&booked, PREMIUM),
        update(TerminalState::Completed, None),
        Some(usage(100, 100)),
        tools,
    );
    finalize_ok(&rig, &finalize).await;
    let rows = all_quota_rows(&rig.db).await;
    assert!(!rows.is_empty());
    for row in &rows {
        assert_eq!(row.file_search_calls, 0, "{row:?}");
        assert_eq!(row.rag_retrieval_calls, 0, "{row:?}");
        assert_eq!(row.image_inputs, 0, "{row:?}");
        assert_eq!(row.image_upload_bytes, 0, "{row:?}");
        assert!(
            row.bucket == "total" || row.bucket == "tier:premium",
            "no tier:standard row: {row:?}"
        );
    }
}

// ── the usage message ────────────────────────────────────────────────────────

#[tokio::test]
async fn the_settlement_enqueues_exactly_one_usage_message_with_the_dedupe_key() {
    let rig = quota_rig().await;
    let booked = booked_standard(&rig).await;
    let settlement = finalize_ok(&rig, &completed(&booked, STANDARD, Some(usage(900, 300)))).await;

    let rows = usage_rows(&rig.db, &rig.usage_queue()).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].queue, "mini-chat.usage_snapshot");
    assert_eq!(rows[0].payload_type, USAGE_PAYLOAD_TYPE);
    assert_eq!(rows[0].partition, i64::from(partition_for(TENANT_A, 4)));
    assert_eq!(
        outbox_body_count(&rig.db).await,
        1,
        "no other queue received anything"
    );

    let event = rig.events().await.remove(0);
    assert_eq!(event, settlement.result.usage_event);
    let expected = format!(
        "{}/{}/{}",
        TENANT_A.simple(),
        booked.turn_id.simple(),
        booked.identity.request_id.simple()
    );
    assert_eq!(event.dedupe_key.as_deref(), Some(expected.as_str()));
    assert_eq!(event.tenant_id, TENANT_A);
    assert_eq!(event.user_id, Some(USER_A1));
    assert_eq!(event.chat_id, booked.identity.chat_id);
    assert_eq!(event.turn_id, Some(booked.turn_id));
    assert_eq!(event.request_id, booked.identity.request_id);
    assert_eq!(event.policy_version_applied, 1);
    assert_eq!(event.requester_type, "user");
    assert_eq!(event.usage, Some(usage(900, 300)));
    settlement.wake.fire();
}

#[tokio::test]
async fn the_usage_queue_is_the_configured_name() {
    let rig = quota_rig_with(QuotaRigOptions {
        outbox: OutboxConfig {
            queue_name: "acme.usage".to_owned(),
            ..OutboxConfig::default()
        },
        ..QuotaRigOptions::new()
    })
    .await;
    let booked = booked_standard(&rig).await;
    finalize_ok(&rig, &completed(&booked, STANDARD, Some(usage(900, 300)))).await;
    assert_eq!(usage_rows(&rig.db, "acme.usage").await.len(), 1);
    assert!(
        usage_rows(&rig.db, "mini-chat.usage_snapshot")
            .await
            .is_empty()
    );
}

// ── rollback of the whole transaction ────────────────────────────────────────

#[tokio::test]
async fn a_failed_enqueue_rolls_back_the_cas_and_the_quota_change() {
    let failing = FailingUsageQueue::internal();
    let rig = quota_rig_with(QuotaRigOptions {
        queue: UsageQueueKind::Custom(failing.clone()),
        ..QuotaRigOptions::new()
    })
    .await;
    let booked = booked_standard(&rig).await;
    let rows = all_quota_rows(&rig.db).await;
    let error = rig
        .finalize(&completed(&booked, STANDARD, Some(usage(900, 300))))
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::Internal { .. }), "{error:?}");
    assert_eq!(failing.call_count(), 1);
    assert_eq!(turn_row(&rig.db, booked.turn_id).await.state, "running");
    assert_eq!(
        all_quota_rows(&rig.db).await,
        rows,
        "quota is never debited without a usage message"
    );
}

#[tokio::test]
async fn an_oversized_usage_payload_surfaces_through_the_enqueue_error_and_rolls_back() {
    let rig = quota_rig_with(QuotaRigOptions {
        queue: UsageQueueKind::Custom(FailingUsageQueue::payload_too_large()),
        ..QuotaRigOptions::new()
    })
    .await;
    let booked = booked_standard(&rig).await;
    let rows = all_quota_rows(&rig.db).await;
    let error = rig
        .finalize(&completed(&booked, STANDARD, Some(usage(900, 300))))
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            DomainError::OutboxPayloadTooLarge {
                site: EnqueueSite::TurnFinalize
            }
        ),
        "{error:?}"
    );
    assert_eq!(turn_row(&rig.db, booked.turn_id).await.state, "running");
    assert_eq!(all_quota_rows(&rig.db).await, rows);
}

#[tokio::test]
async fn a_database_failure_of_the_real_enqueue_rolls_back_the_cas_and_the_quota_change() {
    let rig = quota_rig().await;
    let booked = booked_standard(&rig).await;
    let rows = all_quota_rows(&rig.db).await;
    fail_outbox_inserts(&rig.db).await;
    assert!(
        rig.finalize(&completed(&booked, STANDARD, Some(usage(900, 300))))
            .await
            .is_err()
    );
    assert_eq!(turn_row(&rig.db, booked.turn_id).await.state, "running");
    assert_eq!(all_quota_rows(&rig.db).await, rows);
}

#[tokio::test]
async fn a_snapshot_failure_leaves_the_turn_running_and_changes_nothing() {
    let rig = quota_rig().await;
    let booked = booked_standard(&rig).await;
    let rows = all_quota_rows(&rig.db).await;
    rig.gateway
        .fail_version
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let error = rig
        .finalize(&completed(&booked, STANDARD, Some(usage(900, 300))))
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            DomainError::PolicyUnavailable { .. } | DomainError::Internal { .. }
        ),
        "{error:?}"
    );
    assert_eq!(turn_row(&rig.db, booked.turn_id).await.state, "running");
    assert_eq!(all_quota_rows(&rig.db).await, rows);
    assert!(rig.events().await.is_empty());
}

#[traced_test]
#[tokio::test]
async fn a_credit_computation_failure_fails_the_transaction_and_names_the_offending_count() {
    let rig = quota_rig().await;
    let booked = booked_standard(&rig).await;
    let rows = all_quota_rows(&rig.db).await;
    let error = rig
        .finalize(&completed(&booked, STANDARD, Some(usage(10_000_001, 0))))
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::Internal { .. }), "{error:?}");
    assert!(logs_contain("10000001"));
    assert_eq!(turn_row(&rig.db, booked.turn_id).await.state, "running");
    assert_eq!(all_quota_rows(&rig.db).await, rows);
    assert!(rig.events().await.is_empty());
}

#[tokio::test]
async fn a_zero_multiplier_in_the_applied_snapshot_fails_the_transaction() {
    let rig = quota_rig().await;
    let booked = booked_standard(&rig).await;
    let rows = all_quota_rows(&rig.db).await;
    // the applied version now lists the model with a zero multiplier
    let mut broken = standard_model();
    broken.input_tokens_credit_multiplier_micro = 0;
    rig.gateway
        .add_version(catalog_v(1, vec![premium_model(), broken]));
    let error = rig
        .finalize(&completed(&booked, STANDARD, Some(usage(900, 300))))
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::Internal { .. }), "{error:?}");
    assert_eq!(turn_row(&rig.db, booked.turn_id).await.state, "running");
    assert_eq!(all_quota_rows(&rig.db).await, rows);
}

// ── the policy version of the turn ───────────────────────────────────────────

#[tokio::test]
async fn the_multipliers_and_the_tier_come_from_the_snapshot_of_the_applied_version() {
    let rig = quota_rig().await;
    let booked = booked_premium(&rig).await;
    let current_calls = rig.gateway.current_call_count();

    // version 2 is current now: the premium model got five times dearer and became standard
    let mut moved = model(PREMIUM, ModelTier::Standard, 12_500_000_000);
    moved.multimodal_capabilities = vec!["VISION_INPUT".to_owned()];
    rig.gateway
        .add_version(catalog_v(2, vec![moved, standard_model()]));
    rig.gateway.set_current(2);

    let settlement = finalize_ok(&rig, &completed(&booked, PREMIUM, Some(usage(800, 400)))).await;
    // priced at the 2.5-credit multiplier of version 1, not the version 2 multiplier
    assert_eq!(settlement.result.committed_credits_micro, 3_000_000);
    assert_eq!(settlement.result.tier, Some(ModelTier::Premium));
    for bucket in ["total", "tier:premium"] {
        assert_eq!(
            spent_of(&rig.db, "daily", DAY, bucket).await,
            3_000_000,
            "{bucket}"
        );
    }
    // only the snapshot of the applied version was asked, never the current version
    assert_eq!(
        *rig.gateway.version_calls.lock().unwrap(),
        vec![(USER_A1, 1)]
    );
    assert_eq!(rig.gateway.current_call_count(), current_calls);
    assert_eq!(rig.events().await[0].policy_version_applied, 1);
}

// ── a turn without a reserve ─────────────────────────────────────────────────

#[traced_test]
#[tokio::test]
async fn a_turn_without_reserve_columns_changes_no_row_and_enqueues_the_placeholder_event() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    let turn_id = seed_turn(
        &rig.db,
        &TurnSeed::running(&chat, utc(2026, 2, 28, 15, 0, 0)),
    )
    .await;
    seed_rows(&rig.db, &[row("daily", DAY, "total").spent(9).reserved(3)]).await;
    let rows = all_quota_rows(&rig.db).await;

    let finalize = FinalizeRequest {
        tenant_id: TENANT_A,
        turn_id,
        update: update(TerminalState::Failed, Some("orphan_timeout")),
        finalizer: orphan_finalizer(),
        usage: None,
        tool_counts: ToolCallCounts::default(),
    };
    let settlement = finalize_ok(&rig, &finalize).await;
    assert_eq!(settlement.result.billing_outcome, BillingOutcome::Aborted);
    assert_eq!(
        settlement.result.settlement_method,
        SettlementMethod::Estimated
    );
    assert_eq!(settlement.result.committed_credits_micro, 0);
    assert_eq!(settlement.result.tier, None);
    assert_eq!(all_quota_rows(&rig.db).await, rows);
    assert_eq!(
        rig.gateway.total_calls(),
        0,
        "no snapshot is needed without a reserve"
    );
    assert_eq!(turn_row(&rig.db, turn_id).await.state, "failed");

    let event = rig.events().await.remove(0);
    assert_eq!(event.billing_outcome, "aborted");
    assert_eq!(event.settlement_method, "estimated");
    assert_eq!(event.actual_credits_micro, 0);
    assert_eq!(event.usage, None);
    assert_eq!(event.effective_model, "");
    assert_eq!(event.selected_model, "");
    assert_eq!(event.policy_version_applied, 0);
    let expected = format!(
        "{}/{}/{}",
        TENANT_A.simple(),
        turn_id.simple(),
        turn_row(&rig.db, turn_id).await.request_id.simple()
    );
    assert_eq!(event.dedupe_key.as_deref(), Some(expected.as_str()));
    assert!(
        logs_contain("WARN"),
        "the skipped bucket updates are logged as a warning"
    );
}

#[tokio::test]
async fn a_turn_without_a_requester_user_is_also_settled_as_a_turn_without_a_reserve() {
    let rig = quota_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    let seed = TurnSeed::running(&chat, default_now())
        .reserved(ReserveCols {
            policy_version: 1,
            effective_model: STANDARD.to_owned(),
            reserve_tokens: 1500,
            max_output_tokens: 500,
            reserved_credits_micro: 1_500_000,
            floor: 50,
        })
        .without_user();
    let turn_id = seed_turn(&rig.db, &seed).await;
    seed_rows(&rig.db, &[row("daily", DAY, "total").reserved(1_500_000)]).await;
    let rows = all_quota_rows(&rig.db).await;
    let finalize = FinalizeRequest {
        tenant_id: TENANT_A,
        turn_id,
        update: update(TerminalState::Cancelled, None),
        finalizer: Finalizer::Stream {
            selected_model: STANDARD.to_owned(),
            period_starts: PeriodStarts::of(default_now()),
        },
        usage: None,
        tool_counts: ToolCallCounts::default(),
    };
    let settlement = finalize_ok(&rig, &finalize).await;
    assert_eq!(settlement.result.committed_credits_micro, 0);
    assert_eq!(all_quota_rows(&rig.db).await, rows);
    assert_eq!(rig.events().await[0].policy_version_applied, 0);
}

// ── the period boundary ──────────────────────────────────────────────────────

/// A turn reserved at 23:59:59.999 of the last February day, with consumption already present
/// on day 2 and in March so that an accidental settlement there would be visible.
async fn boundary_rig() -> (QuotaRig, std::sync::Arc<FixedClock>) {
    let clock = FixedClock::at(utc_ms(2026, 2, 28, 23, 59, 59, 999));
    let rig = quota_rig_with(QuotaRigOptions {
        clock: clock.clone(),
        ..QuotaRigOptions::new()
    })
    .await;
    seed_rows(
        &rig.db,
        &[
            row("daily", "2026-03-01", "total").reserved(777).spent(5),
            row("monthly", "2026-03-01", "total").reserved(777).spent(5),
        ],
    )
    .await;
    (rig, clock)
}

fn assert_day_one_settled_and_day_two_untouched(rows: &[crate::test_support::quota::QuotaRowView]) {
    let find = |period: &str, start: &str| {
        rows.iter()
            .find(|r| r.period == period && r.start == start && r.bucket == "total")
            .unwrap_or_else(|| panic!("row {period} {start} missing"))
    };
    for (period, start) in [("daily", DAY), ("monthly", MONTH)] {
        let row = find(period, start);
        assert_eq!(row.reserved, 0, "{period}: the day-1 reserve is released");
        assert_eq!(row.spent, 1_200_000, "{period}: the day-1 rows are settled");
    }
    for period in ["daily", "monthly"] {
        let row = find(period, "2026-03-01");
        assert_eq!(
            (row.reserved, row.spent, row.calls),
            (777, 5, 0),
            "{period}: day 2 is untouched"
        );
    }
    assert!(
        rows.iter().all(|r| r.reserved >= 0),
        "no negative reserve: {rows:?}"
    );
}

#[tokio::test]
async fn a_turn_reserved_before_midnight_and_settled_after_it_on_the_stream_path_settles_day_one() {
    let (rig, clock) = boundary_rig().await;
    let booked = booked_standard(&rig).await;
    assert_eq!(booked.decision.period_starts.daily, date(2026, 2, 28));
    clock.set(utc(2026, 3, 1, 0, 0, 1));
    finalize_ok(&rig, &completed(&booked, STANDARD, Some(usage(900, 300)))).await;
    assert_day_one_settled_and_day_two_untouched(&all_quota_rows(&rig.db).await);
}

#[tokio::test]
async fn the_orphan_path_derives_the_same_rows_from_started_at() {
    let (rig, clock) = boundary_rig().await;
    let booked = booked_standard(&rig).await;
    assert_eq!(
        turn_row(&rig.db, booked.turn_id).await.started_at,
        utc_ms(2026, 2, 28, 23, 59, 59, 999),
        "started_at is the preflight instant"
    );
    clock.set(utc(2026, 3, 1, 0, 6, 0));
    let finalize = request(
        &booked,
        Finalizer::Orphan {
            stale_cutoff: utc(2026, 3, 1, 0, 5, 0),
        },
        update(TerminalState::Failed, Some("orphan_timeout")),
        None,
        ToolCallCounts::default(),
    );
    let settlement = finalize_ok(&rig, &finalize).await;
    assert_eq!(settlement.result.period_starts.daily, date(2026, 2, 28));
    let rows = all_quota_rows(&rig.db).await;
    for (period, start) in [("daily", DAY), ("monthly", MONTH)] {
        let row = rows
            .iter()
            .find(|r| r.period == period && r.start == start && r.bucket == "total")
            .unwrap();
        assert_eq!((row.reserved, row.spent), (0, 1_050_000), "{period}");
    }
    for period in ["daily", "monthly"] {
        let day_two = rows
            .iter()
            .find(|r| r.period == period && r.start == "2026-03-01")
            .unwrap();
        assert_eq!(
            (day_two.reserved, day_two.spent, day_two.calls),
            (777, 5, 0),
            "{period}"
        );
    }
    assert!(rows.iter().all(|r| r.reserved >= 0));
}

#[tokio::test]
async fn a_retry_or_edit_turn_started_earlier_is_settled_on_its_own_preflight_rows() {
    let (rig, clock) = boundary_rig().await;
    let chat = rig.chat_a1(STANDARD).await;
    // the mutation flow committed the turn at another instant, before the preflight
    let turn = TurnSeed::running(&chat, utc(2026, 2, 28, 23, 0, 0));
    let turn_id = seed_turn(&rig.db, &turn).await;
    let decision = rig.preflight(&chat, STANDARD, plain_facts()).await.unwrap();
    rig.reserve_existing(&QuotaRig::identity(&chat), &decision, turn_id)
        .await
        .unwrap();
    assert_eq!(
        turn_row(&rig.db, turn_id).await.started_at,
        utc_ms(2026, 2, 28, 23, 59, 59, 999)
    );

    clock.set(utc(2026, 3, 1, 0, 6, 0));
    let finalize = FinalizeRequest {
        tenant_id: TENANT_A,
        turn_id,
        update: update(TerminalState::Failed, Some("orphan_timeout")),
        finalizer: Finalizer::Orphan {
            stale_cutoff: utc(2026, 3, 1, 0, 5, 0),
        },
        usage: None,
        tool_counts: ToolCallCounts::default(),
    };
    finalize_ok(&rig, &finalize).await;
    let rows = all_quota_rows(&rig.db).await;
    for (period, start) in [("daily", DAY), ("monthly", MONTH)] {
        let row = rows
            .iter()
            .find(|r| r.period == period && r.start == start && r.bucket == "total")
            .unwrap();
        assert_eq!(row.reserved, 0, "{period}");
    }
    assert!(rows.iter().all(|r| r.reserved >= 0), "{rows:?}");
}

// ── metrics of the result ────────────────────────────────────────────────────

#[tokio::test]
async fn an_actual_settlement_records_the_commit_the_actual_tokens_and_the_code_interpreter_calls()
{
    let rig = quota_rig().await;
    let booked = booked_standard(&rig).await;
    let tools = ToolCallCounts {
        web_search: 0,
        code_interpreter: 3,
        file_search: 0,
    };
    let finalize = request(
        &booked,
        stream_of(&booked, STANDARD),
        update(TerminalState::Completed, None),
        Some(usage(900, 300)),
        tools,
    );
    let settlement = finalize_ok(&rig, &finalize).await;
    assert_eq!(settlement.result.actual_tokens, 1200);
    assert_eq!(settlement.result.code_interpreter_calls, 3);
    rig.metrics.clear();
    settlement.result.record_after_commit(rig.metrics.as_ref());
    let calls = rig.metrics.calls();
    assert_eq!(
        calls
            .iter()
            .filter(|c| matches!(c, MetricCall::Commit(_)))
            .count(),
        2,
        "{calls:?}"
    );
    assert!(calls.contains(&MetricCall::Commit(PeriodType::Daily)));
    assert!(calls.contains(&MetricCall::Commit(PeriodType::Monthly)));
    assert!(calls.contains(&MetricCall::ActualTokens(1200)));
    assert!(calls.contains(&MetricCall::CodeInterpreterCalls {
        model: STANDARD.to_owned(),
        calls: 3
    }));
    assert!(
        !calls.iter().any(|c| matches!(c, MetricCall::Overshoot(_))),
        "{calls:?}"
    );
}

#[tokio::test]
async fn an_estimated_settlement_records_the_commit_only() {
    let rig = quota_rig().await;
    let booked = booked_standard(&rig).await;
    let tools = ToolCallCounts {
        web_search: 1,
        code_interpreter: 3,
        file_search: 0,
    };
    let finalize = request(
        &booked,
        stream_of(&booked, STANDARD),
        update(TerminalState::Cancelled, None),
        None,
        tools,
    );
    let settlement = finalize_ok(&rig, &finalize).await;
    rig.metrics.clear();
    settlement.result.record_after_commit(rig.metrics.as_ref());
    let calls = rig.metrics.calls();
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, MetricCall::ActualTokens(_))),
        "{calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, MetricCall::CodeInterpreterCalls { .. })),
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|c| matches!(c, MetricCall::Overshoot(_))),
        "{calls:?}"
    );
}

#[tokio::test]
async fn the_finalization_itself_records_no_metric_before_the_commit() {
    let rig = quota_rig().await;
    let booked = booked_standard(&rig).await;
    rig.metrics.clear();
    finalize_ok(&rig, &completed(&booked, STANDARD, Some(usage(900, 300)))).await;
    assert!(rig.metrics.calls().is_empty());
}

// ── the turn billing states ──────────────────────────────────────────────────

#[tokio::test]
async fn billing_state_in_progress_moves_to_exactly_one_terminal_state() {
    for (state, code, expected_state, outcome) in [
        (
            TerminalState::Completed,
            None,
            "completed",
            BillingOutcome::Completed,
        ),
        (
            TerminalState::Failed,
            Some("provider_error"),
            "failed",
            BillingOutcome::Failed,
        ),
        (
            TerminalState::Cancelled,
            None,
            "cancelled",
            BillingOutcome::Aborted,
        ),
        (
            TerminalState::Failed,
            Some("orphan_timeout"),
            "failed",
            BillingOutcome::Aborted,
        ),
    ] {
        let rig = quota_rig().await;
        let booked = booked_standard(&rig).await;
        assert_eq!(turn_row(&rig.db, booked.turn_id).await.state, "running");
        let finalize = request(
            &booked,
            if code == Some("orphan_timeout") {
                orphan_finalizer()
            } else {
                stream_of(&booked, STANDARD)
            },
            update(state, code),
            None,
            ToolCallCounts::default(),
        );
        let settlement = finalize_ok(&rig, &finalize).await;
        assert_eq!(
            settlement.result.billing_outcome, outcome,
            "{expected_state} {code:?}"
        );
        assert_eq!(
            turn_row(&rig.db, booked.turn_id).await.state,
            expected_state
        );
        // a terminal state is immutable: a later signal cannot move it
        let late = request(
            &booked,
            stream_of(&booked, STANDARD),
            update(TerminalState::Completed, None),
            Some(usage(1, 1)),
            ToolCallCounts::default(),
        );
        assert!(matches!(
            rig.finalize(&late).await.unwrap(),
            FinalizeOutcome::AlreadyFinalized
        ));
        assert_eq!(
            turn_row(&rig.db, booked.turn_id).await.state,
            expected_state
        );
    }
}

#[tokio::test]
async fn reserve_state_reserved_moves_to_one_settled_state_per_method() {
    for (code, state, reported, method) in [
        (
            None,
            TerminalState::Completed,
            Some(usage(10, 10)),
            SettlementMethod::Actual,
        ),
        (
            Some("provider_error"),
            TerminalState::Failed,
            None,
            SettlementMethod::Estimated,
        ),
        (
            Some("turn_setup_failed"),
            TerminalState::Failed,
            None,
            SettlementMethod::Released,
        ),
    ] {
        let rig = quota_rig().await;
        let booked = booked_standard(&rig).await;
        assert_eq!(
            quota_row(&rig.db, "daily", DAY, "total")
                .await
                .unwrap()
                .reserved,
            1_500_000
        );
        let finalize = request(
            &booked,
            stream_of(&booked, STANDARD),
            update(state, code),
            reported,
            ToolCallCounts::default(),
        );
        let settlement = finalize_ok(&rig, &finalize).await;
        assert_eq!(settlement.result.settlement_method, method);
        assert_no_reserve(&all_quota_rows(&rig.db).await);
    }
}

// ── the quota warnings read after the settlement ─────────────────────────────

#[tokio::test]
async fn the_warnings_read_after_the_settlement_show_the_exhaustion_this_turn_caused() {
    use crate::domain::quota::QuotaTier;
    use crate::test_support::chats::a1;

    let rig = quota_rig().await;
    // a daily total limit of exactly one standard turn at its full reserve
    rig.gateway
        .set_limits((1_500_000, 600_000_000), (22_000_000, 300_000_000));
    let booked = booked_standard(&rig).await;
    // 1,000 input and 500 output tokens: the whole reserve of 1,500,000 is spent
    finalize_ok(&rig, &completed(&booked, STANDARD, Some(usage(1000, 500)))).await;

    let warnings = rig.service.quota_warnings(&a1()).await.unwrap();
    let find = |tier: QuotaTier, period: PeriodType| {
        warnings
            .iter()
            .find(|w| w.tier == tier && w.period == period)
            .copied()
            .unwrap_or_else(|| panic!("{tier:?} {period:?} missing in {warnings:?}"))
    };
    let daily_total = find(QuotaTier::Total, PeriodType::Daily);
    assert_eq!(daily_total.remaining_percentage, 0);
    assert!(daily_total.exhausted);
    assert!(daily_total.warning);
    assert_eq!(daily_total.next_reset, Some(utc(2026, 3, 1, 0, 0, 0)));
    let monthly_total = find(QuotaTier::Total, PeriodType::Monthly);
    assert!(!monthly_total.warning);
    assert_eq!(monthly_total.next_reset, None);
    assert_eq!(warnings.len(), 4);

    // the same rows and rules as the status endpoint
    let status = rig.service.quota_status(&a1()).await.unwrap();
    for tier in &status.tiers {
        for listed in &tier.periods {
            let entry = find(tier.tier, listed.period);
            assert_eq!(entry.remaining_percentage, listed.remaining_percentage);
            assert_eq!(entry.warning, listed.warning);
            assert_eq!(entry.exhausted, listed.exhausted);
        }
    }
}
