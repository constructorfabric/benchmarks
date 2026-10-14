//! Start-time check of `thread_summary_worker.summary_model_id` against the catalog.
//!
//! Slice S2, algorithm `gear-start` step `inst-start-summary-model-check`.

use mini_chat_sdk::PolicySnapshot;
use toolkit_macros::domain_model;

/// What the catalog says about the configured summary model.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryModelStatus {
    /// The id names an enabled catalog entry.
    Enabled,
    /// The id names a catalog entry that is disabled.
    Disabled,
    /// The id is empty or names no catalog entry.
    Missing,
}

/// Classify `model_id` against the snapshot catalog.
#[must_use]
pub fn summary_model_status(snapshot: &PolicySnapshot, model_id: &str) -> SummaryModelStatus {
    match snapshot
        .model_catalog
        .iter()
        .find(|entry| !model_id.is_empty() && entry.id == model_id)
    {
        Some(entry) if entry.enabled => SummaryModelStatus::Enabled,
        Some(_) => SummaryModelStatus::Disabled,
        None => SummaryModelStatus::Missing,
    }
}

#[cfg(test)]
#[path = "summary_model_tests.rs"]
mod summary_model_tests;
