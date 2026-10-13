//! Audit delivery handler (queue `outbox.audit_queue_name`, ADR-0009).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{AuditPluginError, MiniChatAuditEvent};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use tracing::warn;

use super::{decode, delivery_attempt};
use crate::domain::ports::{AuditPort, AuditResolution};
use crate::infra::metrics::MiniChatMetrics;

/// Bound of one audit plugin call; a timeout is transient.
pub const AUDIT_PLUGIN_TIMEOUT: Duration = Duration::from_secs(30);

/// A `Retry` on this delivery attempt dead-letters the event instead.
pub const AUDIT_MAX_ATTEMPTS: u32 = 120;

/// Outcomes counted by an [`AuditHandler`] (also recorded as
/// `audit_emit{result}`: `ok` / `dropped` / `retry` / `reject`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AuditCounts {
    pub delivered: u64,
    /// No audit plugin registered: acknowledged and dropped.
    pub dropped: u64,
    pub retry: u64,
    pub reject: u64,
}

/// Delivers audit events through the [`AuditPort`] (the audit gateway).
///
/// Malformed payload → `Reject` (before any plugin lookup); delivered or no
/// plugin → `Ok`; transient plugin error, plugin timeout, resolution failure
/// or missing client → `Retry`, except on attempt [`AUDIT_MAX_ATTEMPTS`]
/// (`Reject`); permanent plugin error → `Reject`.
pub struct AuditHandler {
    audit: Arc<dyn AuditPort>,
    timeout: Duration,
    delivered: AtomicU64,
    dropped: AtomicU64,
    retry: AtomicU64,
    reject: AtomicU64,
    metrics: Arc<MiniChatMetrics>,
}

impl AuditHandler {
    /// Handler with the [`AUDIT_PLUGIN_TIMEOUT`] plugin call bound.
    #[must_use]
    pub fn new(audit: Arc<dyn AuditPort>) -> Self {
        Self {
            audit,
            timeout: AUDIT_PLUGIN_TIMEOUT,
            delivered: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            retry: AtomicU64::new(0),
            reject: AtomicU64::new(0),
            metrics: Arc::new(MiniChatMetrics::noop()),
        }
    }

    /// Record the outcomes on `metrics` (default: no-op instruments).
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<MiniChatMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Override the plugin call bound (tests).
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Outcomes so far.
    #[must_use]
    pub fn counts(&self) -> AuditCounts {
        AuditCounts {
            delivered: self.delivered.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            retry: self.retry.load(Ordering::Relaxed),
            reject: self.reject.load(Ordering::Relaxed),
        }
    }

    fn count(&self, counter: &AtomicU64, result: &'static str) {
        counter.fetch_add(1, Ordering::Relaxed);
        self.metrics.audit_emit(result);
    }
}

#[async_trait]
impl LeasedMessageHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let attempt = delivery_attempt(msg);
        let ev: MiniChatAuditEvent = match decode("audit", msg) {
            Ok(ev) => ev,
            Err(reject) => {
                self.count(&self.reject, "reject");
                return reject;
            }
        };
        let res = tokio::time::timeout(self.timeout, self.audit.emit(ev))
            .await
            .unwrap_or(Err(AuditPluginError::PluginTimeout));
        match res {
            Ok(AuditResolution::Delivered) => {
                self.count(&self.delivered, "ok");
                MessageResult::Ok
            }
            Ok(AuditResolution::NoPlugin) => {
                self.count(&self.dropped, "dropped");
                MessageResult::Ok
            }
            Err(AuditPluginError::Permanent(e)) => {
                self.count(&self.reject, "reject");
                warn!(error = %e, "audit plugin rejected the event; dead-lettering");
                MessageResult::Reject(format!("audit plugin rejected the event: {e}"))
            }
            Err(e) if attempt >= AUDIT_MAX_ATTEMPTS => {
                self.count(&self.reject, "reject");
                warn!(attempt, error = %e, "audit delivery attempts exhausted; dead-lettering");
                MessageResult::Reject(format!(
                    "audit delivery: max attempts ({AUDIT_MAX_ATTEMPTS}) reached: {e}"
                ))
            }
            Err(e) => {
                self.count(&self.retry, "retry");
                warn!(attempt, error = %e, "audit delivery failed; retrying");
                MessageResult::Retry
            }
        }
    }
}
