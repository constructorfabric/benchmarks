//! Model-policy and audit plugin gateways (resolved lazily through
//! types-registry, DESIGN §3.2).

use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mini_chat_sdk::{
    AuditEvent, MiniChatAuditPluginClientV1, MiniChatAuditPluginError, MiniChatAuditPluginSpecV1,
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginSpecV1, PolicySnapshot, PublishError,
    UsageEvent, UserLimits,
};
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{ChoosePluginError, GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};
use uuid::Uuid;

use crate::domain::error::{DomainError, DomainResult};
use crate::domain::ports::{AuditDelivery, AuditPort, PolicyPort, UsagePublishError};

/// Why a plugin client could not be obtained.
#[derive(Debug, Clone)]
pub enum ResolveError {
    /// No plugin instance registered for the vendor.
    NotRegistered,
    /// Instance found but its client is not in `ClientHub` yet.
    ClientMissing(String),
    /// types-registry failure or an invalid instance.
    Failed(String),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotRegistered => write!(f, "no plugin instance registered"),
            Self::ClientMissing(id) => write!(f, "plugin client not registered for '{id}'"),
            Self::Failed(e) => write!(f, "plugin resolution failed: {e}"),
        }
    }
}

/// Source of a plugin client.
#[async_trait]
pub trait ClientSource<C: ?Sized + Send + Sync>: Send + Sync {
    async fn get(&self) -> Result<Arc<C>, ResolveError>;
    async fn reset(&self);
}

/// Fixed client (tests, embedded use).
pub struct FixedSource<C: ?Sized>(pub Arc<C>);

#[async_trait]
impl<C: ?Sized + Send + Sync + 'static> ClientSource<C> for FixedSource<C> {
    async fn get(&self) -> Result<Arc<C>, ResolveError> {
        Ok(Arc::clone(&self.0))
    }
    async fn reset(&self) {}
}

/// No plugin (tests of the "no audit plugin" path).
pub struct NoSource;

#[async_trait]
impl<C: ?Sized + Send + Sync + 'static> ClientSource<C> for NoSource {
    async fn get(&self) -> Result<Arc<C>, ResolveError> {
        Err(ResolveError::NotRegistered)
    }
    async fn reset(&self) {}
}

/// Variance/ownership marker for [`GtsSource`] (owns neither `S` nor `C`).
type SourceMarker<S, C> = PhantomData<fn() -> (S, Box<C>)>;

/// Resolution through types-registry (vendor + lowest priority) and `ClientHub`.
pub struct GtsSource<S, C: ?Sized> {
    hub: Arc<ClientHub>,
    vendor: String,
    selector: GtsPluginSelector,
    _p: SourceMarker<S, C>,
}

impl<S, C: ?Sized> GtsSource<S, C> {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self { hub, vendor, selector: GtsPluginSelector::new(), _p: PhantomData }
    }
}

pub trait SpecTypeId {
    fn type_id_str() -> String;
}

impl SpecTypeId for MiniChatModelPolicyPluginSpecV1 {
    fn type_id_str() -> String {
        MiniChatModelPolicyPluginSpecV1::gts_type_id().to_string()
    }
}

impl SpecTypeId for MiniChatAuditPluginSpecV1 {
    fn type_id_str() -> String {
        MiniChatAuditPluginSpecV1::gts_type_id().to_string()
    }
}

impl<S, C> GtsSource<S, C>
where
    S: SpecTypeId + gts::GtsSchema + for<'de> gts::GtsDeserialize<'de> + Send + Sync,
    C: ?Sized + Send + Sync + 'static,
{
    async fn resolve_instance(&self) -> Result<String, ResolveError> {
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| ResolveError::Failed(format!("types-registry client: {e}")))?;
        let type_id = S::type_id_str();
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{type_id}*")))
            .await
            .map_err(|e| ResolveError::Failed(format!("list instances: {e}")))?;
        choose_plugin_instance::<S>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        )
        .map_err(|e| match e {
            ChoosePluginError::PluginNotFound { .. } => ResolveError::NotRegistered,
            ChoosePluginError::InvalidPluginInstance { gts_id, reason } => {
                ResolveError::Failed(format!("invalid plugin instance {gts_id}: {reason}"))
            }
        })
    }
}

#[async_trait]
impl<S, C> ClientSource<C> for GtsSource<S, C>
where
    S: SpecTypeId + gts::GtsSchema + for<'de> gts::GtsDeserialize<'de> + Send + Sync,
    C: ?Sized + Send + Sync + 'static,
{
    async fn get(&self) -> Result<Arc<C>, ResolveError> {
        // "No plugin" is not cached: the selector only caches a found id.
        let id = self.selector.get_or_init(|| self.resolve_instance()).await?;
        let scope = ClientScope::gts_id(id.as_ref());
        match self.hub.try_get_scoped::<C>(&scope) {
            Some(c) => Ok(c),
            None => Err(ResolveError::ClientMissing(id.to_string())),
        }
    }

    async fn reset(&self) {
        self.selector.reset().await;
    }
}

/// Model policy gateway.
pub struct PolicyGateway {
    source: Box<dyn ClientSource<dyn MiniChatModelPolicyPluginClientV1>>,
}

impl PolicyGateway {
    #[must_use]
    pub fn new(source: Box<dyn ClientSource<dyn MiniChatModelPolicyPluginClientV1>>) -> Self {
        Self { source }
    }

    #[must_use]
    pub fn from_hub(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self::new(Box::new(GtsSource::<
            MiniChatModelPolicyPluginSpecV1,
            dyn MiniChatModelPolicyPluginClientV1,
        >::new(hub, vendor)))
    }

    async fn client(&self) -> DomainResult<Arc<dyn MiniChatModelPolicyPluginClientV1>> {
        self.source
            .get()
            .await
            .map_err(|e| DomainError::internal(format!("model policy plugin: {e}")))
    }
}

#[async_trait]
impl PolicyPort for PolicyGateway {
    async fn current_snapshot(&self, user_id: Uuid) -> DomainResult<Arc<PolicySnapshot>> {
        let client = self.client().await?;
        let version = client
            .get_current_policy_version(user_id)
            .await
            .map_err(|e| DomainError::internal(format!("policy version: {e}")))?;
        let snap = client
            .get_policy_snapshot(user_id, version)
            .await
            .map_err(|e| DomainError::internal(format!("policy snapshot: {e}")))?;
        Ok(Arc::new(snap))
    }

    async fn snapshot(&self, user_id: Uuid, version: u64) -> DomainResult<Arc<PolicySnapshot>> {
        let client = self.client().await?;
        let snap = client
            .get_policy_snapshot(user_id, version)
            .await
            .map_err(|e| DomainError::internal(format!("policy snapshot {version}: {e}")))?;
        Ok(Arc::new(snap))
    }

    async fn user_limits(&self, user_id: Uuid, version: u64) -> DomainResult<UserLimits> {
        let client = self.client().await?;
        client
            .get_user_limits(user_id, version)
            .await
            .map_err(|e| DomainError::internal(format!("user limits: {e}")))
    }

    async fn publish_usage(&self, event: UsageEvent) -> Result<(), UsagePublishError> {
        let client = self
            .source
            .get()
            .await
            .map_err(|e| UsagePublishError::Resolve(e.to_string()))?;
        client.publish_usage(event).await.map_err(|e| match e {
            PublishError::Transient(m) => UsagePublishError::Transient(m),
            PublishError::Permanent(m) => UsagePublishError::Permanent(m),
        })
    }
}

/// Audit gateway (30 s plugin timeout, "no plugin" drop semantics).
pub struct AuditGateway {
    source: Box<dyn ClientSource<dyn MiniChatAuditPluginClientV1>>,
    timeout: Duration,
    no_plugin_warned: parking_lot::Mutex<Option<std::time::Instant>>,
}

impl AuditGateway {
    #[must_use]
    pub fn new(source: Box<dyn ClientSource<dyn MiniChatAuditPluginClientV1>>) -> Self {
        Self { source, timeout: Duration::from_secs(30), no_plugin_warned: parking_lot::Mutex::new(None) }
    }

    #[must_use]
    pub fn from_hub(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self::new(Box::new(
            GtsSource::<MiniChatAuditPluginSpecV1, dyn MiniChatAuditPluginClientV1>::new(hub, vendor),
        ))
    }
}

#[async_trait]
impl AuditPort for AuditGateway {
    async fn deliver(&self, event: AuditEvent) -> AuditDelivery {
        let client = match self.source.get().await {
            Ok(c) => c,
            Err(ResolveError::NotRegistered) => {
                let mut last = self.no_plugin_warned.lock();
                let now = std::time::Instant::now();
                if last.is_none_or(|t| now.duration_since(t) > Duration::from_secs(300)) {
                    tracing::warn!("mini-chat: no audit plugin registered; audit events are dropped");
                    *last = Some(now);
                }
                return AuditDelivery::Dropped;
            }
            Err(ResolveError::ClientMissing(id)) => {
                self.source.reset().await;
                return AuditDelivery::Retry(format!("audit plugin client missing for {id}"));
            }
            Err(ResolveError::Failed(e)) => return AuditDelivery::Retry(e),
        };
        match tokio::time::timeout(self.timeout, client.emit(event)).await {
            Err(_) | Ok(Err(MiniChatAuditPluginError::PluginTimeout)) => {
                AuditDelivery::Retry("audit plugin timed out".into())
            }
            Ok(Ok(())) => AuditDelivery::Ok,
            Ok(Err(MiniChatAuditPluginError::Transient(m))) => AuditDelivery::Retry(m),
            Ok(Err(MiniChatAuditPluginError::Permanent(m))) => AuditDelivery::Reject(m),
        }
    }
}
