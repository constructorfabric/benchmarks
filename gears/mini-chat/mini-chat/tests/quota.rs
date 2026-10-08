//! Quota engine over `SQLite` (DESIGN §5.4): preflight cascade on stored bucket
//! rows, the reserve write with its re-check, and settlement.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use mini_chat::config::MiniChatConfig;
use mini_chat::domain::error::DomainError;
use mini_chat::domain::model::{
    Bucket, DowngradeReason, PeriodType, QuotaDecision, QuotaScope, SettlementMethod,
};
use mini_chat::domain::services::quota::candidate_reserve;
use mini_chat::domain::services::{PreflightDecision, PreflightInput, QuotaService, Settlement};
use mini_chat::infra::db::entities::quota_usage;
use mini_chat::infra::db::repos::QuotaRepo;
use mini_chat::infra::db::test_db;
use mini_chat::testing::catalog::{premium_model, standard_model};
use mini_chat::testing::{TestUser, seed};
use mini_chat_sdk::{
    EstimationBudgets, KillSwitches, ModelCatalogEntry, ModelTier, ModelToolSupport,
    PolicySnapshot, TierLimits, UserLimits,
};
use toolkit_db::{DBProvider, Db};

const U: TestUser = TestUser::A1;
const P_RESERVE: i64 = 3_750_000;
const S_RESERVE: i64 = 1_500_000;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 15, 12, 0, 0).unwrap()
}

fn day() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()
}

fn month() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 3, 1).unwrap()
}

/// D§5.10 models: an empty message estimates 1000 input tokens, 500 output max.
fn with_mult(mut m: ModelCatalogEntry, mult: i64) -> ModelCatalogEntry {
    m.input_tokens_credit_multiplier_micro = mult;
    m.output_tokens_credit_multiplier_micro = mult;
    m.max_output_tokens = 500;
    m.estimation_budgets = EstimationBudgets {
        bytes_per_token_conservative: 1,
        fixed_overhead_tokens: 1000,
        safety_margin_pct: 0,
        image_token_budget: 0,
        tool_surcharge_tokens: 0,
        web_search_surcharge_tokens: 0,
        code_interpreter_surcharge_tokens: 0,
        minimal_generation_floor: 50,
    };
    m
}

fn catalog() -> Vec<ModelCatalogEntry> {
    let mut p = with_mult(premium_model("P"), 2_500_000_000);
    p.general_config.tool_support = ModelToolSupport {
        web_search: true,
        ..ModelToolSupport::default()
    };
    vec![p, with_mult(standard_model("S"), 1_000_000_000)]
}

fn limits() -> UserLimits {
    UserLimits {
        user_id: U.user_id,
        policy_version: 3,
        standard: TierLimits {
            limit_daily_credits_micro: 60_000_000,
            limit_monthly_credits_micro: 600_000_000,
        },
        premium: TierLimits {
            limit_daily_credits_micro: 22_000_000,
            limit_monthly_credits_micro: 300_000_000,
        },
    }
}

fn input(selected: &str) -> PreflightInput {
    PreflightInput {
        tenant_id: U.tenant_id,
        user_id: U.user_id,
        selected_model_id: selected.to_owned(),
        snapshot: Arc::new(PolicySnapshot {
            policy_version: 3,
            model_catalog: catalog(),
            kill_switches: KillSwitches {
                disable_premium_tier: false,
                force_standard_tier: false,
                disable_web_search: false,
                disable_file_search: false,
                disable_images: false,
                disable_code_interpreter: false,
            },
        }),
        user_limits: limits(),
        content: String::new(),
        image_count: 0,
        prior_context_tokens: 0,
        chat_has_ready_docs: false,
        chat_has_ready_xlsx: false,
        web_search_requested: false,
        now: now(),
    }
}

struct Env {
    db: Db,
    provider: DBProvider<DomainError>,
    quota: Arc<QuotaService>,
}

async fn env() -> Env {
    let db = test_db().await;
    let provider = DBProvider::new(db.clone());
    let quota = Arc::new(QuotaService::new(
        Arc::new(MiniChatConfig::default()),
        Arc::new(DBProvider::new(db.clone())),
    ));
    Env {
        db,
        provider,
        quota,
    }
}

impl Env {
    async fn seed(&self, bucket: Bucket, period: PeriodType, spent: i64, reserved: i64) {
        let start = match period {
            PeriodType::Daily => day(),
            PeriodType::Monthly => month(),
        };
        seed::insert_quota_row(
            &self.db,
            U.tenant_id,
            U.user_id,
            bucket.as_str(),
            period,
            start,
            spent,
            reserved,
        )
        .await;
    }

    async fn rows(&self) -> Vec<quota_usage::Model> {
        QuotaRepo::rows_for_periods(
            &self.db.conn().unwrap(),
            U.tenant_id,
            U.user_id,
            day(),
            month(),
        )
        .await
        .unwrap()
    }

    async fn row(&self, bucket: Bucket, period: PeriodType) -> Option<quota_usage::Model> {
        self.rows()
            .await
            .into_iter()
            .find(|r| r.bucket == bucket.as_str() && r.period_type == period.as_str())
    }

    async fn reserve(&self, d: &PreflightDecision) -> Result<(), DomainError> {
        let quota = Arc::clone(&self.quota);
        let d = d.clone();
        self.provider
            .transaction(move |tx| {
                Box::pin(async move {
                    quota
                        .reserve_in_tx(tx, U.tenant_id, U.user_id, &d, &limits())
                        .await
                })
            })
            .await
    }

    async fn settle(&self, s: Settlement) {
        let quota = Arc::clone(&self.quota);
        self.provider
            .transaction(move |tx| Box::pin(async move { quota.settle_in_tx(tx, &s).await }))
            .await
            .unwrap();
    }
}

fn reserved(row: Option<quota_usage::Model>) -> i64 {
    row.expect("bucket row").reserved_credits_micro
}

const BUCKETS: [(Bucket, PeriodType); 4] = [
    (Bucket::Total, PeriodType::Daily),
    (Bucket::Total, PeriodType::Monthly),
    (Bucket::TierPremium, PeriodType::Daily),
    (Bucket::TierPremium, PeriodType::Monthly),
];

#[tokio::test]
async fn cascade_allows_selected_premium_when_available() {
    let e = env().await;
    let inp = input("P");
    let d = e.quota.preflight(&inp).await.unwrap();
    assert_eq!(d.effective.id, "P");
    assert_eq!(d.decision, QuotaDecision::Allow);
    assert_eq!(d.downgrade_reason, None);
    let (est, max_out, credits) = candidate_reserve(
        &d.effective,
        &inp,
        &MiniChatConfig::default(),
        &inp.snapshot.kill_switches,
    );
    assert_eq!((est, max_out, credits), (1000, 500, P_RESERVE));
    assert_eq!(d.reserved_credits_micro, credits);
    assert_eq!(d.reserve_tokens, est + max_out);

    e.reserve(&d).await.unwrap();
    for (bucket, period) in BUCKETS {
        assert_eq!(
            reserved(e.row(bucket, period).await),
            credits,
            "{bucket} {period}"
        );
    }
}

#[tokio::test]
async fn cascade_premium_daily_exhausted_downgrades_premium_quota_exhausted() {
    let e = env().await;
    e.seed(Bucket::TierPremium, PeriodType::Daily, 20_000_000, 0)
        .await;
    e.seed(Bucket::TierPremium, PeriodType::Monthly, 200_000_000, 0)
        .await;
    e.seed(Bucket::Total, PeriodType::Daily, 25_000_000, 0)
        .await;
    e.seed(Bucket::Total, PeriodType::Monthly, 240_000_000, 0)
        .await;

    let d = e.quota.preflight(&input("P")).await.unwrap();
    assert_eq!(d.effective.id, "S");
    assert_eq!(d.effective_tier, ModelTier::Standard);
    assert_eq!(d.decision, QuotaDecision::Downgrade);
    assert_eq!(
        d.downgrade_reason,
        Some(DowngradeReason::PremiumQuotaExhausted)
    );
    assert_eq!(d.reserved_credits_micro, S_RESERVE);

    e.reserve(&d).await.unwrap();
    assert_eq!(
        reserved(e.row(Bucket::Total, PeriodType::Daily).await),
        S_RESERVE
    );
    assert_eq!(
        reserved(e.row(Bucket::Total, PeriodType::Monthly).await),
        S_RESERVE
    );
    assert_eq!(
        reserved(e.row(Bucket::TierPremium, PeriodType::Daily).await),
        0
    );
    assert_eq!(
        reserved(e.row(Bucket::TierPremium, PeriodType::Monthly).await),
        0
    );
}

#[tokio::test]
async fn preflight_all_exhausted_is_quota_exceeded_tokens() {
    let e = env().await;
    e.seed(Bucket::Total, PeriodType::Monthly, 599_000_000, 0)
        .await;
    let err = e.quota.preflight(&input("P")).await.unwrap_err();
    assert!(
        matches!(
            err,
            DomainError::QuotaExceeded {
                scope: QuotaScope::Tokens
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn reserve_upserts_total_and_premium_rows_daily_and_monthly() {
    let e = env().await;
    e.seed(Bucket::Total, PeriodType::Daily, 1_000_000, 100)
        .await;

    let d = e.quota.preflight(&input("P")).await.unwrap();
    e.reserve(&d).await.unwrap();
    let rows = e.rows().await;
    assert_eq!(rows.len(), 4);
    let total_daily = e.row(Bucket::Total, PeriodType::Daily).await.unwrap();
    assert_eq!(total_daily.reserved_credits_micro, 100 + P_RESERVE);
    assert_eq!(total_daily.spent_credits_micro, 1_000_000);
    for (bucket, period) in &BUCKETS[1..] {
        let r = e.row(*bucket, *period).await.unwrap();
        assert_eq!(r.reserved_credits_micro, P_RESERVE);
        assert_eq!(r.spent_credits_micro, 0);
        assert_eq!(r.tenant_id, U.tenant_id);
        assert!(r.updated_at.is_some());
    }

    // A second reserve increments the same rows.
    e.reserve(&d).await.unwrap();
    assert_eq!(e.rows().await.len(), 4);
    assert_eq!(
        reserved(e.row(Bucket::TierPremium, PeriodType::Monthly).await),
        2 * P_RESERVE
    );
    assert_eq!(
        reserved(e.row(Bucket::Total, PeriodType::Daily).await),
        100 + 2 * P_RESERVE
    );
}

#[tokio::test]
async fn reserve_recheck_rejects_when_concurrent_reserve_filled_limit() {
    let e = env().await;
    let d = e.quota.preflight(&input("P")).await.unwrap();
    assert_eq!(d.effective.id, "P");
    // Another turn booked its reserve after this preflight.
    e.seed(Bucket::TierPremium, PeriodType::Daily, 0, 20_000_000)
        .await;

    let err = e.reserve(&d).await.unwrap_err();
    assert!(
        matches!(
            err,
            DomainError::QuotaExceeded {
                scope: QuotaScope::Tokens
            }
        ),
        "{err:?}"
    );
    // Rolled back: only the concurrent reserve remains.
    let rows = e.rows().await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].reserved_credits_micro, 20_000_000);
}

#[tokio::test]
async fn standard_turn_touches_only_total() {
    let e = env().await;
    let d = e.quota.preflight(&input("S")).await.unwrap();
    assert_eq!(d.decision, QuotaDecision::Allow);
    e.reserve(&d).await.unwrap();
    let rows = e.rows().await;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|r| r.bucket == "total"));
    assert!(rows.iter().all(|r| r.reserved_credits_micro == S_RESERVE));
}

fn settlement(d: &PreflightDecision, method: SettlementMethod) -> Settlement {
    Settlement {
        tenant_id: U.tenant_id,
        user_id: U.user_id,
        daily_start: d.daily_start,
        monthly_start: d.monthly_start,
        premium: d.effective_tier == ModelTier::Premium,
        turn_reserved_credits_micro: d.reserved_credits_micro,
        committed_credits_micro: 1_200_000,
        actual_input_tokens: 900,
        actual_output_tokens: 300,
        web_search_calls: 2,
        code_interpreter_calls: 1,
        method,
    }
}

/// `(reserved, spent, calls, input, output, web, ci)` of a row.
fn counters(r: &quota_usage::Model) -> (i64, i64, i32, i64, i64, i32, i32) {
    (
        r.reserved_credits_micro,
        r.spent_credits_micro,
        r.calls,
        r.input_tokens,
        r.output_tokens,
        r.web_search_calls,
        r.code_interpreter_calls,
    )
}

#[tokio::test]
async fn settle_actual_updates_counters() {
    let e = env().await;
    let d = e.quota.preflight(&input("P")).await.unwrap();
    e.reserve(&d).await.unwrap();
    e.settle(settlement(&d, SettlementMethod::Actual)).await;

    for period in [PeriodType::Daily, PeriodType::Monthly] {
        let total = e.row(Bucket::Total, period).await.unwrap();
        assert_eq!(counters(&total), (0, 1_200_000, 1, 900, 300, 2, 1));
        let premium = e.row(Bucket::TierPremium, period).await.unwrap();
        assert_eq!(counters(&premium), (0, 1_200_000, 1, 0, 0, 0, 0));
    }
}

#[tokio::test]
async fn settle_estimated_adds_tool_calls_not_tokens() {
    let e = env().await;
    let d = e.quota.preflight(&input("S")).await.unwrap();
    e.reserve(&d).await.unwrap();
    e.settle(settlement(&d, SettlementMethod::Estimated)).await;

    assert_eq!(e.rows().await.len(), 2);
    for period in [PeriodType::Daily, PeriodType::Monthly] {
        let total = e.row(Bucket::Total, period).await.unwrap();
        assert_eq!(counters(&total), (0, 1_200_000, 1, 0, 0, 2, 1));
    }
}

#[tokio::test]
async fn settle_released_counts_call_only() {
    let e = env().await;
    let d = e.quota.preflight(&input("P")).await.unwrap();
    e.reserve(&d).await.unwrap();
    let mut s = settlement(&d, SettlementMethod::Released);
    s.committed_credits_micro = 0;
    e.settle(s).await;

    for period in [PeriodType::Daily, PeriodType::Monthly] {
        let total = e.row(Bucket::Total, period).await.unwrap();
        assert_eq!(counters(&total), (0, 0, 1, 0, 0, 0, 0));
        let premium = e.row(Bucket::TierPremium, period).await.unwrap();
        assert_eq!(counters(&premium), (0, 0, 1, 0, 0, 0, 0));
    }
}

#[tokio::test]
async fn web_search_daily_quota_reads_daily_total_row() {
    let e = env().await;
    let d = e.quota.preflight(&input("S")).await.unwrap();
    e.reserve(&d).await.unwrap();
    let mut s = settlement(&d, SettlementMethod::Estimated);
    s.web_search_calls = 75;
    e.settle(s).await;

    let mut inp = input("P");
    inp.web_search_requested = true;
    let err = e.quota.preflight(&inp).await.unwrap_err();
    assert!(
        matches!(
            err,
            DomainError::QuotaExceeded {
                scope: QuotaScope::WebSearch
            }
        ),
        "{err:?}"
    );
    // The same request without web search passes.
    let d = e.quota.preflight(&input("P")).await.unwrap();
    assert_eq!(d.effective.id, "P");
}
