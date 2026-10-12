//! Attachments: upload, get, delete, the provider-side cleanup and the upload reaper (DESIGN
//! "Upload Attachment", "Get Attachment", "Attachment Deletion", "Cleanup on Chat Deletion",
//! B.7, B.8, B.9.2, B.9.5).

mod background;
mod chat_cleanup;
mod cleanup;
mod indexing;
mod mime;
mod model;
mod ports;
mod reaper;
mod service;
mod thumbnail;
mod upload;
mod vector_store;

pub use chat_cleanup::ChatCleanup;
pub use cleanup::{AttachmentCleanup, CleanupResult};
pub use mime::{XLSX_CONTENT_TYPE, file_extension, normalize_filename, resolve_content_type};
pub use model::{
    AttachmentDetail, AttachmentKind, CLEANUP_DONE, CLEANUP_FAILED, CLEANUP_PENDING, ChatLimits,
    ERROR_INDEXING_FAILED, ERROR_UPLOAD_ABANDONED, ERROR_UPLOAD_FAILED, ERROR_VECTOR_STORE_FAILED,
    IncomingFile, IndexingTimings, NewAttachment, PROVIDER_KIND_ANTHROPIC, Purposes, ReadyUpdate,
    SECONDARY_FAILED, SECONDARY_NOT_ATTEMPTED, SECONDARY_UPLOADED, STATUS_FAILED, STATUS_PENDING,
    STATUS_READY, STATUS_UPLOADED, SecondaryCopy, StoredAttachment, VectorStoreRow,
};
pub use ports::{
    AttachmentRepository, ChatStorage, CleanupFailure, CleanupRepository, DeleteOutcome,
    FileUpload, IndexingStatus, PendingCleanup, SecondaryRoute, StaleUpload, StorageError,
    StorageFlavor, StorageGateway, StorageRoute, UploadReaperRepository,
};
pub use reaper::{ReaperReport, UploadReaper};
pub use service::{Admission, AttachmentService, AttachmentServiceParts, UploadSession};
pub use thumbnail::generate as generate_thumbnail;

#[cfg(test)]
#[path = "mime_tests.rs"]
mod mime_tests;
#[cfg(test)]
#[path = "thumbnail_tests.rs"]
mod thumbnail_tests;
