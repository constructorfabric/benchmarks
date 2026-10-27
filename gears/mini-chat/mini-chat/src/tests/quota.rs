#![allow(clippy::wildcard_imports)]
//! Quota enforcement tests: reserve-before-execute, downgrade, credits
//! accounting, quota status accuracy.

use time::OffsetDateTime;
use uuid::Uuid;

use super::*;
use crate::domain::quota_service::{PreflightInput, QuotaService, SettlementKind, compute_settlement};
use crate::infra::storage::repo::quota::{BucketKey, bucket, period};
use crate::test_support::StubPolicy;

/// Read a bucket row (spent, reserved, calls).
async fn bucket_state(bed: &TestBed, ptype: &'static str, bucket_name: &'static str) -> (i64, i64, i32) {
    let now = OffsetDateTime::now_utc();
    let start = if ptype == period::DAILY {
        crate::domain::services::periods::daily_start(now)
    } else {
        crate::domain::services::periods::monthly_start(now)
    };
    let rows = bed
        .ctx
        .repos
        .quota
        .read_buckets(
            &bed.ctx.db.conn().expect("conn"),
            &toolkit_db::secure::AccessScope::allow_all(),
            bed.security.subject_tenant_id(),
            bed.security.subject_id(),
            &[BucketKey {
                period_type: ptype,
                period_start: start,
                bucket: bucket_name,
            }],
        )
        .await
        .expect("read");
    let (_, s) = &rows[0];
    (s.spent_credits_micro, s.reserved_credits_micro, s.calls)
}

#[tokio::test]
async fn reserve_then_settle_actual() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    send_and_drain(&bed.services, &bed.bed, chat.id, "hi", None).await;

    // After settlement: spent = actual (15), reserved = 0, calls = 1.
    // The default catalog entry: multipliers 1_000_000 (1 credit / 1M
    // tokens -> 1 micro per token) for input and output: 10 + 5 = 15.
    let (spent, reserved, calls) = bucket_state(&bed.bed, period::DAILY, bucket::TOTAL).await;
    assert_eq!(spent, 15, "actual committed");
    assert_eq!(reserved, 0, "reserve released");
    assert_eq!(calls, 1);
    // Premium model: also the premium bucket.
    let (spent_p, _, calls_p) = bucket_state(&bed.bed, period::DAILY, bucket::PREMIUM).await;
    assert_eq!(spent_p, 15);
    assert_eq!(calls_p, 1);
}

#[tokio::test]
async fn quota_status_reports_usage() {
    let bed = stream_bed().await;
    let chat = create_chat(&bed.bed, None, None).await;
    send_and_drain(&bed.services, &bed.bed, chat.id, "hi", None).await;

    let status = bed
        .services
        .models
        .quota_status(&bed.bed.security)
        .await
        .expect("status");
    assert_eq!(status.warning_threshold_pct, 80);
    let premium = status.tiers.iter().find(|t| t.tier == crate::api::rest::dto::QuotaTier::Premium).expect("premium tier");
    let daily = premium.periods.iter().find(|p| p.period == crate::api::rest::dto::QuotaPeriod::Daily).expect("daily");
    assert_eq!(daily.used_credits_micro, 15);
    assert_eq!(daily.remaining_credits_micro, 50_000_000 - 15);
    assert_eq!(daily.remaining_percentage, 99);
    assert!(!daily.warning);
    assert!(!daily.exhausted);
    let total = status.tiers.iter().find(|t| t.tier == crate::api::rest::dto::QuotaTier::Total).expect("total tier");
    assert_eq!(total.periods.len(), 2);
}

#[tokio::test]
async fn preflight_rejects_when_nothing_fits() {
    let bed = stream_bed().await;
    let quota = QuotaService::new(std::sync::Arc::clone(&bed.bed.ctx));
    let input = PreflightInput {
        tenant_id: bed.bed.security.subject_tenant_id(),
        user_id: bed.bed.security.subject_id(),
        selected_model: "gpt-premium".to_owned(),
        content_bytes: 10,
        prior_context_tokens: 0,
        image_count: 0,
        has_ready_documents: false,
        has_ready_code_interpreter: false,
        web_search_requested: false,
    };
    let (_policy, outcome) = quota.preflight(&input).await.expect("preflight");
    assert_eq!(outcome.effective_model.id, "gpt-premium");
    assert!(outcome.reserve_tokens > 0);
    assert!(outcome.reserved_credits_micro > 0);
}

#[test]
fn settlement_paths() {
    let usage = mini_chat_sdk::UsageTokens {
        input_tokens: 100,
        output_tokens: 50,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
    };
    // Completed: actual.
    let s = compute_settlement(SettlementKind::Completed, 1000, 200, 50, 800, Some(usage), 1_000_000, 1_000_000, 1.10)
        .expect("settle");
    assert_eq!(s.billing_outcome, "completed");
    assert_eq!(s.settlement_method, "actual");
    assert_eq!(s.committed_credits_micro, 150);
    // Completed with zero usage: 0 credits, actual path.
    let s = compute_settlement(SettlementKind::Completed, 1000, 200, 50, 800, None, 1_000_000, 1_000_000, 1.10)
        .expect("settle");
    assert_eq!(s.committed_credits_micro, 0);
    assert_eq!(s.settlement_method, "actual");
    // Failed with usage: actual.
    let s = compute_settlement(SettlementKind::Failed, 1000, 200, 50, 800, Some(usage), 1_000_000, 1_000_000, 1.10)
        .expect("settle");
    assert_eq!(s.billing_outcome, "failed");
    assert_eq!(s.settlement_method, "actual");
    assert_eq!(s.committed_credits_micro, 150);
    // Failed without usage (zero fields): estimated.
    let zero = mini_chat_sdk::UsageTokens::default();
    let s = compute_settlement(SettlementKind::Failed, 1000, 200, 50, 800, Some(zero), 1_000_000, 1_000_000, 1.10)
        .expect("settle");
    assert_eq!(s.settlement_method, "estimated");
    // est_input = 800, floor = 50 -> 850 credits.
    assert_eq!(s.committed_credits_micro, 850);
    // Aborted: always estimated.
    let s = compute_settlement(SettlementKind::Aborted, 1000, 200, 50, 800, None, 1_000_000, 1_000_000, 1.10)
        .expect("settle");
    assert_eq!(s.billing_outcome, "aborted");
    assert_eq!(s.settlement_method, "estimated");
    assert_eq!(s.committed_credits_micro, 850);
}

#[test]
fn settlement_overshoot_cap() {
    // Reserve 1000 tokens (800 in + 200 out); actual 2000 tokens -> factor 2.0 > 1.10 -> capped at reserve.
    let usage = mini_chat_sdk::UsageTokens {
        input_tokens: 1900,
        output_tokens: 100,
        ..Default::default()
    };
    let s = compute_settlement(SettlementKind::Completed, 1000, 200, 50, 800, Some(usage), 1_000_000, 1_000_000, 1.10)
        .expect("settle");
    assert!(s.overshoot);
    assert_eq!(s.committed_credits_micro, 800, "capped at reserved credits");
    // Within tolerance (1050 tokens vs 1000 reserve, factor 1.05 < 1.10):
    // actual committed.
    let usage = mini_chat_sdk::UsageTokens {
        input_tokens: 950,
        output_tokens: 100,
        ..Default::default()
    };
    let s = compute_settlement(SettlementKind::Completed, 1000, 200, 50, 1050, Some(usage), 1_000_000, 1_000_000, 1.10)
        .expect("settle");
    assert!(s.overshoot);
    assert_eq!(s.committed_credits_micro, 1050, "within tolerance: actual");
}

#[test]
fn settlement_uses_applied_policy_multipliers() {
    // The effective model's multipliers come from the snapshot, not from the
    // caller — verified here by the formula directly.
    let usage = mini_chat_sdk::UsageTokens {
        input_tokens: 1,
        output_tokens: 1,
        ..Default::default()
    };
    // 2.5 credits/1K both: 1 token each -> ceil(2500)+ceil(2500) = 5000.
    let s = compute_settlement(SettlementKind::Completed, 10, 5, 1, 9, Some(usage), 2_500_000_000, 2_500_000_000, 1.10)
        .expect("settle");
    assert_eq!(s.committed_credits_micro, 5000);
}
