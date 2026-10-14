//! The in-request indexing wait
//! (`cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait`): add the file to the chat
//! vector store with the single attribute `attachment_id`, then poll with a wait of 250 ms
//! doubling to 2 s until 25 s after the request start.

use std::time::Duration;

use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::attachments::provider::{StorageProvider, StorageTarget};
use crate::domain::attachments::timing::Timer;

/// The provider's indexing state of a vector store file.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexStatus {
    InProgress,
    Completed,
    Failed,
}

/// Map a provider status text: `None` and `in_progress` are in progress, `completed` is
/// completed, every other value (`failed`, `cancelled`, unknown) is failed.
#[must_use]
pub fn classify_status(status: Option<&str>) -> IndexStatus {
    match status {
        None | Some("in_progress") => IndexStatus::InProgress,
        Some("completed") => IndexStatus::Completed,
        Some(_) => IndexStatus::Failed,
    }
}

/// The result of the in-request wait.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexingOutcome {
    /// Indexing completed.
    Ready,
    /// Still `in_progress` at the deadline: the caller spawns the background task.
    StillInProgress,
    /// `indexing_failed`: a failed add, a failed status, or a non-transient read error.
    Failed,
}

/// The file whose indexing is awaited.
#[domain_model]
#[allow(
    clippy::struct_field_names,
    reason = "the fields are the provider and attachment identifiers of the file"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFile {
    pub vector_store_id: String,
    pub provider_file_id: String,
    pub attachment_id: Uuid,
}

/// Wait for the indexing of `file` until `started_at` (the monotonic reading of the request
/// start) plus the 25 s deadline. Every wait goes through the injected `timer`.
pub async fn wait_for_indexing(
    provider: &dyn StorageProvider,
    timer: &dyn Timer,
    target: &StorageTarget,
    file: &IndexedFile,
    started_at: Duration,
) -> IndexingOutcome {
    use crate::domain::attachments::provider::ProviderFailure;
    use crate::domain::attachments::timing::{
        INDEX_WAIT_INITIAL, INDEX_WAIT_MAX, INDEXING_DEADLINE,
    };

    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-deadline
    let deadline = started_at + INDEXING_DEADLINE;
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-deadline
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-add
    // the call is not retried
    let added = provider
        .add_vector_store_file(
            target,
            &file.vector_store_id,
            &file.provider_file_id,
            file.attachment_id,
        )
        .await;
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-add
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-add-failed
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-return-add-failed
    let Ok(initial) = added else {
        return IndexingOutcome::Failed;
    };
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-return-add-failed
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-add-failed
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-initial-status
    // a response without `status` counts as `in_progress`
    let mut status = classify_status(initial.as_deref());
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-initial-status
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-initial-wait
    let mut wait = INDEX_WAIT_INITIAL;
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-initial-wait
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-loop
    while status == IndexStatus::InProgress {
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-deadline-reached
        if timer.monotonic() + wait >= deadline {
            return IndexingOutcome::StillInProgress;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-deadline-reached
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-backoff
        timer.sleep(wait).await;
        wait = (wait * 2).min(INDEX_WAIT_MAX);
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-backoff
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-read
        // the read ends at the deadline
        let timeout = deadline.saturating_sub(timer.monotonic());
        let read = provider
            .read_vector_store_file(
                target,
                &file.vector_store_id,
                &file.provider_file_id,
                timeout,
            )
            .await;
        // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-read
        match read {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-status
            Ok(text) => status = classify_status(text.as_deref()),
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-status
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-transient
            // a provider 5xx, a gateway failure or a timeout keeps the status `in_progress`
            Err(ProviderFailure::Transient) => {}
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-transient
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-read-failed
            Err(ProviderFailure::Other | ProviderFailure::NotFound) => {
                return IndexingOutcome::Failed;
            } // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-read-failed
        }
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-loop
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-completed
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-return-ready
    if status == IndexStatus::Completed {
        return IndexingOutcome::Ready;
    }
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-return-ready
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-completed
    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-return-failed
    // `failed`, `cancelled` and any unknown status
    IndexingOutcome::Failed
    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-indexing-wait:p1:inst-iw-return-failed
}

#[cfg(test)]
#[path = "indexing_tests.rs"]
mod indexing_tests;
