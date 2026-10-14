//! The attachment error contract: every upload failure of the FEATURE maps to its canonical
//! Problem (`type`, `title`, `status`, `detail`, `instance`, `trace_id`, `context`, no top-level
//! `code`) without provider identifiers, provider text or the row `error_code`.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`;
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-error-contract`; definitions of
//! done `cpt-cf-mini-chat-dod-attachments-and-file-search-errors-observability` and
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-upload-intake`. Slice
//! S2-deletion-cleanup-tools adds `inst-err-locked` (the 409 `attachment_locked` of the delete
//! flow, `cpt-cf-mini-chat-dod-attachments-and-file-search-delete-api`) and the 500 of an
//! oversized delete payload (`inst-err-internal`).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::response::IntoResponse;
use serde_json::Value;
use toolkit_canonical_errors::{CanonicalError, Problem};

use crate::domain::error::{AttachmentError, DomainError};
use crate::test_support::attachments::ATTACHMENT_RESOURCE;

fn canonical(error: AttachmentError) -> CanonicalError {
    CanonicalError::from(DomainError::Attachment(error))
}

fn problem(error: AttachmentError) -> Value {
    serde_json::to_value(Problem::from(canonical(error))).expect("problem serializes")
}

const ALL: [AttachmentError; 13] = [
    AttachmentError::BoundaryRequired,
    AttachmentError::MultipartError,
    AttachmentError::MissingFile,
    AttachmentError::MissingContentType,
    AttachmentError::UnsupportedContentType,
    AttachmentError::CodeInterpreterUnavailable,
    AttachmentError::FileTooLarge,
    AttachmentError::DocumentLimit,
    AttachmentError::StorageLimit,
    AttachmentError::ProviderMismatch,
    AttachmentError::StorageUnavailable,
    AttachmentError::UploadBusy,
    AttachmentError::Locked,
];

fn invalid_argument(error: AttachmentError, field: &str, reason: &str) {
    let value = problem(error);
    assert_eq!(value["status"], 400, "{error:?}: {value}");
    assert!(
        value["type"].as_str().unwrap().contains("invalid_argument"),
        "{error:?}: {value}"
    );
    let violation = &value["context"]["field_violations"][0];
    assert_eq!(violation["field"], field, "{error:?}: {value}");
    assert_eq!(violation["reason"], reason, "{error:?}: {value}");
}

#[test]
fn every_attachment_error_is_a_problem_with_the_contract_fields_and_no_top_level_code() {
    // inst-err-return
    for error in ALL {
        let value = problem(error);
        let object = value.as_object().unwrap();
        for key in ["type", "title", "status", "detail", "context"] {
            assert!(
                object.contains_key(key),
                "{error:?}: missing `{key}` in {value}"
            );
        }
        assert!(
            !object.contains_key("code"),
            "{error:?}: top-level code in {value}"
        );
        assert!(
            value["type"].as_str().unwrap().starts_with("gts://"),
            "{error:?}"
        );
        let response = canonical(error).into_response();
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/problem+json",
            "{error:?}"
        );
    }
}

#[test]
fn a_missing_boundary_is_a_400_on_the_content_type_field() {
    // inst-intake-multipart
    invalid_argument(
        AttachmentError::BoundaryRequired,
        "content_type",
        "BOUNDARY_REQUIRED",
    );
}

#[test]
fn an_unreadable_body_is_a_400_on_the_multipart_field() {
    invalid_argument(
        AttachmentError::MultipartError,
        "multipart",
        "MULTIPART_ERROR",
    );
}

#[test]
fn a_missing_file_part_is_a_400_on_the_file_field() {
    invalid_argument(AttachmentError::MissingFile, "file", "MISSING_FILE");
}

#[test]
fn a_missing_part_content_type_is_a_400_on_the_content_type_field() {
    invalid_argument(
        AttachmentError::MissingContentType,
        "content_type",
        "MISSING_CONTENT_TYPE",
    );
}

#[test]
fn an_unsupported_mime_type_is_a_400_with_the_unsupported_content_type_reason() {
    // inst-err-unsupported: the old 415 unsupported_file_type is gone
    invalid_argument(
        AttachmentError::UnsupportedContentType,
        "content_type",
        "UNSUPPORTED_CONTENT_TYPE",
    );
}

#[test]
fn an_unavailable_code_interpreter_is_a_400_on_the_file_field_naming_the_attachment_type() {
    // inst-err-ci
    invalid_argument(
        AttachmentError::CodeInterpreterUnavailable,
        "file",
        "CODE_INTERPRETER_UNAVAILABLE",
    );
    let value = problem(AttachmentError::CodeInterpreterUnavailable);
    assert_eq!(value["context"]["resource_type"], ATTACHMENT_RESOURCE);
}

#[test]
fn a_file_above_the_limit_is_a_400_out_of_range_on_the_content_length_field() {
    // inst-err-too-large
    let value = problem(AttachmentError::FileTooLarge);
    assert_eq!(value["status"], 400);
    assert!(
        value["type"].as_str().unwrap().contains("out_of_range"),
        "{value}"
    );
    let violation = &value["context"]["field_violations"][0];
    assert_eq!(violation["field"], "content_length");
    assert_eq!(violation["reason"], "FILE_TOO_LARGE");
}

#[test]
fn the_per_chat_limits_are_429_resource_exhausted_with_the_limit_as_subject() {
    // inst-err-limits
    for (error, subject) in [
        (AttachmentError::DocumentLimit, "document_limit"),
        (AttachmentError::StorageLimit, "storage_limit"),
    ] {
        let value = problem(error);
        assert_eq!(value["status"], 429, "{error:?}");
        assert!(
            value["type"]
                .as_str()
                .unwrap()
                .contains("resource_exhausted"),
            "{error:?}: {value}"
        );
        assert_eq!(
            value["context"]["violations"][0]["subject"], subject,
            "{error:?}"
        );
    }
}

#[test]
fn a_provider_mismatch_is_a_409_already_exists_named_provider_mismatch() {
    // inst-err-mismatch
    let value = problem(AttachmentError::ProviderMismatch);
    assert_eq!(value["status"], 409);
    assert!(
        value["type"].as_str().unwrap().contains("already_exists"),
        "{value}"
    );
    assert_eq!(value["context"]["resource_name"], "provider_mismatch");
}

#[test]
fn a_storage_failure_is_a_503_with_retry_after_10_and_the_generic_detail() {
    // inst-err-storage-503, inst-uf-case-unavailable
    let value = problem(AttachmentError::StorageUnavailable);
    assert_eq!(value["status"], 503);
    assert!(
        value["type"]
            .as_str()
            .unwrap()
            .contains("service_unavailable"),
        "{value}"
    );
    assert_eq!(value["context"]["retry_after_seconds"], 10);
    assert_eq!(value["detail"], "Service temporarily unavailable");
    let response = canonical(AttachmentError::StorageUnavailable).into_response();
    assert_eq!(response.status(), 503);
    assert_eq!(response.headers().get("retry-after").unwrap(), "10");
}

#[test]
fn the_storage_failure_body_carries_neither_the_error_code_nor_provider_text() {
    let rendered = problem(AttachmentError::StorageUnavailable).to_string();
    for needle in [
        "indexing_failed",
        "upload_failed",
        "vector_store_failed",
        "provider",
    ] {
        assert!(
            !rendered.contains(needle),
            "`{needle}` leaked in {rendered}"
        );
    }
}

#[test]
fn a_busy_upload_limit_is_a_503_with_retry_after_5() {
    // inst-err-busy-503
    let value = problem(AttachmentError::UploadBusy);
    assert_eq!(value["status"], 503);
    assert!(
        value["type"]
            .as_str()
            .unwrap()
            .contains("service_unavailable"),
        "{value}"
    );
    assert_eq!(value["context"]["retry_after_seconds"], 5);
    let response = canonical(AttachmentError::UploadBusy).into_response();
    assert_eq!(response.headers().get("retry-after").unwrap(), "5");
}

#[test]
fn no_attachment_problem_names_a_file_vector_store_or_credential() {
    // inst-err-no-provider-text
    for error in ALL {
        let rendered = problem(error).to_string();
        for needle in ["file-", "vs_", "Bearer", "api-key", "secret"] {
            assert!(
                !rendered.contains(needle),
                "{error:?}: `{needle}` in {rendered}"
            );
        }
    }
}

#[test]
fn an_unknown_attachment_is_a_404_with_the_attachment_resource_type() {
    // inst-err-attachment-404 (the existing not-found mapping is reused)
    let value = serde_json::to_value(Problem::from(CanonicalError::from(DomainError::NotFound {
        resource: crate::domain::error::ResourceKind::Attachment,
        id: Some("a-1".to_owned()),
    })))
    .unwrap();
    assert_eq!(value["status"], 404);
    assert_eq!(value["context"]["resource_type"], ATTACHMENT_RESOURCE);
}

#[test]
fn images_disabled_is_a_400_failed_precondition_with_the_images_subject() {
    // inst-err-images-disabled (the existing feature-disabled mapping is reused)
    let value = serde_json::to_value(Problem::from(CanonicalError::from(
        DomainError::FeatureDisabled { subject: "images" },
    )))
    .unwrap();
    assert_eq!(value["status"], 400);
    assert!(
        value["type"]
            .as_str()
            .unwrap()
            .contains("failed_precondition"),
        "{value}"
    );
    assert_eq!(value["context"]["violations"][0]["subject"], "images");
    assert_eq!(
        value["context"]["violations"][0]["type"],
        "FEATURE_DISABLED"
    );
}

#[test]
fn a_locked_attachment_is_a_409_already_exists_named_attachment_locked() {
    // inst-err-locked, inst-del-case-locked: the attachment resource type, no provider text
    let value = problem(AttachmentError::Locked);
    assert_eq!(value["status"], 409, "{value}");
    assert!(
        value["type"].as_str().unwrap().contains("already_exists"),
        "{value}"
    );
    assert_eq!(
        value["context"]["resource_name"], "attachment_locked",
        "{value}"
    );
    assert_eq!(
        value["context"]["resource_type"], ATTACHMENT_RESOURCE,
        "{value}"
    );
    let response = canonical(AttachmentError::Locked).into_response();
    assert_eq!(response.status().as_u16(), 409);
}

#[test]
fn an_oversized_delete_payload_is_a_500_internal_with_a_generic_detail() {
    // inst-err-internal, inst-del-case-500: the cleanup payload exceeds the outbox limit
    let value = serde_json::to_value(Problem::from(CanonicalError::from(
        DomainError::OutboxPayloadTooLarge {
            site: crate::domain::error::EnqueueSite::AttachmentDelete,
        },
    )))
    .unwrap();
    assert_eq!(value["status"], 500, "{value}");
    assert!(
        value["type"].as_str().unwrap().contains("internal"),
        "{value}"
    );
    assert!(!value.to_string().contains("attachment_cleanup"), "{value}");
}
