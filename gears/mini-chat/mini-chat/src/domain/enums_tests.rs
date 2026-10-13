#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn text_round_trip() {
    for v in MessageRole::ALL {
        assert_eq!(MessageRole::parse(v.as_str()), Some(*v));
    }
    for v in TurnState::ALL {
        assert_eq!(TurnState::parse(v.as_str()), Some(*v));
    }
    for v in QuotaBucket::ALL {
        assert_eq!(QuotaBucket::parse(v.as_str()), Some(*v));
    }
    assert_eq!(MessageRole::parse("robot"), None);
    assert_eq!(QuotaBucket::TierPremium.as_str(), "tier:premium");
    assert_eq!(SecondaryStatus::NotAttempted.to_string(), "not_attempted");
}

#[test]
fn terminal_states() {
    assert!(!TurnState::Running.is_terminal());
    assert!(TurnState::Completed.is_terminal());
    assert!(TurnState::Failed.is_terminal());
    assert!(TurnState::Cancelled.is_terminal());
}
