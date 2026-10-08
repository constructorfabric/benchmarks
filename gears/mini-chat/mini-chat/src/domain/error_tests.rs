use super::*;
use toolkit_canonical_errors::Problem;

fn problem(e: DomainError) -> serde_json::Value {
    let p = Problem::from(CanonicalError::from(e));
    serde_json::to_value(p).unwrap()
}

fn field_reason(v: &serde_json::Value) -> (String, String) {
    let fv = &v["context"]["field_violations"][0];
    (fv["field"].as_str().unwrap().to_owned(), fv["reason"].as_str().unwrap().to_owned())
}

#[test]
fn not_found_resource_types() {
    for (e, rt) in [
        (DomainError::ChatNotFound, "gts.cf.core.mini_chat.chat.v1~"),
        (DomainError::MessageNotFound, "gts.cf.core.mini_chat.message.v1~"),
        (DomainError::TurnNotFound, "gts.cf.core.mini_chat.turn.v1~"),
        (DomainError::AttachmentNotFound, "gts.cf.core.mini_chat.attachment.v1~"),
        (DomainError::ModelNotFound, "gts.cf.core.mini_chat.model.v1~"),
    ] {
        let v = problem(e);
        assert_eq!(v["status"], 404);
        assert_eq!(v["context"]["resource_type"], rt);
        assert!(v.get("code").is_none(), "no top-level code field");
    }
}

#[test]
fn invalid_argument_reasons() {
    for (e, field, reason, status) in [
        (DomainError::InvalidModel, "model", "INVALID_MODEL", 400),
        (DomainError::EmptyContent, "content", "EMPTY_CONTENT", 400),
        (DomainError::InvalidTitle, "title", "INVALID_TITLE", 400),
        (DomainError::InvalidReaction, "reaction", "INVALID_REACTION", 400),
        (DomainError::InvalidAttachment("dup".into()), "attachment", "invalid_attachment", 400),
        (DomainError::UnsupportedContentType("x/y".into()), "content_type", "UNSUPPORTED_CONTENT_TYPE", 400),
        (DomainError::CodeInterpreterUnavailable, "file", "CODE_INTERPRETER_UNAVAILABLE", 400),
        (DomainError::VisionNotSupported, "content_type", "VISION_NOT_SUPPORTED", 400),
        (DomainError::FileTooLarge { limit_bytes: 1 }, "content_length", "FILE_TOO_LARGE", 400),
        (DomainError::TooManyImages { limit: 4 }, "image_count", "TOO_MANY_IMAGES", 400),
        (DomainError::InputTooLong, "content", "INPUT_TOO_LONG", 400),
        (DomainError::ContextBudgetExceeded, "content", "CONTEXT_BUDGET_EXCEEDED", 400),
        (
            DomainError::Multipart { field: "file", reason: "MISSING_FILE", description: "x".into() },
            "file",
            "MISSING_FILE",
            400,
        ),
    ] {
        let v = problem(e);
        assert_eq!(v["status"], status);
        assert_eq!(field_reason(&v), (field.to_owned(), reason.to_owned()));
    }
    let v = problem(DomainError::FileTooLarge { limit_bytes: 1 });
    assert!(v["type"].as_str().unwrap().contains("out_of_range"));
    let v = problem(DomainError::CodeInterpreterUnavailable);
    assert_eq!(v["context"]["resource_type"], "gts.cf.core.mini_chat.attachment.v1~");
}

#[test]
fn precondition_violations() {
    for (e, subject, ty) in [
        (DomainError::FeatureDisabled("web_search"), "web_search", "FEATURE_DISABLED"),
        (DomainError::FeatureDisabled("images"), "images", "FEATURE_DISABLED"),
        (DomainError::TurnNotTerminal, "turn_state", "STATE"),
        (DomainError::ReactionTarget, "reaction_target", "STATE"),
    ] {
        let v = problem(e);
        assert_eq!(v["status"], 400);
        assert!(v["type"].as_str().unwrap().contains("failed_precondition"));
        assert_eq!(v["context"]["violations"][0]["subject"], subject);
        assert_eq!(v["context"]["violations"][0]["type"], ty);
    }
}

#[test]
fn aborted_and_conflicts() {
    for (e, reason) in [
        (DomainError::TurnAlreadyRunning, "turn_already_running"),
        (DomainError::RequestIdConflict("x".into()), "request_id_conflict"),
        (DomainError::NotLatestTurn, "NOT_LATEST_TURN"),
        (DomainError::GenerationInProgress, "GENERATION_IN_PROGRESS"),
        (DomainError::Replay, "REPLAY"),
    ] {
        let v = problem(e);
        assert_eq!(v["status"], 409);
        assert_eq!(v["context"]["reason"], reason);
    }
    for (e, name) in [
        (DomainError::AttachmentLocked, "attachment_locked"),
        (DomainError::ProviderMismatch, "provider_mismatch"),
        (DomainError::Conflict("unique_violation".into()), "unique_violation"),
    ] {
        let v = problem(e);
        assert_eq!(v["status"], 409);
        assert!(v["type"].as_str().unwrap().contains("already_exists"));
        assert_eq!(v["context"]["resource_name"], name);
    }
    let v = problem(DomainError::RequestIdConflict("turn 123 driver text".into()));
    assert!(!v["detail"].as_str().unwrap().contains("driver"), "internal message only logged");
}

#[test]
fn quota_and_limits() {
    for (scope, subject) in [
        (QuotaScope::Tokens, "tokens"),
        (QuotaScope::WebSearch, "web_search"),
        (QuotaScope::CodeInterpreter, "code_interpreter"),
    ] {
        let v = problem(DomainError::QuotaExceeded(scope));
        assert_eq!(v["status"], 429);
        assert_eq!(v["context"]["violations"][0]["subject"], subject);
        assert_eq!(v["context"]["violations"][0]["description"], "quota_exceeded");
    }
    assert_eq!(problem(DomainError::DocumentLimit)["context"]["violations"][0]["subject"], "document_limit");
    assert_eq!(problem(DomainError::StorageLimit)["context"]["violations"][0]["subject"], "storage_limit");
}

#[test]
fn authz_and_availability() {
    let v = problem(DomainError::PermissionDenied);
    assert_eq!(v["status"], 403);
    assert_eq!(v["context"]["reason"], "AUTHZ_DENIED");
    let v = problem(DomainError::PdpUnavailable);
    assert_eq!(v["status"], 503);
    assert_eq!(v["context"]["retry_after_seconds"], 5);
    let v = problem(DomainError::StorageUnavailable("boom".into()));
    assert_eq!(v["status"], 503);
    assert_eq!(v["context"]["retry_after_seconds"], 10);
    assert!(!v["detail"].as_str().unwrap().contains("boom"));
    let v = problem(DomainError::UploadConcurrency);
    assert_eq!(v["context"]["retry_after_seconds"], 5);
    let v = problem(DomainError::Internal("secret db text".into()));
    assert_eq!(v["status"], 500);
    assert!(!v["detail"].as_str().unwrap().contains("secret"));
    let v = problem(DomainError::CleanupPayloadTooLarge("payload too large".into()));
    assert_eq!(v["status"], 400);
    assert_eq!(v["context"]["format"], "payload too large");
}

#[test]
fn odata_errors_keep_odata_resource_type() {
    let v = problem(DomainError::from(toolkit_odata::Error::InvalidCursor));
    assert_eq!(v["status"], 400);
    assert_eq!(v["context"]["resource_type"], "gts.cf.core.odata.query.v1~");
}
