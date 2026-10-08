//! Settlement arithmetic, bucket updates and usage events.

use std::sync::Arc;

use mini_chat_sdk::UsageTokens;
use uuid::Uuid;

use super::{book, get_row, req, row_count, turn_of};
use crate::domain::error::DomainError;
use crate::domain::quota::{
    BUCKET_PREMIUM, BUCKET_TOTAL, BillingOutcome, PERIOD_DAILY, PERIOD_MONTHLY, PreflightDecision, Settlement,
    SettlementInput, SettlementMethod, dedupe_key, enqueue_usage, preflight, settle, usage_event,
};
use crate::domain::services::AppServices;
use crate::testing::{PREMIUM, STANDARD, TENANT_A, TestApp, USER_A1};

const STANDARD_RESERVE: i64 = 220 + 12_288;
const PREMIUM_RESERVE: i64 = 660 + 61_440;

fn usage(i: i64, o: i64) -> UsageTokens {
    UsageTokens { input_tokens: i, output_tokens: o, ..UsageTokens::default() }
}

fn input(d: Option<&PreflightDecision>, method: SettlementMethod, usage: Option<UsageTokens>) -> SettlementInput {
    let started = crate::clock::now();
    SettlementInput {
        tenant_id: TENANT_A,
        user_id: USER_A1,
        turn: turn_of(d, started),
        billing_outcome: BillingOutcome::Completed,
        method,
        usage,
        web_search_calls: 2,
        code_interpreter_calls: 1,
        periods: d.map_or_else(|| crate::domain::quota::period_starts(started), |d| d.periods),
    }
}

async fn run_settle(app: &Arc<AppServices>, input: &SettlementInput) -> Result<Settlement, DomainError> {
    let (app2, input) = (Arc::clone(app), input.clone());
    app.db.transaction(move |tx| Box::pin(async move { settle(&app2, tx, &input).await })).await
}

async fn reserved(t: &TestApp, model: &str) -> PreflightDecision {
    let d = preflight(&t.app, &req(model)).await.unwrap();
    book(&t.app, &d).await.unwrap();
    d
}

#[tokio::test]
async fn actual_standard_settlement_updates_total_rows() {
    let t = TestApp::new().await;
    let d = reserved(&t, STANDARD).await;
    let s = run_settle(&t.app, &input(Some(&d), SettlementMethod::Actual, Some(usage(100, 50)))).await.unwrap();
    assert_eq!(s.actual_credits_micro, 250);
    assert!(!s.overshoot_capped);
    assert_eq!(s.method, SettlementMethod::Actual);
    assert_eq!(s.billing_outcome, BillingOutcome::Completed);
    for (pt, start) in [(PERIOD_DAILY, d.periods.daily), (PERIOD_MONTHLY, d.periods.monthly)] {
        let row = get_row(&t.app, pt, start, BUCKET_TOTAL).await.unwrap();
        assert_eq!(row.reserved_credits_micro, 0, "{pt}");
        assert_eq!(row.spent_credits_micro, 250);
        assert_eq!(row.calls, 1);
        assert_eq!(row.input_tokens, 100);
        assert_eq!(row.output_tokens, 50);
        assert_eq!(row.web_search_calls, 2);
        assert_eq!(row.code_interpreter_calls, 1);
    }
    assert!(get_row(&t.app, PERIOD_DAILY, d.periods.daily, BUCKET_PREMIUM).await.is_none());
}

#[tokio::test]
async fn actual_premium_settlement_updates_both_buckets_telemetry_on_total_only() {
    let t = TestApp::new().await;
    let d = reserved(&t, PREMIUM).await;
    let s = run_settle(&t.app, &input(Some(&d), SettlementMethod::Actual, Some(usage(100, 50)))).await.unwrap();
    assert_eq!(s.actual_credits_micro, 300 + 750);
    for (pt, start) in [(PERIOD_DAILY, d.periods.daily), (PERIOD_MONTHLY, d.periods.monthly)] {
        let total = get_row(&t.app, pt, start, BUCKET_TOTAL).await.unwrap();
        assert_eq!((total.reserved_credits_micro, total.spent_credits_micro, total.calls), (0, 1050, 1));
        assert_eq!((total.input_tokens, total.output_tokens, total.web_search_calls, total.code_interpreter_calls), (100, 50, 2, 1));
        let premium = get_row(&t.app, pt, start, BUCKET_PREMIUM).await.unwrap();
        assert_eq!((premium.reserved_credits_micro, premium.spent_credits_micro, premium.calls), (0, 1050, 1));
        assert_eq!(
            (premium.input_tokens, premium.output_tokens, premium.web_search_calls, premium.code_interpreter_calls),
            (0, 0, 0, 0)
        );
    }
}

#[tokio::test]
async fn estimated_settlement_charges_input_plus_floor() {
    let t = TestApp::new().await;
    let d = reserved(&t, STANDARD).await;
    let mut i = input(Some(&d), SettlementMethod::Estimated, None);
    i.billing_outcome = BillingOutcome::Aborted;
    let s = run_settle(&t.app, &i).await.unwrap();
    // est_in = 4316 - 4096 = 220; floor 50 -> 220 * 1 + 50 * 3
    assert_eq!(s.actual_credits_micro, 370);
    let row = get_row(&t.app, PERIOD_DAILY, d.periods.daily, BUCKET_TOTAL).await.unwrap();
    assert_eq!((row.reserved_credits_micro, row.spent_credits_micro, row.calls), (0, 370, 1));
    assert_eq!((row.input_tokens, row.output_tokens), (0, 0), "no token telemetry on estimated");
    assert_eq!((row.web_search_calls, row.code_interpreter_calls), (2, 1));
}

#[tokio::test]
async fn released_settlement_charges_nothing() {
    let t = TestApp::new().await;
    let d = reserved(&t, PREMIUM).await;
    let mut i = input(Some(&d), SettlementMethod::Released, Some(usage(10, 10)));
    i.billing_outcome = BillingOutcome::Failed;
    let s = run_settle(&t.app, &i).await.unwrap();
    assert_eq!(s.actual_credits_micro, 0);
    for bucket in [BUCKET_TOTAL, BUCKET_PREMIUM] {
        let row = get_row(&t.app, PERIOD_MONTHLY, d.periods.monthly, bucket).await.unwrap();
        assert_eq!((row.reserved_credits_micro, row.spent_credits_micro, row.calls), (0, 0, 1), "{bucket}");
        assert_eq!((row.input_tokens, row.web_search_calls, row.code_interpreter_calls), (0, 0, 0));
    }
}

#[tokio::test]
async fn overshoot_beyond_tolerance_is_capped_at_reserve() {
    let t = TestApp::new().await;
    let d = reserved(&t, STANDARD).await;
    // reserve_tokens 4316; 5000 tokens -> ratio 1.158 > 1.10
    let s = run_settle(&t.app, &input(Some(&d), SettlementMethod::Actual, Some(usage(5000, 0)))).await.unwrap();
    assert!(s.overshoot_capped);
    assert_eq!(s.actual_credits_micro, STANDARD_RESERVE);
    let row = get_row(&t.app, PERIOD_DAILY, d.periods.daily, BUCKET_TOTAL).await.unwrap();
    assert_eq!(row.spent_credits_micro, STANDARD_RESERVE);
    assert_eq!(row.input_tokens, 5000, "actual tokens kept as telemetry");
}

#[tokio::test]
async fn overshoot_within_tolerance_commits_actual() {
    let t = TestApp::new().await;
    let d = reserved(&t, STANDARD).await;
    // 4700 / 4316 = 1.089 <= 1.10
    let s = run_settle(&t.app, &input(Some(&d), SettlementMethod::Actual, Some(usage(4700, 0)))).await.unwrap();
    assert!(!s.overshoot_capped);
    assert_eq!(s.actual_credits_micro, 4700);
}

#[tokio::test]
async fn completed_without_usage_charges_zero() {
    let t = TestApp::new().await;
    let d = reserved(&t, STANDARD).await;
    let s = run_settle(&t.app, &input(Some(&d), SettlementMethod::Actual, None)).await.unwrap();
    assert_eq!(s.actual_credits_micro, 0);
    let row = get_row(&t.app, PERIOD_DAILY, d.periods.daily, BUCKET_TOTAL).await.unwrap();
    assert_eq!((row.reserved_credits_micro, row.spent_credits_micro, row.calls), (0, 0, 1));
}

#[tokio::test]
async fn turn_without_reserve_is_skipped() {
    let t = TestApp::new().await;
    let s = run_settle(&t.app, &input(None, SettlementMethod::Estimated, None)).await.unwrap();
    assert_eq!(s.actual_credits_micro, 0);
    assert_eq!(s.method, SettlementMethod::Estimated);
    assert_eq!(row_count(&t.app).await, 0);
}

#[tokio::test]
async fn reserve_release_is_floored_at_zero() {
    let t = TestApp::new().await;
    let d = preflight(&t.app, &req(STANDARD)).await.unwrap(); // no reserve booked
    let s = run_settle(&t.app, &input(Some(&d), SettlementMethod::Actual, Some(usage(10, 10)))).await.unwrap();
    assert_eq!(s.actual_credits_micro, 40);
    let row = get_row(&t.app, PERIOD_DAILY, d.periods.daily, BUCKET_TOTAL).await.unwrap();
    assert_eq!(row.reserved_credits_micro, 0);
    assert_eq!(row.spent_credits_micro, 40);
}

#[tokio::test]
async fn settlement_uses_persisted_periods() {
    let t = TestApp::new().await;
    let d = reserved(&t, STANDARD).await;
    let mut i = input(Some(&d), SettlementMethod::Actual, Some(usage(1, 1)));
    let yesterday = d.periods.daily.previous_day().unwrap();
    i.periods.daily = yesterday;
    run_settle(&t.app, &i).await.unwrap();
    let old = get_row(&t.app, PERIOD_DAILY, yesterday, BUCKET_TOTAL).await.unwrap();
    assert_eq!(old.spent_credits_micro, 4);
    let today = get_row(&t.app, PERIOD_DAILY, d.periods.daily, BUCKET_TOTAL).await.unwrap();
    assert_eq!(today.reserved_credits_micro, STANDARD_RESERVE, "today's reserve untouched");
}

#[tokio::test]
async fn credit_computation_failure_is_internal() {
    let t = TestApp::new().await;
    let d = reserved(&t, STANDARD).await;
    let err = run_settle(&t.app, &input(Some(&d), SettlementMethod::Actual, Some(usage(20_000_000, 0)))).await.unwrap_err();
    assert!(matches!(err, DomainError::Internal(_)), "{err:?}");
    let row = get_row(&t.app, PERIOD_DAILY, d.periods.daily, BUCKET_TOTAL).await.unwrap();
    assert_eq!(row.reserved_credits_micro, STANDARD_RESERVE, "nothing changed");
}

#[tokio::test]
async fn premium_reserve_released_on_both_buckets() {
    let t = TestApp::new().await;
    let d = reserved(&t, PREMIUM).await;
    let row = get_row(&t.app, PERIOD_DAILY, d.periods.daily, BUCKET_PREMIUM).await.unwrap();
    assert_eq!(row.reserved_credits_micro, PREMIUM_RESERVE);
    let mut i = input(Some(&d), SettlementMethod::Estimated, None);
    i.billing_outcome = BillingOutcome::Aborted;
    let s = run_settle(&t.app, &i).await.unwrap();
    // 220 * 3 + 50 * 15
    assert_eq!(s.actual_credits_micro, 660 + 750);
    let row = get_row(&t.app, PERIOD_DAILY, d.periods.daily, BUCKET_PREMIUM).await.unwrap();
    assert_eq!((row.reserved_credits_micro, row.spent_credits_micro), (0, 1410));
}

#[test]
fn dedupe_key_is_simple_hex() {
    let t = Uuid::from_u128(0x0000_0000_df51_5b42_9538_d2b5_6b7e_e953);
    let turn = Uuid::from_u128(1);
    let req = Uuid::from_u128(0xabc);
    let key = dedupe_key(t, turn, req);
    assert_eq!(
        key,
        "00000000df515b429538d2b56b7ee953/00000000000000000000000000000001/00000000000000000000000000000abc"
    );
    assert_eq!(key.len(), 32 * 3 + 2);
}

#[tokio::test]
async fn usage_event_fields() {
    let t = TestApp::new().await;
    let d = preflight(&t.app, &req(PREMIUM)).await.unwrap();
    let i = input(Some(&d), SettlementMethod::Actual, Some(usage(100, 50)));
    let s = Settlement {
        method: SettlementMethod::Actual,
        billing_outcome: BillingOutcome::Completed,
        actual_credits_micro: 1050,
        overshoot_capped: false,
    };
    let now = crate::clock::now();
    let e = usage_event(&i, &s, "selected-x", 3, "completed", now);
    assert_eq!(e.tenant_id, TENANT_A);
    assert_eq!(e.user_id, Some(USER_A1));
    assert_eq!(e.chat_id, i.turn.chat_id);
    assert_eq!(e.turn_id, Some(i.turn.id));
    assert_eq!(e.request_id, i.turn.request_id);
    assert_eq!(e.effective_model, PREMIUM);
    assert_eq!(e.selected_model, "selected-x");
    assert_eq!(e.terminal_state, "completed");
    assert_eq!(e.billing_outcome, "completed");
    assert_eq!(e.settlement_method, "actual");
    assert_eq!(e.usage, Some(usage(100, 50)));
    assert_eq!(e.actual_credits_micro, 1050);
    assert_eq!(e.policy_version_applied, 1);
    assert_eq!((e.web_search_calls, e.code_interpreter_calls, e.file_search_calls), (2, 1, 3));
    assert_eq!(e.timestamp, now);
    assert_eq!(e.requester_type, "user");
    assert_eq!(e.dedupe_key, dedupe_key(TENANT_A, i.turn.id, i.turn.request_id));
    assert_eq!(e.system_task_type, None);

    // estimated settlements carry no usage; a turn without reserve reports defaults
    let mut i2 = input(None, SettlementMethod::Estimated, Some(usage(1, 1)));
    i2.user_id = Uuid::nil();
    let s2 = Settlement { method: SettlementMethod::Estimated, billing_outcome: BillingOutcome::Aborted, ..s };
    let e2 = usage_event(&i2, &s2, "", 0, "cancelled", now);
    assert_eq!(e2.usage, None);
    assert_eq!(e2.user_id, None);
    assert_eq!(e2.effective_model, "");
    assert_eq!(e2.policy_version_applied, 0);
    assert_eq!(e2.billing_outcome, "aborted");
    assert_eq!(e2.settlement_method, "estimated");
}

#[tokio::test]
async fn enqueue_usage_reaches_policy_plugin() {
    let t = TestApp::new().await;
    let d = reserved(&t, STANDARD).await;
    let i = input(Some(&d), SettlementMethod::Actual, Some(usage(100, 50)));
    let app = Arc::clone(&t.app);
    let i2 = i.clone();
    let (s, wake) = t
        .app
        .db
        .transaction(move |tx| {
            Box::pin(async move {
                let s = settle(&app, tx, &i2).await?;
                let e = usage_event(&i2, &s, STANDARD, 0, "completed", crate::clock::now());
                let wake = enqueue_usage(&app, tx, &e).await?;
                Ok((s, wake))
            })
        })
        .await
        .unwrap();
    wake.fire();
    assert_eq!(s.actual_credits_micro, 250);
    let policy = Arc::clone(&t.policy);
    t.eventually("usage published", || {
        let p = Arc::clone(&policy);
        async move { p.published.lock().unwrap().len() == 1 }
    })
    .await;
    let published = t.policy.published.lock().unwrap()[0].clone();
    assert_eq!(published.actual_credits_micro, 250);
    assert_eq!(published.dedupe_key, dedupe_key(TENANT_A, i.turn.id, i.turn.request_id));
}
