//! Audit plugin gateway.
//!
//! "No plugin registered" is not cached: every delivery looks the plugin up
//! again, so a plugin registered later is used. A found instance id is
//! cached; when the instance resolves in the types-registry but its client is
//! not in the `ClientHub`, the cached id is reset and the delivery is retried.

use std::sync::Arc;
use std::time::{Duration, Instant};

use mini_chat_sdk::{MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1};
use parking_lot::Mutex;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{ChoosePluginError, GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};

/// Outcome of an audit plugin lookup.
pub enum AuditPluginResolution {
    Plugin(Arc<dyn MiniChatAuditPluginClientV1>),
    /// No audit plugin instance is registered (events are dropped).
    NoPlugin,
    /// Transient resolution failure (the delivery is retried).
    Retry(String),
}

/// How often the "no audit plugin" warning is logged.
const NO_PLUGIN_WARN_PERIOD: Duration = Duration::from_secs(300);

pub struct AuditGateway {
    hub: Arc<ClientHub>,
    vendor: String,
    selector: GtsPluginSelector,
    last_no_plugin_warning: Mutex<Option<Instant>>,
}

enum ResolveError {
    NotFound,
    Transient(String),
}

impl AuditGateway {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            selector: GtsPluginSelector::new(),
            last_no_plugin_warning: Mutex::new(None),
        }
    }

    async fn resolve_instance(&self) -> Result<String, ResolveError> {
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| ResolveError::Transient(format!("types-registry unavailable: {e}")))?;
        let type_id = MiniChatAuditPluginSpecV1::gts_type_id();
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await
            .map_err(|e| ResolveError::Transient(format!("types-registry list failed: {e}")))?;
        choose_plugin_instance::<MiniChatAuditPluginSpecV1>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        )
        .map_err(|e| match e {
            ChoosePluginError::PluginNotFound { .. } => ResolveError::NotFound,
            ChoosePluginError::InvalidPluginInstance { .. } => {
                ResolveError::Transient(e.to_string())
            }
        })
    }

    /// Resolve the audit plugin for one delivery.
    pub async fn resolve(&self) -> AuditPluginResolution {
        let id = match self.selector.get_or_init(|| self.resolve_instance()).await {
            Ok(id) => id,
            Err(ResolveError::NotFound) => {
                self.warn_no_plugin();
                return AuditPluginResolution::NoPlugin;
            }
            Err(ResolveError::Transient(reason)) => return AuditPluginResolution::Retry(reason),
        };
        if let Some(client) = self
            .hub
            .try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(&id))
        {
            return AuditPluginResolution::Plugin(client);
        }
        self.selector.reset().await;
        AuditPluginResolution::Retry(format!("audit plugin client not registered for '{id}'"))
    }

    fn warn_no_plugin(&self) {
        let mut last = self.last_no_plugin_warning.lock();
        let due = last.is_none_or(|t| t.elapsed() >= NO_PLUGIN_WARN_PERIOD);
        if due {
            *last = Some(Instant::now());
            tracing::warn!(
                vendor = %self.vendor,
                "mini-chat: no audit plugin registered; audit events are dropped"
            );
        }
    }
}
