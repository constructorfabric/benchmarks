#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use mini_chat_sdk::{
    EstimationBudgets, ModelCatalogEntry, ModelPreference, ModelTier, PolicySnapshot, TierLimits,
    UsageTokens, UserLimits,
};
use sea_orm::ActiveValue::Set;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use time::macros::{date, datetime};
use time::{Date, OffsetDateTime};
use toolkit_db::DBProvider;
use toolkit_db::secure::{AccessScope, SecureEntityExt, secure_insert};
use uuid::Uuid;

use super::{
    PeriodStarts, PreflightDecision, PreflightInput, QuotaDecisionKind, QuotaPeriodKind,
    QuotaService, QuotaTierKind, QuotaWarningView, ReserveRequest, SettlementInput,
    SettlementResult,
};
use crate::config::MiniChatConfig;
use crate::domain::billing::SettlementMethod;
use crate::domain::enums::{PeriodType, QuotaBucket};
use crate::domain::error::{DomainError, QuotaScope};
use crate::domain::estimation::{ToolContext, ToolGates};
use crate::infra::db::entities::quota_usage;
use crate::test_support::{FakeAuthz, FakePolicy, catalog_entry, ctx_for, snapshot, test_provider};

// ── Fixtures (numbers from DESIGN section 5.10) ──────────────────────────────

const P_MULT: i64 = 2_500_000_000;
const S_MULT: i64 = 1_000_000_000;

fn standard_limits() -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: 60_000_000,
        limit_monthly_credits_micro: 600_000_000,
    }
}

fn premium_limits() -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: 22_000_000,
        limit_monthly_credits_micro: 300_000_000,
    }
}

/// Every message estimates to exactly 1000 input tokens when empty
/// (`fixed_overhead_tokens = 1000`, no margin); `max_output_tokens = 500`.
fn model(id: &str, tier: ModelTier, mult: i64, is_default: bool) -> ModelCatalogEntry {
    let mut m = catalog_entry(id, true);
    m.tier = tier;
    m.input_tokens_credit_multiplier_micro = mult;
    m.output_tokens_credit_multiplier_micro = mult;
    m.max_output_tokens = 500;
    m.estimation_budgets = EstimationBudgets {
        bytes_per_token_conservative: 4,
        fixed_overhead_tokens: 1000,
        safety_margin_pct: 0,
        image_token_budget: 1000,
        tool_surcharge_tokens: 500,
        web_search_surcharge_tokens: 500,
        code_interpreter_surcharge_tokens: 1000,
        minimal_generation_floor: 50,
    };
    m.preference = Some(ModelPreference {
        is_default,
        sort_order: 0,
    });
    let tools = &mut m.general_config.tool_support;
    tools.web_search = false;
    tools.file_search = false;
    tools.code_interpreter = false;
    m
}

/// `s-alt` precedes the default `s` to prove the default is preferred.
fn catalog() -> Vec<ModelCatalogEntry> {
    let mut p = model("p", ModelTier::Premium, P_MULT, true);
    p.general_config.tool_support.web_search = true;
    p.general_config.tool_support.file_search = true;
    p.general_config.tool_support.code_interpreter = true;
    let mut p_off = model("p-off", ModelTier::Premium, P_MULT, false);
    p_off.enabled = false;
    let mut s_off = model("s-off", ModelTier::Standard, S_MULT, false);
    s_off.enabled = false;
    vec![
        model("s-alt", ModelTier::Standard, S_MULT, false),
        p_off,
        p,
        model("s", ModelTier::Standard, S_MULT, true),
        s_off,
        model("s-big", ModelTier::Standard, 10_000_000_000, false),
    ]
}

struct Fx {
    db: Arc<DBProvider<DomainError>>,
    svc: QuotaService,
    tenant: Uuid,
    user: Uuid,
}

async fn fx_with(snap: PolicySnapshot, cfg: MiniChatConfig) -> Fx {
    let db = test_provider().await;
    let policy = Arc::new(FakePolicy::with_limits(
        snap,
        standard_limits(),
        premium_limits(),
    ));
    let svc = QuotaService::new(
        Arc::clone(&db),
        Arc::new(FakeAuthz::default()),
        policy,
        Arc::new(cfg),
    );
    Fx {
        db,
        svc,
        tenant: Uuid::new_v4(),
        user: Uuid::new_v4(),
    }
}

async fn fx() -> Fx {
    fx_with(snapshot(catalog()), MiniChatConfig::default()).await
}

fn now() -> OffsetDateTime {
    datetime!(2026-10-04 12:00:00 UTC)
}

fn periods() -> PeriodStarts {
    PeriodStarts {
        daily: date!(2026 - 10 - 04),
        monthly: date!(2026 - 10 - 01),
    }
}

impl Fx {
    fn input(&self, selected: &str) -> PreflightInput {
        PreflightInput {
            tenant_id: self.tenant,
            user_id: self.user,
            selected_model: selected.to_owned(),
            message_bytes: 0,
            prior_context_tokens: 0,
            num_images: 0,
            tool_ctx: ToolContext::default(),
            now: now(),
        }
    }

    async fn preflight(&self, selected: &str) -> Result<PreflightDecision, DomainError> {
        self.svc.preflight(&self.input(selected)).await
    }

    fn limits(&self) -> UserLimits {
        UserLimits {
            user_id: self.user,
            policy_version: 1,
            standard: standard_limits(),
            premium: premium_limits(),
        }
    }

    async fn seed(&self, period: PeriodType, bucket: QuotaBucket, spent: i64, reserved: i64) {
        self.seed_row(self.user, period, bucket, spent, reserved, 0, 0)
            .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn seed_row(
        &self,
        user: Uuid,
        period: PeriodType,
        bucket: QuotaBucket,
        spent: i64,
        reserved: i64,
        web_search_calls: i32,
        code_interpreter_calls: i32,
    ) {
        let start = match period {
            PeriodType::Daily => periods().daily,
            PeriodType::Monthly => periods().monthly,
        };
        self.seed_at(
            user,
            period,
            start,
            bucket,
            spent,
            reserved,
            web_search_calls,
            code_interpreter_calls,
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn seed_at(
        &self,
        user: Uuid,
        period: PeriodType,
        start: Date,
        bucket: QuotaBucket,
        spent: i64,
        reserved: i64,
        web_search_calls: i32,
        code_interpreter_calls: i32,
    ) {
        let conn = self.db.conn().unwrap();
        secure_insert::<quota_usage::Entity>(
            quota_usage::ActiveModel {
                id: Set(Uuid::new_v4()),
                tenant_id: Set(self.tenant),
                user_id: Set(user),
                period_type: Set(period.as_str().to_owned()),
                period_start: Set(start),
                bucket: Set(bucket.as_str().to_owned()),
                spent_credits_micro: Set(spent),
                reserved_credits_micro: Set(reserved),
                calls: Set(0),
                input_tokens: Set(0),
                output_tokens: Set(0),
                file_search_calls: Set(0),
                web_search_calls: Set(web_search_calls),
                code_interpreter_calls: Set(code_interpreter_calls),
                rag_retrieval_calls: Set(0),
                image_inputs: Set(0),
                image_upload_bytes: Set(0),
                updated_at: Set(now()),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .unwrap();
    }

    async fn row(&self, period: PeriodType, bucket: QuotaBucket) -> Option<quota_usage::Model> {
        let start = match period {
            PeriodType::Daily => periods().daily,
            PeriodType::Monthly => periods().monthly,
        };
        let conn = self.db.conn().unwrap();
        quota_usage::Entity::find()
            .filter(quota_usage::Column::TenantId.eq(self.tenant))
            .filter(quota_usage::Column::UserId.eq(self.user))
            .filter(quota_usage::Column::PeriodType.eq(period.as_str()))
            .filter(quota_usage::Column::PeriodStart.eq(start))
            .filter(quota_usage::Column::Bucket.eq(bucket.as_str()))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .one(&conn)
            .await
            .unwrap()
    }

    async fn reserve(&self, premium: bool, credits: i64) -> Result<(), DomainError> {
        let req = ReserveRequest {
            tenant_id: self.tenant,
            user_id: self.user,
            premium,
            reserved_credits_micro: credits,
            periods: periods(),
            limits: self.limits(),
        };
        let svc = self.svc.clone();
        self.db
            .transaction(move |tx| Box::pin(async move { svc.reserve_in_tx(tx, &req).await }))
            .await
    }

    fn settlement(&self, method: SettlementMethod) -> SettlementInput {
        SettlementInput {
            tenant_id: self.tenant,
            user_id: self.user,
            premium: false,
            periods: periods(),
            reserve_tokens: 1500,
            reserved_credits_micro: 1_500_000,
            max_output_tokens_applied: 500,
            minimal_generation_floor_applied: 50,
            in_mult: S_MULT,
            out_mult: S_MULT,
            method,
            usage: None,
            web_search_calls: 0,
            code_interpreter_calls: 0,
        }
    }

    async fn settle(&self, s: SettlementInput) -> Result<SettlementResult, DomainError> {
        let svc = self.svc.clone();
        self.db
            .transaction(move |tx| Box::pin(async move { svc.settle_in_tx(tx, &s).await }))
            .await
    }
}

fn usage(input: i64, output: i64) -> UsageTokens {
    UsageTokens {
        input_tokens: input,
        output_tokens: output,
        ..UsageTokens::default()
    }
}

fn tokens_exceeded() -> DomainError {
    DomainError::QuotaExceeded {
        scope: QuotaScope::Tokens,
    }
}

// ── Periods ──────────────────────────────────────────────────────────────────

#[test]
fn period_starts_utc_boundaries() {
    let before = PeriodStarts::at(datetime!(2026-02-28 23:59:59 UTC));
    assert_eq!(before.daily, date!(2026 - 02 - 28));
    assert_eq!(before.monthly, date!(2026 - 02 - 01));

    let after = PeriodStarts::at(datetime!(2026-03-01 00:00:00 UTC));
    assert_eq!(after.daily, date!(2026 - 03 - 01));
    assert_eq!(after.monthly, date!(2026 - 03 - 01));

    // Non-UTC input is converted first: 01:00 +02:00 is 23:00 UTC the day before.
    let offset = PeriodStarts::at(datetime!(2026-03-01 01:00:00 +02:00));
    assert_eq!(offset.daily, date!(2026 - 02 - 28));
    assert_eq!(offset.monthly, date!(2026 - 02 - 01));

    // The watchdog derives the same values from `started_at`.
    assert_eq!(
        PeriodStarts::from_started_at(datetime!(2026-02-28 23:59:59 UTC)),
        before
    );
}

// ── Preflight cascade ────────────────────────────────────────────────────────

#[tokio::test]
async fn allow_when_premium_available() {
    let f = fx().await;
    let d = f.preflight("p").await.unwrap();

    assert_eq!(d.decision, QuotaDecisionKind::Allow);
    assert_eq!(d.downgrade_reason, None);
    assert_eq!(d.effective_model.id, "p");
    assert_eq!(d.selected_model, "p");
    assert_eq!(d.max_output_tokens_applied, 500);
    assert_eq!(d.estimated_input_tokens, 1000);
    assert_eq!(d.reserve_tokens, 1500);
    // ceil(1000 * 2.5e9 / 1e6) + ceil(500 * 2.5e9 / 1e6)
    assert_eq!(d.reserved_credits_micro, 3_750_000);
    assert_eq!(d.minimal_generation_floor_applied, 50);
    assert_eq!(d.periods, periods());
    assert_eq!(d.limits.premium, premium_limits());
    assert_eq!(d.snapshot.policy_version, 1);
    assert_eq!(d.tools, ToolGates::default());
    assert!(d.premium());
}

#[tokio::test]
async fn preflight_counts_images_prior_context_and_tool_surcharges() {
    let f = fx().await;
    let mut input = f.input("p");
    input.message_bytes = 400; // ceil(400 / 4) + 1000 = 1100
    input.prior_context_tokens = 200;
    input.num_images = 2; // 2 * 1000
    input.tool_ctx = ToolContext {
        chat_has_ready_documents: true, // + 500 (file_search)
        chat_has_ready_ci_files: false,
        web_search_requested: true, // + 500 (web_search)
    };
    let d = f.svc.preflight(&input).await.unwrap();

    assert_eq!(d.estimated_input_tokens, 4300);
    assert_eq!(d.reserve_tokens, 4800);
    // ceil(4300 * 2.5e9 / 1e6) + ceil(500 * 2.5e9 / 1e6)
    assert_eq!(d.reserved_credits_micro, 10_750_000 + 1_250_000);
    assert_eq!(
        d.tools,
        ToolGates {
            file_search: true,
            web_search: true,
            code_interpreter: false
        }
    );
}

#[tokio::test]
async fn minimal_generation_floor_is_capped_by_max_output() {
    let mut cfg = MiniChatConfig::default();
    cfg.streaming.max_output_tokens = 40;
    let f = fx_with(snapshot(catalog()), cfg).await;
    let d = f.preflight("p").await.unwrap();

    assert_eq!(d.max_output_tokens_applied, 40);
    assert_eq!(d.minimal_generation_floor_applied, 40);
    assert_eq!(d.reserve_tokens, 1040);
}

#[tokio::test]
async fn downgrade_when_premium_daily_exhausted() {
    let f = fx().await;
    f.seed(PeriodType::Daily, QuotaBucket::TierPremium, 22_000_000, 0)
        .await;
    let d = f.preflight("p").await.unwrap();

    assert_eq!(d.decision, QuotaDecisionKind::Downgrade);
    assert_eq!(d.downgrade_reason, Some("premium_quota_exhausted"));
    assert_eq!(d.effective_model.id, "s");
    assert_eq!(d.selected_model, "p");
    assert_eq!(d.reserved_credits_micro, 1_500_000);
    assert!(!d.premium());
}

#[tokio::test]
async fn downgrade_when_monthly_exhausted() {
    let f = fx().await;
    f.seed(
        PeriodType::Monthly,
        QuotaBucket::TierPremium,
        300_000_000,
        0,
    )
    .await;
    let d = f.preflight("p").await.unwrap();

    assert_eq!(d.decision, QuotaDecisionKind::Downgrade);
    assert_eq!(d.downgrade_reason, Some("premium_quota_exhausted"));
    assert_eq!(d.effective_model.id, "s");
}

#[tokio::test]
async fn design_example_premium_reserve_does_not_fit_premium_daily() {
    // DESIGN 5.10: premium daily 20M + 3.75M > 22M; standard 25M + 1.5M <= 60M.
    let f = fx().await;
    f.seed(PeriodType::Daily, QuotaBucket::TierPremium, 20_000_000, 0)
        .await;
    f.seed(
        PeriodType::Monthly,
        QuotaBucket::TierPremium,
        200_000_000,
        0,
    )
    .await;
    f.seed(PeriodType::Daily, QuotaBucket::Total, 25_000_000, 0)
        .await;
    f.seed(PeriodType::Monthly, QuotaBucket::Total, 240_000_000, 0)
        .await;
    let d = f.preflight("p").await.unwrap();

    assert_eq!(d.effective_model.id, "s");
    assert_eq!(d.reserved_credits_micro, 1_500_000);
    assert_eq!(d.downgrade_reason, Some("premium_quota_exhausted"));
}

#[tokio::test]
async fn in_flight_reserves_count_against_the_limit() {
    let f = fx().await;
    // spent 0 + reserved 20M + 3.75M > 22M
    f.seed(PeriodType::Daily, QuotaBucket::TierPremium, 0, 20_000_000)
        .await;
    let d = f.preflight("p").await.unwrap();
    assert_eq!(d.effective_model.id, "s");
}

#[tokio::test]
async fn reject_when_all_tiers_exhausted_tokens_scope() {
    let f = fx().await;
    f.seed(PeriodType::Daily, QuotaBucket::Total, 60_000_000, 0)
        .await;
    assert_eq!(f.preflight("p").await.unwrap_err(), tokens_exceeded());
    assert_eq!(f.preflight("s").await.unwrap_err(), tokens_exceeded());
}

#[tokio::test]
async fn standard_never_upgrades() {
    let f = fx().await;
    // 10M left in total: the premium reserve (3.75M) would fit, s-big's (15M) does not.
    f.seed(PeriodType::Daily, QuotaBucket::Total, 50_000_000, 0)
        .await;
    assert_eq!(f.preflight("s-big").await.unwrap_err(), tokens_exceeded());

    let d = f.preflight("s").await.unwrap();
    assert_eq!(d.effective_model.id, "s");
    assert_eq!(d.decision, QuotaDecisionKind::Allow);
}

#[tokio::test]
async fn standard_selection_uses_selected_model_not_default() {
    let f = fx().await;
    let d = f.preflight("s-alt").await.unwrap();
    assert_eq!(d.effective_model.id, "s-alt");
    assert_eq!(d.decision, QuotaDecisionKind::Allow);
    assert_eq!(d.downgrade_reason, None);
}

#[tokio::test]
async fn force_standard_tier_reason() {
    let mut snap = snapshot(catalog());
    snap.kill_switches.force_standard_tier = true;
    snap.kill_switches.disable_premium_tier = true;
    let f = fx_with(snap, MiniChatConfig::default()).await;
    let d = f.preflight("p").await.unwrap();

    assert_eq!(d.decision, QuotaDecisionKind::Downgrade);
    assert_eq!(d.downgrade_reason, Some("force_standard_tier"));
    assert_eq!(d.effective_model.id, "s");
}

#[tokio::test]
async fn disable_premium_tier_reason() {
    let mut snap = snapshot(catalog());
    snap.kill_switches.disable_premium_tier = true;
    let f = fx_with(snap, MiniChatConfig::default()).await;
    let d = f.preflight("p").await.unwrap();

    assert_eq!(d.decision, QuotaDecisionKind::Downgrade);
    assert_eq!(d.downgrade_reason, Some("disable_premium_tier"));
    assert_eq!(d.effective_model.id, "s");
}

#[tokio::test]
async fn disabled_selected_model_reason_model_disabled() {
    let f = fx().await;
    let d = f.preflight("p-off").await.unwrap();
    assert_eq!(d.decision, QuotaDecisionKind::Downgrade);
    assert_eq!(d.downgrade_reason, Some("model_disabled"));
    assert_eq!(d.effective_model.id, "p");

    let d = f.preflight("s-off").await.unwrap();
    assert_eq!(d.decision, QuotaDecisionKind::Downgrade);
    assert_eq!(d.downgrade_reason, Some("model_disabled"));
    assert_eq!(d.effective_model.id, "s");
}

#[tokio::test]
async fn model_disabled_reason_is_kept_over_later_reasons() {
    let f = fx().await;
    f.seed(PeriodType::Daily, QuotaBucket::TierPremium, 22_000_000, 0)
        .await;
    let d = f.preflight("p-off").await.unwrap();
    assert_eq!(d.downgrade_reason, Some("model_disabled"));
    assert_eq!(d.effective_model.id, "s");
}

#[tokio::test]
async fn missing_selected_model_starts_premium() {
    let f = fx().await;
    let d = f.preflight("no-such-model").await.unwrap();

    assert_eq!(d.decision, QuotaDecisionKind::Downgrade);
    assert_eq!(d.downgrade_reason, Some("model_disabled"));
    assert_eq!(d.effective_model.id, "p");
    assert_eq!(d.selected_model, "no-such-model");
}

#[tokio::test]
async fn candidate_reserve_uses_own_budgets() {
    let f = fx().await;
    // 2M left in total: premium's 3.75M does not fit, standard's 1.5M does.
    f.seed(PeriodType::Daily, QuotaBucket::Total, 58_000_000, 0)
        .await;
    let d = f.preflight("p").await.unwrap();

    assert_eq!(d.effective_model.id, "s");
    assert_eq!(d.reserved_credits_micro, 1_500_000);
    assert_eq!(d.downgrade_reason, Some("premium_quota_exhausted"));
}

#[tokio::test]
async fn candidate_with_invalid_multiplier_is_unavailable() {
    let mut cat = catalog();
    let p = cat.iter_mut().find(|m| m.id == "p").unwrap();
    p.input_tokens_credit_multiplier_micro = 0;
    let f = fx_with(snapshot(cat), MiniChatConfig::default()).await;
    let d = f.preflight("p").await.unwrap();

    assert_eq!(d.effective_model.id, "s");
    assert_eq!(d.downgrade_reason, Some("premium_quota_exhausted"));
}

#[tokio::test]
async fn no_enabled_model_in_any_tier_is_quota_exceeded() {
    let mut cat = catalog();
    for m in &mut cat {
        m.enabled = false;
    }
    let f = fx_with(snapshot(cat), MiniChatConfig::default()).await;
    assert_eq!(f.preflight("p").await.unwrap_err(), tokens_exceeded());
}

// ── Web search / code interpreter ────────────────────────────────────────────

#[tokio::test]
async fn web_search_kill_switch_before_cascade() {
    let mut snap = snapshot(catalog());
    snap.kill_switches.disable_web_search = true;
    let f = fx_with(snap, MiniChatConfig::default()).await;
    let mut input = f.input("p");
    input.tool_ctx.web_search_requested = true;
    let disabled = DomainError::FeatureDisabled {
        subject: "web_search",
    };
    assert_eq!(f.svc.preflight(&input).await.unwrap_err(), disabled);

    // Also when every tier is exhausted: 400, not 429.
    f.seed(PeriodType::Daily, QuotaBucket::Total, 60_000_000, 0)
        .await;
    assert_eq!(f.svc.preflight(&input).await.unwrap_err(), disabled);

    // Not requested: the kill switch does not apply.
    input.tool_ctx.web_search_requested = false;
    assert_eq!(
        f.svc.preflight(&input).await.unwrap_err(),
        tokens_exceeded()
    );
}

#[tokio::test]
async fn daily_web_search_quota_only_when_tool_sent() {
    let f = fx().await;
    // default quota.web_search_daily_quota = 75
    f.seed_row(f.user, PeriodType::Daily, QuotaBucket::Total, 0, 0, 75, 0)
        .await;
    let exceeded = DomainError::QuotaExceeded {
        scope: QuotaScope::WebSearch,
    };

    let mut input = f.input("p");
    input.tool_ctx.web_search_requested = true;
    assert_eq!(f.svc.preflight(&input).await.unwrap_err(), exceeded);

    // `s` has no tool_support.web_search: no tool, no check.
    let mut input = f.input("s");
    input.tool_ctx.web_search_requested = true;
    let d = f.svc.preflight(&input).await.unwrap();
    assert!(!d.tools.web_search);

    // Not requested: no check.
    f.preflight("p").await.unwrap();
}

#[tokio::test]
async fn daily_web_search_quota_allows_below_limit() {
    let f = fx().await;
    f.seed_row(f.user, PeriodType::Daily, QuotaBucket::Total, 0, 0, 74, 0)
        .await;
    let mut input = f.input("p");
    input.tool_ctx.web_search_requested = true;
    assert!(f.svc.preflight(&input).await.unwrap().tools.web_search);
}

#[tokio::test]
async fn daily_web_search_quota_follows_the_effective_model() {
    let f = fx().await;
    f.seed_row(f.user, PeriodType::Daily, QuotaBucket::Total, 0, 0, 75, 0)
        .await;
    // Premium exhausted: downgraded to `s`, which does not send web search.
    f.seed(PeriodType::Daily, QuotaBucket::TierPremium, 22_000_000, 0)
        .await;
    let mut input = f.input("p");
    input.tool_ctx.web_search_requested = true;
    let d = f.svc.preflight(&input).await.unwrap();
    assert_eq!(d.effective_model.id, "s");
    assert!(!d.tools.web_search);
}

#[tokio::test]
async fn code_interpreter_daily_quota_requires_ready_xlsx() {
    let f = fx().await;
    // default quota.code_interpreter_daily_quota = 50
    f.seed_row(f.user, PeriodType::Daily, QuotaBucket::Total, 0, 0, 0, 50)
        .await;

    let d = f.preflight("p").await.unwrap();
    assert!(!d.tools.code_interpreter);

    let mut input = f.input("p");
    input.tool_ctx.chat_has_ready_ci_files = true;
    assert_eq!(
        f.svc.preflight(&input).await.unwrap_err(),
        DomainError::QuotaExceeded {
            scope: QuotaScope::CodeInterpreter
        }
    );
}

#[tokio::test]
async fn tool_quota_reads_only_the_callers_rows() {
    let f = fx().await;
    let other = Uuid::new_v4();
    f.seed_row(
        other,
        PeriodType::Daily,
        QuotaBucket::Total,
        60_000_000,
        0,
        75,
        50,
    )
    .await;
    let mut input = f.input("p");
    input.tool_ctx.web_search_requested = true;
    input.tool_ctx.chat_has_ready_ci_files = true;
    let d = f.svc.preflight(&input).await.unwrap();
    assert_eq!(d.effective_model.id, "p");
}

// ── Reserve ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn reserve_increments_total_and_premium_rows() {
    let f = fx().await;
    f.reserve(true, 3_750_000).await.unwrap();
    for period in [PeriodType::Daily, PeriodType::Monthly] {
        for bucket in [QuotaBucket::Total, QuotaBucket::TierPremium] {
            let row = f.row(period, bucket).await.unwrap();
            assert_eq!(row.reserved_credits_micro, 3_750_000, "{period} {bucket}");
            assert_eq!(row.spent_credits_micro, 0);
        }
    }

    f.reserve(false, 1_500_000).await.unwrap();
    for period in [PeriodType::Daily, PeriodType::Monthly] {
        let total = f.row(period, QuotaBucket::Total).await.unwrap();
        assert_eq!(total.reserved_credits_micro, 5_250_000);
        let premium = f.row(period, QuotaBucket::TierPremium).await.unwrap();
        assert_eq!(premium.reserved_credits_micro, 3_750_000);
    }
}

#[tokio::test]
async fn standard_reserve_creates_no_premium_row() {
    let f = fx().await;
    f.reserve(false, 1_500_000).await.unwrap();
    assert!(
        f.row(PeriodType::Daily, QuotaBucket::TierPremium)
            .await
            .is_none()
    );
    assert!(
        f.row(PeriodType::Monthly, QuotaBucket::TierPremium)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn reserve_recheck_rejects_concurrent_over_limit() {
    let f = fx().await;
    // 2.5M left: each 1.5M reserve fits alone, both do not.
    f.seed(PeriodType::Daily, QuotaBucket::Total, 57_500_000, 0)
        .await;
    let first = f.preflight("s").await.unwrap();
    let second = f.preflight("s").await.unwrap();
    assert_eq!(first.reserved_credits_micro, 1_500_000);
    assert_eq!(second.reserved_credits_micro, 1_500_000);

    f.reserve(false, first.reserved_credits_micro)
        .await
        .unwrap();
    assert_eq!(
        f.reserve(false, second.reserved_credits_micro)
            .await
            .unwrap_err(),
        tokens_exceeded()
    );

    // The rejected reserve was rolled back.
    let daily = f.row(PeriodType::Daily, QuotaBucket::Total).await.unwrap();
    assert_eq!(daily.reserved_credits_micro, 1_500_000);
    let monthly = f
        .row(PeriodType::Monthly, QuotaBucket::Total)
        .await
        .unwrap();
    assert_eq!(monthly.reserved_credits_micro, 1_500_000);
}

#[tokio::test]
async fn reserve_recheck_covers_premium_bucket() {
    let f = fx().await;
    f.seed(
        PeriodType::Monthly,
        QuotaBucket::TierPremium,
        299_000_000,
        0,
    )
    .await;
    assert_eq!(
        f.reserve(true, 3_750_000).await.unwrap_err(),
        tokens_exceeded()
    );
    // Exactly at the limit is allowed.
    f.reserve(true, 1_000_000).await.unwrap();
}

// ── Settlement ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn settle_actual_within_tolerance() {
    // DESIGN 5.10.4: usage 900 / 300 on the standard model.
    let f = fx().await;
    f.seed(PeriodType::Daily, QuotaBucket::Total, 25_000_000, 1_500_000)
        .await;
    f.seed(
        PeriodType::Monthly,
        QuotaBucket::Total,
        240_000_000,
        1_500_000,
    )
    .await;
    let mut s = f.settlement(SettlementMethod::Actual);
    s.usage = Some(usage(900, 300));
    s.web_search_calls = 1;
    s.code_interpreter_calls = 2;
    let res = f.settle(s).await.unwrap();

    assert_eq!(
        res,
        SettlementResult {
            charged_credits_micro: 1_200_000,
            overshoot_capped: false
        }
    );
    let daily = f.row(PeriodType::Daily, QuotaBucket::Total).await.unwrap();
    assert_eq!(daily.spent_credits_micro, 26_200_000);
    assert_eq!(daily.reserved_credits_micro, 0);
    assert_eq!(daily.calls, 1);
    assert_eq!(daily.input_tokens, 900);
    assert_eq!(daily.output_tokens, 300);
    assert_eq!(daily.web_search_calls, 1);
    assert_eq!(daily.code_interpreter_calls, 2);
    let monthly = f
        .row(PeriodType::Monthly, QuotaBucket::Total)
        .await
        .unwrap();
    assert_eq!(monthly.spent_credits_micro, 241_200_000);
    assert_eq!(monthly.reserved_credits_micro, 0);
    assert_eq!(monthly.calls, 1);
    assert!(
        f.row(PeriodType::Daily, QuotaBucket::TierPremium)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn settle_actual_overshoot_within_tolerance_charges_actual() {
    let f = fx().await;
    let mut s = f.settlement(SettlementMethod::Actual);
    s.reserve_tokens = 1000;
    s.reserved_credits_micro = 1_000_000;
    s.usage = Some(usage(1100, 0)); // 1.10 <= 1.10
    let res = f.settle(s).await.unwrap();
    assert_eq!(res.charged_credits_micro, 1_100_000);
    assert!(!res.overshoot_capped);
}

#[tokio::test]
async fn settle_overshoot_capped_at_reserve() {
    // DESIGN 5.4.5: 11000 + 500 vs reserve 10000 -> 1.15 > 1.10.
    let f = fx().await;
    let mut s = f.settlement(SettlementMethod::Actual);
    s.reserve_tokens = 10_000;
    s.reserved_credits_micro = 2_500_000;
    s.max_output_tokens_applied = 1000;
    s.usage = Some(usage(11_000, 500));
    let res = f.settle(s).await.unwrap();

    assert_eq!(
        res,
        SettlementResult {
            charged_credits_micro: 2_500_000,
            overshoot_capped: true
        }
    );
    let daily = f.row(PeriodType::Daily, QuotaBucket::Total).await.unwrap();
    assert_eq!(daily.spent_credits_micro, 2_500_000);
    assert_eq!(daily.input_tokens, 11_000);
    assert_eq!(daily.output_tokens, 500);
}

#[tokio::test]
async fn settle_actual_without_usage_charges_zero() {
    let f = fx().await;
    let res = f
        .settle(f.settlement(SettlementMethod::Actual))
        .await
        .unwrap();
    assert_eq!(res.charged_credits_micro, 0);
    let daily = f.row(PeriodType::Daily, QuotaBucket::Total).await.unwrap();
    assert_eq!(daily.calls, 1);
}

#[tokio::test]
async fn settle_estimated_formula() {
    let f = fx().await;
    f.seed(PeriodType::Daily, QuotaBucket::Total, 0, 1_500_000)
        .await;
    let mut s = f.settlement(SettlementMethod::Estimated);
    s.web_search_calls = 2;
    s.code_interpreter_calls = 1;
    s.usage = Some(usage(900, 300)); // ignored on the estimated path
    let res = f.settle(s).await.unwrap();

    // credits(1500 - 500, 50) = 1_000_000 + 50_000
    assert_eq!(res.charged_credits_micro, 1_050_000);
    assert!(!res.overshoot_capped);
    let daily = f.row(PeriodType::Daily, QuotaBucket::Total).await.unwrap();
    assert_eq!(daily.spent_credits_micro, 1_050_000);
    assert_eq!(daily.reserved_credits_micro, 0);
    assert_eq!(daily.calls, 1);
    assert_eq!(daily.input_tokens, 0);
    assert_eq!(daily.output_tokens, 0);
    assert_eq!(daily.web_search_calls, 2);
    assert_eq!(daily.code_interpreter_calls, 1);
}

#[tokio::test]
async fn settle_released_zero_charge() {
    let f = fx().await;
    f.seed(PeriodType::Daily, QuotaBucket::Total, 7, 1_500_000)
        .await;
    let mut s = f.settlement(SettlementMethod::Released);
    s.web_search_calls = 2;
    s.usage = Some(usage(900, 300));
    let res = f.settle(s).await.unwrap();

    assert_eq!(res.charged_credits_micro, 0);
    let daily = f.row(PeriodType::Daily, QuotaBucket::Total).await.unwrap();
    assert_eq!(daily.spent_credits_micro, 7);
    assert_eq!(daily.reserved_credits_micro, 0);
    assert_eq!(daily.calls, 1);
    assert_eq!(daily.input_tokens, 0);
    assert_eq!(daily.web_search_calls, 0);
}

#[tokio::test]
async fn settle_premium_updates_both_buckets_and_calls() {
    let f = fx().await;
    f.reserve(true, 3_750_000).await.unwrap();
    let mut s = f.settlement(SettlementMethod::Actual);
    s.premium = true;
    s.reserve_tokens = 1500;
    s.reserved_credits_micro = 3_750_000;
    s.in_mult = P_MULT;
    s.out_mult = P_MULT;
    s.usage = Some(usage(900, 300));
    s.web_search_calls = 1;
    let res = f.settle(s).await.unwrap();

    assert_eq!(res.charged_credits_micro, 3_000_000);
    for period in [PeriodType::Daily, PeriodType::Monthly] {
        let total = f.row(period, QuotaBucket::Total).await.unwrap();
        assert_eq!(total.spent_credits_micro, 3_000_000);
        assert_eq!(total.reserved_credits_micro, 0);
        assert_eq!(total.calls, 1);
        assert_eq!(total.input_tokens, 900);
        assert_eq!(total.web_search_calls, 1);
        let premium = f.row(period, QuotaBucket::TierPremium).await.unwrap();
        assert_eq!(premium.spent_credits_micro, 3_000_000);
        assert_eq!(premium.reserved_credits_micro, 0);
        assert_eq!(premium.calls, 1);
        // telemetry lives in bucket `total` only
        assert_eq!(premium.input_tokens, 0);
        assert_eq!(premium.output_tokens, 0);
        assert_eq!(premium.web_search_calls, 0);
    }
}

#[tokio::test]
async fn settle_targets_the_preflight_periods() {
    let f = fx().await;
    // Reserve on day 1, settle "after midnight" with the same period starts.
    f.reserve(false, 1_500_000).await.unwrap();
    let res = f.settle(f.settlement(SettlementMethod::Released)).await;
    res.unwrap();
    let daily = f.row(PeriodType::Daily, QuotaBucket::Total).await.unwrap();
    assert_eq!(daily.reserved_credits_micro, 0);
}

#[tokio::test]
async fn settle_rejects_out_of_range_usage() {
    let f = fx().await;
    let mut s = f.settlement(SettlementMethod::Actual);
    s.reserve_tokens = 100_000_000; // no cap: the credit check must fail
    s.usage = Some(usage(20_000_000, 0));
    assert!(matches!(
        f.settle(s).await.unwrap_err(),
        DomainError::Internal(_)
    ));
}

// ── Warnings and status ──────────────────────────────────────────────────────

fn warning(
    tier: QuotaTierKind,
    period: QuotaPeriodKind,
    pct: u32,
    warn: bool,
    exhausted: bool,
    next_reset: Option<OffsetDateTime>,
) -> QuotaWarningView {
    QuotaWarningView {
        tier,
        period,
        remaining_percentage: pct,
        warning: warn,
        exhausted,
        next_reset,
    }
}

#[tokio::test]
async fn status_and_warnings_flags() {
    let f = fx().await;
    let limits = UserLimits {
        user_id: f.user,
        policy_version: 1,
        standard: TierLimits {
            limit_daily_credits_micro: 100_000_000,
            limit_monthly_credits_micro: 1_000_000_000,
        },
        premium: TierLimits {
            limit_daily_credits_micro: 100_000_000,
            limit_monthly_credits_micro: 0, // skipped
        },
    };
    // premium daily: used 80M of 100M -> 20 % -> warning (threshold 80)
    f.seed(
        PeriodType::Daily,
        QuotaBucket::TierPremium,
        70_000_000,
        10_000_000,
    )
    .await;
    // total daily: 100M used -> 0 % -> exhausted
    f.seed(PeriodType::Daily, QuotaBucket::Total, 100_000_000, 0)
        .await;
    // total monthly: 790M used -> 21 % -> no warning
    f.seed(PeriodType::Monthly, QuotaBucket::Total, 790_000_000, 0)
        .await;

    let tomorrow = datetime!(2026-10-05 00:00:00 UTC);
    let got = f
        .svc
        .warnings(f.tenant, f.user, &limits, now())
        .await
        .unwrap();
    assert_eq!(
        got,
        vec![
            warning(
                QuotaTierKind::Premium,
                QuotaPeriodKind::Daily,
                20,
                true,
                false,
                Some(tomorrow)
            ),
            warning(
                QuotaTierKind::Total,
                QuotaPeriodKind::Daily,
                0,
                true,
                true,
                Some(tomorrow)
            ),
            warning(
                QuotaTierKind::Total,
                QuotaPeriodKind::Monthly,
                21,
                false,
                false,
                None
            ),
        ]
    );
}

#[tokio::test]
async fn remaining_percentage_floors_and_clamps() {
    let f = fx().await;
    let limits = UserLimits {
        user_id: f.user,
        policy_version: 1,
        standard: TierLimits {
            limit_daily_credits_micro: 100_000_000,
            limit_monthly_credits_micro: 1_000_000_000,
        },
        premium: TierLimits {
            limit_daily_credits_micro: 0,
            limit_monthly_credits_micro: -5,
        },
    };
    // 99.5 % used -> 0.5 % left -> floored to 0 -> exhausted
    f.seed(PeriodType::Daily, QuotaBucket::Total, 99_500_000, 0)
        .await;
    // overspent -> clamped to 0
    f.seed(PeriodType::Monthly, QuotaBucket::Total, 1_200_000_000, 0)
        .await;
    let got = f
        .svc
        .warnings(f.tenant, f.user, &limits, now())
        .await
        .unwrap();
    assert_eq!(got.len(), 2);
    assert!(got.iter().all(|w| w.tier == QuotaTierKind::Total));
    assert!(
        got.iter()
            .all(|w| w.remaining_percentage == 0 && w.exhausted)
    );
    assert_eq!(got[1].next_reset, Some(datetime!(2026-11-01 00:00:00 UTC)));
}

#[tokio::test]
async fn status_reports_caller_usage_with_breakdown() {
    let f = fx().await;
    let ctx = ctx_for(f.tenant, f.user);
    let today = PeriodStarts::at(OffsetDateTime::now_utc());
    f.seed_at(
        f.user,
        PeriodType::Daily,
        today.daily,
        QuotaBucket::TierPremium,
        2_000_000,
        200_000,
        0,
        0,
    )
    .await;
    // Another user in the same tenant is not visible.
    f.seed_at(
        Uuid::new_v4(),
        PeriodType::Daily,
        today.daily,
        QuotaBucket::Total,
        9_000_000,
        0,
        0,
        0,
    )
    .await;

    let st = f.svc.status(&ctx).await.unwrap();
    assert_eq!(st.warning_threshold_pct, 80);
    let tiers: Vec<_> = st.tiers.iter().map(|t| t.tier).collect();
    assert_eq!(tiers, vec![QuotaTierKind::Premium, QuotaTierKind::Total]);
    for t in &st.tiers {
        let periods: Vec<_> = t.periods.iter().map(|p| p.period).collect();
        assert_eq!(
            periods,
            vec![QuotaPeriodKind::Daily, QuotaPeriodKind::Monthly]
        );
    }

    let pd = &st.tiers[0].periods[0];
    assert_eq!(pd.limit_credits_micro, 22_000_000);
    assert_eq!(pd.used_credits_micro, 2_200_000);
    assert_eq!(pd.remaining_credits_micro, 19_800_000);
    assert_eq!(pd.remaining_percentage, 90);
    assert!(!pd.warning && !pd.exhausted);
    let next_day = today.daily.next_day().unwrap().midnight().assume_utc();
    assert_eq!(pd.next_reset, next_day);

    let total_daily = &st.tiers[1].periods[0];
    assert_eq!(total_daily.used_credits_micro, 0);
    assert_eq!(total_daily.remaining_credits_micro, 60_000_000);
    assert_eq!(total_daily.remaining_percentage, 100);
    let total_monthly = &st.tiers[1].periods[1];
    assert_eq!(total_monthly.next_reset.day(), 1);
    assert!(total_monthly.next_reset > OffsetDateTime::now_utc());
}

#[tokio::test]
async fn status_requires_quota_authorization() {
    let db = test_provider().await;
    let svc = QuotaService::new(
        Arc::clone(&db),
        Arc::new(FakeAuthz::denying()),
        Arc::new(FakePolicy::with_limits(
            snapshot(catalog()),
            standard_limits(),
            premium_limits(),
        )),
        Arc::new(MiniChatConfig::default()),
    );
    let ctx = ctx_for(Uuid::new_v4(), Uuid::new_v4());
    assert_eq!(
        svc.status(&ctx).await.unwrap_err(),
        DomainError::AuthzDenied
    );
}
