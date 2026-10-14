//! Tests of the start-time summary model check (`inst-start-summary-model-check`).
//!
//! Slice S2: algorithm `gear-start`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mini_chat_sdk::ModelTier;
use serde_json::json;

use super::{SummaryModelStatus, summary_model_status};
use crate::test_support::{entry, snapshot};

fn catalog() -> mini_chat_sdk::PolicySnapshot {
    snapshot(vec![
        entry("summarizer", ModelTier::Standard, true, &json!({})),
        entry("retired", ModelTier::Standard, false, &json!({})),
    ])
}

#[test]
fn an_enabled_catalog_entry_is_enabled() {
    assert_eq!(
        summary_model_status(&catalog(), "summarizer"),
        SummaryModelStatus::Enabled
    );
}

#[test]
fn a_disabled_catalog_entry_is_disabled() {
    assert_eq!(
        summary_model_status(&catalog(), "retired"),
        SummaryModelStatus::Disabled
    );
}

#[test]
fn an_unknown_or_empty_model_id_is_missing() {
    assert_eq!(
        summary_model_status(&catalog(), "nope"),
        SummaryModelStatus::Missing
    );
    // the default `summary_model_id` is the empty string
    assert_eq!(
        summary_model_status(&catalog(), ""),
        SummaryModelStatus::Missing
    );
}

#[test]
fn an_empty_catalog_has_no_summary_model() {
    assert_eq!(
        summary_model_status(&snapshot(vec![]), "summarizer"),
        SummaryModelStatus::Missing
    );
}
