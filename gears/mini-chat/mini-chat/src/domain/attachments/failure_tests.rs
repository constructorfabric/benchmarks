//! The failure kinds after the row was inserted: the stored `error_code` of each kind and the
//! Problem the client gets.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`;
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure`; definition of
//! done `cpt-cf-mini-chat-dod-attachments-and-file-search-limits-and-provider-upload`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::UploadFailure;
use crate::domain::error::{AttachmentError, DomainError};

#[test]
fn each_failure_kind_stores_its_error_code() {
    // inst-uf-mark-failed
    assert_eq!(UploadFailure::FileTooLarge.error_code(), "file_too_large");
    assert_eq!(UploadFailure::UploadFailed.error_code(), "upload_failed");
    assert_eq!(
        UploadFailure::StorageLimitExceeded.error_code(),
        "storage_limit_exceeded"
    );
    assert_eq!(
        UploadFailure::VectorStoreFailed.error_code(),
        "vector_store_failed"
    );
    assert_eq!(
        UploadFailure::IndexingFailed.error_code(),
        "indexing_failed"
    );
}

#[test]
fn a_provider_mismatch_is_stored_as_vector_store_failed() {
    // the DESIGN lists no provider_mismatch row code
    assert_eq!(
        UploadFailure::ProviderMismatch.error_code(),
        "vector_store_failed"
    );
}

#[test]
fn a_file_too_large_failure_is_the_400_problem() {
    // inst-uf-case-too-large
    assert!(matches!(
        UploadFailure::FileTooLarge.into_error(),
        DomainError::Attachment(AttachmentError::FileTooLarge)
    ));
}

#[test]
fn a_storage_limit_failure_is_the_429_problem() {
    // inst-uf-case-storage
    assert!(matches!(
        UploadFailure::StorageLimitExceeded.into_error(),
        DomainError::Attachment(AttachmentError::StorageLimit)
    ));
}

#[test]
fn a_provider_mismatch_is_the_409_problem() {
    // inst-uf-case-mismatch
    assert!(matches!(
        UploadFailure::ProviderMismatch.into_error(),
        DomainError::Attachment(AttachmentError::ProviderMismatch)
    ));
}

#[test]
fn provider_vector_store_and_indexing_failures_are_the_503_problem() {
    // inst-uf-case-unavailable
    for failure in [
        UploadFailure::UploadFailed,
        UploadFailure::VectorStoreFailed,
        UploadFailure::IndexingFailed,
    ] {
        assert!(
            matches!(
                failure.into_error(),
                DomainError::Attachment(AttachmentError::StorageUnavailable)
            ),
            "{failure:?}"
        );
    }
}
