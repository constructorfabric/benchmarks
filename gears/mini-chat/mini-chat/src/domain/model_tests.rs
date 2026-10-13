#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn turn_state_strings_and_api_state() {
    let all = [
        (TurnState::Running, "running", "running"),
        (TurnState::Completed, "completed", "done"),
        (TurnState::Failed, "failed", "error"),
        (TurnState::Cancelled, "cancelled", "cancelled"),
    ];
    for (s, db, api) in all {
        assert_eq!(s.as_str(), db);
        assert_eq!(s.api_state(), api);
        assert_eq!(TurnState::parse(db), Some(s));
    }
    assert_eq!(TurnState::parse("done"), None);
    assert!(!TurnState::Running.is_terminal());
    assert!(TurnState::Cancelled.is_terminal());
}

#[test]
fn enums_round_trip() {
    for v in ["user", "assistant", "system"] {
        assert_eq!(MessageRole::parse(v).unwrap().as_str(), v);
    }
    for v in ["like", "dislike"] {
        assert_eq!(Reaction::parse(v).unwrap().as_str(), v);
    }
    assert_eq!(Reaction::parse("LIKE"), None);
    for v in ["document", "image"] {
        assert_eq!(AttachmentKind::parse(v).unwrap().as_str(), v);
    }
    for v in ["pending", "uploaded", "ready", "failed"] {
        assert_eq!(AttachmentStatus::parse(v).unwrap().as_str(), v);
    }
    for v in ["allow", "downgrade"] {
        assert_eq!(QuotaDecision::parse(v).unwrap().as_str(), v);
    }
    for v in [
        "premium_quota_exhausted",
        "force_standard_tier",
        "disable_premium_tier",
        "model_disabled",
    ] {
        assert_eq!(DowngradeReason::parse(v).unwrap().as_str(), v);
    }
    for v in ["completed", "failed", "aborted", "system_task"] {
        assert_eq!(BillingOutcome::parse(v).unwrap().as_str(), v);
    }
    for v in ["actual", "estimated", "released", "none"] {
        assert_eq!(SettlementMethod::parse(v).unwrap().as_str(), v);
    }
    for v in ["tokens", "web_search", "code_interpreter"] {
        assert_eq!(QuotaScope::parse(v).unwrap().as_str(), v);
    }
    for v in ["daily", "monthly"] {
        assert_eq!(PeriodType::parse(v).unwrap().as_str(), v);
    }
    for v in ["total", "tier:premium"] {
        assert_eq!(Bucket::parse(v).unwrap().as_str(), v);
    }
    assert_eq!(Bucket::parse("tier:standard"), None);
}
