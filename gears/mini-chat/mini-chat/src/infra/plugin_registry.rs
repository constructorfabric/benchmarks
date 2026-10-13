//! Real adapters of the plugin resolver: the types-registry instance query and
//! the scoped `ClientHub` lookup.
//!
//! Both reach their dependency lazily on every call, so the gateways can be
//! built in `init` before any plugin gear has registered itself.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use toolkit::client_hub::{ClientHub, ClientScope};
use toolkit_canonical_errors::CanonicalError;
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};

use crate::infra::plugin_resolution::{PluginClientLookup, PluginInstance, PluginInstanceSource};

/// Lists the instances of one plugin spec from types-registry.
pub struct TypesRegistryInstanceSource {
    hub: Arc<ClientHub>,
    type_id: String,
}

impl TypesRegistryInstanceSource {
    /// `type_id` is the GTS type id of the plugin spec (ends with `~`).
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, type_id: impl Into<String>) -> Self {
        Self {
            hub,
            type_id: type_id.into(),
        }
    }
}

/// The diagnostic of a canonical error when it has one (the `Display` of an
/// internal error is deliberately generic).
fn describe(error: &CanonicalError) -> String {
    error
        .diagnostic()
        .map_or_else(|| error.to_string(), ToOwned::to_owned)
}

/// The part of a registered `PluginV1` instance the resolver needs.
#[derive(Deserialize)]
struct InstanceHeader {
    vendor: String,
    priority: i16,
}

#[async_trait]
impl PluginInstanceSource for TypesRegistryInstanceSource {
    async fn list_instances(&self) -> Result<Vec<PluginInstance>, String> {
        let registry = self
            .hub
            .get::<dyn TypesRegistryClient>()
            .map_err(|error| format!("types-registry client unavailable: {error}"))?;
        let entities = registry
            .list_instances(InstanceQuery::new().with_pattern(format!("{}*", self.type_id)))
            .await
            .map_err(|error| describe(&error))?;

        Ok(entities
            .iter()
            .filter(|entity| entity.id.as_ref().starts_with(&self.type_id))
            .filter_map(|entity| {
                let id = entity.id.as_ref();
                match serde_json::from_value::<InstanceHeader>(entity.object.clone()) {
                    Ok(header) => Some(PluginInstance {
                        id: id.to_owned(),
                        vendor: header.vendor,
                        priority: header.priority,
                    }),
                    Err(error) => {
                        // A malformed third-party instance must not hide the
                        // valid ones.
                        tracing::warn!(instance = id, %error, "skipping malformed plugin instance");
                        None
                    }
                }
            })
            .collect())
    }
}

/// Looks up the client registered in `ClientHub` under a GTS instance id.
pub struct ClientHubLookup {
    hub: Arc<ClientHub>,
}

impl ClientHubLookup {
    /// Creates the lookup.
    #[must_use]
    pub fn new(hub: Arc<ClientHub>) -> Self {
        Self { hub }
    }
}

impl<C: ?Sized + Send + Sync + 'static> PluginClientLookup<C> for ClientHubLookup {
    fn client(&self, instance_id: &str) -> Option<Arc<C>> {
        self.hub
            .try_get_scoped::<C>(&ClientScope::gts_id(instance_id))
    }
}

#[cfg(test)]
#[path = "plugin_registry_tests.rs"]
mod tests;
