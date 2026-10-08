#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::domain::model::QuotaScope;

#[test]
fn setup_failure_codes_follow_the_unstarted_turn_contract() {
    assert_eq!(
        setup_failure_code(&DomainError::ContextBudgetExceeded),
        "context_length_exceeded"
    );
    assert_eq!(
        setup_failure_code(&DomainError::QuotaExceeded {
            scope: QuotaScope::Tokens
        }),
        "quota_exceeded"
    );
    assert_eq!(
        setup_failure_code(&DomainError::internal("provider resolution")),
        "turn_setup_failed"
    );
    assert_eq!(
        setup_failure_code(&DomainError::TurnAlreadyRunning),
        "turn_setup_failed"
    );
}
