#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of the real adapters of the plugin resolver (types-registry query and
//! `ClientHub` lookup).

use std::sync::Arc;

use serde_json::json;
use toolkit::client_hub::{ClientHub, ClientScope};
use types_registry_sdk::TypesRegistryClient;

use super::{ClientHubLookup, TypesRegistryInstanceSource};
use crate::infra::plugin_resolution::{PluginClientLookup, PluginInstanceSource};
use crate::infra::test_support::FakeTypesRegistry;

const TYPE_ID: &str = "gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.audit_plugin.v1~";

fn instance_id(segment: &str) -> String {
    format!("{TYPE_ID}{segment}")
}

fn content(id: &str, vendor: &str, priority: i16) -> serde_json::Value {
    json!({ "id": id, "vendor": vendor, "priority": priority })
}

fn source(registry: &Arc<FakeTypesRegistry>) -> TypesRegistryInstanceSource {
    let hub = Arc::new(ClientHub::new());
    hub.register::<dyn TypesRegistryClient>(Arc::clone(registry) as Arc<dyn TypesRegistryClient>);
    TypesRegistryInstanceSource::new(hub, TYPE_ID)
}

#[tokio::test]
async fn lists_vendor_and_priority_of_each_instance_of_the_spec() {
    let registry = FakeTypesRegistry::new();
    let a = instance_id("acme.core._.a.v1");
    let b = instance_id("cf.core._.b.v1");
    registry.add_instance(&a, content(&a, "acme", 7));
    registry.add_instance(&b, content(&b, "constructorfabric", -3));

    let instances = source(&registry).list_instances().await.expect("listed");

    assert_eq!(instances.len(), 2);
    assert_eq!(instances[0].id, a);
    assert_eq!(instances[0].vendor, "acme");
    assert_eq!(instances[0].priority, 7);
    assert_eq!(instances[1].vendor, "constructorfabric");
    assert_eq!(instances[1].priority, -3);
}

#[tokio::test]
async fn ignores_instances_of_other_specs() {
    let registry = FakeTypesRegistry::new();
    let other =
        "gts.cf.toolkit.plugins.plugin.v1~cf.core.mini_chat.model_policy_plugin.v1~cf.core._.x.v1";
    registry.add_instance(other, content(other, "constructorfabric", 1));

    let instances = source(&registry).list_instances().await.expect("listed");

    assert!(instances.is_empty());
}

#[tokio::test]
async fn skips_a_malformed_instance_and_keeps_the_valid_ones() {
    let registry = FakeTypesRegistry::new();
    let bad = instance_id("cf.core._.bad.v1");
    let good = instance_id("cf.core._.good.v1");
    registry.add_instance(&bad, json!({ "id": bad, "vendor": "constructorfabric" }));
    registry.add_instance(&good, content(&good, "constructorfabric", 100));

    let instances = source(&registry).list_instances().await.expect("listed");

    assert_eq!(instances.len(), 1);
    assert_eq!(instances[0].id, good);
}

#[tokio::test]
async fn a_registry_failure_is_reported_as_text() {
    let registry = FakeTypesRegistry::new();
    registry.fail_list("registry down");

    let err = source(&registry)
        .list_instances()
        .await
        .expect_err("registry failure");

    assert!(err.contains("registry down"), "{err}");
}

#[tokio::test]
async fn a_missing_registry_client_is_reported() {
    let hub = Arc::new(ClientHub::new());
    let source = TypesRegistryInstanceSource::new(hub, TYPE_ID);

    let err = source.list_instances().await.expect_err("no client");

    assert!(err.contains("unavailable"), "{err}");
}

trait Probe: Send + Sync {}
struct P;
impl Probe for P {}

#[test]
fn the_hub_lookup_returns_the_client_scoped_to_the_instance() {
    let hub = Arc::new(ClientHub::new());
    hub.register_scoped::<dyn Probe>(ClientScope::gts_id("plugin.a"), Arc::new(P));
    let lookup = ClientHubLookup::new(Arc::clone(&hub));

    let found: Option<Arc<dyn Probe>> = lookup.client("plugin.a");
    let other: Option<Arc<dyn Probe>> = lookup.client("plugin.b");

    assert!(found.is_some());
    assert!(other.is_none(), "another instance id has no client");
}
