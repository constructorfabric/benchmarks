#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::response::IntoResponse;
use serde_json::{Value, json};
use toolkit_canonical_errors::{CanonicalError, Problem};

use crate::domain::error::DomainError;
use crate::domain::model::QuotaScope;

fn problem(err: DomainError) -> Problem {
    Problem::from(CanonicalError::from(err))
}

fn status(p: &Problem) -> u16 {
    p.status.expect("status")
}

fn ctx<'a>(p: &'a Problem, key: &str) -> &'a Value {
    &p.context[key]
}

fn category(p: &Problem) -> &str {
    p.problem_type.rsplit("cf.core.err.").next().unwrap()
}

/// Asserts a 4xx `field_violations[0]` on `p`.
fn assert_field(p: &Problem, expected_status: u16, field: &str, reason: &str) {
    assert_eq!(status(p), expected_status, "{p:?}");
    let v = &ctx(p, "field_violations")[0];
    assert_eq!(v["field"], field, "{p:?}");
    assert_eq!(v["reason"], reason, "{p:?}");
    assert_eq!(ctx(p, "field_violations").as_array().unwrap().len(), 1);
}

#[test]
fn chat_not_found_is_404_with_chat_resource_type() {
    let p = problem(DomainError::ChatNotFound);
    assert_eq!(status(&p), 404);
    assert!(category(&p).starts_with("not_found"));
    assert_eq!(ctx(&p, "resource_type"), "gts.cf.core.mini_chat.chat.v1~");
}

#[test]
fn other_not_found_variants_name_their_resource_type() {
    for (err, rt) in [
        (DomainError::MessageNotFound, "message"),
        (DomainError::TurnNotFound, "turn"),
        (DomainError::AttachmentNotFound, "attachment"),
        (DomainError::ModelNotFound, "model"),
    ] {
        let p = problem(err);
        assert_eq!(status(&p), 404);
        assert_eq!(
            ctx(&p, "resource_type"),
            &json!(format!("gts.cf.core.mini_chat.{rt}.v1~"))
        );
    }
}

#[test]
fn invalid_title() {
    let p = problem(DomainError::InvalidTitle);
    assert_field(&p, 400, "title", "INVALID_TITLE");
    assert!(category(&p).starts_with("invalid_argument"));
}

#[test]
fn empty_content() {
    assert_field(
        &problem(DomainError::EmptyContent),
        400,
        "content",
        "EMPTY_CONTENT",
    );
}

#[test]
fn invalid_model() {
    assert_field(
        &problem(DomainError::InvalidModel),
        400,
        "model",
        "INVALID_MODEL",
    );
}

#[test]
fn invalid_reaction() {
    assert_field(
        &problem(DomainError::InvalidReaction),
        400,
        "reaction",
        "INVALID_REACTION",
    );
}

#[test]
fn invalid_attachment() {
    let p = problem(DomainError::InvalidAttachment("duplicate id".into()));
    assert_field(&p, 400, "attachment", "invalid_attachment");
    assert!(category(&p).starts_with("invalid_argument"));
}

#[test]
fn too_many_images() {
    let p = problem(DomainError::TooManyImages { max: 4 });
    assert_field(&p, 400, "image_count", "TOO_MANY_IMAGES");
    assert!(category(&p).starts_with("out_of_range"));
}

#[test]
fn input_too_long() {
    let p = problem(DomainError::InputTooLong);
    assert_field(&p, 400, "content", "INPUT_TOO_LONG");
    assert!(category(&p).starts_with("out_of_range"));
}

#[test]
fn context_budget() {
    let p = problem(DomainError::ContextBudgetExceeded);
    assert_field(&p, 400, "content", "CONTEXT_BUDGET_EXCEEDED");
    assert!(category(&p).starts_with("out_of_range"));
}

#[test]
fn vision() {
    let p = problem(DomainError::VisionNotSupported);
    assert_field(&p, 400, "content_type", "VISION_NOT_SUPPORTED");
    assert!(category(&p).starts_with("invalid_argument"));
}

#[test]
fn file_too_large() {
    let p = problem(DomainError::FileTooLarge { limit_bytes: 1024 });
    assert_field(&p, 400, "content_length", "FILE_TOO_LARGE");
    assert!(category(&p).starts_with("out_of_range"));
}

#[test]
fn unsupported_ct() {
    let p = problem(DomainError::UnsupportedContentType("application/x-foo".into()));
    assert_field(&p, 400, "content_type", "UNSUPPORTED_CONTENT_TYPE");
    assert!(category(&p).starts_with("invalid_argument"));
}

#[test]
fn code_interpreter_unavailable() {
    let p = problem(DomainError::CodeInterpreterUnavailable);
    assert_field(&p, 400, "file", "CODE_INTERPRETER_UNAVAILABLE");
    assert_eq!(
        ctx(&p, "resource_type"),
        "gts.cf.core.mini_chat.attachment.v1~"
    );
}

#[test]
fn multipart_reasons() {
    for (field, reason) in [
        ("content_type", "BOUNDARY_REQUIRED"),
        ("multipart", "MULTIPART_ERROR"),
        ("file", "MISSING_FILE"),
        ("content_type", "MISSING_CONTENT_TYPE"),
    ] {
        let p = problem(DomainError::Multipart {
            field,
            reason,
            detail: "d".into(),
        });
        assert_field(&p, 400, field, reason);
    }
}

#[test]
fn feature_disabled_is_failed_precondition() {
    let p = problem(DomainError::FeatureDisabled {
        subject: "web_search",
    });
    assert_eq!(status(&p), 400);
    assert!(category(&p).starts_with("failed_precondition"));
    let v = &ctx(&p, "violations")[0];
    assert_eq!(v["subject"], "web_search");
    assert_eq!(v["type"], "FEATURE_DISABLED");
    assert!(v["description"].as_str().is_some_and(|s| !s.is_empty()));
}

#[test]
fn turn_not_terminal() {
    let p = problem(DomainError::TurnNotTerminal);
    assert_eq!(status(&p), 400);
    let v = &ctx(&p, "violations")[0];
    assert_eq!(v["subject"], "turn_state");
    assert_eq!(v["type"], "STATE");
}

#[test]
fn reaction_target() {
    let p = problem(DomainError::ReactionTargetNotAssistant);
    assert_eq!(status(&p), 400);
    let v = &ctx(&p, "violations")[0];
    assert_eq!(v["subject"], "reaction_target");
    assert_eq!(v["type"], "STATE");
}

#[test]
fn permission_denied_reason() {
    let p = problem(DomainError::PermissionDenied);
    assert_eq!(status(&p), 403);
    assert_eq!(ctx(&p, "reason"), "AUTHZ_DENIED");
}

#[test]
fn authz_unavailable_is_503_retry_5() {
    let err = CanonicalError::from(DomainError::AuthzUnavailable);
    let resp = err.clone().into_response();
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.headers()["retry-after"], "5");
    let p = Problem::from(err);
    assert_eq!(status(&p), 503);
    assert_eq!(ctx(&p, "retry_after_seconds"), 5);
}

#[test]
fn aborted_reasons() {
    for (err, reason) in [
        (DomainError::TurnAlreadyRunning, "turn_already_running"),
        (DomainError::RequestIdConflict, "request_id_conflict"),
        (DomainError::NotLatestTurn, "NOT_LATEST_TURN"),
        (DomainError::GenerationInProgress, "GENERATION_IN_PROGRESS"),
        (DomainError::Replay, "REPLAY"),
    ] {
        let p = problem(err);
        assert_eq!(status(&p), 409);
        assert!(category(&p).starts_with("aborted"));
        assert_eq!(ctx(&p, "reason"), reason);
    }
}

#[test]
fn already_exists_resource_names() {
    for (err, name) in [
        (DomainError::AttachmentLocked, "attachment_locked"),
        (DomainError::ProviderMismatch, "provider_mismatch"),
        (
            DomainError::Conflict {
                code: "unique_violation".into(),
            },
            "unique_violation",
        ),
    ] {
        let p = problem(err);
        assert_eq!(status(&p), 409);
        assert!(category(&p).starts_with("already_exists"));
        assert_eq!(ctx(&p, "resource_name"), name);
    }
}

#[test]
fn quota_exceeded_scopes() {
    for scope in [
        QuotaScope::Tokens,
        QuotaScope::WebSearch,
        QuotaScope::CodeInterpreter,
    ] {
        let p = problem(DomainError::QuotaExceeded { scope });
        assert_eq!(status(&p), 429);
        assert!(category(&p).starts_with("resource_exhausted"));
        assert_eq!(
            ctx(&p, "violations")[0],
            json!({"subject": scope.as_str(), "description": "quota_exceeded"})
        );
    }
}

#[test]
fn document_and_storage_limit() {
    for (err, subject) in [
        (DomainError::DocumentLimit, "document_limit"),
        (DomainError::StorageLimit, "storage_limit"),
    ] {
        let p = problem(err);
        assert_eq!(status(&p), 429);
        let v = &ctx(&p, "violations")[0];
        assert_eq!(v["subject"], subject);
        assert!(v["description"].as_str().is_some_and(|s| !s.is_empty()));
    }
}

#[test]
fn storage_unavailable_503_retry_10() {
    let err = CanonicalError::from(DomainError::StorageUnavailable("vs_secret down".into()));
    let resp = err.clone().into_response();
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.headers()["retry-after"], "10");
    let p = Problem::from(err);
    assert_eq!(ctx(&p, "retry_after_seconds"), 10);
    assert!(!p.detail.contains("vs_secret"));
}

#[test]
fn upload_concurrency_503_retry_5() {
    let err = CanonicalError::from(DomainError::UploadConcurrencyLimit);
    assert_eq!(err.clone().into_response().headers()["retry-after"], "5");
    let p = Problem::from(err);
    assert_eq!(status(&p), 503);
    assert_eq!(ctx(&p, "retry_after_seconds"), 5);
}

#[test]
fn outbox_payload_too_large_is_400_with_format() {
    let p = problem(DomainError::OutboxPayloadTooLarge("payload 9 > 8".into()));
    assert_eq!(status(&p), 400);
    assert!(category(&p).starts_with("invalid_argument"));
    assert_eq!(p.detail, "payload 9 > 8");
    assert_eq!(ctx(&p, "format"), "payload 9 > 8");
}

#[test]
fn internal_hides_detail() {
    let p = problem(DomainError::Internal("resp_abc123 exploded".into()));
    assert_eq!(status(&p), 500);
    assert!(!p.detail.contains("resp_abc123"));
    assert!(!p.context.to_string().contains("resp_abc123"));
}

#[test]
fn request_id_conflict_detail_is_generic() {
    let p = problem(DomainError::RequestIdConflict);
    assert!(!p.detail.is_empty());
}

#[test]
fn db_unique_violation_becomes_conflict() {
    let err = toolkit_db::DbError::Sea(sea_orm::DbErr::Custom(
        "UNIQUE constraint failed: chats.id".into(),
    ));
    assert!(matches!(
        DomainError::from(err),
        DomainError::Conflict { code } if code == "unique_violation"
    ));
}

#[test]
fn db_other_error_becomes_internal() {
    let err = toolkit_db::DbError::Sea(sea_orm::DbErr::Custom("boom".into()));
    assert!(matches!(DomainError::from(err), DomainError::Internal(_)));
}
