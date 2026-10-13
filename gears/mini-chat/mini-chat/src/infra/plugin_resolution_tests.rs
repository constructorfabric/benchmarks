#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of `cpt-cf-mini-chat-algo-gear-foundation-resolve-plugin` and
//! `cpt-cf-mini-chat-state-gear-foundation-plugin-resolution`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;

use super::{
    PluginClientLookup, PluginInstance, PluginInstanceSource, PluginResolutionError,
    PluginResolver, REGISTRY_QUERY_TIMEOUT,
};

const VENDOR: &str = "constructorfabric";

trait Probe: Send + Sync {
    fn name(&self) -> String;
}

struct NamedProbe(String);

impl Probe for NamedProbe {
    fn name(&self) -> String {
        self.0.clone()
    }
}

#[derive(Clone)]
enum Behavior {
    Instances(Vec<PluginInstance>),
    Fail(String),
    Hang,
}

struct FakeRegistry {
    behavior: Mutex<Behavior>,
    calls: AtomicUsize,
}

impl FakeRegistry {
    fn new(behavior: Behavior) -> Arc<Self> {
        Arc::new(Self {
            behavior: Mutex::new(behavior),
            calls: AtomicUsize::new(0),
        })
    }

    fn set(&self, behavior: Behavior) {
        *self.behavior.lock().unwrap() = behavior;
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl PluginInstanceSource for FakeRegistry {
    async fn list_instances(&self) -> Result<Vec<PluginInstance>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let behavior = self.behavior.lock().unwrap().clone();
        match behavior {
            Behavior::Instances(instances) => Ok(instances),
            Behavior::Fail(message) => Err(message),
            Behavior::Hang => {
                std::future::pending::<()>().await;
                unreachable!("the registry fake never answers")
            }
        }
    }
}

#[derive(Default)]
struct FakeHub {
    clients: Mutex<HashMap<String, Arc<dyn Probe>>>,
}

impl FakeHub {
    fn register(&self, instance_id: &str) {
        self.clients.lock().unwrap().insert(
            instance_id.to_owned(),
            Arc::new(NamedProbe(instance_id.to_owned())),
        );
    }

    fn unregister(&self, instance_id: &str) {
        self.clients.lock().unwrap().remove(instance_id);
    }
}

impl PluginClientLookup<dyn Probe> for FakeHub {
    fn client(&self, instance_id: &str) -> Option<Arc<dyn Probe>> {
        self.clients.lock().unwrap().get(instance_id).cloned()
    }
}

fn instance(id: &str, vendor: &str, priority: i16) -> PluginInstance {
    PluginInstance {
        id: id.to_owned(),
        vendor: vendor.to_owned(),
        priority,
    }
}

fn resolver(registry: &Arc<FakeRegistry>, hub: &Arc<FakeHub>) -> PluginResolver<dyn Probe> {
    PluginResolver::new(
        VENDOR,
        Arc::clone(registry) as Arc<dyn PluginInstanceSource>,
        Arc::clone(hub) as Arc<dyn PluginClientLookup<dyn Probe>>,
    )
}

async fn resolved_name(resolver: &PluginResolver<dyn Probe>) -> String {
    resolver
        .resolve()
        .await
        .expect("resolution succeeds")
        .expect("a plugin is registered")
        .name()
}

#[test]
fn registry_query_is_bounded_by_ten_seconds() {
    assert_eq!(REGISTRY_QUERY_TIMEOUT, Duration::from_secs(10));
}

// ---- selection -----------------------------------------------------------------------

#[tokio::test]
async fn the_lowest_priority_value_wins() {
    let registry = FakeRegistry::new(Behavior::Instances(vec![
        instance("plugin.a", VENDOR, 50),
        instance("plugin.b", VENDOR, 10),
        instance("plugin.c", VENDOR, 30),
    ]));
    let hub = Arc::new(FakeHub::default());
    for id in ["plugin.a", "plugin.b", "plugin.c"] {
        hub.register(id);
    }
    let resolver = resolver(&registry, &hub);

    assert_eq!(resolved_name(&resolver).await, "plugin.b");
    assert_eq!(resolver.cached_instance_id().as_deref(), Some("plugin.b"));
}

#[tokio::test]
async fn equal_priorities_are_broken_by_instance_id_ascending() {
    let registry = FakeRegistry::new(Behavior::Instances(vec![
        instance("plugin.z", VENDOR, 10),
        instance("plugin.m", VENDOR, 10),
        instance("plugin.a", VENDOR, 10),
        instance("plugin.q", VENDOR, 10),
    ]));
    let hub = Arc::new(FakeHub::default());
    for id in ["plugin.z", "plugin.m", "plugin.a", "plugin.q"] {
        hub.register(id);
    }

    assert_eq!(resolved_name(&resolver(&registry, &hub)).await, "plugin.a");
}

#[tokio::test]
async fn only_instances_of_the_gear_vendor_are_considered() {
    let registry = FakeRegistry::new(Behavior::Instances(vec![
        instance("plugin.other", "acme", 1),
        instance("plugin.ours", VENDOR, 200),
    ]));
    let hub = Arc::new(FakeHub::default());
    hub.register("plugin.other");
    hub.register("plugin.ours");

    assert_eq!(
        resolved_name(&resolver(&registry, &hub)).await,
        "plugin.ours"
    );
}

#[tokio::test]
async fn instances_of_another_vendor_only_mean_no_plugin() {
    let registry = FakeRegistry::new(Behavior::Instances(vec![instance(
        "plugin.other",
        "acme",
        1,
    )]));
    let hub = Arc::new(FakeHub::default());
    hub.register("plugin.other");
    let resolver = resolver(&registry, &hub);

    let outcome = resolver.resolve().await.expect("no error");

    assert!(outcome.is_none());
    assert_eq!(resolver.cached_instance_id(), None);
}

// ---- state: unresolved -> resolved -----------------------------------------------------

#[tokio::test]
async fn the_selected_instance_is_cached_after_the_first_resolution() {
    let registry = FakeRegistry::new(Behavior::Instances(vec![instance("plugin.a", VENDOR, 10)]));
    let hub = Arc::new(FakeHub::default());
    hub.register("plugin.a");
    let resolver = resolver(&registry, &hub);

    assert_eq!(resolved_name(&resolver).await, "plugin.a");
    assert_eq!(resolved_name(&resolver).await, "plugin.a");
    assert_eq!(resolved_name(&resolver).await, "plugin.a");

    assert_eq!(registry.calls(), 1, "the registry is queried once");
}

#[tokio::test]
async fn a_cached_instance_is_served_even_when_the_registry_later_fails() {
    let registry = FakeRegistry::new(Behavior::Instances(vec![instance("plugin.a", VENDOR, 10)]));
    let hub = Arc::new(FakeHub::default());
    hub.register("plugin.a");
    let resolver = resolver(&registry, &hub);
    assert_eq!(resolved_name(&resolver).await, "plugin.a");

    registry.set(Behavior::Fail("registry down".to_owned()));

    assert_eq!(resolved_name(&resolver).await, "plugin.a");
    assert_eq!(registry.calls(), 1);
}

// ---- state: unresolved -> unresolved ----------------------------------------------------

#[tokio::test]
async fn no_instance_is_reported_and_not_cached() {
    let registry = FakeRegistry::new(Behavior::Instances(Vec::new()));
    let hub = Arc::new(FakeHub::default());
    let resolver = resolver(&registry, &hub);

    let first = resolver.resolve().await.expect("no error");
    let second = resolver.resolve().await.expect("no error");

    assert!(first.is_none() && second.is_none());
    assert_eq!(resolver.cached_instance_id(), None);
    assert_eq!(registry.calls(), 2, "the next call looks up again");
}

#[tokio::test]
async fn a_plugin_registered_later_is_found_by_the_next_call() {
    let registry = FakeRegistry::new(Behavior::Instances(Vec::new()));
    let hub = Arc::new(FakeHub::default());
    let resolver = resolver(&registry, &hub);
    assert!(resolver.resolve().await.expect("no error").is_none());

    registry.set(Behavior::Instances(vec![instance(
        "plugin.late",
        VENDOR,
        10,
    )]));
    hub.register("plugin.late");

    assert_eq!(resolved_name(&resolver).await, "plugin.late");
    assert_eq!(
        resolver.cached_instance_id().as_deref(),
        Some("plugin.late")
    );
}

// ---- state: resolved -> unresolved --------------------------------------------------------

#[tokio::test]
async fn a_missing_client_is_a_resolution_error_and_resets_the_cache() {
    let registry = FakeRegistry::new(Behavior::Instances(vec![instance("plugin.a", VENDOR, 10)]));
    let hub = Arc::new(FakeHub::default());
    let resolver = resolver(&registry, &hub);

    let err = resolver.resolve().await.err().expect("client missing");

    assert_eq!(
        err,
        PluginResolutionError::ClientMissing {
            instance_id: "plugin.a".to_owned()
        }
    );
    assert_eq!(resolver.cached_instance_id(), None, "cache is reset");

    hub.register("plugin.a");
    assert_eq!(resolved_name(&resolver).await, "plugin.a");
    assert_eq!(
        registry.calls(),
        2,
        "the registry is queried again after the reset"
    );
}

#[tokio::test]
async fn a_client_that_disappears_after_resolution_resets_the_cache() {
    let registry = FakeRegistry::new(Behavior::Instances(vec![instance("plugin.a", VENDOR, 10)]));
    let hub = Arc::new(FakeHub::default());
    hub.register("plugin.a");
    let resolver = resolver(&registry, &hub);
    assert_eq!(resolved_name(&resolver).await, "plugin.a");
    assert_eq!(resolver.cached_instance_id().as_deref(), Some("plugin.a"));

    hub.unregister("plugin.a");
    let err = resolver.resolve().await.err().expect("client missing");

    assert!(
        matches!(err, PluginResolutionError::ClientMissing { .. }),
        "got {err:?}"
    );
    assert_eq!(resolver.cached_instance_id(), None);
    assert_eq!(registry.calls(), 1, "the cached id skipped the registry");

    registry.set(Behavior::Instances(vec![instance("plugin.b", VENDOR, 10)]));
    hub.register("plugin.b");
    assert_eq!(resolved_name(&resolver).await, "plugin.b");
    assert_eq!(registry.calls(), 2);
}

// ---- registry failures ----------------------------------------------------------------------

#[tokio::test]
async fn a_registry_failure_is_a_resolution_error_and_is_not_cached() {
    let registry = FakeRegistry::new(Behavior::Fail("connection refused".to_owned()));
    let hub = Arc::new(FakeHub::default());
    hub.register("plugin.a");
    let resolver = resolver(&registry, &hub);

    let err = resolver.resolve().await.err().expect("registry failure");

    match &err {
        PluginResolutionError::RegistryQuery(message) => {
            assert!(message.contains("connection refused"), "{message}");
        }
        other => panic!("expected RegistryQuery, got {other:?}"),
    }
    assert_eq!(resolver.cached_instance_id(), None);

    registry.set(Behavior::Instances(vec![instance("plugin.a", VENDOR, 10)]));
    assert_eq!(resolved_name(&resolver).await, "plugin.a");
}

#[tokio::test]
async fn a_registry_query_that_exceeds_its_bound_is_a_timeout_error() {
    let registry = FakeRegistry::new(Behavior::Hang);
    let hub = Arc::new(FakeHub::default());
    let resolver = resolver(&registry, &hub).with_registry_timeout(Duration::from_millis(50));

    let err = tokio::time::timeout(Duration::from_secs(5), resolver.resolve())
        .await
        .expect("the resolver enforces its own bound")
        .err()
        .expect("timeout");

    assert_eq!(err, PluginResolutionError::RegistryTimeout);
    assert_eq!(resolver.cached_instance_id(), None);
}
