//! The upload concurrency permits and the upload request completion
//! (`cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion`): the permit is
//! released exactly once on every return path and `mini_chat_attachment_upload` is recorded once
//! per request that took a permit.

use std::sync::Arc;

use toolkit_macros::domain_model;

use crate::domain::attachments::metrics::AttachmentMetrics;

/// The per-process upload semaphore (`rag.max_concurrent_uploads`).
#[domain_model]
pub struct UploadPermits {
    semaphore: Arc<tokio::sync::Semaphore>,
    metrics: Arc<dyn AttachmentMetrics>,
}

/// How an upload request ended.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UploadOutcome {
    /// A 201 response (`ready` or `uploaded`) for an upload of `bytes` bytes.
    Created { bytes: u64 },
    /// Any Problem.
    Problem,
}

/// One upload request holding a permit. Dropping it without completing releases the permit
/// (a client disconnect or a gear stop).
#[domain_model]
pub struct UploadRequest {
    permits: Arc<UploadPermits>,
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
    done: bool,
}

impl UploadPermits {
    /// Permits for `max_concurrent_uploads` concurrent uploads.
    #[must_use]
    pub fn new(max: u16, metrics: Arc<dyn AttachmentMetrics>) -> Arc<Self> {
        Arc::new(Self {
            semaphore: Arc::new(tokio::sync::Semaphore::new(usize::from(max))),
            metrics,
        })
    }

    /// Take one permit without waiting; `None` when none is free.
    #[must_use]
    pub fn try_acquire(self: &Arc<Self>) -> Option<UploadRequest> {
        let permit = Arc::clone(&self.semaphore).try_acquire_owned().ok()?;
        Some(UploadRequest {
            permits: Arc::clone(self),
            permit: Some(permit),
            done: false,
        })
    }

    /// The free permits.
    #[must_use]
    pub fn available(&self) -> usize {
        self.semaphore.available_permits()
    }
}

impl UploadRequest {
    /// Run the upload request completion: release the permit (a second call does nothing) and
    /// record `mini_chat_attachment_upload{kind, result}` once, plus
    /// `mini_chat_attachment_upload_bytes{kind}` for a 201. `mime` is the resolved MIME type
    /// (`""` before it was resolved): the kind label is `image` for the four image types and
    /// `document` otherwise.
    pub fn complete(&mut self, mime: &str, outcome: UploadOutcome) {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion:p1:inst-uc-release
        // exactly once: a second call does nothing; a request dropped without completing releases
        // the permit as well, because the permit lives as long as the request
        if self.done {
            return;
        }
        self.done = true;
        drop(self.permit.take());
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion:p1:inst-uc-release
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion:p1:inst-uc-kind
        let kind = if crate::domain::attachments::intake::IMAGE_MIME_TYPES.contains(&mime) {
            "image"
        } else {
            "document"
        };
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion:p1:inst-uc-kind
        match outcome {
            UploadOutcome::Created { bytes } => {
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion:p1:inst-uc-metric
                self.permits.metrics.upload(kind, "ok");
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion:p1:inst-uc-metric
                // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion:p1:inst-uc-bytes
                self.permits.metrics.upload_bytes(kind, bytes);
                // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion:p1:inst-uc-bytes
            }
            UploadOutcome::Problem => self.permits.metrics.upload(kind, "error"),
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion:p1:inst-uc-no-gauge
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion:p1:inst-uc-return
        // the pending gauge is left to the transitions that move the row out of `pending`
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion:p1:inst-uc-return
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion:p1:inst-uc-no-gauge
    }
}

#[cfg(test)]
#[path = "completion_tests.rs"]
mod completion_tests;
