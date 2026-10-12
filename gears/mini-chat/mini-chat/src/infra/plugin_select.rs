//! Types-registry-backed selection of plugin instances.

use toolkit::client_hub::ClientHub;
use toolkit::plugins::{ChoosePluginError, choose_plugin_instance};
use types_registry_sdk::{InstanceQuery, TypesRegistryClient};

/// Why no plugin instance could be selected.
#[derive(Debug)]
pub enum PluginResolutionError {
    /// The types registry could not be reached or queried.
    RegistryUnavailable(String),
    /// No instance of the plugin type is registered for the configured vendor.
    NotFound {
        /// The configured vendor.
        vendor: String,
    },
    /// A registered instance is malformed.
    InvalidInstance(String),
}

/// Selects the instance of plugin type `P` with the configured vendor and the lowest priority.
///
/// # Errors
/// [`PluginResolutionError`] when the registry is unavailable, no instance matches the vendor or an
/// instance is malformed.
pub async fn select_instance<P>(
    hub: &ClientHub,
    vendor: &str,
) -> Result<String, PluginResolutionError>
where
    P: for<'de> gts::GtsDeserialize<'de> + gts::GtsSchema,
{
    let registry = hub
        .get::<dyn TypesRegistryClient>()
        .map_err(|err| PluginResolutionError::RegistryUnavailable(err.to_string()))?;
    let pattern = format!("{}*", <P as gts::GtsSchema>::TYPE_ID);
    let instances = registry
        .list_instances(InstanceQuery::new().with_pattern(pattern))
        .await
        .map_err(|err| PluginResolutionError::RegistryUnavailable(err.to_string()))?;
    choose_plugin_instance::<P>(
        vendor,
        instances
            .iter()
            .map(|instance| (instance.id.as_ref(), &instance.object)),
    )
    .map_err(|err| match err {
        ChoosePluginError::PluginNotFound { vendor, .. } => {
            PluginResolutionError::NotFound { vendor }
        }
        ChoosePluginError::InvalidPluginInstance { gts_id, reason } => {
            PluginResolutionError::InvalidInstance(format!("{gts_id}: {reason}"))
        }
    })
}
