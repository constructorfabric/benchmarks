#![allow(clippy::unwrap_used)]

use toolkit_canonical_errors::{CanonicalError, Problem};
use uuid::Uuid;

use crate::domain::error::{DisabledFeature, DomainError, MultipartFailure, QuotaScope};

fn problem(err: DomainError) -> serde_json::Value {
    let canonical: CanonicalError = err.into();
    serde_json::to_value(Problem::from(canonical)).unwrap()
}

fn field_reason(p: &serde_json::Value) -> (&str, &str) {
    let v = &p["context"]["field_violations"][0];
    (v["field"].as_str().unwrap(), v["reason"].as_str().unwrap())
}

#[test]
fn not_found_reports_resource_type() {
    let p = problem(DomainError::ChatNotFound { id: Uuid::nil() });
    assert_eq!(p["status"], 404);
    assert_eq!(
        p["context"]["resource_type"],
        "gts.cf.core.mini_chat.chat.v1~"
    );
    let p = problem(DomainError::AttachmentNotFound { id: Uuid::nil() });
    assert_eq!(
        p["context"]["resource_type"],
        "gts.cf.core.mini_chat.attachment.v1~"
    );
    let p = problem(DomainError::TurnNotFound {
        request_id: Uuid::nil(),
    });
    assert_eq!(
        p["context"]["resource_type"],
        "gts.cf.core.mini_chat.turn.v1~"
    );
    let p = problem(DomainError::MessageNotFound { id: Uuid::nil() });
    assert_eq!(
        p["context"]["resource_type"],
        "gts.cf.core.mini_chat.message.v1~"
    );
    let p = problem(DomainError::ModelNotFound { id: "m".into() });
    assert_eq!(
        p["context"]["resource_type"],
        "gts.cf.core.mini_chat.model.v1~"
    );
}

#[test]
fn invalid_argument_reasons() {
    for (err, field, reason) in [
        (DomainError::invalid_model("x"), "model", "INVALID_MODEL"),
        (DomainError::InvalidTitle, "title", "INVALID_TITLE"),
        (DomainError::EmptyContent, "content", "EMPTY_CONTENT"),
        (DomainError::InvalidReaction, "reaction", "INVALID_REACTION"),
        (
            DomainError::invalid_attachment("x"),
            "attachment",
            "invalid_attachment",
        ),
        (
            DomainError::UnsupportedContentType {
                content_type: "x".into(),
            },
            "content_type",
            "UNSUPPORTED_CONTENT_TYPE",
        ),
        (
            DomainError::CodeInterpreterUnavailable,
            "file",
            "CODE_INTERPRETER_UNAVAILABLE",
        ),
        (
            DomainError::VisionNotSupported,
            "content_type",
            "VISION_NOT_SUPPORTED",
        ),
        (
            DomainError::Multipart {
                failure: MultipartFailure::BoundaryRequired,
                detail: "x".into(),
            },
            "content_type",
            "BOUNDARY_REQUIRED",
        ),
        (
            DomainError::Multipart {
                failure: MultipartFailure::MissingFile,
                detail: "x".into(),
            },
            "file",
            "MISSING_FILE",
        ),
    ] {
        let p = problem(err);
        assert_eq!(p["status"], 400);
        assert_eq!(field_reason(&p), (field, reason));
    }
}

#[test]
fn out_of_range_reasons() {
    for (err, field, reason) in [
        (
            DomainError::FileTooLarge { limit_bytes: 1 },
            "content_length",
            "FILE_TOO_LARGE",
        ),
        (
            DomainError::TooManyImages { max: 4 },
            "image_count",
            "TOO_MANY_IMAGES",
        ),
        (
            DomainError::InputTooLong { limit: 10 },
            "content",
            "INPUT_TOO_LONG",
        ),
        (
            DomainError::ContextBudgetExceeded,
            "content",
            "CONTEXT_BUDGET_EXCEEDED",
        ),
    ] {
        let p = problem(err);
        assert_eq!(p["status"], 400);
        assert!(p["type"].as_str().unwrap().contains("out_of_range"));
        assert_eq!(field_reason(&p), (field, reason));
    }
}

#[test]
fn failed_precondition_violations() {
    let p = problem(DomainError::FeatureDisabled {
        feature: DisabledFeature::WebSearch,
    });
    assert_eq!(p["status"], 400);
    assert_eq!(p["context"]["violations"][0]["subject"], "web_search");
    assert_eq!(p["context"]["violations"][0]["type"], "FEATURE_DISABLED");
    let p = problem(DomainError::FeatureDisabled {
        feature: DisabledFeature::Images,
    });
    assert_eq!(p["context"]["violations"][0]["subject"], "images");
    let p = problem(DomainError::TurnNotTerminal);
    assert_eq!(p["context"]["violations"][0]["subject"], "turn_state");
    assert_eq!(p["context"]["violations"][0]["type"], "STATE");
    let p = problem(DomainError::ReactionTargetNotAssistant);
    assert_eq!(p["context"]["violations"][0]["subject"], "reaction_target");
}

#[test]
fn aborted_and_conflict_reasons() {
    for (err, reason) in [
        (DomainError::TurnAlreadyRunning, "turn_already_running"),
        (
            DomainError::RequestIdConflict {
                detail: "internal turn ids".into(),
            },
            "request_id_conflict",
        ),
        (DomainError::NotLatestTurn, "NOT_LATEST_TURN"),
        (DomainError::GenerationInProgress, "GENERATION_IN_PROGRESS"),
        (DomainError::Replay, "REPLAY"),
    ] {
        let p = problem(err);
        assert_eq!(p["status"], 409);
        assert_eq!(p["context"]["reason"], reason);
        assert!(!p["detail"].as_str().unwrap().contains("internal turn ids"));
    }
    for (err, name) in [
        (DomainError::AttachmentLocked, "attachment_locked"),
        (DomainError::ProviderMismatch, "provider_mismatch"),
        (
            DomainError::UniqueViolation {
                detail: "UNIQUE constraint failed".into(),
            },
            "unique_violation",
        ),
    ] {
        let p = problem(err);
        assert_eq!(p["status"], 409);
        assert_eq!(p["context"]["resource_name"], name);
        assert!(!p["detail"].as_str().unwrap().contains("UNIQUE"));
    }
}

#[test]
fn quota_and_limits_are_resource_exhausted() {
    for (scope, subject) in [
        (QuotaScope::Tokens, "tokens"),
        (QuotaScope::WebSearch, "web_search"),
        (QuotaScope::CodeInterpreter, "code_interpreter"),
    ] {
        let p = problem(DomainError::QuotaExceeded { scope });
        assert_eq!(p["status"], 429);
        assert_eq!(p["context"]["violations"][0]["subject"], subject);
        assert_eq!(
            p["context"]["violations"][0]["description"],
            "quota_exceeded"
        );
    }
    let p = problem(DomainError::DocumentLimit);
    assert_eq!(p["status"], 429);
    assert_eq!(p["context"]["violations"][0]["subject"], "document_limit");
    let p = problem(DomainError::StorageLimit);
    assert_eq!(p["context"]["violations"][0]["subject"], "storage_limit");
}

#[test]
fn authorization_and_availability() {
    let p = problem(DomainError::AccessDenied);
    assert_eq!(p["status"], 403);
    assert_eq!(p["context"]["reason"], "AUTHZ_DENIED");
    let p = problem(DomainError::AuthzUnavailable {
        detail: "pdp down".into(),
    });
    assert_eq!(p["status"], 503);
    assert_eq!(p["context"]["retry_after_seconds"], 5);
    assert!(!p["detail"].as_str().unwrap().contains("pdp"));
    let p = problem(DomainError::StorageUnavailable {
        detail: "file-abc".into(),
    });
    assert_eq!(p["status"], 503);
    assert_eq!(p["context"]["retry_after_seconds"], 10);
    let p = problem(DomainError::UploadConcurrencyLimit);
    assert_eq!(p["context"]["retry_after_seconds"], 5);
    let p = problem(DomainError::internal("db exploded at resp_123"));
    assert_eq!(p["status"], 500);
    assert!(!p["detail"].as_str().unwrap().contains("resp_123"));
}
