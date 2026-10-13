//! Audit gateway: delivers audit events to the plugin selected through
//! types-registry (ADR-0009). "No plugin" is not cached: every delivery looks
//! the plugin up again.

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::{AuditEvent, AuditPluginError, MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1};
use parking_lot::Mutex;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{ChoosePluginError, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};

/// Audit delivery outcome (`mini_chat_audit_emit_total{result}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuditOutcome {
    Ok,
    Retry(String),
    Reject(String),
    Dropped,
}

enum Source {
    Hub {
        hub: Arc<ClientHub>,
        vendor: String,
        cached: Mutex<Option<String>>,
    },
    Direct(Option<Arc<dyn MiniChatAuditPluginClientV1>>),
}

/// Audit gateway.
pub struct AuditGateway {
    source: Source,
}

const PLUGIN_TIMEOUT: Duration = Duration::from_secs(30);

impl AuditGateway {
    #[must_use]
    pub fn from_hub(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            source: Source::Hub {
                hub,
                vendor,
                cached: Mutex::new(None),
            },
        }
    }

    #[must_use]
    pub fn direct(client: Option<Arc<dyn MiniChatAuditPluginClientV1>>) -> Self {
        Self {
            source: Source::Direct(client),
        }
    }

    async fn resolve(&self) -> Result<Option<Arc<dyn MiniChatAuditPluginClientV1>>, String> {
        match &self.source {
            Source::Direct(c) => Ok(c.clone()),
            Source::Hub { hub, vendor, cached } => {
                let id = cached.lock().clone();
                let id = if let Some(id) = id {
                    id
                } else {
                    let registry = hub.get::<dyn TypesRegistryClient>().map_err(|e| e.to_string())?;
                    let type_id = MiniChatAuditPluginSpecV1::gts_type_id().clone();
                    let instances = registry
                        .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
                        .await
                        .map_err(|e| e.to_string())?;
                    match choose_plugin_instance::<MiniChatAuditPluginSpecV1>(
                        vendor,
                        instances.iter().map(|e| (e.id.as_ref(), &e.object)),
                    ) {
                        Ok(id) => {
                            *cached.lock() = Some(id.clone());
                            id
                        }
                        Err(ChoosePluginError::PluginNotFound { .. }) => return Ok(None),
                        Err(e) => return Err(e.to_string()),
                    }
                };
                if let Some(c) = hub.try_get_scoped::<dyn MiniChatAuditPluginClientV1>(&ClientScope::gts_id(&id)) {
                    Ok(Some(c))
                } else {
                    *cached.lock() = None;
                    Err("audit plugin client not registered in ClientHub".to_owned())
                }
            }
        }
    }

    /// Delivers an audit event.
    pub async fn emit(&self, event: AuditEvent) -> AuditOutcome {
        let plugin = match self.resolve().await {
            Ok(Some(p)) => p,
            Ok(None) => return AuditOutcome::Dropped,
            Err(e) => return AuditOutcome::Retry(e),
        };
        match tokio::time::timeout(PLUGIN_TIMEOUT, plugin.emit(event)).await {
            Ok(Ok(())) => AuditOutcome::Ok,
            Ok(Err(AuditPluginError::Permanent(e))) => AuditOutcome::Reject(e),
            Ok(Err(e)) => AuditOutcome::Retry(e.to_string()),
            Err(_) => AuditOutcome::Retry(AuditPluginError::PluginTimeout.to_string()),
        }
    }
}
