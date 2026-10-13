use toolkit_canonical_errors::{CanonicalError, Problem};

use crate::domain::error::{DomainError, NotFoundKind};

fn problem(e: DomainError) -> serde_json::Value {
    let c = CanonicalError::from(e);
    serde_json::to_value(Problem::from(c)).unwrap()
}

fn status(e: DomainError) -> u16 {
    CanonicalError::from(e).status_code()
}

#[test]
fn not_found_resource_types() {
    for (k, rt) in [
        (NotFoundKind::Chat, "gts.cf.core.mini_chat.chat.v1~"),
        (NotFoundKind::Message, "gts.cf.core.mini_chat.message.v1~"),
        (NotFoundKind::Turn, "gts.cf.core.mini_chat.turn.v1~"),
        (NotFoundKind::Attachment, "gts.cf.core.mini_chat.attachment.v1~"),
        (NotFoundKind::Model, "gts.cf.core.mini_chat.model.v1~"),
    ] {
        let p = problem(DomainError::NotFound(k));
        assert_eq!(p["status"], 404);
        assert_eq!(p["context"]["resource_type"], rt);
        assert!(p.get("code").is_none());
    }
}

#[test]
fn field_violation_reasons() {
    for (e, field, reason) in [
        (DomainError::InvalidModel, "model", "INVALID_MODEL"),
        (DomainError::EmptyContent, "content", "EMPTY_CONTENT"),
        (DomainError::InvalidTitle, "title", "INVALID_TITLE"),
        (DomainError::InvalidReaction, "reaction", "INVALID_REACTION"),
        (DomainError::InvalidAttachment("x".into()), "attachment", "invalid_attachment"),
        (DomainError::VisionNotSupported, "content_type", "VISION_NOT_SUPPORTED"),
        (DomainError::FileTooLarge, "content_length", "FILE_TOO_LARGE"),
        (DomainError::TooManyImages, "image_count", "TOO_MANY_IMAGES"),
        (DomainError::CodeInterpreterUnavailable, "file", "CODE_INTERPRETER_UNAVAILABLE"),
        (DomainError::Multipart("multipart", "MULTIPART_ERROR", "bad".into()), "multipart", "MULTIPART_ERROR"),
    ] {
        let p = problem(e);
        assert_eq!(p["status"], 400);
        assert_eq!(p["context"]["field_violations"][0]["field"], field);
        assert_eq!(p["context"]["field_violations"][0]["reason"], reason);
    }
    let p = problem(DomainError::InputTooLong);
    assert_eq!(p["context"]["field_violations"][0]["reason"], "INPUT_TOO_LONG");
    let p = problem(DomainError::ContextBudgetExceeded);
    assert_eq!(p["context"]["field_violations"][0]["reason"], "CONTEXT_BUDGET_EXCEEDED");
}

#[test]
fn preconditions_and_conflicts() {
    let p = problem(DomainError::FeatureDisabled("web_search"));
    assert_eq!(p["status"], 400);
    assert_eq!(p["context"]["violations"][0]["subject"], "web_search");
    assert_eq!(p["context"]["violations"][0]["type"], "FEATURE_DISABLED");
    let p = problem(DomainError::TurnNotTerminal);
    assert_eq!(p["context"]["violations"][0]["subject"], "turn_state");
    let p = problem(DomainError::ReactionTarget);
    assert_eq!(p["context"]["violations"][0]["subject"], "reaction_target");
    for (e, reason) in [
        (DomainError::TurnAlreadyRunning, "turn_already_running"),
        (DomainError::RequestIdConflict, "request_id_conflict"),
        (DomainError::NotLatestTurn, "NOT_LATEST_TURN"),
        (DomainError::GenerationInProgress, "GENERATION_IN_PROGRESS"),
    ] {
        let p = problem(e);
        assert_eq!(p["status"], 409);
        assert_eq!(p["context"]["reason"], reason);
    }
    let p = problem(DomainError::AttachmentLocked);
    assert_eq!((p["status"].as_u64(), p["context"]["resource_name"].as_str()), (Some(409), Some("attachment_locked")));
    let p = problem(DomainError::ProviderMismatch);
    assert_eq!(p["context"]["resource_name"], "provider_mismatch");
}

#[test]
fn quota_permission_and_availability() {
    let p = problem(DomainError::QuotaExceeded("tokens"));
    assert_eq!(p["status"], 429);
    assert_eq!(p["context"]["violations"][0]["subject"], "tokens");
    assert_eq!(p["context"]["violations"][0]["description"], "quota_exceeded");
    assert_eq!(problem(DomainError::DocumentLimit)["context"]["violations"][0]["subject"], "document_limit");
    assert_eq!(problem(DomainError::StorageLimit)["context"]["violations"][0]["subject"], "storage_limit");
    let p = problem(DomainError::AccessDenied);
    assert_eq!((p["status"].as_u64(), p["context"]["reason"].as_str()), (Some(403), Some("AUTHZ_DENIED")));
    assert_eq!(status(DomainError::AuthzUnavailable("x".into())), 503);
    assert_eq!(problem(DomainError::StorageUnavailable("x".into()))["context"]["retry_after_seconds"], 10);
    assert_eq!(problem(DomainError::UploadConcurrency)["context"]["retry_after_seconds"], 5);
    let busy = sea_orm::DbErr::Custom("error returned from database: (code: 517) database is locked".into());
    let p = problem(DomainError::from(busy));
    assert_eq!(p["status"], 503, "SQLite busy snapshot is transient contention");
    assert_eq!(p["context"]["retry_after_seconds"], 1);
    assert_eq!(status(DomainError::Internal("x".into())), 500);
}
