//! The upload-side attachment series on the real OTLP instruments: names, kinds and labels of
//! `mini_chat_attachment_upload`, `mini_chat_attachment_upload_bytes`,
//! `mini_chat_attachments_pending` and `mini_chat_attachment_background_indexing`, low
//! cardinality only, and the declared-but-unrecorded series staying unrecorded.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`;
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-metrics` (upload-side series);
//! definition of done `cpt-cf-mini-chat-dod-attachments-and-file-search-errors-observability`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use crate::domain::attachments::AttachmentMetrics;
use crate::test_support::s2::{MetricsHarness, Point, metrics_harness};

fn harness() -> MetricsHarness {
    metrics_harness("mini_chat")
}

fn points(harness: &MetricsHarness, name: &str) -> Vec<Point> {
    harness
        .all_points()
        .into_iter()
        .filter(|(point_name, _, _)| point_name == name)
        .collect()
}

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

#[test]
fn the_upload_counter_is_labelled_by_kind_and_result() {
    // inst-met-upload
    let harness = harness();
    harness.metrics.upload("document", "ok");
    harness.metrics.upload("document", "ok");
    harness.metrics.upload("image", "error");
    let found = points(&harness, "mini_chat_attachment_upload");
    assert!(
        found.contains(&(
            "mini_chat_attachment_upload".to_owned(),
            labels(&[("kind", "document"), ("result", "ok")]),
            2
        )),
        "{found:?}"
    );
    assert!(
        found.contains(&(
            "mini_chat_attachment_upload".to_owned(),
            labels(&[("kind", "image"), ("result", "error")]),
            1
        )),
        "{found:?}"
    );
    assert_eq!(found.len(), 2, "{found:?}");
}

#[test]
fn the_upload_bytes_histogram_is_labelled_by_kind_only() {
    let harness = harness();
    harness.metrics.upload_bytes("image", 2048);
    harness.metrics.upload_bytes("image", 4096);
    let found = points(&harness, "mini_chat_attachment_upload_bytes");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].1, labels(&[("kind", "image")]));
    assert_eq!(found[0].2, 2, "two samples");
}

#[test]
fn the_pending_gauge_follows_the_signed_deltas_without_labels() {
    // inst-met-pending: +1 at the insert, -1 where the row leaves pending
    let harness = harness();
    harness.metrics.pending_add(1);
    harness.metrics.pending_add(1);
    harness.metrics.pending_add(-1);
    let found = points(&harness, "mini_chat_attachments_pending");
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(
        found[0].1.is_empty(),
        "the pending gauge carries labels: {:?}",
        found[0].1
    );
    assert_eq!(found[0].2, 1);
}

#[test]
fn the_background_indexing_counter_is_labelled_by_result() {
    // inst-met-background
    let harness = harness();
    for result in ["ready", "failed", "timeout", "set_ready_failed"] {
        harness.metrics.background_indexing(result);
    }
    harness.metrics.background_indexing("ready");
    let found = points(&harness, "mini_chat_attachment_background_indexing");
    assert_eq!(found.len(), 4, "{found:?}");
    for (_, attributes, value) in &found {
        assert_eq!(attributes.len(), 1);
        let result = &attributes["result"];
        assert_eq!(*value, if result == "ready" { 2 } else { 1 }, "{result}");
    }
}

#[test]
fn the_series_follow_the_configured_prefix() {
    let harness = metrics_harness("custom_prefix");
    harness.metrics.upload("document", "ok");
    assert_eq!(points(&harness, "custom_prefix_attachment_upload").len(), 1);
    assert!(points(&harness, "mini_chat_attachment_upload").is_empty());
}

#[test]
fn no_label_carries_an_identifier() {
    // inst-met-labels: low-cardinality labels only
    let harness = harness();
    harness.metrics.upload("document", "ok");
    harness.metrics.upload_bytes("document", 10);
    harness.metrics.pending_add(1);
    harness.metrics.background_indexing("ready");
    for (name, attributes, _) in harness.all_points() {
        if !name.starts_with("mini_chat_attachment") {
            continue;
        }
        for key in attributes.keys() {
            assert!(
                ["kind", "result"].contains(&key.as_str()),
                "{name} carries the label `{key}`"
            );
        }
    }
}

#[test]
fn the_declared_but_unrecorded_series_stay_unrecorded() {
    // inst-met-deferred
    let harness = harness();
    harness.metrics.upload("document", "ok");
    harness.metrics.upload_bytes("document", 10);
    harness.metrics.pending_add(1);
    harness.metrics.background_indexing("ready");
    let recorded: Vec<String> = harness
        .all_points()
        .into_iter()
        .map(|(name, _, _)| name)
        .collect();
    for deferred in [
        "file_search_latency_ms",
        "retrieval_latency_ms",
        "retrieval_chunks_returned",
        "retrieval_zero_hit",
        "indexed_chunks_per_chat",
        "upload_rejected",
        "vector_stores_per_user",
        "attachment_index",
        "attachment_summary",
        "attachments_failed",
        "attachment_index_latency_ms",
        "media_rejected",
        "cleanup_backlog",
    ] {
        let full = format!("mini_chat_{deferred}");
        assert!(!recorded.contains(&full), "`{full}` was recorded");
    }
}

#[test]
fn the_upload_series_are_registered_instruments() {
    let harness = harness();
    let names = harness.metrics.instrument_names();
    for name in [
        "mini_chat_attachment_upload",
        "mini_chat_attachment_upload_bytes",
        "mini_chat_attachments_pending",
        "mini_chat_attachment_background_indexing",
    ] {
        assert!(
            names.contains(&name.to_owned()),
            "`{name}` is not registered"
        );
    }
}
