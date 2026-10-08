#![allow(clippy::unwrap_used)]

use toolkit_canonical_errors::{CanonicalError, Problem};

use super::{DomainError, QuotaScope};

fn problem(e: DomainError) -> serde_json::Value {
    let c: CanonicalError = e.into();
    serde_json::to_value(Problem::from(c)).unwrap()
}

#[test]
fn not_found_reports_resource_type() {
    for (err, rt) in [
        (DomainError::ChatNotFound("x".into()), "gts.cf.core.mini_chat.chat.v1~"),
        (DomainError::MessageNotFound("x".into()), "gts.cf.core.mini_chat.message.v1~"),
        (DomainError::TurnNotFound("x".into()), "gts.cf.core.mini_chat.turn.v1~"),
        (DomainError::AttachmentNotFound("x".into()), "gts.cf.core.mini_chat.attachment.v1~"),
        (DomainError::ModelNotFound("x".into()), "gts.cf.core.mini_chat.model.v1~"),
    ] {
        let p = problem(err);
        assert_eq!(p["status"], 404);
        assert_eq!(p["context"]["resource_type"], rt);
        assert!(p.get("code").is_none());
    }
}

#[test]
fn field_violation_reasons() {
    for (err, status, field, reason) in [
        (DomainError::InvalidModel("m".into()), 400, "model", "INVALID_MODEL"),
        (DomainError::InvalidTitle("t".into()), 400, "title", "INVALID_TITLE"),
        (DomainError::EmptyContent, 400, "content", "EMPTY_CONTENT"),
        (DomainError::InvalidReaction, 400, "reaction", "INVALID_REACTION"),
        (DomainError::InvalidAttachment("a".into()), 400, "attachment", "invalid_attachment"),
        (DomainError::CodeInterpreterUnavailable, 400, "file", "CODE_INTERPRETER_UNAVAILABLE"),
        (DomainError::UnsupportedContentType("x".into()), 400, "content_type", "UNSUPPORTED_CONTENT_TYPE"),
        (DomainError::VisionNotSupported("m".into()), 400, "content_type", "VISION_NOT_SUPPORTED"),
        (DomainError::FileTooLarge("big".into()), 400, "content_length", "FILE_TOO_LARGE"),
        (DomainError::TooManyImages("n".into()), 400, "image_count", "TOO_MANY_IMAGES"),
        (DomainError::InputTooLong("l".into()), 400, "content", "INPUT_TOO_LONG"),
        (DomainError::ContextBudgetExceeded("c".into()), 400, "content", "CONTEXT_BUDGET_EXCEEDED"),
    ] {
        let p = problem(err);
        assert_eq!(p["status"], status);
        assert_eq!(p["context"]["field_violations"][0]["field"], field);
        assert_eq!(p["context"]["field_violations"][0]["reason"], reason);
    }
}

#[test]
fn code_interpreter_reports_attachment_resource_type() {
    let p = problem(DomainError::CodeInterpreterUnavailable);
    assert_eq!(p["context"]["resource_type"], "gts.cf.core.mini_chat.attachment.v1~");
}

#[test]
fn preconditions_and_aborted() {
    let p = problem(DomainError::FeatureDisabled("web_search"));
    assert_eq!(p["status"], 400);
    assert_eq!(p["context"]["violations"][0]["subject"], "web_search");
    assert_eq!(p["context"]["violations"][0]["type"], "FEATURE_DISABLED");
    let p = problem(DomainError::TurnNotTerminal);
    assert_eq!(p["context"]["violations"][0]["subject"], "turn_state");
    assert_eq!(p["context"]["violations"][0]["type"], "STATE");
    let p = problem(DomainError::ReactionTarget);
    assert_eq!(p["context"]["violations"][0]["subject"], "reaction_target");
    for (err, reason) in [
        (DomainError::TurnAlreadyRunning, "turn_already_running"),
        (DomainError::RequestIdConflict("diag-with-ids".into()), "request_id_conflict"),
        (DomainError::NotLatestTurn, "NOT_LATEST_TURN"),
        (DomainError::GenerationInProgress, "GENERATION_IN_PROGRESS"),
    ] {
        let p = problem(err);
        assert_eq!(p["status"], 409);
        assert_eq!(p["context"]["reason"], reason);
        assert!(!p["detail"].as_str().unwrap().contains("diag-with-ids"));
    }
}

#[test]
fn conflicts_quota_and_availability() {
    let p = problem(DomainError::AttachmentLocked);
    assert_eq!(p["status"], 409);
    assert_eq!(p["context"]["resource_name"], "attachment_locked");
    let p = problem(DomainError::ProviderMismatch);
    assert_eq!(p["context"]["resource_name"], "provider_mismatch");
    for scope in [QuotaScope::Tokens, QuotaScope::WebSearch, QuotaScope::CodeInterpreter] {
        let p = problem(DomainError::QuotaExceeded(scope));
        assert_eq!(p["status"], 429);
        assert_eq!(p["context"]["violations"][0]["subject"], scope.as_str());
        assert_eq!(p["context"]["violations"][0]["description"], "quota_exceeded");
    }
    let p = problem(DomainError::DocumentLimit);
    assert_eq!(p["status"], 429);
    let p = problem(DomainError::AuthzDenied);
    assert_eq!(p["status"], 403);
    assert_eq!(p["context"]["reason"], "AUTHZ_DENIED");
    let p = problem(DomainError::AuthzUnavailable);
    assert_eq!(p["status"], 503);
    assert_eq!(p["context"]["retry_after_seconds"], 5);
    let p = problem(DomainError::StorageUnavailable("provider said file-abc".into()));
    assert_eq!(p["status"], 503);
    assert_eq!(p["context"]["retry_after_seconds"], 10);
    assert_eq!(p["detail"], "Service temporarily unavailable");
    let p = problem(DomainError::Internal("secret diag".into()));
    assert_eq!(p["status"], 500);
    assert!(!p.to_string().contains("secret diag"));
}
