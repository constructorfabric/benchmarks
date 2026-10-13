//! Lazy, types-registry-backed resolution of a plugin client by vendor.
//!
//! The chosen GTS instance id (lowest `priority` among the instances of the
//! configured vendor) is cached once found; misses are never cached. Follows
//! the credstore host-side resolution pattern.

use std::marker::PhantomData;
use std::sync::Arc;

use thiserror::Error;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit::plugins::{ChoosePluginError, GtsPluginSelector, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};

/// Why a plugin client could not be resolved.
#[derive(Debug, Error)]
pub enum ResolveError {
    /// No instance of the configured vendor is registered (not cached).
    #[error("no plugin instance registered for vendor '{vendor}'")]
    NotFound { vendor: String },
    /// The types-registry is unavailable or an instance is invalid.
    #[error("plugin resolution failed: {0}")]
    Failed(String),
    /// The instance resolved but its client is not in `ClientHub` yet; the
    /// cached instance id was reset.
    #[error("plugin client not registered for instance '{0}'")]
    ClientMissing(String),
}

type Marker<S, C> = fn() -> (S, Arc<C>);

/// Resolves the scoped client `C` of the plugin type `S`.
pub struct PluginResolver<S, C: ?Sized> {
    hub: Arc<ClientHub>,
    vendor: String,
    type_pattern: String,
    selector: GtsPluginSelector,
    _marker: PhantomData<Marker<S, C>>,
}

impl<S, C> PluginResolver<S, C>
where
    S: for<'de> gts::GtsDeserialize<'de> + gts::GtsSchema,
    C: ?Sized + Send + Sync + 'static,
{
    /// `type_id` is the GTS type id of the plugin spec (e.g. `S::gts_type_id()`).
    pub fn new(hub: Arc<ClientHub>, vendor: String, type_id: impl std::fmt::Display) -> Self {
        Self {
            hub,
            vendor,
            type_pattern: format!("{type_id}*"),
            selector: GtsPluginSelector::new(),
            _marker: PhantomData,
        }
    }

    async fn choose_instance(&self) -> Result<String, ResolveError> {
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|e| ResolveError::Failed(format!("types registry unavailable: {e}")))?;
        let instances = registry
            .list_instances(InstanceQuery::new().with_pattern(self.type_pattern.clone()))
            .await
            .map_err(|e| ResolveError::Failed(format!("types registry list failed: {e}")))?;
        choose_plugin_instance::<S>(
            &self.vendor,
            instances.iter().map(|e| (e.id.as_ref(), &e.object)),
        )
        .map_err(|e| match e {
            ChoosePluginError::PluginNotFound { vendor, .. } => ResolveError::NotFound { vendor },
            ChoosePluginError::InvalidPluginInstance { .. } => ResolveError::Failed(e.to_string()),
        })
    }

    /// Resolve the plugin client, selecting the instance on first use.
    ///
    /// # Errors
    ///
    /// See [`ResolveError`].
    pub async fn client(&self) -> Result<Arc<C>, ResolveError> {
        let instance_id = self.selector.get_or_init(|| self.choose_instance()).await?;
        if let Some(client) = self
            .hub
            .try_get_scoped::<C>(&ClientScope::gts_id(&instance_id))
        {
            Ok(client)
        } else {
            self.selector.reset().await;
            Err(ResolveError::ClientMissing(instance_id.to_string()))
        }
    }
}
