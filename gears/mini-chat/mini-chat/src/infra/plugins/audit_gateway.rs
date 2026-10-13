//! Audit gateway (DESIGN section 3.2, "Audit plugin and audit outbox").
//!
//! "No plugin registered" is never cached, so a plugin registered later is
//! picked up; a found instance id is cached; an instance whose client is
//! missing from `ClientHub` resets the cache and asks for a retry.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditPluginError, MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::GtsPluginSelector;
use toolkit::telemetry::ThrottledLog;

use super::{ResolveError, resolve_plugin_instance};
use crate::domain::ports::{AuditDelivery, AuditSink};

/// Upper bound on one plugin `emit` call.
const PLUGIN_CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// One "no plugin registered" warning per period.
const NO_PLUGIN_LOG_THROTTLE: Duration = Duration::from_secs(60);

pub struct AuditGateway {
    hub: Arc<ClientHub>,
    vendor: String,
    selector: GtsPluginSelector,
    no_plugin_log: ThrottledLog,
}

impl AuditGateway {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            selector: GtsPluginSelector::new(),
            no_plugin_log: ThrottledLog::new(NO_PLUGIN_LOG_THROTTLE),
        }
    }
}

#[async_trait]
impl AuditSink for AuditGateway {
    async fn deliver(&self, ev: MiniChatAuditEvent) -> AuditDelivery {
        let instance_id = match self
            .selector
            .get_or_init(|| {
                resolve_plugin_instance::<MiniChatAuditPluginSpecV1>(&self.hub, &self.vendor)
            })
            .await
        {
            Ok(id) => id,
            Err(ResolveError::NoPlugin(_)) => {
                if self.no_plugin_log.should_log() {
                    tracing::warn!(
                        vendor = %self.vendor,
                        "no mini-chat audit plugin registered; audit events are dropped"
                    );
                }
                return AuditDelivery::NoPlugin;
            }
            Err(ResolveError::Failed(m)) => {
                return AuditDelivery::Retry(format!("audit plugin resolution failed: {m}"));
            }
        };

        let scope = ClientScope::gts_id(instance_id.as_ref());
        let Some(client) = self
            .hub
            .try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&scope)
        else {
            self.selector.reset().await;
            return AuditDelivery::Retry(format!(
                "audit plugin client not registered for '{instance_id}'"
            ));
        };

        match tokio::time::timeout(PLUGIN_CALL_TIMEOUT, client.emit(ev)).await {
            Ok(Ok(())) => AuditDelivery::Delivered,
            Ok(Err(AuditPluginError::Permanent(m))) => AuditDelivery::Reject(m),
            Ok(Err(AuditPluginError::Transient(m))) => AuditDelivery::Retry(m),
            Ok(Err(AuditPluginError::PluginTimeout)) => {
                AuditDelivery::Retry("audit plugin timeout".to_owned())
            }
            Err(_elapsed) => AuditDelivery::Retry(format!(
                "audit plugin call exceeded {}s",
                PLUGIN_CALL_TIMEOUT.as_secs()
            )),
        }
    }
}
