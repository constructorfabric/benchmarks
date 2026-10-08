//! One test per ADR-0004 row: `(status, category in Problem.type, machine field)`.

use serde_json::Value;
use toolkit_canonical_errors::Problem;

use crate::domain::error::{DomainError, FeatureSubject, QuotaScope};

fn problem(e: DomainError) -> Problem {
    Problem::from(e)
}

/// Asserts the HTTP status and the canonical category carried in `Problem.type`.
fn assert_category(p: &Problem, status: u16, category: &str) {
    assert_eq!(p.status, Some(status), "status of {p:?}");
    assert!(
        p.problem_type
            .contains(&format!("cf.core.err.{category}.v1~")),
        "category {category} not in type {}",
        p.problem_type
    );
}

fn field_violation(p: &Problem) -> (String, String) {
    let v = &p.context["field_violations"][0];
    (
        v["field"].as_str().unwrap_or_default().to_owned(),
        v["reason"].as_str().unwrap_or_default().to_owned(),
    )
}

fn str_at<'a>(p: &'a Problem, key: &str) -> &'a str {
    p.context[key].as_str().unwrap_or_default()
}

// --- not_found -------------------------------------------------------------

fn assert_not_found(e: DomainError, resource_type: &str) {
    let p = problem(e);
    assert_category(&p, 404, "not_found");
    assert_eq!(str_at(&p, "resource_type"), resource_type);
}

#[test]
fn chat_not_found_resource_type() {
    assert_not_found(DomainError::ChatNotFound, "gts.cf.core.mini_chat.chat.v1~");
}

#[test]
fn message_not_found_resource_type() {
    assert_not_found(
        DomainError::MessageNotFound,
        "gts.cf.core.mini_chat.message.v1~",
    );
}

#[test]
fn turn_not_found_resource_type() {
    assert_not_found(DomainError::TurnNotFound, "gts.cf.core.mini_chat.turn.v1~");
}

#[test]
fn attachment_not_found_resource_type() {
    assert_not_found(
        DomainError::AttachmentNotFound,
        "gts.cf.core.mini_chat.attachment.v1~",
    );
}

#[test]
fn model_not_found_resource_type() {
    assert_not_found(
        DomainError::ModelNotFound,
        "gts.cf.core.mini_chat.model.v1~",
    );
}

// --- invalid_argument (400) -------------------------------------------------

fn assert_invalid_field(e: DomainError, field: &str, reason: &str) -> Problem {
    let p = problem(e);
    assert_category(&p, 400, "invalid_argument");
    assert_eq!(field_violation(&p), (field.to_owned(), reason.to_owned()));
    p
}

#[test]
fn invalid_model_field_violation() {
    assert_invalid_field(DomainError::InvalidModel, "model", "INVALID_MODEL");
}

#[test]
fn empty_content_field_violation() {
    assert_invalid_field(DomainError::EmptyContent, "content", "EMPTY_CONTENT");
}

#[test]
fn invalid_title_field_violation() {
    assert_invalid_field(DomainError::InvalidTitle, "title", "INVALID_TITLE");
}

#[test]
fn invalid_reaction_field_violation() {
    assert_invalid_field(DomainError::InvalidReaction, "reaction", "INVALID_REACTION");
}

#[test]
fn invalid_attachment_field_violation() {
    assert_invalid_field(
        DomainError::InvalidAttachment,
        "attachment",
        "invalid_attachment",
    );
}

#[test]
fn unsupported_content_type_reason() {
    let p = problem(DomainError::UnsupportedContentType);
    assert_category(&p, 400, "invalid_argument");
    assert_eq!(field_violation(&p).1, "UNSUPPORTED_CONTENT_TYPE");
}

#[test]
fn code_interpreter_unavailable_field_and_attachment_type() {
    let p = assert_invalid_field(
        DomainError::CodeInterpreterUnavailable,
        "file",
        "CODE_INTERPRETER_UNAVAILABLE",
    );
    assert_eq!(
        str_at(&p, "resource_type"),
        "gts.cf.core.mini_chat.attachment.v1~"
    );
}

#[test]
fn multipart_errors_carry_field_and_reason() {
    for (field, reason) in [
        ("content_type", "BOUNDARY_REQUIRED"),
        ("multipart", "MULTIPART_ERROR"),
        ("file", "MISSING_FILE"),
        ("content_type", "MISSING_CONTENT_TYPE"),
    ] {
        assert_invalid_field(DomainError::Multipart { field, reason }, field, reason);
    }
}

#[test]
fn vision_not_supported_field_content_type() {
    assert_invalid_field(
        DomainError::VisionNotSupported,
        "content_type",
        "VISION_NOT_SUPPORTED",
    );
}

#[test]
fn outbox_payload_too_large_has_detail_and_format() {
    let msg = "payload exceeds the outbox size limit";
    let p = problem(DomainError::OutboxPayloadTooLarge(msg.to_owned()));
    assert_category(&p, 400, "invalid_argument");
    assert_eq!(p.detail, msg);
    assert_eq!(str_at(&p, "format"), msg);
}

// --- out_of_range (400) -----------------------------------------------------

fn assert_out_of_range(e: DomainError, field: Option<&str>, reason: &str) {
    let p = problem(e);
    assert_category(&p, 400, "out_of_range");
    let (f, r) = field_violation(&p);
    assert_eq!(r, reason);
    if let Some(field) = field {
        assert_eq!(f, field);
    }
}

#[test]
fn file_too_large_field_content_length() {
    assert_out_of_range(
        DomainError::FileTooLarge,
        Some("content_length"),
        "FILE_TOO_LARGE",
    );
}

#[test]
fn too_many_images_field_image_count() {
    assert_out_of_range(
        DomainError::TooManyImages,
        Some("image_count"),
        "TOO_MANY_IMAGES",
    );
}

#[test]
fn input_too_long_reason() {
    assert_out_of_range(DomainError::InputTooLong, None, "INPUT_TOO_LONG");
}

#[test]
fn context_budget_exceeded_reason() {
    assert_out_of_range(
        DomainError::ContextBudgetExceeded,
        None,
        "CONTEXT_BUDGET_EXCEEDED",
    );
}

// --- failed_precondition (400) ----------------------------------------------

fn precondition(p: &Problem) -> (String, String) {
    let v = &p.context["violations"][0];
    (
        v["subject"].as_str().unwrap_or_default().to_owned(),
        v["type"].as_str().unwrap_or_default().to_owned(),
    )
}

#[test]
fn feature_disabled_web_search() {
    let p = problem(DomainError::FeatureDisabled(FeatureSubject::WebSearch));
    assert_category(&p, 400, "failed_precondition");
    assert_eq!(
        precondition(&p),
        ("web_search".to_owned(), "FEATURE_DISABLED".to_owned())
    );
}

#[test]
fn feature_disabled_images() {
    let p = problem(DomainError::FeatureDisabled(FeatureSubject::Images));
    assert_category(&p, 400, "failed_precondition");
    assert_eq!(
        precondition(&p),
        ("images".to_owned(), "FEATURE_DISABLED".to_owned())
    );
}

#[test]
fn turn_not_terminal_state_violation() {
    let p = problem(DomainError::TurnNotTerminal);
    assert_category(&p, 400, "failed_precondition");
    assert_eq!(
        precondition(&p),
        ("turn_state".to_owned(), "STATE".to_owned())
    );
}

#[test]
fn reaction_target_not_assistant_state_violation() {
    let p = problem(DomainError::ReactionTargetNotAssistant);
    assert_category(&p, 400, "failed_precondition");
    assert_eq!(
        precondition(&p),
        ("reaction_target".to_owned(), "STATE".to_owned())
    );
}

// --- permission_denied / service_unavailable --------------------------------

#[test]
fn authz_denied_is_403_with_reason() {
    let p = problem(DomainError::AuthzDenied);
    assert_category(&p, 403, "permission_denied");
    assert_eq!(str_at(&p, "reason"), "AUTHZ_DENIED");
}

fn assert_unavailable(e: DomainError, seconds: u64) {
    let p = problem(e);
    assert_category(&p, 503, "service_unavailable");
    assert_eq!(p.context["retry_after_seconds"], Value::from(seconds));
}

#[test]
fn authz_unavailable_has_retry_after_5() {
    assert_unavailable(DomainError::AuthzUnavailable, 5);
}

#[test]
fn authz_unavailable_sets_retry_after_header() {
    use axum::response::IntoResponse;
    let resp = problem(DomainError::AuthzUnavailable).into_response();
    assert_eq!(resp.status().as_u16(), 503);
    assert_eq!(
        resp.headers()
            .get(http::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok()),
        Some("5")
    );
}

#[test]
fn storage_unavailable_has_retry_after_10() {
    assert_unavailable(DomainError::StorageUnavailable, 10);
}

#[test]
fn upload_concurrency_limit_has_retry_after_5() {
    assert_unavailable(DomainError::UploadConcurrencyLimit, 5);
}

// --- aborted (409) ----------------------------------------------------------

fn assert_aborted(e: DomainError, reason: &str) {
    let p = problem(e);
    assert_category(&p, 409, "aborted");
    assert_eq!(str_at(&p, "reason"), reason);
}

#[test]
fn turn_already_running_reason() {
    assert_aborted(DomainError::TurnAlreadyRunning, "turn_already_running");
}

#[test]
fn request_id_conflict_reason() {
    assert_aborted(DomainError::RequestIdConflict, "request_id_conflict");
}

#[test]
fn not_latest_turn_reason() {
    assert_aborted(DomainError::NotLatestTurn, "NOT_LATEST_TURN");
}

#[test]
fn generation_in_progress_reason() {
    assert_aborted(DomainError::GenerationInProgress, "GENERATION_IN_PROGRESS");
}

#[test]
fn replay_reason() {
    assert_aborted(DomainError::Replay, "REPLAY");
}

// --- already_exists (409) ---------------------------------------------------

fn assert_already_exists(e: DomainError, resource_name: &str) -> Problem {
    let p = problem(e);
    assert_category(&p, 409, "already_exists");
    assert_eq!(str_at(&p, "resource_name"), resource_name);
    p
}

#[test]
fn attachment_locked_resource_name() {
    assert_already_exists(DomainError::AttachmentLocked, "attachment_locked");
}

#[test]
fn provider_mismatch_resource_name() {
    assert_already_exists(DomainError::ProviderMismatch, "provider_mismatch");
}

#[test]
fn unique_violation_resource_name_and_generic_detail() {
    let driver = "UNIQUE constraint failed: chat_turns.chat_id, chat_turns.request_id";
    let p = assert_already_exists(
        DomainError::UniqueViolation(driver.to_owned()),
        "unique_violation",
    );
    assert!(
        !p.detail.contains("chat_turns"),
        "detail leaks driver text: {}",
        p.detail
    );
}

// --- resource_exhausted (429) -----------------------------------------------

fn assert_quota(e: DomainError, subject: &str) {
    let p = problem(e);
    assert_category(&p, 429, "resource_exhausted");
    assert_eq!(p.context["violations"][0]["subject"], subject);
    assert_eq!(p.context["violations"][0]["description"], "quota_exceeded");
}

#[test]
fn quota_tokens_is_429_with_subject_tokens() {
    assert_quota(DomainError::QuotaExceeded(QuotaScope::Tokens), "tokens");
}

#[test]
fn quota_web_search_subject() {
    assert_quota(
        DomainError::QuotaExceeded(QuotaScope::WebSearch),
        "web_search",
    );
}

#[test]
fn quota_code_interpreter_subject() {
    assert_quota(
        DomainError::QuotaExceeded(QuotaScope::CodeInterpreter),
        "code_interpreter",
    );
}

#[test]
fn document_limit_is_429() {
    let p = problem(DomainError::DocumentLimit);
    assert_category(&p, 429, "resource_exhausted");
    assert_eq!(p.context["violations"][0]["subject"], "document_limit");
}

#[test]
fn storage_limit_is_429() {
    let p = problem(DomainError::StorageLimit);
    assert_category(&p, 429, "resource_exhausted");
    assert_eq!(p.context["violations"][0]["subject"], "storage_limit");
}

// --- internal (500) ---------------------------------------------------------

#[test]
fn internal_variants_are_500_without_leaking_detail() {
    for e in [
        DomainError::PluginUnavailable("policy plugin gone".to_owned()),
        DomainError::ProviderResolution("provider openai-azure missing".to_owned()),
        DomainError::Internal("boom secret".to_owned()),
        DomainError::Database("sqlite: no such table chats".to_owned()),
        DomainError::from(sea_orm::DbErr::Custom(
            "sqlite (code: 517) database is locked".to_owned(),
        )),
    ] {
        let p = problem(e);
        assert_category(&p, 500, "internal");
        for leak in ["policy plugin", "openai", "secret", "sqlite"] {
            assert!(
                !p.detail.contains(leak),
                "detail leaks {leak}: {}",
                p.detail
            );
        }
    }
}
