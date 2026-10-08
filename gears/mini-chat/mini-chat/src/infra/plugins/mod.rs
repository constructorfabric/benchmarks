//! Built-in plugin implementations and the gateways that reach plugins.

pub mod audit_gateway;
pub mod policy_gateway;
pub mod static_audit;
pub mod static_model_policy;

#[cfg(test)]
#[path = "gateways_tests.rs"]
mod gateways_tests;

use std::sync::Arc;

use toolkit::client_hub::ClientHub;
use toolkit::plugins::{ChoosePluginError, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};

/// Why a plugin instance could not be chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResolveError {
    /// The registry is reachable but lists no instance for the vendor.
    NoPlugin(String),
    /// The registry is unavailable or returned an invalid instance.
    Failed(String),
}

/// Lists the instances of plugin spec `P` in types-registry and returns the
/// GTS id of the one with the lowest priority for `vendor`.
pub(crate) async fn resolve_plugin_instance<P>(
    hub: &Arc<ClientHub>,
    vendor: &str,
) -> Result<String, ResolveError>
where
    P: for<'de> gts::GtsDeserialize<'de> + gts::GtsSchema,
{
    let registry = hub
        .get::<dyn TypesRegistryClient>()
        .map_err(|e| ResolveError::Failed(format!("types-registry client unavailable: {e}")))?;
    let instances = registry
        .list_instances(InstanceQuery::new().with_pattern(format!("{}*", P::TYPE_ID)))
        .await
        .map_err(|e| ResolveError::Failed(format!("types-registry list failed: {e}")))?;
    choose_plugin_instance::<P>(vendor, instances.iter().map(|i| (i.id.as_ref(), &i.object)))
        .map_err(|e| match e {
            ChoosePluginError::PluginNotFound { type_id, vendor } => ResolveError::NoPlugin(
                format!("no plugin of type '{type_id}' for vendor '{vendor}'"),
            ),
            ChoosePluginError::InvalidPluginInstance { gts_id, reason } => {
                ResolveError::Failed(format!("invalid plugin instance '{gts_id}': {reason}"))
            }
        })
}
