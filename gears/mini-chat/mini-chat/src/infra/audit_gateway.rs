//! Audit gateway: resolves the audit plugin through the types registry.
//! "No plugin registered" is not cached (a plugin registered later is used);
//! a found instance id is cached and reset when its client is missing.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mini_chat_sdk::{MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{ChoosePluginError, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};

/// Resolution outcome.
pub enum AuditResolution {
    Client(Arc<dyn MiniChatAuditPluginClientV1>),
    /// No plugin registered for the vendor: acknowledge and drop.
    NoPlugin,
    /// Resolution failed or the client is not in `ClientHub` yet: retry.
    Retry(String),
}

/// Port for the audit outbox handler.
#[async_trait]
pub trait AuditResolver: Send + Sync {
    async fn resolve(&self) -> AuditResolution;
}

/// Types-registry backed resolver.
pub struct GtsAuditResolver {
    hub: Arc<ClientHub>,
    vendor: String,
    cached: Mutex<Option<String>>,
    last_warn: Mutex<Option<Instant>>,
}

impl GtsAuditResolver {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            cached: Mutex::new(None),
            last_warn: Mutex::new(None),
        }
    }

    fn warn_no_plugin(&self) {
        if let Ok(mut g) = self.last_warn.lock() {
            let due = g.is_none_or(|t| t.elapsed() > Duration::from_secs(300));
            if due {
                tracing::warn!(vendor = %self.vendor, "no mini-chat audit plugin registered; audit events are dropped");
                *g = Some(Instant::now());
            }
        }
    }
}

#[async_trait]
impl AuditResolver for GtsAuditResolver {
    async fn resolve(&self) -> AuditResolution {
        let cached = self.cached.lock().ok().and_then(|g| g.clone());
        let id = if let Some(id) = cached {
            id
        } else {
            let Ok(registry) = self.hub.get::<dyn TypesRegistryClient>() else {
                return AuditResolution::Retry("types registry unavailable".into());
            };
            let type_id = MiniChatAuditPluginSpecV1::gts_type_id();
            let instances = match registry
                .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
                .await
            {
                Ok(i) => i,
                Err(e) => return AuditResolution::Retry(format!("types registry: {e}")),
            };
            match choose_plugin_instance::<MiniChatAuditPluginSpecV1>(
                &self.vendor,
                instances.iter().map(|e| (e.id.as_ref(), &e.object)),
            ) {
                Ok(id) => {
                    if let Ok(mut g) = self.cached.lock() {
                        *g = Some(id.clone());
                    }
                    id
                }
                Err(ChoosePluginError::PluginNotFound { .. }) => {
                    self.warn_no_plugin();
                    return AuditResolution::NoPlugin;
                }
                Err(e) => return AuditResolution::Retry(e.to_string()),
            }
        };
        if let Some(c) = self
            .hub
            .try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(&id))
        {
            return AuditResolution::Client(c);
        }
        if let Ok(mut g) = self.cached.lock() {
            *g = None;
        }
        AuditResolution::Retry(format!("audit plugin client not registered for '{id}'"))
    }
}
