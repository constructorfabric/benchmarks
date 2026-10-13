//! Lazy plugin resolution through types-registry (vendor + lowest priority).

use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{ChoosePluginError, GtsPluginSelector, choose_plugin_instance};
use toolkit::telemetry::ThrottledLog;
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};

/// Outcome of a plugin lookup.
pub enum Resolved<T: ?Sized> {
    /// Client ready.
    Ready(Arc<T>),
    /// No instance registered for the vendor (not cached; looked up again next time).
    NotRegistered,
    /// The instance resolves but its client is missing from `ClientHub` (cache reset).
    ClientMissing(String),
    /// types-registry lookup failed.
    Error(String),
}

/// Variance/ownership marker of [`PluginResolver`] (no `S`/`T` values are stored).
type Marker<S, T> = PhantomData<fn() -> (S, Box<T>)>;

/// Lazy, cached resolver of one plugin contract.
pub struct PluginResolver<S, T: ?Sized> {
    hub: Arc<ClientHub>,
    vendor: String,
    selector: GtsPluginSelector,
    warn: ThrottledLog,
    _p: Marker<S, T>,
}

enum LookupErr {
    NotFound,
    Other(String),
}

impl<S, T> PluginResolver<S, T>
where
    S: gts::GtsSchema + for<'de> gts::GtsDeserialize<'de> + 'static,
    T: ?Sized + Send + Sync + 'static,
{
    /// New resolver.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, vendor: String) -> Self {
        Self {
            hub,
            vendor,
            selector: GtsPluginSelector::new(),
            warn: ThrottledLog::new(Duration::from_secs(60)),
            _p: PhantomData,
        }
    }

    async fn lookup(&self) -> Result<String, LookupErr> {
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| LookupErr::Other(format!("types-registry unavailable: {e}")))?;
        let pattern = format!("{}*", S::TYPE_ID);
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(pattern))
            .await
            .map_err(|e| LookupErr::Other(format!("types-registry list failed: {e}")))?;
        choose_plugin_instance::<S>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        )
        .map_err(|e| match e {
            ChoosePluginError::PluginNotFound { .. } => LookupErr::NotFound,
            other @ ChoosePluginError::InvalidPluginInstance { .. } => LookupErr::Other(other.to_string()),
        })
    }

    /// Resolves the plugin client.
    pub async fn resolve(&self) -> Resolved<T> {
        let id = match self.selector.get_or_init(|| self.lookup()).await {
            Ok(id) => id,
            Err(LookupErr::NotFound) => {
                if self.warn.should_log() {
                    tracing::warn!(vendor = %self.vendor, plugin = S::TYPE_ID, "no mini-chat plugin registered");
                }
                return Resolved::NotRegistered;
            }
            Err(LookupErr::Other(e)) => return Resolved::Error(e),
        };
        let scope = ClientScope::gts_id(id.as_ref());
        if let Some(client) = self.hub.try_get_scoped::<T>(&scope) {
            Resolved::Ready(client)
        } else {
            self.selector.reset().await;
            Resolved::ClientMissing(id.to_string())
        }
    }
}
