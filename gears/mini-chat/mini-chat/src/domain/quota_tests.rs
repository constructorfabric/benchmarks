use mini_chat_sdk::{
    EstimationBudgets, KillSwitches, ModelCatalogEntry, ModelPreference, ModelTier, PolicySnapshot,
    TierLimits, UserLimits,
};
use time::macros::{date, datetime};
use uuid::Uuid;

use super::*;
use crate::infra::db::entities::quota_usage;

fn model(id: &str, tier: ModelTier, in_mult: u64, out_mult: u64) -> ModelCatalogEntry {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "provider_model_id": id,
        "display_name": id,
        "provider_id": "openai",
        "provider_display_name": "OpenAI",
        "tier": tier,
        "enabled": true,
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "max_input_tokens": 100_000,
        "input_tokens_credit_multiplier_micro": in_mult,
        "output_tokens_credit_multiplier_micro": out_mult,
        "max_num_results": 5,
        "general_config": {"tool_support": {"web_search": true, "file_search": true, "code_interpreter": true}}
    }))
    .unwrap()
}

fn snapshot(models: Vec<ModelCatalogEntry>, ks: KillSwitches) -> PolicySnapshot {
    PolicySnapshot {
        policy_version: 1,
        model_catalog: models,
        kill_switches: ks,
    }
}

fn catalog() -> Vec<ModelCatalogEntry> {
    let mut premium = model("big", ModelTier::Premium, 3_000_000, 15_000_000);
    premium.preference = Some(ModelPreference {
        is_default: true,
        sort_order: 0,
    });
    let standard = model("small", ModelTier::Standard, 1_000_000, 3_000_000);
    let standard2 = model("small2", ModelTier::Standard, 1_000_000, 3_000_000);
    vec![premium, standard, standard2]
}

fn limits(premium_daily: i64, total_daily: i64) -> UserLimits {
    UserLimits {
        user_id: Uuid::nil(),
        policy_version: 1,
        standard: TierLimits {
            limit_daily_credits_micro: total_daily,
            limit_monthly_credits_micro: total_daily * 30,
        },
        premium: TierLimits {
            limit_daily_credits_micro: premium_daily,
            limit_monthly_credits_micro: premium_daily * 30,
        },
    }
}

fn row(
    period: &str,
    start: time::Date,
    bucket: &str,
    spent: i64,
    reserved: i64,
) -> quota_usage::Model {
    quota_usage::Model {
        id: Uuid::new_v4(),
        tenant_id: Uuid::nil(),
        user_id: Uuid::nil(),
        period_type: period.to_owned(),
        period_start: start,
        bucket: bucket.to_owned(),
        spent_credits_micro: spent,
        reserved_credits_micro: reserved,
        calls: 0,
        input_tokens: 0,
        output_tokens: 0,
        file_search_calls: 0,
        web_search_calls: 3,
        code_interpreter_calls: 1,
        rag_retrieval_calls: 0,
        image_inputs: 0,
        image_upload_bytes: 0,
        updated_at: datetime!(2026-10-03 10:00 UTC),
    }
}

fn inputs() -> ReserveInputs {
    ReserveInputs {
        message_bytes: 400,
        prior_context_tokens: 0,
        image_count: 0,
        chat: ChatToolState::default(),
        max_output_tokens_cfg: 4096,
    }
}

// ── credits ────────────────────────────────────────────────────────────────

#[test]
fn credits_use_ceil_per_side() {
    assert_eq!(credits_micro(20, 10, 3_000_000, 15_000_000), Ok(210));
    assert_eq!(credits_micro(1, 1, 1, 1), Ok(2));
    assert_eq!(credits_micro(0, 0, 1, 1), Ok(0));
    assert_eq!(credits_micro(1_000_000, 0, 1_000_000, 1), Ok(1_000_000));
    assert_eq!(credits_micro(3, 0, 500_000, 1), Ok(2));
}

#[test]
fn credits_reject_bad_inputs() {
    assert_eq!(
        credits_micro(-1, 0, 1, 1),
        Err(CreditsError::InvalidTokenCount(-1))
    );
    assert_eq!(
        credits_micro(MAX_TOKENS + 1, 0, 1, 1),
        Err(CreditsError::InvalidTokenCount(MAX_TOKENS + 1))
    );
    assert_eq!(credits_micro(1, 1, 0, 1), Err(CreditsError::ZeroMultiplier));
    assert_eq!(
        credits_micro(1, 1, 1, MAX_MULT + 1),
        Err(CreditsError::InvalidMultiplier(MAX_MULT + 1))
    );
    // the largest legal values do not overflow
    assert!(credits_micro(MAX_TOKENS, MAX_TOKENS, MAX_MULT, MAX_MULT).is_ok());
}

#[test]
fn text_estimate_is_conservative() {
    let b = EstimationBudgets::default();
    // 400 bytes / 4 = 100 + 100 overhead = 200, +10% = 220
    assert_eq!(estimate_text_tokens(400, &b), 220);
    assert_eq!(estimate_text_tokens(0, &b), 110);
    let zero = EstimationBudgets {
        bytes_per_token_conservative: 0,
        ..EstimationBudgets::default()
    };
    assert!(estimate_text_tokens(10, &zero) > 0);
}

// ── periods ────────────────────────────────────────────────────────────────

#[test]
fn periods_and_resets_are_utc_calendar() {
    let p = Periods::at(datetime!(2026-12-31 23:59:59 UTC));
    assert_eq!(p.daily, date!(2026 - 12 - 31));
    assert_eq!(p.monthly, date!(2026 - 12 - 01));
    assert_eq!(p.next_reset(PERIOD_DAILY), datetime!(2027-01-01 0:00 UTC));
    assert_eq!(p.next_reset(PERIOD_MONTHLY), datetime!(2027-01-01 0:00 UTC));
    let p = Periods::at(datetime!(2026-03-15 01:00 +05:00));
    assert_eq!(p.daily, date!(2026 - 03 - 14));
    assert_eq!(p.next_reset(PERIOD_MONTHLY), datetime!(2026-04-01 0:00 UTC));
}

#[test]
fn usage_ignores_rows_of_other_periods() {
    let p = Periods::at(datetime!(2026-10-03 10:00 UTC));
    let rows = vec![
        row(PERIOD_DAILY, date!(2026 - 10 - 03), BUCKET_TOTAL, 100, 50),
        row(PERIOD_DAILY, date!(2026 - 10 - 02), BUCKET_TOTAL, 9_999, 0),
        row(PERIOD_MONTHLY, date!(2026 - 10 - 01), BUCKET_PREMIUM, 7, 0),
    ];
    let u = Usage::from_rows(&rows, p);
    assert_eq!(u.used(PERIOD_DAILY, BUCKET_TOTAL), (100, 50));
    assert_eq!(u.used(PERIOD_MONTHLY, BUCKET_PREMIUM), (7, 0));
    assert_eq!(u.used(PERIOD_MONTHLY, BUCKET_TOTAL), (0, 0));
    assert_eq!(u.daily_tool_calls(), (3, 1));
}

// ── reserve and cascade ────────────────────────────────────────────────────

#[test]
fn reserve_adds_surcharges_only_for_enabled_tools() {
    let m = model("big", ModelTier::Premium, 3_000_000, 15_000_000);
    let ks = KillSwitches::default();
    let base = candidate_reserve(&m, &inputs(), ks);
    assert_eq!(base.estimated_input_tokens, 220);
    assert_eq!(base.max_output_tokens_applied, 4096);
    assert_eq!(base.reserve_tokens, 220 + 4096);
    assert_eq!(base.reserved_credits_micro, 220 * 3 + 4096 * 15);

    let mut inp = inputs();
    inp.chat = ChatToolState {
        has_ready_documents: true,
        has_ready_code_interpreter_files: true,
        web_search_requested: true,
    };
    inp.image_count = 2;
    let r = candidate_reserve(&m, &inp, ks);
    assert_eq!(r.estimated_input_tokens, 220 + 2 * 1000 + 500 + 500 + 1000);
    assert_eq!(
        r.tools,
        ToolSet {
            file_search: true,
            web_search: true,
            code_interpreter: true
        }
    );

    let ks = KillSwitches {
        disable_file_search: true,
        disable_code_interpreter: true,
        ..KillSwitches::default()
    };
    let r = candidate_reserve(&m, &inp, ks);
    assert!(!r.tools.file_search && !r.tools.code_interpreter && r.tools.web_search);
    assert_eq!(r.estimated_input_tokens, 220 + 2000 + 500);

    // max output is the smaller of the model and the gear setting
    let mut inp = inputs();
    inp.max_output_tokens_cfg = 1000;
    assert_eq!(
        candidate_reserve(&m, &inp, KillSwitches::default()).max_output_tokens_applied,
        1000
    );
}

#[test]
fn cascade_allows_selected_model_with_quota() {
    let snap = snapshot(catalog(), KillSwitches::default());
    let d = cascade(
        "big",
        &snap,
        &Usage::default(),
        &limits(50_000_000, 100_000_000),
        &inputs(),
    )
    .unwrap();
    assert_eq!(d.effective.id, "big");
    assert!(!d.downgrade);
    assert_eq!(d.decision_str(), "allow");
    assert_eq!(d.downgrade_reason, None);
}

#[test]
fn cascade_downgrades_when_premium_exhausted() {
    let snap = snapshot(catalog(), KillSwitches::default());
    let d = cascade(
        "big",
        &snap,
        &Usage::default(),
        &limits(5_000, 100_000_000),
        &inputs(),
    )
    .unwrap();
    assert_eq!(d.effective.id, "small");
    assert_eq!(d.tier, ModelTier::Standard);
    assert!(d.downgrade);
    assert_eq!(d.decision_str(), "downgrade");
    assert_eq!(d.downgrade_reason, Some("premium_quota_exhausted"));
}

#[test]
fn cascade_counts_spent_and_reserved() {
    let snap = snapshot(catalog(), KillSwitches::default());
    let p = Periods::at(OffsetDateTime::now_utc());
    let premium_reserve =
        candidate_reserve(&snap.model_catalog[0], &inputs(), KillSwitches::default())
            .reserved_credits_micro;
    let limit = 1_000_000;
    // exactly fits
    let rows = vec![row(
        PERIOD_DAILY,
        p.daily,
        BUCKET_PREMIUM,
        limit - premium_reserve - 10,
        10,
    )];
    let d = cascade(
        "big",
        &snap,
        &Usage::from_rows(&rows, p),
        &limits(limit, 100_000_000),
        &inputs(),
    )
    .unwrap();
    assert_eq!(d.effective.id, "big");
    // one credit too many
    let rows = vec![row(
        PERIOD_DAILY,
        p.daily,
        BUCKET_PREMIUM,
        limit - premium_reserve - 10,
        11,
    )];
    let d = cascade(
        "big",
        &snap,
        &Usage::from_rows(&rows, p),
        &limits(limit, 100_000_000),
        &inputs(),
    )
    .unwrap();
    assert_eq!(d.effective.id, "small");
}

#[test]
fn cascade_rejects_when_total_exhausted() {
    let snap = snapshot(catalog(), KillSwitches::default());
    assert!(
        cascade(
            "big",
            &snap,
            &Usage::default(),
            &limits(5_000, 5_000),
            &inputs()
        )
        .is_none()
    );
    assert!(
        cascade(
            "small",
            &snap,
            &Usage::default(),
            &limits(50_000_000, 5_000),
            &inputs()
        )
        .is_none()
    );
    // a standard model never upgrades
    let d = cascade(
        "small2",
        &snap,
        &Usage::default(),
        &limits(50_000_000, 100_000_000),
        &inputs(),
    )
    .unwrap();
    assert_eq!(d.effective.id, "small2");
    assert!(!d.downgrade);
}

#[test]
fn cascade_kill_switch_reasons() {
    let ks = KillSwitches {
        force_standard_tier: true,
        ..KillSwitches::default()
    };
    let d = cascade(
        "big",
        &snapshot(catalog(), ks),
        &Usage::default(),
        &limits(50_000_000, 100_000_000),
        &inputs(),
    )
    .unwrap();
    assert_eq!(
        (d.effective.id.as_str(), d.downgrade_reason),
        ("small", Some("force_standard_tier"))
    );
    let ks = KillSwitches {
        disable_premium_tier: true,
        ..KillSwitches::default()
    };
    let d = cascade(
        "big",
        &snapshot(catalog(), ks),
        &Usage::default(),
        &limits(50_000_000, 100_000_000),
        &inputs(),
    )
    .unwrap();
    assert_eq!(d.downgrade_reason, Some("disable_premium_tier"));
}

#[test]
fn cascade_disabled_or_missing_model() {
    let mut cat = catalog();
    cat[0].enabled = false;
    let mut extra = model("big2", ModelTier::Premium, 3_000_000, 15_000_000);
    extra.enabled = true;
    cat.push(extra);
    let snap = snapshot(cat, KillSwitches::default());
    let d = cascade(
        "big",
        &snap,
        &Usage::default(),
        &limits(50_000_000, 100_000_000),
        &inputs(),
    )
    .unwrap();
    assert_eq!(d.effective.id, "big2");
    assert!(d.downgrade);
    assert_eq!(d.downgrade_reason, Some("model_disabled"));
    let d = cascade(
        "gone",
        &snap,
        &Usage::default(),
        &limits(50_000_000, 100_000_000),
        &inputs(),
    )
    .unwrap();
    assert_eq!(d.downgrade_reason, Some("model_disabled"));
}

#[test]
fn buckets_by_tier() {
    assert_eq!(
        buckets_for(ModelTier::Premium),
        &[BUCKET_TOTAL, BUCKET_PREMIUM]
    );
    assert_eq!(buckets_for(ModelTier::Standard), &[BUCKET_TOTAL]);
}

// ── status / warnings ──────────────────────────────────────────────────────

#[test]
fn statuses_compute_percentages_and_flags() {
    let p = Periods::at(datetime!(2026-10-03 10:00 UTC));
    let rows = vec![
        row(PERIOD_DAILY, p.daily, BUCKET_TOTAL, 70, 10),
        row(PERIOD_MONTHLY, p.monthly, BUCKET_TOTAL, 2_999, 0),
        row(PERIOD_DAILY, p.daily, BUCKET_PREMIUM, 50, 0),
    ];
    let mut l = limits(100, 100);
    l.standard.limit_monthly_credits_micro = 3_000;
    l.premium.limit_monthly_credits_micro = 0; // skipped
    let st = statuses(&Usage::from_rows(&rows, p), &l, p, 80);
    let get = |tier: &str, period: &str| {
        st.iter()
            .find(|s| s.tier == tier && s.period == period)
            .cloned()
    };
    assert!(get("premium", PERIOD_MONTHLY).is_none());
    let pd = get("premium", PERIOD_DAILY).unwrap();
    assert_eq!(
        (
            pd.used,
            pd.remaining,
            pd.remaining_percentage,
            pd.warning,
            pd.exhausted
        ),
        (50, 50, 50, false, false)
    );
    let td = get("total", PERIOD_DAILY).unwrap();
    assert_eq!(
        (
            td.used,
            td.remaining,
            td.remaining_percentage,
            td.warning,
            td.exhausted
        ),
        (80, 20, 20, true, false)
    );
    // less than 1% left floors to 0 -> exhausted
    let tm = get("total", PERIOD_MONTHLY).unwrap();
    assert_eq!(
        (
            tm.remaining,
            tm.remaining_percentage,
            tm.exhausted,
            tm.warning
        ),
        (1, 0, true, true)
    );
    assert_eq!(tm.next_reset, datetime!(2026-11-01 0:00 UTC));
    assert_eq!(td.next_reset, datetime!(2026-10-04 0:00 UTC));
}

#[test]
fn statuses_clamp_overspend() {
    let p = Periods::at(datetime!(2026-10-03 10:00 UTC));
    let rows = vec![row(PERIOD_DAILY, p.daily, BUCKET_TOTAL, 500, 0)];
    let st = statuses(&Usage::from_rows(&rows, p), &limits(100, 100), p, 80);
    let td = st
        .iter()
        .find(|s| s.tier == "total" && s.period == PERIOD_DAILY)
        .unwrap();
    assert_eq!(
        (td.remaining, td.remaining_percentage, td.exhausted),
        (0, 0, true)
    );
}
