//! Preflight cascade, kill switches, tool quotas and reserve booking.

use mini_chat_sdk::{ModelTier, TierLimits};

use super::{assert_quota_exceeded, book, get_row, put_row, req, row_count};
use crate::domain::error::DomainError;
use crate::domain::quota::{
    BUCKET_PREMIUM, BUCKET_TOTAL, EnabledTools, PERIOD_DAILY, PERIOD_MONTHLY, QuotaDecisionKind, ToolInputs,
    period_starts, preflight,
};
use crate::testing::{NO_VISION, PREMIUM, STANDARD, TINY, TestApp};

const PREMIUM_RESERVE: i64 = 660 + 61_440;
const STANDARD_RESERVE: i64 = 220 + 12_288;

#[tokio::test]
async fn allow_on_selected_premium_model() {
    let t = TestApp::new().await;
    let d = preflight(&t.app, &req(PREMIUM)).await.unwrap();
    assert_eq!(d.decision, QuotaDecisionKind::Allow);
    assert_eq!(d.decision.as_str(), "allow");
    assert_eq!(d.effective_model.id, PREMIUM);
    assert_eq!(d.effective_tier, ModelTier::Premium);
    assert_eq!(d.downgrade_reason, None);
    assert_eq!(d.selected_model, PREMIUM);
    assert_eq!(d.estimated_input_tokens, 220);
    assert_eq!(d.max_output_tokens_applied, 4096);
    assert_eq!(d.reserve_tokens, 220 + 4096);
    assert_eq!(d.reserved_credits_micro, PREMIUM_RESERVE);
    assert_eq!(d.minimal_generation_floor_applied, 50);
    assert_eq!(d.policy_version, 1);
    assert_eq!(d.tools, EnabledTools::default());
    assert_eq!(d.periods, period_starts(req(PREMIUM).now));
    // preflight writes nothing
    assert_eq!(row_count(&t.app).await, 0);
}

#[tokio::test]
async fn standard_selected_stays_standard() {
    let t = TestApp::new().await;
    let d = preflight(&t.app, &req(STANDARD)).await.unwrap();
    assert_eq!(d.decision, QuotaDecisionKind::Allow);
    assert_eq!(d.effective_model.id, STANDARD);
    assert_eq!(d.effective_tier, ModelTier::Standard);
    assert_eq!(d.reserved_credits_micro, STANDARD_RESERVE);

    // a non-default standard model is its own candidate
    let d = preflight(&t.app, &req(NO_VISION)).await.unwrap();
    assert_eq!(d.effective_model.id, NO_VISION);
    assert_eq!(d.decision, QuotaDecisionKind::Allow);
}

#[tokio::test]
async fn standard_never_upgrades_when_total_exhausted() {
    let t = TestApp::new().await;
    let p = period_starts(crate::clock::now());
    put_row(&t.app, PERIOD_DAILY, p.daily, BUCKET_TOTAL, |r| r.spent_credits_micro = 100_000_000 - 1000).await;
    let err = preflight(&t.app, &req(STANDARD)).await.unwrap_err();
    assert_quota_exceeded(&err, "tokens");
}

#[tokio::test]
async fn premium_daily_exhausted_downgrades_to_standard() {
    let t = TestApp::new().await;
    let p = period_starts(crate::clock::now());
    put_row(&t.app, PERIOD_DAILY, p.daily, BUCKET_PREMIUM, |r| r.spent_credits_micro = 50_000_000 - 1000).await;
    let d = preflight(&t.app, &req(PREMIUM)).await.unwrap();
    assert_eq!(d.decision, QuotaDecisionKind::Downgrade);
    assert_eq!(d.decision.as_str(), "downgrade");
    assert_eq!(d.effective_model.id, STANDARD);
    assert_eq!(d.effective_tier, ModelTier::Standard);
    assert_eq!(d.downgrade_reason.as_deref(), Some("premium_quota_exhausted"));
    assert_eq!(d.reserved_credits_micro, STANDARD_RESERVE);
}

#[tokio::test]
async fn premium_monthly_exhausted_downgrades() {
    let t = TestApp::new().await;
    let p = period_starts(crate::clock::now());
    put_row(&t.app, PERIOD_MONTHLY, p.monthly, BUCKET_PREMIUM, |r| r.reserved_credits_micro = 500_000_000).await;
    let d = preflight(&t.app, &req(PREMIUM)).await.unwrap();
    assert_eq!(d.effective_model.id, STANDARD);
    assert_eq!(d.downgrade_reason.as_deref(), Some("premium_quota_exhausted"));
}

#[tokio::test]
async fn candidate_reserve_too_big_for_remaining_total() {
    let t = TestApp::new().await;
    let p = period_starts(crate::clock::now());
    // 20_000 left in the overall daily cap: premium reserve (62_100) does not fit, standard (12_508) does.
    put_row(&t.app, PERIOD_DAILY, p.daily, BUCKET_TOTAL, |r| {
        r.spent_credits_micro = 100_000_000 - 30_000;
        r.reserved_credits_micro = 10_000;
    })
    .await;
    let d = preflight(&t.app, &req(PREMIUM)).await.unwrap();
    assert_eq!(d.effective_model.id, STANDARD);
    assert_eq!(d.downgrade_reason.as_deref(), Some("premium_quota_exhausted"));

    // exactly-fitting reserve is available (<= limit)
    let t = TestApp::new().await;
    put_row(&t.app, PERIOD_DAILY, p.daily, BUCKET_TOTAL, |r| r.spent_credits_micro = 100_000_000 - STANDARD_RESERVE).await;
    let d = preflight(&t.app, &req(STANDARD)).await.unwrap();
    assert_eq!(d.effective_model.id, STANDARD);
    put_row(&t.app, PERIOD_MONTHLY, p.monthly, BUCKET_TOTAL, |r| r.spent_credits_micro = 1_000_000_000 - STANDARD_RESERVE + 1)
        .await;
    assert_quota_exceeded(&preflight(&t.app, &req(STANDARD)).await.unwrap_err(), "tokens");
}

#[tokio::test]
async fn all_tiers_exhausted_rejects_with_tokens() {
    let t = TestApp::new().await;
    let p = period_starts(crate::clock::now());
    put_row(&t.app, PERIOD_MONTHLY, p.monthly, BUCKET_TOTAL, |r| r.spent_credits_micro = 1_000_000_000).await;
    let err = preflight(&t.app, &req(PREMIUM)).await.unwrap_err();
    assert_quota_exceeded(&err, "tokens");
}

#[tokio::test]
async fn kill_switches_skip_premium_with_reason() {
    let t = TestApp::new().await;
    t.policy.with_snapshot(|s| s.kill_switches.force_standard_tier = true);
    let d = preflight(&t.app, &req(PREMIUM)).await.unwrap();
    assert_eq!(d.effective_model.id, STANDARD);
    assert_eq!(d.decision, QuotaDecisionKind::Downgrade);
    assert_eq!(d.downgrade_reason.as_deref(), Some("force_standard_tier"));

    t.policy.with_snapshot(|s| {
        s.kill_switches.force_standard_tier = false;
        s.kill_switches.disable_premium_tier = true;
    });
    let d = preflight(&t.app, &req(PREMIUM)).await.unwrap();
    assert_eq!(d.effective_model.id, STANDARD);
    assert_eq!(d.downgrade_reason.as_deref(), Some("disable_premium_tier"));

    // standard selections are unaffected
    let d = preflight(&t.app, &req(STANDARD)).await.unwrap();
    assert_eq!(d.decision, QuotaDecisionKind::Allow);
}

#[tokio::test]
async fn disabled_selected_model_downgrades_with_model_disabled() {
    let t = TestApp::new().await;
    // "old-model" is a disabled standard model: cascade [standard], first enabled standard model.
    let d = preflight(&t.app, &req("old-model")).await.unwrap();
    assert_eq!(d.decision, QuotaDecisionKind::Downgrade);
    assert_eq!(d.effective_model.id, STANDARD);
    assert_eq!(d.downgrade_reason.as_deref(), Some("model_disabled"));
    assert_eq!(d.selected_model, "old-model");
}

#[tokio::test]
async fn missing_selected_model_starts_at_premium() {
    let t = TestApp::new().await;
    let d = preflight(&t.app, &req("no-such-model")).await.unwrap();
    assert_eq!(d.decision, QuotaDecisionKind::Downgrade);
    assert_eq!(d.effective_model.id, PREMIUM, "premium default model is the candidate");
    assert_eq!(d.downgrade_reason.as_deref(), Some("model_disabled"));

    // the reason set first is kept when premium is then exhausted
    let p = period_starts(crate::clock::now());
    put_row(&t.app, PERIOD_DAILY, p.daily, BUCKET_PREMIUM, |r| r.spent_credits_micro = 50_000_000).await;
    let d = preflight(&t.app, &req("no-such-model")).await.unwrap();
    assert_eq!(d.effective_model.id, STANDARD);
    assert_eq!(d.downgrade_reason.as_deref(), Some("model_disabled"));
}

#[tokio::test]
async fn standard_default_preference_is_candidate() {
    let t = TestApp::new().await;
    t.policy.with_snapshot(|s| {
        for m in &mut s.model_catalog {
            if m.id == NO_VISION {
                m.preference = Some(mini_chat_sdk::ModelPreference { is_default: true, sort_order: 0 });
            }
        }
    });
    t.policy.with_snapshot(|s| s.kill_switches.force_standard_tier = true);
    let d = preflight(&t.app, &req(PREMIUM)).await.unwrap();
    assert_eq!(d.effective_model.id, NO_VISION);
}

#[tokio::test]
async fn uncomputable_candidate_reserve_is_unavailable() {
    let t = TestApp::new().await;
    t.policy.with_snapshot(|s| {
        for m in &mut s.model_catalog {
            if m.id == PREMIUM {
                m.input_tokens_credit_multiplier_micro = 0;
            }
        }
    });
    let d = preflight(&t.app, &req(PREMIUM)).await.unwrap();
    assert_eq!(d.effective_model.id, STANDARD);
    assert_eq!(d.downgrade_reason.as_deref(), Some("premium_quota_exhausted"));
}

#[tokio::test]
async fn surcharges_tools_images_and_prior_context() {
    let t = TestApp::new().await;
    let mut r = req(PREMIUM);
    r.image_count = 2;
    r.prior_context_tokens = 1000;
    r.tools = ToolInputs { has_ready_documents: true, has_ready_code_interpreter: true, web_search_requested: true };
    let d = preflight(&t.app, &r).await.unwrap();
    assert_eq!(d.estimated_input_tokens, 220 + 1000 + 2000 + 500 + 500 + 1000);
    assert_eq!(d.tools, EnabledTools { file_search: true, web_search: true, code_interpreter: true });

    // kill switches remove the file search / code interpreter tools and their surcharges
    t.policy.with_snapshot(|s| {
        s.kill_switches.disable_file_search = true;
        s.kill_switches.disable_code_interpreter = true;
    });
    let d = preflight(&t.app, &r).await.unwrap();
    assert_eq!(d.estimated_input_tokens, 220 + 1000 + 2000 + 500);
    assert_eq!(d.tools, EnabledTools { file_search: false, web_search: true, code_interpreter: false });

    // a model without tool support gets no tools and no surcharges
    let d = preflight(&t.app, &{ let mut x = r.clone(); x.selected_model = NO_VISION.to_owned(); x }).await.unwrap();
    assert_eq!(d.estimated_input_tokens, 220 + 1000 + 2000);
    assert_eq!(d.tools, EnabledTools::default());
}

#[tokio::test]
async fn max_output_and_floor_follow_config() {
    let t = TestApp::with_config(|c| c.streaming.max_output_tokens = 1000).await;
    let d = preflight(&t.app, &req(STANDARD)).await.unwrap();
    assert_eq!(d.max_output_tokens_applied, 1000);
    assert_eq!(d.minimal_generation_floor_applied, 50);
    assert_eq!(d.reserve_tokens, 1220);
    assert_eq!(d.reserved_credits_micro, 220 + 3000);

    // the floor is capped by the applied max output (catalog max 1024 of the tiny model)
    let t = TestApp::with_config(|c| c.estimation_budgets.minimal_generation_floor = 2000).await;
    let d = preflight(&t.app, &req(TINY)).await.unwrap();
    assert_eq!(d.max_output_tokens_applied, 1024);
    assert_eq!(d.minimal_generation_floor_applied, 1024);
}

#[tokio::test]
async fn web_search_kill_switch_rejects_before_cascade() {
    let t = TestApp::new().await;
    let p = period_starts(crate::clock::now());
    put_row(&t.app, PERIOD_DAILY, p.daily, BUCKET_TOTAL, |r| r.spent_credits_micro = 100_000_000).await;
    t.policy.with_snapshot(|s| s.kill_switches.disable_web_search = true);
    let mut r = req(PREMIUM);
    r.tools.web_search_requested = true;
    match preflight(&t.app, &r).await.unwrap_err() {
        DomainError::FailedPrecondition { subject, violation_type, .. } => {
            assert_eq!(subject, "web_search");
            assert_eq!(violation_type, "FEATURE_DISABLED");
        }
        other => panic!("unexpected {other:?}"),
    }
    // without the web search request the quota rejection applies
    assert_quota_exceeded(&preflight(&t.app, &req(PREMIUM)).await.unwrap_err(), "tokens");
}

#[tokio::test]
async fn daily_web_search_quota_only_when_tool_sent() {
    let t = TestApp::new().await;
    let p = period_starts(crate::clock::now());
    put_row(&t.app, PERIOD_DAILY, p.daily, BUCKET_TOTAL, |r| r.web_search_calls = 75).await;
    let mut r = req(PREMIUM);
    r.tools.web_search_requested = true;
    assert_quota_exceeded(&preflight(&t.app, &r).await.unwrap_err(), "web_search");
    // not requested -> fine
    assert!(preflight(&t.app, &req(PREMIUM)).await.is_ok());
    // model without web search support -> tool not sent -> fine
    r.selected_model = NO_VISION.to_owned();
    let d = preflight(&t.app, &r).await.unwrap();
    assert!(!d.tools.web_search);
}

#[tokio::test]
async fn daily_web_search_quota_below_limit_passes() {
    let t = TestApp::new().await;
    let p = period_starts(crate::clock::now());
    put_row(&t.app, PERIOD_DAILY, p.daily, BUCKET_TOTAL, |r| r.web_search_calls = 74).await;
    let mut r = req(PREMIUM);
    r.tools.web_search_requested = true;
    assert!(preflight(&t.app, &r).await.unwrap().tools.web_search);
}

#[tokio::test]
async fn daily_code_interpreter_quota_only_when_tool_sent() {
    let t = TestApp::new().await;
    let p = period_starts(crate::clock::now());
    put_row(&t.app, PERIOD_DAILY, p.daily, BUCKET_TOTAL, |r| r.code_interpreter_calls = 50).await;
    let mut r = req(PREMIUM);
    r.tools.has_ready_code_interpreter = true;
    assert_quota_exceeded(&preflight(&t.app, &r).await.unwrap_err(), "code_interpreter");
    assert!(preflight(&t.app, &req(PREMIUM)).await.is_ok());
    t.policy.with_snapshot(|s| s.kill_switches.disable_code_interpreter = true);
    let d = preflight(&t.app, &r).await.unwrap();
    assert!(!d.tools.code_interpreter);
}

#[tokio::test]
async fn reserve_books_total_and_premium_rows() {
    let t = TestApp::new().await;
    let d = preflight(&t.app, &req(PREMIUM)).await.unwrap();
    book(&t.app, &d).await.unwrap();
    let p = d.periods;
    for (pt, start) in [(PERIOD_DAILY, p.daily), (PERIOD_MONTHLY, p.monthly)] {
        for bucket in [BUCKET_TOTAL, BUCKET_PREMIUM] {
            let row = get_row(&t.app, pt, start, bucket).await.unwrap();
            assert_eq!(row.reserved_credits_micro, PREMIUM_RESERVE, "{pt} {bucket}");
            assert_eq!(row.spent_credits_micro, 0);
            assert_eq!(row.calls, 0);
        }
    }
    // a second reserve accumulates on the same rows
    book(&t.app, &d).await.unwrap();
    let row = get_row(&t.app, PERIOD_DAILY, p.daily, BUCKET_TOTAL).await.unwrap();
    assert_eq!(row.reserved_credits_micro, 2 * PREMIUM_RESERVE);
    assert_eq!(row_count(&t.app).await, 4);
}

#[tokio::test]
async fn reserve_standard_books_total_only() {
    let t = TestApp::new().await;
    let d = preflight(&t.app, &req(STANDARD)).await.unwrap();
    book(&t.app, &d).await.unwrap();
    let p = d.periods;
    assert_eq!(get_row(&t.app, PERIOD_DAILY, p.daily, BUCKET_TOTAL).await.unwrap().reserved_credits_micro, STANDARD_RESERVE);
    assert_eq!(get_row(&t.app, PERIOD_MONTHLY, p.monthly, BUCKET_TOTAL).await.unwrap().reserved_credits_micro, STANDARD_RESERVE);
    assert!(get_row(&t.app, PERIOD_DAILY, p.daily, BUCKET_PREMIUM).await.is_none());
    assert_eq!(row_count(&t.app).await, 2);
}

#[tokio::test]
async fn reserve_recheck_rolls_back_second_reserve() {
    let t = TestApp::new().await;
    let p = period_starts(crate::clock::now());
    // 20_000 left: one standard reserve (12_508) fits, two do not.
    put_row(&t.app, PERIOD_DAILY, p.daily, BUCKET_TOTAL, |r| r.spent_credits_micro = 100_000_000 - 20_000).await;
    let first = preflight(&t.app, &req(STANDARD)).await.unwrap();
    let second = preflight(&t.app, &req(STANDARD)).await.unwrap(); // both pass the (read-only) preflight
    book(&t.app, &first).await.unwrap();
    let err = book(&t.app, &second).await.unwrap_err();
    assert_quota_exceeded(&err, "tokens");
    let daily = get_row(&t.app, PERIOD_DAILY, p.daily, BUCKET_TOTAL).await.unwrap();
    assert_eq!(daily.reserved_credits_micro, STANDARD_RESERVE, "second reserve rolled back");
    let monthly = get_row(&t.app, PERIOD_MONTHLY, p.monthly, BUCKET_TOTAL).await.unwrap();
    assert_eq!(monthly.reserved_credits_micro, STANDARD_RESERVE);
}

#[tokio::test]
async fn reserve_recheck_against_changed_limits() {
    let t = TestApp::new().await;
    let d = preflight(&t.app, &req(PREMIUM)).await.unwrap();
    // premium subcap shrinks between preflight and reserve: the re-check uses the decision limits,
    // so a reserve over the decision's own premium limit is rejected and nothing is written.
    let mut tight = d.clone();
    tight.limits.premium = TierLimits { limit_daily_credits_micro: 1000, limit_monthly_credits_micro: 500_000_000 };
    assert_quota_exceeded(&book(&t.app, &tight).await.unwrap_err(), "tokens");
    assert_eq!(row_count(&t.app).await, 0, "rows created in the failed transaction are rolled back");
    book(&t.app, &d).await.unwrap();
    assert_eq!(row_count(&t.app).await, 4);
}
