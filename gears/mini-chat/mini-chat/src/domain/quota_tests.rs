use super::*;
use mini_chat_sdk::{ModelPreference, UserLimits};
use time::macros::datetime;

fn model(id: &str, tier: ModelTier, enabled: bool, default: bool) -> ModelCatalogEntry {
    serde_json::from_value(serde_json::json!({
        "id": id, "tier": tier, "enabled": enabled,
        "max_output_tokens": 1000,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 1_000_000,
        "preference": if default { Some(ModelPreference { is_default: true, sort_order: 0 }) } else { None },
    }))
    .unwrap()
}

fn snap(models: Vec<ModelCatalogEntry>) -> PolicySnapshot {
    PolicySnapshot { policy_version: 1, model_catalog: models, kill_switches: KillSwitches::default() }
}

fn limits() -> UserLimits {
    UserLimits {
        user_id: uuid::Uuid::nil(),
        policy_version: 1,
        standard: TierLimits { limit_daily_credits_micro: 100_000, limit_monthly_credits_micro: 1_000_000 },
        premium: TierLimits { limit_daily_credits_micro: 50_000, limit_monthly_credits_micro: 500_000 },
    }
}

fn facts() -> RequestFacts {
    RequestFacts { message: "hello".into(), cfg_max_output_tokens: 32768, ..RequestFacts::default() }
}

#[test]
fn allow_when_quota_available() {
    let s = snap(vec![model("p", ModelTier::Premium, true, true), model("s", ModelTier::Standard, true, false)]);
    let d = resolve_effective_model("p", &s, &limits(), &UsageSnapshot::default(), &facts()).unwrap();
    assert_eq!(d.effective.id, "p");
    assert_eq!(d.quota_decision(), "allow");
    assert!(d.downgrade_reason.is_none());
}

#[test]
fn downgrade_when_premium_exhausted() {
    let s = snap(vec![model("p", ModelTier::Premium, true, true), model("s", ModelTier::Standard, true, false)]);
    let mut u = UsageSnapshot::default();
    u.premium_daily.spent = 50_000;
    let d = resolve_effective_model("p", &s, &limits(), &u, &facts()).unwrap();
    assert_eq!(d.effective.id, "s");
    assert_eq!(d.downgrade_reason.as_deref(), Some("premium_quota_exhausted"));
}

#[test]
fn reject_when_all_exhausted() {
    let s = snap(vec![model("p", ModelTier::Premium, true, true), model("s", ModelTier::Standard, true, false)]);
    let mut u = UsageSnapshot::default();
    u.total_monthly.spent = 1_000_000;
    assert!(resolve_effective_model("p", &s, &limits(), &u, &facts()).is_none());
}

#[test]
fn standard_never_upgrades() {
    let s = snap(vec![model("p", ModelTier::Premium, true, true), model("s", ModelTier::Standard, true, false)]);
    let mut u = UsageSnapshot::default();
    u.total_daily.spent = 100_000;
    assert!(resolve_effective_model("s", &s, &limits(), &u, &facts()).is_none());
}

#[test]
fn kill_switches_and_disabled_model() {
    let mut s = snap(vec![model("p", ModelTier::Premium, true, true), model("s", ModelTier::Standard, true, false)]);
    s.kill_switches.force_standard_tier = true;
    let d = resolve_effective_model("p", &s, &limits(), &UsageSnapshot::default(), &facts()).unwrap();
    assert_eq!(d.downgrade_reason.as_deref(), Some("force_standard_tier"));
    s.kill_switches.force_standard_tier = false;
    s.kill_switches.disable_premium_tier = true;
    let d = resolve_effective_model("p", &s, &limits(), &UsageSnapshot::default(), &facts()).unwrap();
    assert_eq!(d.downgrade_reason.as_deref(), Some("disable_premium_tier"));

    let s = snap(vec![model("off", ModelTier::Standard, false, false), model("s", ModelTier::Standard, true, false)]);
    let d = resolve_effective_model("off", &s, &limits(), &UsageSnapshot::default(), &facts()).unwrap();
    assert_eq!(d.effective.id, "s");
    assert_eq!(d.downgrade_reason.as_deref(), Some("model_disabled"));
    let d = resolve_effective_model("missing", &s, &limits(), &UsageSnapshot::default(), &facts()).unwrap();
    assert_eq!(d.downgrade_reason.as_deref(), Some("model_disabled"));
}

#[test]
fn reserve_must_fit_not_only_remaining() {
    let s = snap(vec![model("s", ModelTier::Standard, true, false)]);
    let mut u = UsageSnapshot::default();
    // reserve ~ (110+... ) + 1000 output tokens = > 1000 credits; leave 500 remaining
    u.total_daily.spent = 99_500;
    assert!(resolve_effective_model("s", &s, &limits(), &u, &facts()).is_none());
}

#[test]
fn status_and_warnings() {
    let mut u = UsageSnapshot::default();
    u.total_daily.spent = 85_000;
    u.premium_daily.spent = 50_000;
    let now = datetime!(2026-02-28 15:30 UTC);
    let st = quota_status(&u, &limits(), 80, now);
    assert_eq!(st.len(), 4);
    let td = st.iter().find(|p| p.tier == "total" && p.period == "daily").unwrap();
    assert_eq!(td.remaining_percentage, 15);
    assert!(td.warning && !td.exhausted);
    assert_eq!(td.next_reset, datetime!(2026-03-01 0:00 UTC));
    let pd = st.iter().find(|p| p.tier == "premium" && p.period == "daily").unwrap();
    assert!(pd.exhausted);
    let tm = st.iter().find(|p| p.tier == "total" && p.period == "monthly").unwrap();
    assert!(!tm.warning);
    assert_eq!(tm.next_reset, datetime!(2026-03-01 0:00 UTC));
}

#[test]
fn periods_utc() {
    let p = Periods::at(datetime!(2026-02-28 23:59:59 UTC));
    assert_eq!(p.daily.to_string(), "2026-02-28");
    assert_eq!(p.monthly.to_string(), "2026-02-01");
    assert_eq!(next_reset(PERIOD_MONTHLY, datetime!(2026-12-15 1:00 UTC)), datetime!(2027-01-01 0:00 UTC));
}
