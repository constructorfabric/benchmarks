//! The send pipeline rejections as canonical Problems: category, status, machine-readable reason
//! per row of the DESIGN error table, the generic 409 detail and the absence of a top-level
//! `code`.
//!
//! Slice S1-setup-path of `cpt-cf-mini-chat-feature-message-streaming`: algorithm
//! `cpt-cf-mini-chat-algo-message-streaming-send-error-contract`; definition of done
//! `cpt-cf-mini-chat-dod-message-streaming-send-preflight` and
//! `cpt-cf-mini-chat-dod-message-streaming-error-sanitization` (the pre-stream Problems).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::response::IntoResponse;
use serde_json::Value;
use toolkit_canonical_errors::{CanonicalError, Problem};

use crate::domain::error::{ConflictReason, DomainError};

fn problem(err: DomainError) -> Value {
    serde_json::to_value(Problem::from(CanonicalError::from(err))).expect("problem serializes")
}

fn field_violation(value: &Value) -> (String, String) {
    let violations = value["context"]["field_violations"]
        .as_array()
        .unwrap_or_else(|| {
            panic!("no field_violations in {value}");
        });
    assert_eq!(violations.len(), 1, "{value}");
    (
        violations[0]["field"].as_str().unwrap().to_owned(),
        violations[0]["reason"].as_str().unwrap().to_owned(),
    )
}

/// `(error, status, category in the type, field, reason)` of every field-violation row.
fn field_rows() -> Vec<(DomainError, u64, &'static str, &'static str, &'static str)> {
    vec![
        (
            DomainError::EmptyContent,
            400,
            "invalid_argument",
            "content",
            "EMPTY_CONTENT",
        ),
        (
            DomainError::InvalidAttachment,
            400,
            "invalid_argument",
            "attachment",
            "invalid_attachment",
        ),
        (
            DomainError::TooManyImages,
            400,
            "out_of_range",
            "image_count",
            "TOO_MANY_IMAGES",
        ),
        (
            DomainError::VisionNotSupported,
            400,
            "invalid_argument",
            "content_type",
            "VISION_NOT_SUPPORTED",
        ),
        (
            DomainError::InputTooLong,
            400,
            "out_of_range",
            "content",
            "INPUT_TOO_LONG",
        ),
        (
            DomainError::ContextBudgetExceeded,
            400,
            "out_of_range",
            "context",
            "CONTEXT_BUDGET_EXCEEDED",
        ),
    ]
}

#[test]
fn each_content_attachment_and_budget_rejection_is_a_400_with_its_field_and_reason() {
    for (error, status, category, field, reason) in field_rows() {
        let label = format!("{error}");
        let value = problem(error);
        assert_eq!(value["status"], status, "{label}: {value}");
        assert!(
            value["type"].as_str().unwrap().contains(category),
            "{label}: {value}"
        );
        assert_eq!(
            field_violation(&value),
            (field.to_owned(), reason.to_owned()),
            "{label}"
        );
    }
}

#[test]
fn every_rejection_is_a_problem_with_the_contract_fields_and_no_top_level_code() {
    let mut errors: Vec<DomainError> = field_rows().into_iter().map(|row| row.0).collect();
    errors.push(DomainError::Conflict {
        reason: ConflictReason::RequestIdConflict,
    });
    errors.push(DomainError::Conflict {
        reason: ConflictReason::TurnAlreadyRunning,
    });
    for error in errors {
        let label = format!("{error}");
        let value = problem(error);
        for key in ["type", "title", "status", "detail", "context"] {
            assert!(
                value.get(key).is_some(),
                "{label}: missing `{key}` in {value}"
            );
        }
        assert!(
            value.get("code").is_none(),
            "{label}: top-level code in {value}"
        );
        assert!(
            value["type"].as_str().unwrap().starts_with("gts://"),
            "{label}"
        );
    }
}

#[test]
fn the_rejections_are_served_as_problem_json_never_as_a_stream() {
    for error in [
        DomainError::EmptyContent,
        DomainError::TooManyImages,
        DomainError::Conflict {
            reason: ConflictReason::TurnAlreadyRunning,
        },
    ] {
        let response = CanonicalError::from(error).into_response();
        let content_type = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(content_type, "application/problem+json");
    }
}

#[test]
fn a_conflict_is_a_409_aborted_with_its_reason_in_the_context() {
    for (reason, wire) in [
        (ConflictReason::RequestIdConflict, "request_id_conflict"),
        (ConflictReason::TurnAlreadyRunning, "turn_already_running"),
    ] {
        let value = problem(DomainError::Conflict { reason });
        assert_eq!(value["status"], 409, "{value}");
        assert!(
            value["type"].as_str().unwrap().contains("aborted"),
            "{value}"
        );
        assert_eq!(value["context"]["reason"], wire, "{value}");
    }
}

#[test]
fn the_two_conflicts_are_told_apart_by_the_reason_alone() {
    let a = problem(DomainError::Conflict {
        reason: ConflictReason::RequestIdConflict,
    });
    let b = problem(DomainError::Conflict {
        reason: ConflictReason::TurnAlreadyRunning,
    });
    assert_ne!(a["context"]["reason"], b["context"]["reason"]);
    assert_eq!(a["status"], b["status"]);
}

#[test]
fn the_conflict_detail_is_generic_text_without_identifiers() {
    for reason in [
        ConflictReason::RequestIdConflict,
        ConflictReason::TurnAlreadyRunning,
    ] {
        let value = problem(DomainError::Conflict { reason });
        let detail = value["detail"].as_str().unwrap();
        assert!(!detail.is_empty());
        assert!(
            !detail.chars().any(|c| c == '-') || !detail.contains(|c: char| c.is_ascii_digit()),
            "the detail looks like it carries an identifier: {detail}"
        );
        let lowered = detail.to_lowercase();
        for leak in ["sqlite", "unique", "constraint", "chat_turns", "uuid"] {
            assert!(!lowered.contains(leak), "{leak} in {detail}");
        }
    }
}

#[test]
fn the_feature_disabled_rows_carry_the_subject_and_the_feature_disabled_type() {
    for subject in ["web_search", "images"] {
        let value = problem(DomainError::FeatureDisabled { subject });
        assert_eq!(value["status"], 400);
        assert!(
            value["type"]
                .as_str()
                .unwrap()
                .contains("failed_precondition"),
            "{value}"
        );
        let violations = value["context"]["violations"].as_array().unwrap();
        assert_eq!(violations[0]["subject"], subject);
        assert_eq!(violations[0]["type"], "FEATURE_DISABLED");
    }
}
