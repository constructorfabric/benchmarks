#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The labels of the mutation metrics, the classification of an error into a
//! `result` label and the mapping of a failed setup step to its `error_code`.

use super::{MutationOp, MutationResult, SetupFailureCode};
use crate::domain::error::{DisabledFeature, DomainError, QuotaSubject, ResourceKind};

#[test]
fn the_op_labels_are_retry_edit_and_delete() {
    assert_eq!(MutationOp::Retry.as_str(), "retry");
    assert_eq!(MutationOp::Edit.as_str(), "edit");
    assert_eq!(MutationOp::Delete.as_str(), "delete");
}

#[test]
fn the_result_labels_are_the_seven_of_the_feature() {
    let labels: Vec<&str> = [
        MutationResult::Ok,
        MutationResult::NotLatestTurn,
        MutationResult::GenerationInProgress,
        MutationResult::NotTerminal,
        MutationResult::Rejected,
        MutationResult::SetupFailed,
        MutationResult::Error,
    ]
    .iter()
    .map(|result| result.as_str())
    .collect();

    assert_eq!(
        labels,
        [
            "ok",
            "not_latest_turn",
            "generation_in_progress",
            "not_terminal",
            "rejected",
            "setup_failed",
            "error"
        ]
    );
}

#[test]
fn a_conflict_is_labelled_by_its_reason() {
    assert_eq!(
        MutationResult::before_commit(&DomainError::NotLatestTurn),
        MutationResult::NotLatestTurn
    );
    assert_eq!(
        MutationResult::before_commit(&DomainError::GenerationInProgress),
        MutationResult::GenerationInProgress
    );
    assert_eq!(
        MutationResult::before_commit(&DomainError::TurnNotTerminal),
        MutationResult::NotTerminal
    );
}

#[test]
fn every_other_refusal_before_the_commit_is_rejected() {
    for error in [
        DomainError::NotFound {
            resource: ResourceKind::Chat,
        },
        DomainError::NotFound {
            resource: ResourceKind::Turn,
        },
        DomainError::AuthzDenied,
        DomainError::PdpUnavailable,
        DomainError::EmptyContent,
        DomainError::InvalidModel,
        DomainError::TooManyImages,
        DomainError::InputTooLong,
        DomainError::VisionNotSupported,
        DomainError::FeatureDisabled(DisabledFeature::WebSearch),
        DomainError::FeatureDisabled(DisabledFeature::Images),
        DomainError::QuotaExceeded(QuotaSubject::Tokens),
        DomainError::QuotaExceeded(QuotaSubject::WebSearch),
    ] {
        assert_eq!(
            MutationResult::before_commit(&error),
            MutationResult::Rejected,
            "{error:?}"
        );
    }
}

#[test]
fn a_server_failure_before_the_commit_is_an_error() {
    for error in [
        DomainError::Database("down".to_owned()),
        DomainError::Internal("bug".to_owned()),
        DomainError::PolicyResolution("plugin".to_owned()),
        DomainError::UniqueViolation,
    ] {
        assert_eq!(
            MutationResult::before_commit(&error),
            MutationResult::Error,
            "{error:?}"
        );
    }
}

#[test]
fn the_setup_failure_codes_are_the_three_of_the_error_code_list() {
    assert_eq!(
        SetupFailureCode::ContextLengthExceeded.as_str(),
        "context_length_exceeded"
    );
    assert_eq!(
        SetupFailureCode::TurnSetupFailed.as_str(),
        "turn_setup_failed"
    );
    assert_eq!(SetupFailureCode::QuotaExceeded.as_str(), "quota_exceeded");
}

#[test]
fn a_context_budget_failure_is_context_length_exceeded() {
    assert_eq!(
        SetupFailureCode::of_error(&DomainError::ContextBudgetExceeded),
        SetupFailureCode::ContextLengthExceeded
    );
}

#[test]
fn a_reserve_recheck_failure_is_quota_exceeded() {
    assert_eq!(
        SetupFailureCode::of_error(&DomainError::QuotaExceeded(QuotaSubject::Tokens)),
        SetupFailureCode::QuotaExceeded
    );
}

#[test]
fn every_other_setup_failure_is_turn_setup_failed() {
    for error in [
        DomainError::Database("history".to_owned()),
        DomainError::Internal("build".to_owned()),
        DomainError::ProviderResolution("no provider".to_owned()),
        DomainError::PolicyResolution("plugin".to_owned()),
    ] {
        assert_eq!(
            SetupFailureCode::of_error(&error),
            SetupFailureCode::TurnSetupFailed,
            "{error:?}"
        );
    }
}
