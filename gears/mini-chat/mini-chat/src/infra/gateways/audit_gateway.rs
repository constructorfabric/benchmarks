//! Audit gateway: resolves the audit plugin through types-registry and adapts
//! it to [`AuditPort`] (ADR-0009 transport rules).
//!
//! - "No plugin registered" is never cached: every delivery looks it up again
//!   and returns [`AuditResolution::NoPlugin`] (warning throttled).
//! - A found instance id is cached.
//! - An instance without a `ClientHub` client resets the cache and is
//!   [`AuditPluginError::Transient`]; so are registry failures.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditPluginError, MiniChatAuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{ChoosePluginError, choose_plugin_instance};
use toolkit::telemetry::ThrottledLog;
use tracing::{info, warn};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};

use crate::domain::ports::{AuditPort, AuditResolution};

const NO_PLUGIN_LOG_THROTTLE: Duration = Duration::from_secs(60);

/// [`AuditPort`] backed by the audit plugin selected via types-registry.
pub struct AuditGateway {
    hub: Arc<ClientHub>,
    vendor: String,
    cached: RwLock<Option<Arc<str>>>,
    no_plugin_log: ThrottledLog,
}

impl AuditGateway {
    /// Create a gateway selecting plugin instances of `vendor`.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: impl Into<String>) -> Self {
        Self {
            hub,
            vendor: vendor.into(),
            cached: RwLock::new(None),
            no_plugin_log: ThrottledLog::new(NO_PLUGIN_LOG_THROTTLE),
        }
    }

    fn cached_id(&self) -> Option<Arc<str>> {
        self.cached.read().ok().and_then(|g| g.clone())
    }

    fn set_cached(&self, id: Option<Arc<str>>) {
        if let Ok(mut g) = self.cached.write() {
            *g = id;
        }
    }

    /// `Ok(None)` = no plugin registered for the vendor.
    async fn resolve_instance(&self) -> Result<Option<Arc<str>>, AuditPluginError> {
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| AuditPluginError::Transient(format!("types-registry: {e}")))?;
        let type_id = MiniChatAuditPluginSpecV1::gts_type_id().clone();
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await
            .map_err(|e| AuditPluginError::Transient(format!("types-registry: {e}")))?;
        match choose_plugin_instance::<MiniChatAuditPluginSpecV1>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        ) {
            Ok(id) => {
                info!(plugin_gts_id = %id, vendor = %self.vendor, "selected mini-chat audit plugin");
                let id: Arc<str> = id.into();
                self.set_cached(Some(Arc::clone(&id)));
                Ok(Some(id))
            }
            Err(ChoosePluginError::PluginNotFound { .. }) => Ok(None),
            Err(e @ ChoosePluginError::InvalidPluginInstance { .. }) => {
                Err(AuditPluginError::Transient(e.to_string()))
            }
        }
    }
}

#[async_trait]
impl AuditPort for AuditGateway {
    async fn emit(&self, ev: MiniChatAuditEvent) -> Result<AuditResolution, AuditPluginError> {
        let id = match self.cached_id() {
            Some(id) => id,
            None => {
                if let Some(id) = self.resolve_instance().await? {
                    id
                } else {
                    if self.no_plugin_log.should_log() {
                        warn!(
                            vendor = %self.vendor,
                            "no mini-chat audit plugin registered; audit events are dropped"
                        );
                    }
                    return Ok(AuditResolution::NoPlugin);
                }
            }
        };
        let Some(client) = self
            .hub
            .try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(&id))
        else {
            self.set_cached(None);
            return Err(AuditPluginError::Transient(format!(
                "audit plugin {id}: client not registered"
            )));
        };
        client.emit(ev).await?;
        Ok(AuditResolution::Delivered)
    }
}

#[cfg(test)]
#[path = "audit_gateway_tests.rs"]
mod tests;
