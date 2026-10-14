//! Per-chat limit arithmetic: the document count and the storage sum thresholds and the storage
//! re-check after the provider upload.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`;
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-per-chat-limits`; definition of
//! done `cpt-cf-mini-chat-dod-attachments-and-file-search-limits-and-provider-upload`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::ChatLimits;
use crate::config::RagConfig;

const MIB: u64 = 1_048_576;

fn defaults() -> ChatLimits {
    ChatLimits::from_rag(&RagConfig::default())
}

#[test]
fn the_limits_come_from_the_rag_section_defaults() {
    // inst-lim-doc-limit, inst-lim-storage-limit: 50 documents, 100 MiB
    let limits = defaults();
    assert_eq!(limits.max_documents, 50);
    assert_eq!(limits.max_total_bytes, 100 * MIB);
}

#[test]
fn the_limits_follow_a_changed_configuration() {
    let rag = RagConfig {
        max_documents_per_chat: 3,
        max_total_upload_mb_per_chat: 7,
        ..RagConfig::default()
    };
    let limits = ChatLimits::from_rag(&rag);
    assert_eq!(limits.max_documents, 3);
    assert_eq!(limits.max_total_bytes, 7 * MIB);
}

#[test]
fn the_fifty_first_document_is_rejected_and_the_fiftieth_is_not() {
    // inst-lim-doc-limit: reject when the count is AT LEAST the limit
    let limits = defaults();
    assert!(!limits.document_limit_reached(0));
    assert!(!limits.document_limit_reached(49));
    assert!(limits.document_limit_reached(50));
    assert!(limits.document_limit_reached(51));
}

#[test]
fn the_storage_limit_is_reached_at_exactly_the_configured_size() {
    // inst-lim-storage-limit: reject when the sum is at least the limit
    let limits = defaults();
    assert!(!limits.storage_limit_reached(0));
    assert!(!limits.storage_limit_reached(100 * MIB - 1));
    assert!(limits.storage_limit_reached(100 * MIB));
    assert!(limits.storage_limit_reached(100 * MIB + 1));
}

#[test]
fn the_recheck_allows_a_total_equal_to_the_limit_and_rejects_one_byte_more() {
    // inst-lim-recheck-sum: reject when other + final EXCEEDS the limit
    let limits = defaults();
    assert!(!limits.recheck_exceeded(60 * MIB, 40 * MIB));
    assert!(limits.recheck_exceeded(60 * MIB, 40 * MIB + 1));
    assert!(!limits.recheck_exceeded(0, 100 * MIB));
    assert!(limits.recheck_exceeded(0, 100 * MIB + 1));
}

#[test]
fn the_recheck_does_not_overflow_on_huge_sums() {
    let limits = defaults();
    assert!(limits.recheck_exceeded(u64::MAX, u64::MAX));
    assert!(limits.storage_limit_reached(u64::MAX));
}
