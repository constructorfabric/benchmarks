#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 1 and 5: the attachment error contract
//! (`cpt-cf-mini-chat-dod-attachments-error-contract`): the field per multipart
//! reason (closes foundation finding K-TCS-001), the limit violation shape,
//! `retry_after_seconds` of the two 503s and the 409 names.

use axum::response::IntoResponse;
use serde_json::Value;
use toolkit_canonical_errors::{CanonicalError, Problem};

use crate::domain::error::{DisabledFeature, DomainError, MultipartIssue};

const ATTACHMENT_TYPE: &str = "gts.cf.core.mini_chat.attachment.v1~";

fn problem(err: DomainError) -> (u16, Value) {
    let problem = Problem::from(CanonicalError::from(err));
    (
        problem.status.expect("status"),
        serde_json::to_value(&problem).expect("serialize problem"),
    )
}

fn violations(body: &Value) -> Vec<(String, String)> {
    body["context"]["field_violations"]
        .as_array()
        .expect("field_violations")
        .iter()
        .map(|v| {
            (
                v["field"].as_str().unwrap().to_owned(),
                v["reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn each_multipart_reason_names_its_own_field() {
    for (issue, field, reason) in [
        (
            MultipartIssue::BoundaryRequired,
            "content_type",
            "BOUNDARY_REQUIRED",
        ),
        (
            MultipartIssue::MissingContentType,
            "content_type",
            "MISSING_CONTENT_TYPE",
        ),
        (
            MultipartIssue::MultipartError,
            "multipart",
            "MULTIPART_ERROR",
        ),
        (MultipartIssue::MissingFile, "file", "MISSING_FILE"),
    ] {
        let (status, body) = problem(DomainError::InvalidMultipart(issue));

        assert_eq!(status, 400, "{reason}");
        assert!(body["type"].as_str().unwrap().contains("invalid_argument"));
        assert_eq!(
            violations(&body),
            vec![(field.to_owned(), reason.to_owned())],
            "{reason}"
        );
        assert_eq!(
            body["context"]["resource_type"], ATTACHMENT_TYPE,
            "{reason}"
        );
    }
}

#[test]
fn unsupported_content_type_and_code_interpreter_unavailable_use_the_file_field() {
    for (err, reason) in [
        (
            DomainError::UnsupportedContentType,
            "UNSUPPORTED_CONTENT_TYPE",
        ),
        (
            DomainError::CodeInterpreterUnavailable,
            "CODE_INTERPRETER_UNAVAILABLE",
        ),
    ] {
        let (status, body) = problem(err);

        assert_eq!(status, 400, "{reason}");
        assert_eq!(
            violations(&body),
            vec![("file".to_owned(), reason.to_owned())]
        );
        assert_eq!(body["context"]["resource_type"], ATTACHMENT_TYPE);
    }
}

#[test]
fn file_too_large_is_out_of_range_on_content_length_of_the_attachment() {
    let (status, body) = problem(DomainError::FileTooLarge);

    assert_eq!(status, 400);
    assert!(body["type"].as_str().unwrap().contains("out_of_range"));
    assert_eq!(
        violations(&body),
        vec![("content_length".to_owned(), "FILE_TOO_LARGE".to_owned())]
    );
    assert_eq!(body["context"]["resource_type"], ATTACHMENT_TYPE);
}

#[test]
fn the_images_kill_switch_is_a_failed_precondition_on_the_images_subject() {
    let (status, body) = problem(DomainError::FeatureDisabled(DisabledFeature::Images));

    assert_eq!(status, 400);
    assert!(
        body["type"]
            .as_str()
            .unwrap()
            .contains("failed_precondition")
    );
    let violation = &body["context"]["violations"][0];
    assert_eq!(violation["subject"], "images");
    assert_eq!(violation["type"], "FEATURE_DISABLED");
}

#[test]
fn a_chat_limit_violation_repeats_the_reason_as_subject_and_description() {
    for (err, reason) in [
        (DomainError::DocumentLimit, "document_limit"),
        (DomainError::StorageLimit, "storage_limit"),
    ] {
        let (status, body) = problem(err);

        assert_eq!(status, 429, "{reason}");
        assert!(
            body["type"]
                .as_str()
                .unwrap()
                .contains("resource_exhausted")
        );
        let list = body["context"]["violations"].as_array().unwrap();
        assert_eq!(list.len(), 1, "{reason}");
        assert_eq!(list[0]["subject"], reason);
        assert_eq!(list[0]["description"], reason);
    }
}

#[test]
fn the_two_503s_carry_their_retry_after_in_the_header_and_the_context() {
    for (err, seconds) in [
        (DomainError::UploadConcurrencyLimit, 5),
        (DomainError::StorageUnavailable, 10),
    ] {
        let label = format!("{err:?}");
        let response = CanonicalError::from(match &err {
            DomainError::UploadConcurrencyLimit => DomainError::UploadConcurrencyLimit,
            _ => DomainError::StorageUnavailable,
        })
        .into_response();
        let (status, body) = problem(err);

        assert_eq!(status, 503, "{label}");
        assert_eq!(body["context"]["retry_after_seconds"], seconds, "{label}");
        assert_eq!(
            response
                .headers()
                .get("retry-after")
                .unwrap()
                .to_str()
                .unwrap(),
            seconds.to_string(),
            "{label}"
        );
        assert_eq!(body["detail"], "Service temporarily unavailable", "{label}");
    }
}

#[test]
fn locked_and_mismatched_attachments_are_conflicts_named_after_their_reason() {
    for (err, name) in [
        (DomainError::AttachmentLocked, "attachment_locked"),
        (DomainError::ProviderMismatch, "provider_mismatch"),
    ] {
        let (status, body) = problem(err);

        assert_eq!(status, 409, "{name}");
        assert!(body["type"].as_str().unwrap().contains("already_exists"));
        assert_eq!(body["context"]["resource_name"], name);
        assert_eq!(body["context"]["resource_type"], ATTACHMENT_TYPE);
    }
}

#[test]
fn the_invalid_model_violation_is_on_the_model_field() {
    let (status, body) = problem(DomainError::InvalidModel);

    assert_eq!(status, 400);
    assert_eq!(
        violations(&body),
        vec![("model".to_owned(), "INVALID_MODEL".to_owned())]
    );
}
