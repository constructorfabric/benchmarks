//! The audit gateway: the leased handler of the audit queue that delivers each event to the
//! audit plugin resolved through types-registry (`MiniChatAuditPluginClientV1`).
//!
//! Slice S2, algorithm `audit-delivery`, state machine `audit-delivery`. Outcomes: `Ok`,
//! `Retry` or `Reject`; every path that would `Retry` is bounded: on the 120th attempt it
//! records `reject` and returns `Reject`. `OutboxMessage::attempts` is 0 on the first
//! attempt, so the 120th attempt carries `attempts == 119`. The plugin call has a hardcoded
//! 30 s timeout (a timeout is `AuditPluginError::PluginTimeout`, transient).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditEvent, AuditPluginError, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1,
};
use tokio::time::Instant;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{ChoosePluginError, GtsPluginSelector, choose_plugin_instance};
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};

use crate::infra::metrics::{AuditEmitResult, MiniChatMetrics};

/// Payload type of an audit event message (the JSON of `mini_chat_sdk::AuditEvent`).
pub const AUDIT_PAYLOAD_TYPE: &str = "mini_chat.audit_event";

/// Hardcoded timeout of one plugin call.
const PLUGIN_TIMEOUT: Duration = Duration::from_secs(30);

/// `OutboxMessage::attempts` of the 120th delivery attempt (0 on the first).
const LAST_ATTEMPT: i16 = 119;

/// How often the "no audit plugin" warning repeats while no plugin is registered.
const NO_PLUGIN_WARN_PERIOD: Duration = Duration::from_secs(600);

/// Why the plugin instance could not be selected.
enum ResolveError {
    /// No plugin of the configured vendor is registered.
    NoPlugin,
    /// Any other failure of the lookup (types-registry call, invalid instance).
    Failed(String),
}

/// Types-registry-backed audit delivery.
pub struct AuditGateway {
    hub: Arc<ClientHub>,
    vendor: String,
    selector: GtsPluginSelector,
    metrics: Arc<MiniChatMetrics>,
    /// When the "no plugin" warning was last logged; cleared once a plugin is found.
    no_plugin_warned_at: Mutex<Option<Instant>>,
}

impl AuditGateway {
    /// Create the gateway; the plugin is resolved lazily on the first delivery.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String, metrics: Arc<MiniChatMetrics>) -> Self {
        // the plugin is resolved through types-registry on the first delivery, not at init
        Self {
            hub,
            vendor,
            selector: GtsPluginSelector::new(),
            metrics,
            no_plugin_warned_at: Mutex::new(None),
        }
    }

    /// Look the plugin instance up by `vendor` and `priority` and cache a found id. A
    /// "no plugin registered" result is never cached, so each delivery looks up again.
    async fn resolve_instance(&self) -> Result<String, ResolveError> {
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| ResolveError::Failed(format!("types-registry client unavailable: {e}")))?;
        let type_id = MiniChatAuditPluginSpecV1::gts_type_id();
        let type_prefix: &str = type_id.as_ref();
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await
            .map_err(|e| {
                ResolveError::Failed(format!("types-registry list_instances failed: {e}"))
            })?;
        choose_plugin_instance::<MiniChatAuditPluginSpecV1>(
            &self.vendor,
            instances
                .iter()
                // only instances of the audit plugin spec compete
                .filter(|instance| instance.id.as_ref().starts_with(type_prefix))
                .map(|instance| (instance.id.as_ref(), &instance.object)),
        )
        .map_err(|e| match e {
            ChoosePluginError::PluginNotFound { .. } => ResolveError::NoPlugin,
            ChoosePluginError::InvalidPluginInstance { gts_id, reason } => ResolveError::Failed(
                format!("invalid audit plugin instance `{gts_id}`: {reason}"),
            ),
        })
    }

    /// Record `reject` and return the dead-letter outcome.
    fn reject(&self, reason: String) -> MessageResult {
        // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-audit-delivery:p1:inst-audit-state-dead-direct
        // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-audit-delivery:p1:inst-audit-state-dead-limit
        // a corrupt payload or a permanent plugin error dead-letters at once; a retry path
        // reaches here on the 120th attempt
        self.metrics.record_audit_emit(AuditEmitResult::Reject);
        tracing::error!(%reason, "audit event rejected (dead-lettered)");
        MessageResult::Reject(reason)
        // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-audit-delivery:p1:inst-audit-state-dead-limit
        // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-audit-delivery:p1:inst-audit-state-dead-direct
    }

    /// Record `retry` and return the retry outcome.
    fn retry(&self, cause: &str, attempts: i16) -> MessageResult {
        // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-audit-delivery:p1:inst-audit-state-retrying
        self.metrics.record_audit_emit(AuditEmitResult::Retry);
        tracing::debug!(%cause, attempts, "audit event delivery will be retried");
        MessageResult::Retry
        // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-audit-delivery:p1:inst-audit-state-retrying
    }

    /// Log the "no plugin" warning once per period without a plugin.
    fn warn_no_plugin_once_per_period(&self) {
        let mut warned_at = match self.no_plugin_warned_at.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let due = warned_at.is_none_or(|at| at.elapsed() >= NO_PLUGIN_WARN_PERIOD);
        if due {
            *warned_at = Some(Instant::now());
            tracing::warn!(
                vendor = %self.vendor,
                "no audit plugin is registered; audit events are acknowledged and dropped"
            );
        }
    }

    /// A plugin was found: the next period without one warns again at once.
    fn plugin_found(&self) {
        let mut warned_at = match self.no_plugin_warned_at.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *warned_at = None;
    }
}

#[async_trait]
impl LeasedMessageHandler for AuditGateway {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        // the 120th attempt: a path that would retry rejects instead
        let last_attempt = msg.attempts >= LAST_ATTEMPT;

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-deserialize
        let parsed = serde_json::from_slice::<AuditEvent>(&msg.payload);
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-deserialize
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-corrupt
        let event = match parsed {
            Ok(event) => event,
            Err(error) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-return-reject-corrupt
                // dead-lettered whether or not a plugin is available; the payload text is
                // never logged
                return self.reject(format!("corrupt audit payload: {error}"));
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-return-reject-corrupt
            }
        };
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-corrupt

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-resolve
        let resolved = self.selector.get_or_init(|| self.resolve_instance()).await;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-resolve
        let instance_id = match resolved {
            Ok(instance_id) => instance_id,
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-resolve-error
            Err(ResolveError::Failed(cause)) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-resolve-attempt-limit
                if last_attempt {
                    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-return-reject-resolve-limit
                    return self.reject(format!(
                        "audit plugin resolution failed on the last attempt: {cause}"
                    ));
                    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-return-reject-resolve-limit
                }
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-resolve-attempt-limit
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-return-retry-resolve
                return self.retry(&cause, msg.attempts);
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-return-retry-resolve
            }
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-resolve-error
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-no-plugin
            Err(ResolveError::NoPlugin) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-return-dropped
                // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-audit-delivery:p1:inst-audit-state-dropped
                // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-audit-delivery:p1:inst-audit-state-retry-dropped
                // acknowledged and dropped, so a missing plugin cannot block chat traffic
                self.warn_no_plugin_once_per_period();
                self.metrics.record_audit_emit(AuditEmitResult::Dropped);
                return MessageResult::Ok;
                // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-audit-delivery:p1:inst-audit-state-retry-dropped
                // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-audit-delivery:p1:inst-audit-state-dropped
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-return-dropped
            } // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-no-plugin
        };
        self.plugin_found();

        let scope = ClientScope::gts_id(instance_id.as_ref());
        let Some(plugin) = self
            .hub
            .try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&scope)
        else {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-client-missing
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-reset-cached-id
            // the client may register later: select the instance again on the next attempt
            self.selector.reset().await;
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-reset-cached-id
            let cause = format!("audit plugin client `{instance_id}` is not registered");
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-client-attempt-limit
            if last_attempt {
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-return-reject-client-limit
                return self.reject(format!("{cause} on the last attempt"));
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-return-reject-client-limit
            }
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-client-attempt-limit
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-return-retry-client
            return self.retry(&cause, msg.attempts);
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-return-retry-client
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-client-missing
        };

        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-try
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-deliver
        let delivered = match tokio::time::timeout(PLUGIN_TIMEOUT, plugin.emit(event)).await {
            Ok(outcome) => outcome,
            Err(_elapsed) => Err(AuditPluginError::PluginTimeout),
        };
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-deliver
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-try
        match delivered {
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-catch-transient
            Err(error @ (AuditPluginError::Transient(_) | AuditPluginError::PluginTimeout)) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-attempt-limit
                if last_attempt {
                    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-return-reject-limit
                    // a misconfigured plugin does not block the partition
                    return self.reject(format!(
                        "audit delivery failed on the last attempt: {error}"
                    ));
                    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-return-reject-limit
                }
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-attempt-limit
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-return-retry
                self.retry(&error.to_string(), msg.attempts)
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-return-retry
            }
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-catch-transient
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-catch-permanent
            Err(error @ AuditPluginError::Permanent(_)) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-return-reject-permanent
                self.reject(format!("audit plugin rejected the event: {error}"))
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-return-reject-permanent
            }
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-catch-permanent
            Ok(()) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-return-ok
                // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-audit-delivery:p1:inst-audit-state-delivered
                // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-audit-delivery:p1:inst-audit-state-retry-delivered
                // nothing is counted at enqueue time; the delivery outcome is counted here
                self.metrics.record_audit_emit(AuditEmitResult::Ok);
                MessageResult::Ok
                // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-audit-delivery:p1:inst-audit-state-retry-delivered
                // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-audit-delivery:p1:inst-audit-state-delivered
                // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-audit-delivery:p1:inst-audit-return-ok
            }
        }
    }
}

#[cfg(test)]
#[path = "audit_gateway_tests.rs"]
mod audit_gateway_tests;
