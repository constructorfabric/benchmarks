//! The OTLP implementation of the upload-side attachment metrics: `mini_chat_attachment_upload`,
//! `mini_chat_attachment_upload_bytes`, `mini_chat_attachments_pending` and
//! `mini_chat_attachment_background_indexing`, labelled with low-cardinality values only.

use std::time::Duration;

use opentelemetry::KeyValue;

use super::MiniChatMetrics;
use crate::domain::attachments::AttachmentMetrics;

// @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-return
impl AttachmentMetrics for MiniChatMetrics {
    fn upload(&self, kind: &str, result: &str) {
        self.attachment.upload.add(
            1,
            &[
                KeyValue::new("kind", kind.to_owned()),
                KeyValue::new("result", result.to_owned()),
            ],
        );
    }

    fn upload_bytes(&self, kind: &str, bytes: u64) {
        self.attachment.upload_bytes.record(
            // a byte count is far below 2^53: the conversion is exact in practice
            #[allow(clippy::cast_precision_loss)]
            {
                bytes as f64
            },
            &[KeyValue::new("kind", kind.to_owned())],
        );
    }

    fn pending_add(&self, delta: i64) {
        self.attachment.pending.add(delta, &[]);
    }

    fn background_indexing(&self, result: &str) {
        self.attachment
            .background
            .add(1, &[KeyValue::new("result", result.to_owned())]);
    }

    fn upload_abandoned(&self, from_status: &str) {
        self.attachment
            .abandoned
            .add(1, &[KeyValue::new("from_status", from_status.to_owned())]);
    }

    fn reaper_scan_duration(&self, elapsed: Duration) {
        self.attachment
            .reaper_scan
            .record(elapsed.as_secs_f64(), &[]);
    }

    fn cleanup_completed(&self) {
        self.attachment
            .cleanup_completed
            .add(1, &[KeyValue::new("resource_type", "file")]);
    }

    fn cleanup_retry(&self, reason: &str) {
        self.attachment.cleanup_retry.add(
            1,
            &[
                KeyValue::new("resource_type", "file"),
                KeyValue::new("reason", reason.to_owned()),
            ],
        );
    }

    fn cleanup_failed(&self) {
        // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-admin-observability:p1:inst-adm-cleanup-failed
        // the series an admin watches before inspecting the dead-letter store
        self.attachment
            .cleanup_failed
            .add(1, &[KeyValue::new("resource_type", "file")]);
        // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-admin-observability:p1:inst-adm-cleanup-failed
    }

    fn secondary_cleanup_skipped(&self, provider_kind: &str) {
        self.attachment.secondary_skipped.add(
            1,
            &[KeyValue::new("provider_kind", provider_kind.to_owned())],
        );
    }

    fn knowledge_search(&self, result: &str) {
        self.attachment
            .knowledge
            .add(1, &[KeyValue::new("result", result.to_owned())]);
    }

    fn knowledge_search_latency(&self, elapsed: Duration) {
        self.attachment
            .knowledge_latency
            .record(elapsed.as_secs_f64() * 1000.0, &[]);
    }

    fn knowledge_search_chunks(&self, chunks: usize) {
        self.attachment.knowledge_chunks.record(
            // a chunk count is tiny: the conversion is exact
            #[allow(clippy::cast_precision_loss)]
            {
                chunks as f64
            },
            &[],
        );
    }
}
// @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-metrics:p1:inst-met-return

#[cfg(test)]
#[path = "attachments_tests.rs"]
mod attachments_tests;

#[cfg(test)]
#[path = "attachment_ops_tests.rs"]
mod attachment_ops_tests;
