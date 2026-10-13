//! Lazy resolution of the model-policy and audit plugins through types-registry (by vendor).

use std::sync::Arc;
use std::time::{Duration, Instant};

use mini_chat_sdk::{
    MiniChatAuditPluginClientV1, MiniChatAuditPluginSpecV1, MiniChatModelPolicyPluginClientV1,
    MiniChatModelPolicyPluginSpecV1, ModelCatalogEntry, PolicySnapshot, UserLimits,
};
use parking_lot::Mutex;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{ChoosePluginError, GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::error::DomainError;

/// Outcome of a plugin lookup.
pub enum PluginLookup<T: ?Sized> {
    Found(Arc<T>),
    /// No instance for the vendor is registered (not cached; retried next time).
    NotRegistered,
    /// The instance resolved but its client is not in `ClientHub` (cached id reset).
    ClientMissing,
}

async fn resolve_instance<P>(hub: &ClientHub, vendor: &str) -> Result<Option<String>, String>
where
    P: for<'de> gts::GtsDeserialize<'de> + gts::GtsSchema,
{
    let registry = hub
        .get::<dyn TypesRegistryClient>()
        .map_err(|e| format!("types-registry client unavailable: {e}"))?;
    let type_id = <P as gts::GtsSchema>::TYPE_ID;
    let instances = registry
        .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
        .await
        .map_err(|e| format!("types-registry list_instances failed: {e}"))?;
    match choose_plugin_instance::<P>(vendor, instances.iter().map(|e| (e.id.as_ref(), &e.object))) {
        Ok(id) => Ok(Some(id)),
        Err(ChoosePluginError::PluginNotFound { .. }) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Generic selector for one plugin kind.
struct Selector {
    hub: Arc<ClientHub>,
    vendor: String,
    selector: GtsPluginSelector,
}

impl Selector {
    fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            selector: GtsPluginSelector::new(),
        }
    }

    async fn lookup<P, T>(&self) -> Result<PluginLookup<T>, String>
    where
        P: for<'de> gts::GtsDeserialize<'de> + gts::GtsSchema,
        T: ?Sized + Send + Sync + 'static,
    {
        let hub = Arc::clone(&self.hub);
        let vendor = self.vendor.clone();
        let res = self
            .selector
            .get_or_init(|| async move {
                match resolve_instance::<P>(&hub, &vendor).await {
                    Ok(Some(id)) => Ok(id),
                    Ok(None) => Err(None),
                    Err(e) => Err(Some(e)),
                }
            })
            .await;
        match res {
            Ok(id) => {
                if let Some(client) = self.hub.try_get_scoped::<T>(&ClientScope::gts_id(id.as_ref())) {
                    Ok(PluginLookup::Found(client))
                } else {
                    self.selector.reset().await;
                    Ok(PluginLookup::ClientMissing)
                }
            }
            Err(None) => Ok(PluginLookup::NotRegistered),
            Err(Some(e)) => Err(e),
        }
    }
}

/// Model policy gateway: policy snapshots, user limits and usage publication.
pub struct PolicyGateway {
    sel: Selector,
}

impl PolicyGateway {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            sel: Selector::new(hub, vendor),
        }
    }

    /// Construct with a fixed client (tests).
    #[must_use]
    pub fn with_client(client: Arc<dyn MiniChatModelPolicyPluginClientV1>) -> Self {
        let hub = Arc::new(ClientHub::new());
        let id = "gts.test.mini_chat.policy.v1";
        hub.register_scoped::<dyn MiniChatModelPolicyPluginClientV1>(ClientScope::gts_id(id), client);
        Self {
            sel: Selector {
                hub,
                vendor: "test".to_owned(),
                selector: GtsPluginSelector::pre_cached(id.to_owned()),
            },
        }
    }

    /// Resolves the plugin client.
    ///
    /// # Errors
    /// `Internal` when no plugin is available (500 on the request path).
    pub async fn client(&self) -> Result<Arc<dyn MiniChatModelPolicyPluginClientV1>, DomainError> {
        match self
            .sel
            .lookup::<MiniChatModelPolicyPluginSpecV1, dyn MiniChatModelPolicyPluginClientV1>()
            .await
        {
            Ok(PluginLookup::Found(c)) => Ok(c),
            Ok(PluginLookup::NotRegistered) => Err(DomainError::internal(
                "no mini-chat model policy plugin registered for vendor",
            )),
            Ok(PluginLookup::ClientMissing) => Err(DomainError::internal(
                "model policy plugin client not registered in ClientHub",
            )),
            Err(e) => Err(DomainError::internal(e)),
        }
    }

    /// Lookup without mapping (used by the usage outbox handler for Retry decisions).
    ///
    /// # Errors
    /// Resolution failure text.
    pub async fn lookup(
        &self,
    ) -> Result<PluginLookup<dyn MiniChatModelPolicyPluginClientV1>, String> {
        self.sel
            .lookup::<MiniChatModelPolicyPluginSpecV1, dyn MiniChatModelPolicyPluginClientV1>()
            .await
    }

    /// Current policy snapshot for the user.
    ///
    /// # Errors
    /// `Internal` on plugin failure.
    pub async fn current_snapshot(&self, user_id: Uuid) -> Result<PolicySnapshot, DomainError> {
        let client = self.client().await?;
        let version = client
            .get_current_policy_version(user_id)
            .await
            .map_err(DomainError::internal)?;
        client
            .get_policy_snapshot(user_id, version.policy_version)
            .await
            .map_err(DomainError::internal)
    }

    /// Snapshot for a specific version (settlement).
    ///
    /// # Errors
    /// `Internal` on plugin failure.
    pub async fn snapshot_version(
        &self,
        user_id: Uuid,
        version: u64,
    ) -> Result<PolicySnapshot, DomainError> {
        self.client()
            .await?
            .get_policy_snapshot(user_id, version)
            .await
            .map_err(DomainError::internal)
    }

    /// Per-user limits for `version`.
    ///
    /// # Errors
    /// `Internal` on plugin failure.
    pub async fn user_limits(&self, user_id: Uuid, version: u64) -> Result<UserLimits, DomainError> {
        self.client()
            .await?
            .get_user_limits(user_id, version)
            .await
            .map_err(DomainError::internal)
    }

    /// Resolves a chat model. `enabled_only` applies the enabled filter (create chat, summary);
    /// otherwise disabled entries are returned too. A missing model is `INVALID_MODEL` (400).
    ///
    /// # Errors
    /// `INVALID_MODEL` when missing (or disabled with `enabled_only`), `Internal` on plugin failure.
    pub async fn resolve_model(
        &self,
        user_id: Uuid,
        model_id: &str,
        enabled_only: bool,
    ) -> Result<(PolicySnapshot, ModelCatalogEntry), DomainError> {
        let snapshot = self.current_snapshot(user_id).await?;
        let entry = if enabled_only {
            snapshot.find_enabled(model_id)
        } else {
            snapshot.find(model_id)
        }
        .cloned()
        .ok_or_else(DomainError::invalid_model)?;
        Ok((snapshot, entry))
    }
}

/// Audit gateway: delivers audit events to the audit plugin.
pub struct AuditGateway {
    sel: Selector,
    last_missing_warn: Mutex<Option<Instant>>,
}

impl AuditGateway {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            sel: Selector::new(hub, vendor),
            last_missing_warn: Mutex::new(None),
        }
    }

    /// Plugin lookup; logs a warning at most once per 5 minutes when no plugin is registered.
    ///
    /// # Errors
    /// Resolution failure text.
    pub async fn lookup(&self) -> Result<PluginLookup<dyn MiniChatAuditPluginClientV1>, String> {
        let res = self
            .sel
            .lookup::<MiniChatAuditPluginSpecV1, dyn MiniChatAuditPluginClientV1>()
            .await;
        if matches!(res, Ok(PluginLookup::NotRegistered)) {
            let mut last = self.last_missing_warn.lock();
            if last.is_none_or(|t| t.elapsed() > Duration::from_secs(300)) {
                tracing::warn!("no mini-chat audit plugin registered; audit events are dropped");
                *last = Some(Instant::now());
            }
        }
        res
    }
}
