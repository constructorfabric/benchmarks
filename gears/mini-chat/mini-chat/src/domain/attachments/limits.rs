//! The per-chat limit arithmetic
//! (`cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits`).

use toolkit_macros::domain_model;

use crate::config::RagConfig;

/// The per-chat limits of the `rag` section.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatLimits {
    /// `rag.max_documents_per_chat`.
    pub max_documents: u32,
    /// `rag.max_total_upload_mb_per_chat` times 1048576.
    pub max_total_bytes: u64,
}

impl ChatLimits {
    /// The limits of the configuration.
    #[must_use]
    pub fn from_rag(rag: &RagConfig) -> Self {
        Self {
            max_documents: rag.max_documents_per_chat,
            max_total_bytes: u64::from(rag.max_total_upload_mb_per_chat) * 1_048_576,
        }
    }

    /// Whether `documents` live document rows (`pending`, `uploaded`, `ready`) already reach the
    /// document limit: the next document is rejected.
    #[must_use]
    pub fn document_limit_reached(&self, documents: u64) -> bool {
        documents >= u64::from(self.max_documents)
    }

    /// Whether `stored_bytes` of live rows already reach the storage limit: the next upload (a
    /// document or an image) is rejected.
    #[must_use]
    pub fn storage_limit_reached(&self, stored_bytes: u64) -> bool {
        stored_bytes >= self.max_total_bytes
    }

    /// The re-check after the provider upload: whether the other rows plus the final byte count
    /// exceed the storage limit.
    #[must_use]
    pub fn recheck_exceeded(&self, other_bytes: u64, final_bytes: u64) -> bool {
        other_bytes.saturating_add(final_bytes) > self.max_total_bytes
    }
}

#[cfg(test)]
#[path = "limits_tests.rs"]
mod limits_tests;
