//! Background work: outbox handlers, orphan watchdog and upload reaper.

pub mod cleanup;
pub mod orphan;
pub mod reaper;
pub mod thread_summary;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatAuditEvent, PublishError, UsageEvent};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

use crate::domain::service::MiniChat;
use crate::infra::audit_gateway::{AuditResolution, AuditResolver};

/// Late-bound service handle shared by the outbox handlers (the pipeline
/// starts before the service is built).
pub type ServiceSlot = Arc<OnceLock<Arc<MiniChat>>>;

/// Maximum deliveries of an audit message before it is dead-lettered.
pub const AUDIT_MAX_ATTEMPTS: i16 = 120;

/// Usage queue handler: publishes through the model-policy plugin.
pub struct UsageHandler {
    pub slot: ServiceSlot,
}

#[async_trait]
impl LeasedMessageHandler for UsageHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event: UsageEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("malformed usage payload: {e}")),
        };
        let Some(svc) = self.slot.get() else {
            return MessageResult::Retry;
        };
        let client = match svc.policy.client().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "usage publish: policy plugin unavailable");
                return MessageResult::Retry;
            }
        };
        match client.publish_usage(event).await {
            Ok(()) => MessageResult::Ok,
            Err(PublishError::Transient(e)) => {
                tracing::warn!(error = %e, "usage publish transient failure");
                MessageResult::Retry
            }
            Err(PublishError::Permanent(e)) => MessageResult::Reject(format!("usage publish: {e}")),
        }
    }
}

/// Audit queue handler.
pub struct AuditHandler {
    pub resolver: Arc<dyn AuditResolver>,
}

#[async_trait]
impl LeasedMessageHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let event: MiniChatAuditEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e,
            Err(e) => return MessageResult::Reject(format!("malformed audit payload: {e}")),
        };
        let last_attempt = msg.attempts + 1 >= AUDIT_MAX_ATTEMPTS;
        let retry = |why: String| {
            if last_attempt {
                MessageResult::Reject(format!("audit delivery gave up after {AUDIT_MAX_ATTEMPTS} attempts: {why}"))
            } else {
                MessageResult::Retry
            }
        };
        match self.resolver.resolve().await {
            AuditResolution::NoPlugin => MessageResult::Ok,
            AuditResolution::Retry(why) => retry(why),
            AuditResolution::Client(c) => {
                match tokio::time::timeout(Duration::from_secs(30), c.emit_audit_event(event)).await {
                    Err(_) => retry("audit plugin timeout".into()),
                    Ok(Ok(())) => MessageResult::Ok,
                    Ok(Err(e)) if e.is_transient() => retry(e.to_string()),
                    Ok(Err(e)) => MessageResult::Reject(format!("audit plugin: {e}")),
                }
            }
        }
    }
}

/// Run `f` every `interval` until `cancel` fires.
pub async fn periodic<F, Fut>(interval: Duration, cancel: tokio_util::sync::CancellationToken, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tick.tick() => f().await,
        }
    }
}
