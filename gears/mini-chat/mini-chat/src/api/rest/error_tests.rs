//! Pins category, status and machine-readable reason of the REST error
//! mapping (ADR-0004 confirmation).

use serde_json::Value;
use toolkit_canonical_errors::Problem;

use super::{to_canonical, to_canonical_strict};
use crate::domain::error::{DomainError, FeatureSubject, QuotaScope};

fn problem(e: DomainError) -> (u16, Value) {
    let c = to_canonical(e);
    let status = c.status_code();
    (status, serde_json::to_value(Problem::from(c)).expect("problem json"))
}

fn category(v: &Value) -> String {
    v["type"]
        .as_str()
        .unwrap()
        .trim_start_matches("gts://gts.cf.core.errors.err.v1~cf.core.err.")
        .trim_end_matches(".v1~")
        .to_owned()
}

#[test]
fn not_found_resource_types() {
    let cases = [
        (DomainError::ChatNotFound, "chat"),
        (DomainError::MessageNotFound, "message"),
        (DomainError::TurnNotFound, "turn"),
        (DomainError::AttachmentNotFound, "attachment"),
        (DomainError::ModelNotFound, "model"),
    ];
    for (e, res) in cases {
        let (s, v) = problem(e);
        assert_eq!(s, 404);
        assert_eq!(category(&v), "not_found");
        assert_eq!(v["context"]["resource_type"], format!("gts.cf.core.mini_chat.{res}.v1~"));
    }
}

#[test]
fn field_violation_reasons() {
    let cases = [
        (DomainError::InvalidModel, 400, "invalid_argument", "model", "INVALID_MODEL"),
        (DomainError::InvalidTitle, 400, "invalid_argument", "title", "INVALID_TITLE"),
        (DomainError::EmptyContent, 400, "invalid_argument", "content", "EMPTY_CONTENT"),
        (DomainError::InvalidReaction, 400, "invalid_argument", "reaction", "INVALID_REACTION"),
        (
            DomainError::InvalidAttachment("dup".to_owned()),
            400,
            "invalid_argument",
            "attachment",
            "invalid_attachment",
        ),
        (DomainError::TooManyImages, 400, "out_of_range", "image_count", "TOO_MANY_IMAGES"),
        (
            DomainError::VisionNotSupported,
            400,
            "invalid_argument",
            "content_type",
            "VISION_NOT_SUPPORTED",
        ),
        (DomainError::InputTooLong, 400, "out_of_range", "content", "INPUT_TOO_LONG"),
        (
            DomainError::ContextBudgetExceeded,
            400,
            "out_of_range",
            "content",
            "CONTEXT_BUDGET_EXCEEDED",
        ),
        (DomainError::FileTooLarge, 400, "out_of_range", "content_length", "FILE_TOO_LARGE"),
        (
            DomainError::UnsupportedContentType,
            400,
            "invalid_argument",
            "content_type",
            "UNSUPPORTED_CONTENT_TYPE",
        ),
        (
            DomainError::CodeInterpreterUnavailable,
            400,
            "invalid_argument",
            "file",
            "CODE_INTERPRETER_UNAVAILABLE",
        ),
    ];
    for (e, status, cat, field, reason) in cases {
        let (s, v) = problem(e);
        assert_eq!(s, status, "{reason}");
        assert_eq!(category(&v), cat, "{reason}");
        assert_eq!(v["context"]["field_violations"][0]["field"], field, "{reason}");
        assert_eq!(v["context"]["field_violations"][0]["reason"], reason);
    }
}

#[test]
fn preconditions_and_aborts() {
    let (s, v) = problem(DomainError::FeatureDisabled(FeatureSubject::WebSearch));
    assert_eq!((s, category(&v).as_str()), (400, "failed_precondition"));
    assert_eq!(v["context"]["violations"][0]["subject"], "web_search");
    assert_eq!(v["context"]["violations"][0]["type"], "FEATURE_DISABLED");
    let (_, v) = problem(DomainError::FeatureDisabled(FeatureSubject::Images));
    assert_eq!(v["context"]["violations"][0]["subject"], "images");
    let (_, v) = problem(DomainError::TurnNotTerminal);
    assert_eq!(v["context"]["violations"][0]["subject"], "turn_state");
    assert_eq!(v["context"]["violations"][0]["type"], "STATE");
    let (_, v) = problem(DomainError::ReactionTargetNotAssistant);
    assert_eq!(v["context"]["violations"][0]["subject"], "reaction_target");

    for (e, reason) in [
        (DomainError::TurnAlreadyRunning, "turn_already_running"),
        (DomainError::RequestIdConflict, "request_id_conflict"),
        (DomainError::NotLatestTurn, "NOT_LATEST_TURN"),
        (DomainError::GenerationInProgress, "GENERATION_IN_PROGRESS"),
        (DomainError::Replay, "REPLAY"),
    ] {
        let (s, v) = problem(e);
        assert_eq!((s, category(&v).as_str()), (409, "aborted"), "{reason}");
        assert_eq!(v["context"]["reason"], reason);
    }
    for (e, name) in [
        (DomainError::AttachmentLocked, "attachment_locked"),
        (DomainError::ProviderMismatch, "provider_mismatch"),
        (DomainError::UniqueViolation, "unique_violation"),
    ] {
        let (s, v) = problem(e);
        assert_eq!((s, category(&v).as_str()), (409, "already_exists"), "{name}");
        assert_eq!(v["context"]["resource_name"], name);
    }
}

#[test]
fn quota_authz_and_availability() {
    for (scope, subject) in [
        (QuotaScope::Tokens, "tokens"),
        (QuotaScope::WebSearch, "web_search"),
        (QuotaScope::CodeInterpreter, "code_interpreter"),
    ] {
        let (s, v) = problem(DomainError::QuotaExceeded(scope));
        assert_eq!((s, category(&v).as_str()), (429, "resource_exhausted"));
        assert_eq!(v["context"]["violations"][0]["subject"], subject);
        assert_eq!(v["context"]["violations"][0]["description"], "quota_exceeded");
    }
    for (e, subject) in [(DomainError::DocumentLimit, "document_limit"), (DomainError::StorageLimit, "storage_limit")] {
        let (s, v) = problem(e);
        assert_eq!(s, 429);
        assert_eq!(v["context"]["violations"][0]["subject"], subject);
    }
    let (s, v) = problem(DomainError::AccessDenied);
    assert_eq!((s, category(&v).as_str()), (403, "permission_denied"));
    assert_eq!(v["context"]["reason"], "AUTHZ_DENIED");
    for (e, retry) in [
        (DomainError::AuthzUnavailable, 5),
        (DomainError::UploadConcurrency, 5),
        (DomainError::StorageUnavailable("x".to_owned()), 10),
    ] {
        let (s, v) = problem(e);
        assert_eq!((s, category(&v).as_str()), (503, "service_unavailable"));
        assert_eq!(v["context"]["retry_after_seconds"], retry);
    }
    let (s, v) = problem(DomainError::Internal("secret details".to_owned()));
    assert_eq!(s, 500);
    assert!(!v.to_string().contains("secret details"));
}

#[test]
fn multipart_and_outbox_payload_errors() {
    let (s, v) = problem(DomainError::Multipart {
        field: "file",
        reason: "MISSING_FILE",
        detail: "no file".to_owned(),
    });
    assert_eq!(s, 400);
    assert_eq!(v["context"]["field_violations"][0]["field"], "file");
    assert_eq!(v["context"]["field_violations"][0]["reason"], "MISSING_FILE");
    // chat delete: 400 with the message in context.format; other paths: 500
    let (s, v) = problem(DomainError::OutboxPayloadTooLarge("payload too large".to_owned()));
    assert_eq!(s, 400);
    assert_eq!(v["context"]["format"], "payload too large");
    let strict = to_canonical_strict(DomainError::OutboxPayloadTooLarge("x".to_owned()));
    assert_eq!(strict.status_code(), 500);
}
