//! The failure kinds of the upload path after the row was inserted and their Problems
//! (`cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure`).

use toolkit_macros::domain_model;

use crate::domain::error::{AttachmentError, DomainError};

/// A failure reported by the provider file upload step or the file search path.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadFailure {
    FileTooLarge,
    UploadFailed,
    StorageLimitExceeded,
    VectorStoreFailed,
    ProviderMismatch,
    IndexingFailed,
}

impl UploadFailure {
    /// The `attachments.error_code` stored for this failure (a provider mismatch is stored as
    /// `vector_store_failed`).
    #[must_use]
    pub fn error_code(self) -> &'static str {
        match self {
            Self::FileTooLarge => "file_too_large",
            Self::UploadFailed => "upload_failed",
            Self::StorageLimitExceeded => "storage_limit_exceeded",
            Self::VectorStoreFailed | Self::ProviderMismatch => "vector_store_failed",
            Self::IndexingFailed => "indexing_failed",
        }
    }

    /// The Problem the client gets: 400 `FILE_TOO_LARGE`, 429 `storage_limit`, 409
    /// `provider_mismatch`, or 503 with `Retry-After: 10` for the other kinds.
    #[must_use]
    pub fn into_error(self) -> DomainError {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-match
        DomainError::Attachment(match self {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-case-too-large
            Self::FileTooLarge => AttachmentError::FileTooLarge,
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-case-too-large
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-case-storage
            Self::StorageLimitExceeded => AttachmentError::StorageLimit,
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-case-storage
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-case-mismatch
            Self::ProviderMismatch => AttachmentError::ProviderMismatch,
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-case-mismatch
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-case-unavailable
            Self::UploadFailed | Self::VectorStoreFailed | Self::IndexingFailed => {
                AttachmentError::StorageUnavailable
            } // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-case-unavailable
        })
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-failure:p1:inst-uf-match
    }
}

#[cfg(test)]
#[path = "failure_tests.rs"]
mod failure_tests;
