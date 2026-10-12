use serde_json::{Value, json};
use toolkit_canonical_errors::{CanonicalError, Problem};

use super::{
    AbortReason, DomainError, ExhaustedLimit, InvalidArgumentReason, OutOfRangeReason,
    PreconditionSubject, QuotaScope, ResourceKind,
};

const CHAT_RT: &str = "gts.cf.core.mini_chat.chat.v1~";
const MESSAGE_RT: &str = "gts.cf.core.mini_chat.message.v1~";
const TURN_RT: &str = "gts.cf.core.mini_chat.turn.v1~";
const ATTACHMENT_RT: &str = "gts.cf.core.mini_chat.attachment.v1~";
const MODEL_RT: &str = "gts.cf.core.mini_chat.model.v1~";
const ODATA_RT: &str = "gts.cf.core.odata.query.v1~";

fn problem(err: DomainError) -> Problem {
    Problem::from(CanonicalError::from(err))
}

fn category(problem: &Problem) -> &str {
    problem
        .problem_type
        .rsplit("cf.core.err.")
        .next()
        .and_then(|tail| tail.strip_suffix(".v1~"))
        .unwrap_or_default()
}

fn assert_shape(problem: &Problem, status: u16, expected_category: &str) {
    assert_eq!(problem.status, Some(status), "{problem:?}");
    assert_eq!(category(problem), expected_category, "{problem:?}");
    assert!(problem.context.is_object(), "{problem:?}");
    assert!(!problem.detail.is_empty(), "{problem:?}");
}

fn field_violation(problem: &Problem) -> (&str, &str) {
    let violation = &problem.context["field_violations"][0];
    (
        violation["field"].as_str().unwrap_or_default(),
        violation["reason"].as_str().unwrap_or_default(),
    )
}

// ── not_found ────────────────────────────────────────────────────────────

#[test]
fn not_found_reports_resource_type_per_kind() {
    for (kind, rt) in [
        (ResourceKind::Chat, CHAT_RT),
        (ResourceKind::Message, MESSAGE_RT),
        (ResourceKind::Turn, TURN_RT),
        (ResourceKind::Attachment, ATTACHMENT_RT),
        (ResourceKind::Model, MODEL_RT),
    ] {
        let problem = problem(DomainError::not_found(kind, "requested-id"));

        assert_shape(&problem, 404, "not_found");
        assert_eq!(problem.context["resource_type"], json!(rt));
        assert_eq!(problem.context["resource_name"], json!("requested-id"));
        assert_eq!(kind.resource_type(), rt);
    }
}

#[test]
fn not_found_detail_is_generic() {
    let problem = problem(DomainError::not_found(
        ResourceKind::Chat,
        "0f8e7d6c-0000-0000-0000-000000000001",
    ));

    assert_eq!(problem.detail, "Chat not found");
}

#[test]
fn not_found_disabled_and_unknown_model_have_the_same_context_keys() {
    let disabled = problem(DomainError::not_found(ResourceKind::Model, "gpt-disabled"));
    let unknown = problem(DomainError::not_found(ResourceKind::Model, "gpt-unknown"));

    let keys = |p: &Problem| {
        p.context
            .as_object()
            .map(|map| map.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default()
    };
    assert_eq!(keys(&disabled), keys(&unknown));
    assert_eq!(disabled.problem_type, unknown.problem_type);
    assert_eq!(disabled.detail, unknown.detail);
}

// ── invalid_argument ─────────────────────────────────────────────────────

#[test]
fn invalid_argument_reasons_and_fields() {
    let cases = [
        (
            InvalidArgumentReason::InvalidModel,
            "model",
            "INVALID_MODEL",
            MODEL_RT,
        ),
        (
            InvalidArgumentReason::EmptyContent,
            "content",
            "EMPTY_CONTENT",
            MESSAGE_RT,
        ),
        (
            InvalidArgumentReason::InvalidTitle,
            "title",
            "INVALID_TITLE",
            CHAT_RT,
        ),
        (
            InvalidArgumentReason::InvalidReaction,
            "reaction",
            "INVALID_REACTION",
            MESSAGE_RT,
        ),
        (
            InvalidArgumentReason::InvalidAttachment,
            "attachment",
            "invalid_attachment",
            ATTACHMENT_RT,
        ),
        (
            InvalidArgumentReason::UnsupportedContentType,
            "content_type",
            "UNSUPPORTED_CONTENT_TYPE",
            ATTACHMENT_RT,
        ),
        (
            InvalidArgumentReason::VisionNotSupported,
            "content_type",
            "VISION_NOT_SUPPORTED",
            MESSAGE_RT,
        ),
        (
            InvalidArgumentReason::CodeInterpreterUnavailable,
            "file",
            "CODE_INTERPRETER_UNAVAILABLE",
            ATTACHMENT_RT,
        ),
        (
            InvalidArgumentReason::BoundaryRequired,
            "content_type",
            "BOUNDARY_REQUIRED",
            ATTACHMENT_RT,
        ),
        (
            InvalidArgumentReason::MultipartError,
            "multipart",
            "MULTIPART_ERROR",
            ATTACHMENT_RT,
        ),
        (
            InvalidArgumentReason::MissingFile,
            "file",
            "MISSING_FILE",
            ATTACHMENT_RT,
        ),
        (
            InvalidArgumentReason::MissingContentType,
            "content_type",
            "MISSING_CONTENT_TYPE",
            ATTACHMENT_RT,
        ),
    ];

    for (reason, field, code, rt) in cases {
        let problem = problem(DomainError::invalid_argument(reason));

        assert_shape(&problem, 400, "invalid_argument");
        assert_eq!(field_violation(&problem), (field, code), "{reason:?}");
        assert_eq!(problem.context["resource_type"], json!(rt), "{reason:?}");
    }
}

#[test]
fn invalid_argument_keeps_an_explicit_field() {
    let problem = problem(DomainError::InvalidArgument {
        field: "attachment_ids",
        reason: InvalidArgumentReason::InvalidAttachment,
    });

    assert_eq!(
        field_violation(&problem),
        ("attachment_ids", "invalid_attachment")
    );
}

#[test]
fn invalid_format_puts_the_message_in_detail_and_context_format() {
    let message = "The chat has too many attachments to delete at once";
    let problem = problem(DomainError::InvalidFormat {
        message: message.to_owned(),
    });

    assert_shape(&problem, 400, "invalid_argument");
    assert_eq!(problem.detail, message);
    assert_eq!(problem.context["format"], json!(message));
}

#[test]
fn odata_errors_keep_the_odata_resource_type_and_reason() {
    let problem = problem(DomainError::from(
        toolkit_odata::Error::InvalidOrderByField("secret_column".to_owned()),
    ));

    assert_shape(&problem, 400, "invalid_argument");
    assert_eq!(problem.context["resource_type"], json!(ODATA_RT));
    assert_eq!(
        field_violation(&problem),
        ("$orderby", "INVALID_ORDERBY_FIELD")
    );

    let cursor = problem_for_odata(toolkit_odata::Error::InvalidCursor);
    assert_eq!(field_violation(&cursor), ("cursor", "INVALID_CURSOR"));
}

#[test]
fn odata_db_error_is_internal_without_the_driver_text() {
    let problem = problem_for_odata(toolkit_odata::Error::Db(
        "connection to db.internal:5432 refused".to_owned(),
    ));

    assert_shape(&problem, 500, "internal");
    assert!(!problem.detail.contains("db.internal"), "{problem:?}");
}

fn problem_for_odata(err: toolkit_odata::Error) -> Problem {
    problem(DomainError::InvalidQuery(err))
}

// ── out_of_range ─────────────────────────────────────────────────────────

#[test]
fn out_of_range_reasons_and_fields() {
    for (reason, field, code) in [
        (
            OutOfRangeReason::FileTooLarge,
            "content_length",
            "FILE_TOO_LARGE",
        ),
        (
            OutOfRangeReason::TooManyImages,
            "image_count",
            "TOO_MANY_IMAGES",
        ),
        (OutOfRangeReason::InputTooLong, "content", "INPUT_TOO_LONG"),
        (
            OutOfRangeReason::ContextBudgetExceeded,
            "content",
            "CONTEXT_BUDGET_EXCEEDED",
        ),
    ] {
        let problem = problem(DomainError::out_of_range(reason));

        assert_shape(&problem, 400, "out_of_range");
        assert_eq!(field_violation(&problem), (field, code), "{reason:?}");
    }
}

// ── failed_precondition ──────────────────────────────────────────────────

#[test]
fn failed_precondition_subjects_and_types() {
    for (subject, wire_subject, wire_type) in [
        (
            PreconditionSubject::WebSearch,
            "web_search",
            "FEATURE_DISABLED",
        ),
        (PreconditionSubject::Images, "images", "FEATURE_DISABLED"),
        (PreconditionSubject::TurnState, "turn_state", "STATE"),
        (
            PreconditionSubject::ReactionTarget,
            "reaction_target",
            "STATE",
        ),
    ] {
        let problem = problem(DomainError::failed_precondition(subject));

        assert_shape(&problem, 400, "failed_precondition");
        let violation = &problem.context["violations"][0];
        assert_eq!(violation["subject"], json!(wire_subject));
        assert_eq!(violation["type"], json!(wire_type));
    }
}

// ── permission_denied / service_unavailable ──────────────────────────────

#[test]
fn permission_denied_has_authz_denied_reason() {
    let problem = problem(DomainError::PermissionDenied {
        resource: ResourceKind::Chat,
    });

    assert_shape(&problem, 403, "permission_denied");
    assert_eq!(problem.context["reason"], json!("AUTHZ_DENIED"));
}

#[test]
fn pdp_unavailable_is_503_with_retry_after_5() {
    let canonical = CanonicalError::from(DomainError::PdpUnavailable);
    assert_eq!(canonical.status_code(), 503);

    let problem = Problem::from(canonical);
    assert_shape(&problem, 503, "service_unavailable");
    assert_eq!(problem.context["retry_after_seconds"], json!(5));
}

#[test]
fn storage_and_upload_concurrency_retry_after() {
    let storage = problem(DomainError::storage_unavailable());
    assert_shape(&storage, 503, "service_unavailable");
    assert_eq!(storage.context["retry_after_seconds"], json!(10));

    let uploads = problem(DomainError::upload_concurrency_limit());
    assert_shape(&uploads, 503, "service_unavailable");
    assert_eq!(uploads.context["retry_after_seconds"], json!(5));
}

// ── aborted ──────────────────────────────────────────────────────────────

#[test]
fn aborted_reasons() {
    for (reason, code) in [
        (AbortReason::TurnAlreadyRunning, "turn_already_running"),
        (AbortReason::RequestIdConflict, "request_id_conflict"),
        (AbortReason::NotLatestTurn, "NOT_LATEST_TURN"),
        (AbortReason::GenerationInProgress, "GENERATION_IN_PROGRESS"),
        (AbortReason::Replay, "REPLAY"),
    ] {
        let problem = problem(DomainError::aborted(reason));

        assert_shape(&problem, 409, "aborted");
        assert_eq!(problem.context["reason"], json!(code));
    }
}

// ── already_exists ───────────────────────────────────────────────────────

#[test]
fn already_exists_reports_the_conflict_code_as_resource_name() {
    for (err, code, rt) in [
        (
            DomainError::attachment_locked(),
            "attachment_locked",
            ATTACHMENT_RT,
        ),
        (
            DomainError::provider_mismatch(),
            "provider_mismatch",
            ATTACHMENT_RT,
        ),
        (
            DomainError::unique_violation(ResourceKind::Chat),
            "unique_violation",
            CHAT_RT,
        ),
    ] {
        let problem = problem(err);

        assert_shape(&problem, 409, "already_exists");
        assert_eq!(problem.context["resource_name"], json!(code));
        assert_eq!(problem.context["resource_type"], json!(rt));
    }
}

// ── resource_exhausted ───────────────────────────────────────────────────

#[test]
fn quota_exceeded_carries_the_scope_as_subject() {
    for (scope, subject) in [
        (QuotaScope::Tokens, "tokens"),
        (QuotaScope::WebSearch, "web_search"),
        (QuotaScope::CodeInterpreter, "code_interpreter"),
    ] {
        let problem = problem(DomainError::quota_exceeded(scope));

        assert_shape(&problem, 429, "resource_exhausted");
        let violation = &problem.context["violations"][0];
        assert_eq!(violation["subject"], json!(subject));
        assert_eq!(violation["description"], json!("quota_exceeded"));
    }
}

#[test]
fn per_chat_limits_are_resource_exhausted() {
    for (limit, subject) in [
        (ExhaustedLimit::DocumentLimit, "document_limit"),
        (ExhaustedLimit::StorageLimit, "storage_limit"),
    ] {
        let problem = problem(DomainError::ResourceExhausted { limit });

        assert_shape(&problem, 429, "resource_exhausted");
        assert_eq!(problem.context["violations"][0]["subject"], json!(subject));
        assert_eq!(problem.context["resource_type"], json!(ATTACHMENT_RT));
    }
}

// ── internal ─────────────────────────────────────────────────────────────

#[test]
fn internal_never_serializes_the_diagnostic() {
    let diagnostic = "duplicate key value violates unique constraint chat_turns_pkey (id=42)";
    let canonical = CanonicalError::from(DomainError::internal(diagnostic));
    assert_eq!(canonical.diagnostic(), Some(diagnostic));

    let problem = Problem::from(canonical);
    assert_shape(&problem, 500, "internal");
    let wire = serde_json::to_string(&problem).unwrap_or_default();
    assert!(!wire.contains("chat_turns_pkey"), "{wire}");
    assert!(!wire.contains("id=42"), "{wire}");
}

#[test]
fn problem_has_no_top_level_code() {
    let problem = problem(DomainError::aborted(AbortReason::NotLatestTurn));
    let wire: Value = serde_json::to_value(&problem).unwrap_or_default();

    assert!(wire.get("code").is_none(), "{wire}");
    for key in ["type", "title", "status", "detail", "context"] {
        assert!(wire.get(key).is_some(), "missing {key}: {wire}");
    }
}
