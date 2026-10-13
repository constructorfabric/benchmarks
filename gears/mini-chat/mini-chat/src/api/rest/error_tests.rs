#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{Value, json};
use toolkit_canonical_errors::{CanonicalError, Problem};
use uuid::Uuid;

use crate::domain::error::{
    DisabledFeature, DomainError, MultipartIssue, ProtectedResource, QuotaScope,
};

fn render(err: DomainError) -> (u16, Value) {
    let canonical = CanonicalError::from(err);
    let status = canonical.status_code();
    let problem = Problem::from(canonical);
    (status, serde_json::to_value(problem).unwrap())
}

fn category(problem: &Value) -> &str {
    let kind = problem["type"].as_str().unwrap();
    kind.rsplit_once("cf.core.err.")
        .unwrap()
        .1
        .trim_end_matches(".v1~")
}

/// `context.field_violations[0].reason` of an `invalid_argument` or `out_of_range` problem.
fn field_reason(problem: &Value) -> (&str, &str) {
    let violation = &problem["context"]["field_violations"][0];
    (
        violation["field"].as_str().unwrap(),
        violation["reason"].as_str().unwrap(),
    )
}

#[test]
fn missing_resources_are_404_and_name_their_resource_type() {
    let id = Uuid::new_v4();
    let cases = [
        (DomainError::ChatNotFound { chat_id: id }, "chat"),
        (DomainError::MessageNotFound { message_id: id }, "message"),
        (DomainError::TurnNotFound { request_id: id }, "turn"),
        (
            DomainError::AttachmentNotFound { attachment_id: id },
            "attachment",
        ),
        (
            DomainError::ModelNotFound {
                model_id: "m".to_owned(),
            },
            "model",
        ),
    ];

    for (err, resource) in cases {
        let (status, problem) = render(err);
        assert_eq!(status, 404, "{resource}");
        assert_eq!(category(&problem), "not_found");
        assert_eq!(
            problem["context"]["resource_type"],
            json!(format!("gts.cf.core.mini_chat.{resource}.v1~"))
        );
    }
}

#[test]
fn validation_errors_are_400_with_field_reasons() {
    let cases = [
        (
            DomainError::InvalidModel {
                detail: "x".to_owned(),
            },
            ("model", "INVALID_MODEL"),
        ),
        (
            DomainError::InvalidTitle {
                detail: "x".to_owned(),
            },
            ("title", "INVALID_TITLE"),
        ),
        (DomainError::EmptyContent, ("content", "EMPTY_CONTENT")),
        (
            DomainError::InvalidReaction,
            ("reaction", "INVALID_REACTION"),
        ),
        (
            DomainError::UnsupportedContentType,
            ("content_type", "UNSUPPORTED_CONTENT_TYPE"),
        ),
        (
            DomainError::CodeInterpreterUnavailable,
            ("file", "CODE_INTERPRETER_UNAVAILABLE"),
        ),
        (
            DomainError::VisionNotSupported,
            ("content_type", "VISION_NOT_SUPPORTED"),
        ),
        (
            DomainError::InvalidAttachment,
            ("attachment", "invalid_attachment"),
        ),
    ];

    for (err, expected) in cases {
        let (status, problem) = render(err);
        assert_eq!(status, 400);
        assert_eq!(category(&problem), "invalid_argument");
        assert_eq!(field_reason(&problem), expected);
    }
}

#[test]
fn multipart_issues_report_their_own_reason() {
    let cases = [
        (
            MultipartIssue::BoundaryRequired,
            ("content_type", "BOUNDARY_REQUIRED"),
        ),
        (MultipartIssue::Malformed, ("multipart", "MULTIPART_ERROR")),
        (MultipartIssue::MissingFile, ("file", "MISSING_FILE")),
        (
            MultipartIssue::MissingContentType,
            ("content_type", "MISSING_CONTENT_TYPE"),
        ),
    ];

    for (issue, expected) in cases {
        let (status, problem) = render(DomainError::InvalidMultipart { issue });
        assert_eq!(status, 400);
        assert_eq!(field_reason(&problem), expected);
        assert_eq!(
            problem["context"]["resource_type"],
            json!("gts.cf.core.mini_chat.attachment.v1~")
        );
    }
}

#[test]
fn range_errors_are_400_out_of_range() {
    let cases = [
        (
            DomainError::FileTooLarge,
            ("content_length", "FILE_TOO_LARGE"),
        ),
        (
            DomainError::TooManyImages,
            ("image_count", "TOO_MANY_IMAGES"),
        ),
        (DomainError::InputTooLong, ("content", "INPUT_TOO_LONG")),
        (
            DomainError::ContextBudgetExceeded,
            ("context", "CONTEXT_BUDGET_EXCEEDED"),
        ),
    ];

    for (err, expected) in cases {
        let (status, problem) = render(err);
        assert_eq!(status, 400);
        assert_eq!(category(&problem), "out_of_range");
        assert_eq!(field_reason(&problem), expected);
    }
}

#[test]
fn failed_preconditions_are_400_with_typed_violations() {
    let cases = [
        (
            DomainError::FeatureDisabled {
                feature: DisabledFeature::WebSearch,
            },
            ("web_search", "FEATURE_DISABLED"),
        ),
        (
            DomainError::FeatureDisabled {
                feature: DisabledFeature::Images,
            },
            ("images", "FEATURE_DISABLED"),
        ),
        (DomainError::InvalidTurnState, ("turn_state", "STATE")),
        (
            DomainError::InvalidReactionTarget,
            ("reaction_target", "STATE"),
        ),
    ];

    for (err, (subject, kind)) in cases {
        let (status, problem) = render(err);
        assert_eq!(status, 400);
        assert_eq!(category(&problem), "failed_precondition");
        let violation = &problem["context"]["violations"][0];
        assert_eq!(violation["subject"], json!(subject));
        assert_eq!(violation["type"], json!(kind));
    }
}

#[test]
fn authorization_failures_are_403_and_503_with_retry_after() {
    for resource in [
        ProtectedResource::Chat,
        ProtectedResource::Model,
        ProtectedResource::UserQuota,
    ] {
        let (status, problem) = render(DomainError::AuthzDenied { resource });
        assert_eq!(status, 403);
        assert_eq!(category(&problem), "permission_denied");
        assert_eq!(problem["context"]["reason"], json!("AUTHZ_DENIED"));
    }

    let (status, problem) = render(DomainError::AuthzUnavailable);
    assert_eq!(status, 503);
    assert_eq!(category(&problem), "service_unavailable");
    assert_eq!(problem["context"]["retry_after_seconds"], json!(5));
}

#[test]
fn conflicts_are_409_with_reasons_or_resource_names() {
    let aborted = [
        (DomainError::TurnAlreadyRunning, "turn_already_running"),
        (DomainError::RequestIdConflict, "request_id_conflict"),
        (DomainError::NotLatestTurn, "NOT_LATEST_TURN"),
        (DomainError::GenerationInProgress, "GENERATION_IN_PROGRESS"),
        (DomainError::Replay, "REPLAY"),
    ];
    for (err, reason) in aborted {
        let (status, problem) = render(err);
        assert_eq!(status, 409);
        assert_eq!(category(&problem), "aborted");
        assert_eq!(problem["context"]["reason"], json!(reason));
    }

    let exists = [
        (DomainError::AttachmentLocked, "attachment_locked"),
        (DomainError::ProviderMismatch, "provider_mismatch"),
        (DomainError::UniqueViolation, "unique_violation"),
    ];
    for (err, name) in exists {
        let (status, problem) = render(err);
        assert_eq!(status, 409);
        assert_eq!(category(&problem), "already_exists");
        assert_eq!(problem["context"]["resource_name"], json!(name));
    }
}

#[test]
fn quota_and_limit_errors_are_429_and_503() {
    for (scope, subject) in [
        (QuotaScope::Tokens, "tokens"),
        (QuotaScope::WebSearch, "web_search"),
        (QuotaScope::CodeInterpreter, "code_interpreter"),
    ] {
        let (status, problem) = render(DomainError::QuotaExceeded { scope });
        assert_eq!(status, 429);
        assert_eq!(category(&problem), "resource_exhausted");
        assert_eq!(
            problem["context"]["violations"][0]["subject"],
            json!(subject)
        );
        assert_eq!(
            problem["context"]["violations"][0]["description"],
            json!("quota_exceeded")
        );
    }
    for (err, subject) in [
        (DomainError::DocumentLimit, "document_limit"),
        (DomainError::StorageLimit, "storage_limit"),
    ] {
        let (status, problem) = render(err);
        assert_eq!(status, 429);
        assert_eq!(
            problem["context"]["violations"][0]["subject"],
            json!(subject)
        );
    }

    let (status, problem) = render(DomainError::StorageUnavailable);
    assert_eq!(
        (status, &problem["context"]["retry_after_seconds"]),
        (503, &json!(10))
    );
    let (status, problem) = render(DomainError::UploadConcurrencyLimit);
    assert_eq!(
        (status, &problem["context"]["retry_after_seconds"]),
        (503, &json!(5))
    );
}

#[test]
fn an_oversized_cleanup_payload_is_a_400_with_the_detail_as_format() {
    let (status, problem) = render(DomainError::OutboxPayloadTooLarge {
        detail: "too big".to_owned(),
    });

    assert_eq!(status, 400);
    assert_eq!(category(&problem), "invalid_argument");
    assert_eq!(problem["context"]["format"], json!("too big"));
    assert_eq!(problem["detail"], json!("too big"));
}

#[test]
fn internal_failures_are_opaque_500s() {
    for err in [
        DomainError::internal("connection string with a secret"),
        DomainError::PolicyUnavailable {
            diagnostic: "plugin down".to_owned(),
        },
    ] {
        let (status, problem) = render(err);
        assert_eq!(status, 500);
        assert_eq!(category(&problem), "internal");
        let text = problem.to_string();
        assert!(!text.contains("secret"), "{text}");
        assert!(!text.contains("plugin down"), "{text}");
    }
}

#[test]
fn odata_errors_keep_the_query_resource_type() {
    let (status, problem) = render(DomainError::from(toolkit_odata::Error::InvalidCursor));

    assert_eq!(status, 400);
    assert_eq!(
        problem["context"]["resource_type"],
        json!("gts.cf.core.odata.query.v1~")
    );
    assert_eq!(field_reason(&problem), ("cursor", "INVALID_CURSOR"));
}
