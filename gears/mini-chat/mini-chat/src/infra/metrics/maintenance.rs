//! The OTLP implementation of the background-maintenance series
//! (`cpt-cf-mini-chat-algo-background-maintenance-metrics`): `mini_chat_thread_summary_execution`,
//! `mini_chat_thread_summary_cas_conflicts`, `mini_chat_summary_fallback`,
//! `mini_chat_cleanup_completed`, `mini_chat_cleanup_retry`, `mini_chat_cleanup_failed`,
//! `mini_chat_cleanup_vector_store_with_failed_attachments` and
//! `mini_chat_secondary_cleanup_skipped`, on the instruments the foundation registered, labelled
//! with low-cardinality values only.

use opentelemetry::KeyValue;

use super::MiniChatMetrics;
use crate::domain::maintenance::metrics::{CleanupMetrics, SummaryMetrics};

// @cpt-begin:cpt-cf-mini-chat-algo-background-maintenance-metrics:p1:inst-mt-return
// the series are recorded on the instruments of the foundation's observability registration; the
// attachment cleanup handler records the same cleanup series for its own path
// @cpt-end:cpt-cf-mini-chat-algo-background-maintenance-metrics:p1:inst-mt-return
// @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-read-metrics
// @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-outbox-surface
// @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-return
// an admin reads these series from the OTLP pipeline; queue lag, retry backlog, lease churn and
// dead-letter depth come from the shared outbox surface, not from Mini Chat series (no outbox
// dead-letter metric exists in P1)
impl SummaryMetrics for MiniChatMetrics {
    fn execution(&self, result: &str) {
        self.maintenance
            .execution
            .add(1, &[KeyValue::new("result", result.to_owned())]);
    }

    fn cas_conflict(&self) {
        // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-summary-alert
        // the series an admin watches with `mini_chat_thread_summary_execution{result="provider_error"}`
        self.maintenance.cas_conflicts.add(1, &[]);
        // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-summary-alert
    }

    fn fallback(&self) {
        // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-summary-alert
        self.maintenance.fallback.add(1, &[]);
        // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-summary-alert
    }
}

impl CleanupMetrics for MiniChatMetrics {
    fn completed(&self, resource_type: &str) {
        self.attachment.cleanup_completed.add(
            1,
            &[KeyValue::new("resource_type", resource_type.to_owned())],
        );
    }

    fn retry(&self, resource_type: &str, reason: &str) {
        self.attachment.cleanup_retry.add(
            1,
            &[
                KeyValue::new("resource_type", resource_type.to_owned()),
                KeyValue::new("reason", reason.to_owned()),
            ],
        );
    }

    fn failed(&self, resource_type: &str) {
        // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-store-alert
        // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-file-debt
        // `resource_type` `vector_store` is the series of a store delete that kept failing, and
        // `file` the series of a row that became provider-file debt
        self.attachment.cleanup_failed.add(
            1,
            &[KeyValue::new("resource_type", resource_type.to_owned())],
        );
        // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-file-debt
        // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-store-alert
    }

    fn vector_store_with_failed_attachments(&self) {
        // @cpt-begin:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-file-debt
        self.maintenance.debt.add(1, &[]);
        // @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-file-debt
    }

    fn secondary_skipped(&self, provider_kind: &str) {
        self.attachment.secondary_skipped.add(
            1,
            &[KeyValue::new("provider_kind", provider_kind.to_owned())],
        );
    }
}

// @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-return
// @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-outbox-surface
// @cpt-end:cpt-cf-mini-chat-flow-background-maintenance-admin-observability:p1:inst-adm-read-metrics

#[cfg(test)]
#[path = "maintenance_tests.rs"]
mod maintenance_tests;
