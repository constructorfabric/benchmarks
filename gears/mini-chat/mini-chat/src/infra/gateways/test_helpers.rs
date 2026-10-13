//! Shared helpers for gateway unit tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use toolkit::client_hub::ClientHub;
use toolkit::gts::PluginV1;
use types_registry_sdk::TypesRegistryClient;
use types_registry_sdk::testing::{MockTypesRegistryClient, make_test_instance};

/// Build a `PluginV1<P>` registration and return `(instance_id, GtsInstance)`.
pub fn plugin_instance<P>(
    segment: &str,
    vendor: &str,
    priority: i16,
) -> (String, types_registry_sdk::GtsInstance)
where
    P: gts::GtsSchema + gts::GtsSerialize + Default,
{
    let (id, payload) = PluginV1::<P>::build_registration(segment, vendor, priority).unwrap();
    let id = id.to_string();
    let inst = make_test_instance(&id, payload);
    (id, inst)
}

/// Hub with the given mock registry registered as `dyn TypesRegistryClient`.
pub fn hub_with_registry(registry: &Arc<MockTypesRegistryClient>) -> Arc<ClientHub> {
    let hub = Arc::new(ClientHub::new());
    let api: Arc<dyn TypesRegistryClient> = registry.clone();
    hub.register::<dyn TypesRegistryClient>(api);
    hub
}
