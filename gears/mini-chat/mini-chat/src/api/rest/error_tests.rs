use toolkit_canonical_errors::{CanonicalError, Problem};
use uuid::Uuid;

use super::*;
use crate::domain::error::DomainError;

fn problem(e: DomainError) -> (u16, serde_json::Value) {
    let ce: CanonicalError = e.into();
    let status = ce.status_code();
    let p = Problem::from(ce);
    (status, serde_json::to_value(&p).unwrap())
}

fn field_reason(v: &serde_json::Value) -> (String, String) {
    let fv = &v["context"]["field_violations"][0];
    (fv["field"].as_str().unwrap().to_owned(), fv["reason"].as_str().unwrap().to_owned())
}

#[test]
fn not_found_resource_types() {
    let cases = [
        (DomainError::ChatNotFound(Uuid::nil()), CHAT_RESOURCE_TYPE),
        (DomainError::MessageNotFound(Uuid::nil()), MESSAGE_RESOURCE_TYPE),
        (DomainError::TurnNotFound(Uuid::nil()), TURN_RESOURCE_TYPE),
        (DomainError::AttachmentNotFound(Uuid::nil()), ATTACHMENT_RESOURCE_TYPE),
        (DomainError::ModelNotFound("m".into()), MODEL_RESOURCE_TYPE),
    ];
    for (e, rt) in cases {
        let (status, v) = problem(e);
        assert_eq!(status, 404);
        assert_eq!(v["context"]["resource_type"], rt);
        assert!(v["type"].as_str().unwrap().ends_with("not_found.v1~"));
        assert!(v.get("code").is_none(), "no top-level code field");
    }
}

#[test]
fn invalid_argument_reasons() {
    let cases: Vec<(DomainError, u16, &str, &str)> = vec![
        (DomainError::InvalidModel("x".into()), 400, "model", "INVALID_MODEL"),
        (DomainError::EmptyContent, 400, "content", "EMPTY_CONTENT"),
        (DomainError::InvalidTitle("t".into()), 400, "title", "INVALID_TITLE"),
        (DomainError::InvalidReaction("r".into()), 400, "reaction", "INVALID_REACTION"),
        (DomainError::InvalidAttachment("a".into()), 400, "attachment", "invalid_attachment"),
        (DomainError::VisionNotSupported("m".into()), 400, "content_type", "VISION_NOT_SUPPORTED"),
        (DomainError::CodeInterpreterUnavailable, 400, "file", "CODE_INTERPRETER_UNAVAILABLE"),
        (DomainError::FileTooLarge { limit_bytes: 1 }, 400, "content_length", "FILE_TOO_LARGE"),
        (DomainError::TooManyImages { count: 5, max: 4 }, 400, "image_count", "TOO_MANY_IMAGES"),
        (DomainError::InputTooLong { estimated: 9, max: 1 }, 400, "content", "INPUT_TOO_LONG"),
        (DomainError::ContextBudgetExceeded("x".into()), 400, "content", "CONTEXT_BUDGET_EXCEEDED"),
        (
            DomainError::Multipart { field: "content_type", reason: "BOUNDARY_REQUIRED", detail: "x".into() },
            400,
            "content_type",
            "BOUNDARY_REQUIRED",
        ),
        (
            DomainError::UnsupportedContentType { field: "file", content_type: "application/x".into() },
            400,
            "file",
            "UNSUPPORTED_CONTENT_TYPE",
        ),
    ];
    for (e, status, field, reason) in cases {
        let (s, v) = problem(e);
        assert_eq!(s, status, "{v}");
        assert_eq!(field_reason(&v), (field.to_owned(), reason.to_owned()), "{v}");
    }
}

#[test]
fn preconditions_and_aborts() {
    let (s, v) = problem(DomainError::FeatureDisabled("web_search"));
    assert_eq!(s, 400);
    assert_eq!(v["context"]["violations"][0]["subject"], "web_search");
    assert_eq!(v["context"]["violations"][0]["type"], "FEATURE_DISABLED");
    let (_, v) = problem(DomainError::TurnNotTerminal);
    assert_eq!(v["context"]["violations"][0]["subject"], "turn_state");
    assert_eq!(v["context"]["violations"][0]["type"], "STATE");
    let (_, v) = problem(DomainError::ReactionTargetNotAssistant);
    assert_eq!(v["context"]["violations"][0]["subject"], "reaction_target");
    for (e, reason) in [
        (DomainError::TurnAlreadyRunning, "turn_already_running"),
        (DomainError::RequestIdConflict("internal".into()), "request_id_conflict"),
        (DomainError::NotLatestTurn, "NOT_LATEST_TURN"),
        (DomainError::GenerationInProgress, "GENERATION_IN_PROGRESS"),
        (DomainError::Replay, "REPLAY"),
    ] {
        let (s, v) = problem(e);
        assert_eq!(s, 409);
        assert_eq!(v["context"]["reason"], reason);
        assert!(!v["detail"].as_str().unwrap().contains("internal"));
    }
}

#[test]
fn conflicts_quota_and_availability() {
    let (s, v) = problem(DomainError::AttachmentLocked);
    assert_eq!((s, v["context"]["resource_name"].as_str()), (409, Some("attachment_locked")));
    let (_, v) = problem(DomainError::ProviderMismatch);
    assert_eq!(v["context"]["resource_name"], "provider_mismatch");
    let (_, v) = problem(DomainError::UniqueViolation("driver text".into()));
    assert_eq!(v["context"]["resource_name"], "unique_violation");
    assert!(!v["detail"].as_str().unwrap().contains("driver"));
    for scope in ["tokens", "web_search", "code_interpreter"] {
        let (s, v) = problem(DomainError::QuotaExceeded(match scope {
            "tokens" => crate::domain::error::quota_scope::TOKENS,
            "web_search" => crate::domain::error::quota_scope::WEB_SEARCH,
            _ => crate::domain::error::quota_scope::CODE_INTERPRETER,
        }));
        assert_eq!(s, 429);
        assert_eq!(v["context"]["violations"][0]["subject"], scope);
        assert_eq!(v["context"]["violations"][0]["description"], "quota_exceeded");
    }
    let (s, v) = problem(DomainError::DocumentLimit);
    assert_eq!((s, v["context"]["violations"][0]["subject"].as_str()), (429, Some("document_limit")));
    let (_, v) = problem(DomainError::StorageLimit);
    assert_eq!(v["context"]["violations"][0]["subject"], "storage_limit");
    let (s, v) = problem(DomainError::AuthzUnavailable("pdp down".into()));
    assert_eq!((s, v["context"]["retry_after_seconds"].as_u64()), (503, Some(5)));
    let (s, v) = problem(DomainError::StorageUnavailable("x".into()));
    assert_eq!((s, v["context"]["retry_after_seconds"].as_u64()), (503, Some(10)));
    let (s, v) = problem(DomainError::UploadConcurrencyLimit);
    assert_eq!((s, v["context"]["retry_after_seconds"].as_u64()), (503, Some(5)));
    let (s, v) = problem(DomainError::Forbidden);
    assert_eq!((s, v["context"]["reason"].as_str()), (403, Some("AUTHZ_DENIED")));
    let (s, v) = problem(DomainError::Internal("secret db detail".into()));
    assert_eq!(s, 500);
    assert!(!v["detail"].as_str().unwrap().contains("secret"));
    let (s, v) = problem(DomainError::ChatCleanupPayloadTooLarge("too big".into()));
    assert_eq!((s, v["context"]["format"].as_str()), (400, Some("too big")));
}

#[test]
fn odata_errors_keep_odata_resource_type() {
    let (s, v) = problem(DomainError::OData(toolkit_odata::Error::InvalidCursor));
    assert_eq!(s, 400);
    assert_eq!(v["context"]["resource_type"], "gts.cf.core.odata.query.v1~");
    assert_eq!(field_reason(&v).1, "INVALID_CURSOR");
}
