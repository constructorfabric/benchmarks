#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of `cpt-cf-mini-chat-algo-gear-foundation-map-error` and
//! `cpt-cf-mini-chat-dod-gear-foundation-error-contract`: every row of the
//! mapping table.

use axum::response::IntoResponse;
use serde_json::Value;
use toolkit_canonical_errors::{CanonicalError, Problem};

use crate::domain::error::{
    DisabledFeature, DomainError, MultipartIssue, QuotaSubject, ResourceKind,
};

const SENTINELS: [&str; 6] = [
    "file-abc123",
    "file_xyz789",
    "assistant-77",
    "vs_9f8e7d",
    "sk-secret-token",
    "SELECT * FROM chats",
];

fn leaky() -> String {
    SENTINELS.join(" | ")
}

/// Renders the error as the wire `Problem`: `(status, body)`.
fn problem(err: DomainError) -> (u16, Value) {
    let problem = Problem::from(CanonicalError::from(err));
    let status = problem.status.expect("status");
    (
        status,
        serde_json::to_value(&problem).expect("serialize problem"),
    )
}

fn category(body: &Value) -> String {
    let ty = body["type"].as_str().expect("type");
    ty.rsplit("cf.core.err.")
        .next()
        .and_then(|rest| rest.strip_suffix(".v1~"))
        .unwrap_or_else(|| panic!("unexpected problem type `{ty}`"))
        .to_owned()
}

fn field_violations(body: &Value) -> Vec<(String, String)> {
    body["context"]["field_violations"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|v| {
                    (
                        v["field"].as_str().unwrap_or_default().to_owned(),
                        v["reason"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

fn has_reason(body: &Value, reason: &str) -> bool {
    field_violations(body).iter().any(|(_, r)| r == reason)
}

fn retry_after_header(err: DomainError) -> Option<String> {
    let response = CanonicalError::from(err).into_response();
    response
        .headers()
        .get(http::header::RETRY_AFTER)
        .map(|v| v.to_str().expect("ascii header").to_owned())
}

/// One instance of every variant of the domain error.
fn every_error() -> Vec<DomainError> {
    vec![
        DomainError::NotFound {
            resource: ResourceKind::Chat,
        },
        DomainError::NotFound {
            resource: ResourceKind::Model,
        },
        DomainError::InvalidModel,
        DomainError::EmptyContent,
        DomainError::InvalidTitle,
        DomainError::InvalidReaction,
        DomainError::InvalidQuery(toolkit_odata::Error::InvalidFilter("x".to_owned())),
        DomainError::UnsupportedContentType,
        DomainError::CodeInterpreterUnavailable,
        DomainError::InvalidMultipart(MultipartIssue::BoundaryRequired),
        DomainError::CleanupPayloadTooLarge {
            detail: "cleanup payload exceeds the outbox limit".to_owned(),
        },
        DomainError::VisionNotSupported,
        DomainError::InvalidAttachmentIds,
        DomainError::FileTooLarge,
        DomainError::TooManyImages,
        DomainError::InputTooLong,
        DomainError::ContextBudgetExceeded,
        DomainError::FeatureDisabled(DisabledFeature::WebSearch),
        DomainError::TurnNotTerminal,
        DomainError::ReactionTargetNotAssistant,
        DomainError::AuthzDenied,
        DomainError::PdpUnavailable,
        DomainError::TurnAlreadyRunning,
        DomainError::RequestIdConflict,
        DomainError::NotLatestTurn,
        DomainError::GenerationInProgress,
        DomainError::Replay,
        DomainError::AttachmentLocked,
        DomainError::ProviderMismatch,
        DomainError::UniqueViolation,
        DomainError::QuotaExceeded(QuotaSubject::Tokens),
        DomainError::DocumentLimit,
        DomainError::StorageLimit,
        DomainError::StorageUnavailable,
        DomainError::UploadConcurrencyLimit,
        DomainError::ProviderResolution(leaky()),
        DomainError::PolicyResolution(leaky()),
        DomainError::Database(leaky()),
        DomainError::Internal(leaky()),
    ]
}

// ---- not_found -------------------------------------------------------------------

#[test]
fn not_found_reports_the_gts_resource_type_of_each_kind() {
    for (resource, gts) in [
        (ResourceKind::Chat, "gts.cf.core.mini_chat.chat.v1~"),
        (ResourceKind::Message, "gts.cf.core.mini_chat.message.v1~"),
        (ResourceKind::Turn, "gts.cf.core.mini_chat.turn.v1~"),
        (
            ResourceKind::Attachment,
            "gts.cf.core.mini_chat.attachment.v1~",
        ),
        (ResourceKind::Model, "gts.cf.core.mini_chat.model.v1~"),
    ] {
        assert_eq!(resource.gts_type(), gts);

        let (status, body) = problem(DomainError::NotFound { resource });

        assert_eq!(status, 404, "{resource:?}");
        assert_eq!(category(&body), "not_found");
        assert_eq!(body["context"]["resource_type"], gts);
    }
}

// ---- invalid_argument ----------------------------------------------------------------

#[test]
fn invalid_argument_rows_carry_field_and_reason() {
    let rows: Vec<(DomainError, &str, &str)> = vec![
        (DomainError::InvalidModel, "model", "INVALID_MODEL"),
        (DomainError::EmptyContent, "content", "EMPTY_CONTENT"),
        (DomainError::InvalidTitle, "title", "INVALID_TITLE"),
        (DomainError::InvalidReaction, "reaction", "INVALID_REACTION"),
        (
            DomainError::InvalidAttachmentIds,
            "attachment",
            "invalid_attachment",
        ),
        (
            DomainError::CodeInterpreterUnavailable,
            "file",
            "CODE_INTERPRETER_UNAVAILABLE",
        ),
    ];

    for (err, field, reason) in rows {
        let label = format!("{err:?}");

        let (status, body) = problem(err);

        assert_eq!(status, 400, "{label}");
        assert_eq!(category(&body), "invalid_argument", "{label}");
        assert!(
            field_violations(&body).contains(&(field.to_owned(), reason.to_owned())),
            "{label}: expected field `{field}` reason `{reason}` in {body}"
        );
    }
}

#[test]
fn code_interpreter_unavailable_uses_the_attachment_resource_type() {
    let (_, body) = problem(DomainError::CodeInterpreterUnavailable);

    assert_eq!(
        body["context"]["resource_type"],
        "gts.cf.core.mini_chat.attachment.v1~"
    );
}

#[test]
fn upload_and_multipart_rows_carry_their_reason() {
    let mut rows: Vec<(DomainError, &str)> = vec![
        (
            DomainError::UnsupportedContentType,
            "UNSUPPORTED_CONTENT_TYPE",
        ),
        (DomainError::VisionNotSupported, "VISION_NOT_SUPPORTED"),
    ];
    for (issue, reason) in [
        (MultipartIssue::BoundaryRequired, "BOUNDARY_REQUIRED"),
        (MultipartIssue::MultipartError, "MULTIPART_ERROR"),
        (MultipartIssue::MissingFile, "MISSING_FILE"),
        (MultipartIssue::MissingContentType, "MISSING_CONTENT_TYPE"),
    ] {
        rows.push((DomainError::InvalidMultipart(issue), reason));
    }

    for (err, reason) in rows {
        let label = format!("{err:?}");

        let (status, body) = problem(err);

        assert_eq!(status, 400, "{label}");
        assert_eq!(category(&body), "invalid_argument", "{label}");
        assert!(
            has_reason(&body, reason),
            "{label}: reason `{reason}` in {body}"
        );
    }
}

#[test]
fn cleanup_payload_too_large_mirrors_the_detail_in_context_format() {
    let detail = "cleanup payload exceeds the outbox limit";

    let (status, body) = problem(DomainError::CleanupPayloadTooLarge {
        detail: detail.to_owned(),
    });

    assert_eq!(status, 400);
    assert_eq!(category(&body), "invalid_argument");
    assert_eq!(body["context"]["format"], detail);
}

#[test]
fn bad_odata_query_uses_the_odata_resource_type_and_library_reason() {
    let (status, body) = problem(DomainError::InvalidQuery(
        toolkit_odata::Error::InvalidFilter("unbalanced".to_owned()),
    ));

    assert_eq!(status, 400);
    assert_eq!(category(&body), "invalid_argument");
    assert_eq!(
        body["context"]["resource_type"],
        "gts.cf.core.odata.query.v1~"
    );
    assert!(has_reason(&body, "INVALID_FILTER"), "{body}");
}

// ---- out_of_range -----------------------------------------------------------------------

#[test]
fn out_of_range_rows_carry_field_violations() {
    let rows: Vec<(DomainError, Option<&str>, &str)> = vec![
        (
            DomainError::FileTooLarge,
            Some("content_length"),
            "FILE_TOO_LARGE",
        ),
        (
            DomainError::TooManyImages,
            Some("image_count"),
            "TOO_MANY_IMAGES",
        ),
        (DomainError::InputTooLong, None, "INPUT_TOO_LONG"),
        (
            DomainError::ContextBudgetExceeded,
            None,
            "CONTEXT_BUDGET_EXCEEDED",
        ),
    ];

    for (err, field, reason) in rows {
        let label = format!("{err:?}");

        let (status, body) = problem(err);

        assert_eq!(status, 400, "{label}");
        assert_eq!(category(&body), "out_of_range", "{label}");
        assert!(
            has_reason(&body, reason),
            "{label}: reason `{reason}` in {body}"
        );
        if let Some(field) = field {
            assert!(
                field_violations(&body).contains(&(field.to_owned(), reason.to_owned())),
                "{label}: field `{field}` in {body}"
            );
        }
    }
}

// ---- failed_precondition ----------------------------------------------------------------

#[test]
fn failed_precondition_rows_carry_subject_and_type() {
    let rows: Vec<(DomainError, &str, &str)> = vec![
        (
            DomainError::FeatureDisabled(DisabledFeature::WebSearch),
            "web_search",
            "FEATURE_DISABLED",
        ),
        (
            DomainError::FeatureDisabled(DisabledFeature::Images),
            "images",
            "FEATURE_DISABLED",
        ),
        (DomainError::TurnNotTerminal, "turn_state", "STATE"),
        (
            DomainError::ReactionTargetNotAssistant,
            "reaction_target",
            "STATE",
        ),
    ];

    for (err, subject, type_) in rows {
        let label = format!("{err:?}");

        let (status, body) = problem(err);

        assert_eq!(status, 400, "{label}");
        assert_eq!(category(&body), "failed_precondition", "{label}");
        let violations = body["context"]["violations"]
            .as_array()
            .expect("violations");
        assert!(
            violations
                .iter()
                .any(|v| v["subject"] == subject && v["type"] == type_),
            "{label}: expected subject `{subject}` type `{type_}` in {body}"
        );
        assert!(
            violations
                .iter()
                .all(|v| v["description"].as_str().is_some()),
            "{label}: description is present"
        );
    }
}

// ---- permission_denied / service_unavailable ------------------------------------------------

#[test]
fn authz_denied_is_403_permission_denied_with_the_reason() {
    let (status, body) = problem(DomainError::AuthzDenied);

    assert_eq!(status, 403);
    assert_eq!(category(&body), "permission_denied");
    assert_eq!(body["context"]["reason"], "AUTHZ_DENIED");
}

#[test]
fn pdp_unavailable_is_503_with_retry_after_5() {
    let (status, body) = problem(DomainError::PdpUnavailable);

    assert_eq!(status, 503);
    assert_eq!(category(&body), "service_unavailable");
    assert_eq!(body["context"]["retry_after_seconds"], 5);
    assert_eq!(
        retry_after_header(DomainError::PdpUnavailable).as_deref(),
        Some("5")
    );
}

#[test]
fn pdp_unavailable_detail_is_generic() {
    let (_, body) = problem(DomainError::PdpUnavailable);

    let detail = body["detail"].as_str().expect("detail").to_lowercase();
    for leaked in ["deny_reason", "timeout", "evaluate", "authz-resolver"] {
        assert!(
            !detail.contains(leaked),
            "detail leaks `{leaked}`: {detail}"
        );
    }
}

#[test]
fn storage_and_upload_limits_are_503_with_their_retry_after() {
    for (make, seconds) in [
        (
            (|| DomainError::StorageUnavailable) as fn() -> DomainError,
            10_u64,
        ),
        (|| DomainError::UploadConcurrencyLimit, 5),
    ] {
        let label = format!("{:?}", make());

        let (status, body) = problem(make());

        assert_eq!(status, 503, "{label}");
        assert_eq!(category(&body), "service_unavailable", "{label}");
        assert_eq!(body["context"]["retry_after_seconds"], seconds, "{label}");
        assert_eq!(
            retry_after_header(make()).as_deref(),
            Some(seconds.to_string().as_str()),
            "{label}: Retry-After header equals context.retry_after_seconds"
        );
    }
}

// ---- aborted / already_exists ----------------------------------------------------------------

#[test]
fn conflict_rows_are_409_aborted_with_a_reason() {
    let rows: Vec<(DomainError, &str)> = vec![
        (DomainError::TurnAlreadyRunning, "turn_already_running"),
        (DomainError::RequestIdConflict, "request_id_conflict"),
        (DomainError::NotLatestTurn, "NOT_LATEST_TURN"),
        (DomainError::GenerationInProgress, "GENERATION_IN_PROGRESS"),
        (DomainError::Replay, "REPLAY"),
    ];

    for (err, reason) in rows {
        let label = format!("{err:?}");

        let (status, body) = problem(err);

        assert_eq!(status, 409, "{label}");
        assert_eq!(category(&body), "aborted", "{label}");
        assert_eq!(body["context"]["reason"], reason, "{label}");
    }
}

#[test]
fn unique_conflicts_are_409_already_exists_with_a_resource_name() {
    let rows: Vec<(DomainError, &str)> = vec![
        (DomainError::AttachmentLocked, "attachment_locked"),
        (DomainError::ProviderMismatch, "provider_mismatch"),
        (DomainError::UniqueViolation, "unique_violation"),
    ];

    for (err, name) in rows {
        let label = format!("{err:?}");

        let (status, body) = problem(err);

        assert_eq!(status, 409, "{label}");
        assert_eq!(category(&body), "already_exists", "{label}");
        assert_eq!(body["context"]["resource_name"], name, "{label}");
    }
}

// ---- resource_exhausted -------------------------------------------------------------------

#[test]
fn quota_exhaustion_is_429_with_subject_and_quota_exceeded() {
    for (subject, name) in [
        (QuotaSubject::Tokens, "tokens"),
        (QuotaSubject::WebSearch, "web_search"),
        (QuotaSubject::CodeInterpreter, "code_interpreter"),
    ] {
        let (status, body) = problem(DomainError::QuotaExceeded(subject));

        assert_eq!(status, 429, "{subject:?}");
        assert_eq!(category(&body), "resource_exhausted");
        let violations = body["context"]["violations"]
            .as_array()
            .expect("violations");
        assert!(
            violations
                .iter()
                .any(|v| v["subject"] == name && v["description"] == "quota_exceeded"),
            "{subject:?}: {body}"
        );
    }
}

#[test]
fn chat_limits_are_429_with_the_limit_reason() {
    for (err, reason) in [
        (DomainError::DocumentLimit, "document_limit"),
        (DomainError::StorageLimit, "storage_limit"),
    ] {
        let (status, body) = problem(err);

        assert_eq!(status, 429, "{reason}");
        assert_eq!(category(&body), "resource_exhausted");
        assert!(
            body["context"]["violations"].to_string().contains(reason),
            "{reason} in {body}"
        );
    }
}

// ---- internal ------------------------------------------------------------------------------

#[test]
fn internal_class_errors_are_500_with_a_generic_detail() {
    let rows: Vec<DomainError> = vec![
        DomainError::ProviderResolution(leaky()),
        DomainError::PolicyResolution(leaky()),
        DomainError::Database(leaky()),
        DomainError::Internal(leaky()),
    ];

    for err in rows {
        let label = format!("{err:?}");

        let (status, body) = problem(err);

        assert_eq!(status, 500, "{label}");
        assert_eq!(category(&body), "internal");
        let rendered = body.to_string();
        for sentinel in SENTINELS {
            assert!(
                !rendered.contains(sentinel),
                "{label}: `{sentinel}` leaked into {rendered}"
            );
        }
    }
}

// ---- cross-cutting -------------------------------------------------------------------------

#[test]
fn no_variant_emits_a_top_level_code_field() {
    for err in every_error() {
        let label = format!("{err:?}");

        let (_, body) = problem(err);

        assert!(
            body.get("code").is_none(),
            "{label}: top-level `code` present in {body}"
        );
        assert!(
            body["type"]
                .as_str()
                .is_some_and(|t| t.starts_with("gts://gts.cf.core.errors.err.v1~cf.core.err.")),
            "{label}: canonical type in {body}"
        );
    }
}

#[test]
fn no_variant_leaks_provider_ids_or_secrets_in_the_wire_body() {
    for err in every_error() {
        let label = format!("{err:?}");

        let (_, body) = problem(err);

        let rendered = body.to_string();
        for sentinel in SENTINELS {
            assert!(
                !rendered.contains(sentinel),
                "{label}: `{sentinel}` leaked into {rendered}"
            );
        }
    }
}

#[test]
fn only_internal_class_errors_map_to_500() {
    for err in every_error() {
        let internal_class = matches!(
            err,
            DomainError::ProviderResolution(_)
                | DomainError::PolicyResolution(_)
                | DomainError::Database(_)
                | DomainError::Internal(_)
        );
        let label = format!("{err:?}");

        let (status, body) = problem(err);

        assert!(
            matches!(status, 400 | 403 | 404 | 409 | 429 | 500 | 503),
            "{label}: unexpected status {status} in {body}"
        );
        assert_eq!(
            status == 500,
            internal_class,
            "{label}: status {status} in {body}"
        );
        assert_eq!(category(&body) == "internal", internal_class, "{label}");
    }
}
