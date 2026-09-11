//! In-memory implementations of the three repository traits
//! (`cpt-cf-oagw-dod-inmemory-repos`, DECOMPOSITION correction 3).
//!
//! The stores are keyed by the composite `(tenant_id, id)` of the aggregate,
//! plus the natural-key indexes of `cpt-cf-oagw-db-schema`: `(tenant_id, alias)`
//! for upstreams and `(tenant_id, name)` for plugins. One shared state, guarded
//! by a single [`parking_lot::RwLock`], carries all three stores, so the
//! multi-field writes of one operation — a write with its index update, a
//! delete with its cascade — are atomic, and a SQL implementation can replace
//! them without changing a caller, because nothing behind the traits names a
//! storage technology.

// @cpt-begin:cpt-cf-oagw-dod-inmemory-repos:p1:inst-full

pub mod plugin;
pub mod route;
pub mod upstream;

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, Route, Upstream};
use crate::domain::repo::{PluginRepository, ResourceLifecycle};

/// A stored aggregate together with its lifecycle state.
#[derive(Debug, Clone)]
pub(crate) struct Stored<T> {
    /// The aggregate exactly as the caller handed it over.
    pub(crate) aggregate: T,
    /// The lifecycle state the write put it in.
    pub(crate) lifecycle: ResourceLifecycle,
    /// The insertion sequence of the write, the order the management list
    /// endpoints hand the aggregates back in.
    pub(crate) seq: u64,
}

impl<T> Stored<T> {
    /// A freshly stored aggregate.
    pub(crate) fn new(aggregate: T, lifecycle: ResourceLifecycle, seq: u64) -> Self {
        Self {
            aggregate,
            lifecycle,
            seq,
        }
    }
}

/// The tenant an aggregate belongs to, the scope of every store access.
pub(crate) trait TenantScoped {
    /// The tenant the aggregate is stamped with.
    fn tenant(&self) -> Option<Uuid>;
}

impl TenantScoped for Upstream {
    fn tenant(&self) -> Option<Uuid> {
        self.tenant_id
    }
}

impl TenantScoped for Route {
    fn tenant(&self) -> Option<Uuid> {
        self.tenant_id
    }
}

impl TenantScoped for Plugin {
    fn tenant(&self) -> Option<Uuid> {
        self.tenant_id
    }
}

/// The shared in-memory state of the three stores.
#[derive(Debug, Default)]
pub(crate) struct Inner {
    /// Upstreams by `(tenant_id, id)`.
    pub(crate) upstreams: HashMap<(Uuid, Uuid), Stored<Upstream>>,
    /// The unique-alias index by `(tenant_id, alias)`.
    pub(crate) upstream_aliases: HashMap<(Uuid, String), Uuid>,
    /// Routes by `(tenant_id, id)`.
    pub(crate) routes: HashMap<(Uuid, Uuid), Stored<Route>>,
    /// Plugins by `(tenant_id, id)`.
    pub(crate) plugins: HashMap<(Uuid, Uuid), Stored<Plugin>>,
    /// The unique-plugin-name index by `(tenant_id, name)`.
    pub(crate) plugin_names: HashMap<(Uuid, String), Uuid>,
    /// The `Deleted` tombstones: deleting is terminal, so an identifier that
    /// was deleted is not free again — re-creating the resource mints a new id.
    pub(crate) deleted: HashMap<(ResourceKind, Uuid, Uuid), ResourceLifecycle>,
    /// The monotonically increasing insertion sequence, shared by the three
    /// stores so a list reads back in insertion order.
    pub(crate) seq: u64,
}

impl Inner {
    /// Reserves the next insertion sequence number. Callers hold the write
    /// lock, so the reservation is atomic with the write that uses it.
    pub(crate) fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}

/// The three stores over one shared state, the unit a caller holds.
///
/// Cloning the bundle shares the state, so the handles stay consistent with one
/// another and a delete issued through one store is visible to the others.
#[derive(Debug, Clone, Default)]
pub struct InMemoryStores {
    inner: Arc<RwLock<Inner>>,
}

impl InMemoryStores {
    /// Creates the empty store triple: a restart begins with an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The upstream store.
    #[must_use]
    pub fn upstreams(&self) -> upstream::InMemoryUpstreamRepository {
        upstream::InMemoryUpstreamRepository::new(Arc::clone(&self.inner))
    }

    /// The route store.
    #[must_use]
    pub fn routes(&self) -> route::InMemoryRouteRepository {
        route::InMemoryRouteRepository::new(Arc::clone(&self.inner))
    }

    /// The plugin store.
    #[must_use]
    pub fn plugins(&self) -> plugin::InMemoryPluginRepository {
        plugin::InMemoryPluginRepository::new(Arc::clone(&self.inner))
    }

    /// Removes a plugin that no upstream and no route of the tenant still
    /// references, atomically (`cpt-cf-oagw-flow-plugin-delete`).
    ///
    /// The reference check and the removal happen under one write lock, so a
    /// concurrent write cannot slip a binding in between them. A plugin that is
    /// still referenced is left untouched and the references are reported.
    ///
    /// # Errors
    /// Returns the still-live references, empty when the plugin is absent from
    /// the tenant (the caller turns that into the not-found response).
    pub fn delete_plugin_unreferenced(
        &self,
        tenant_id: Uuid,
        plugin_id: Uuid,
    ) -> Result<(), Vec<PluginReference>> {
        // The behaviour lives on the `PluginRepository` trait, where every
        // implementation of the store can carry it; this inherent method stays
        // as the management surface's entry point and forwards to it.
        self.plugins()
            .delete_plugin_unreferenced(tenant_id, plugin_id)
    }
}

/// One still-live reference to a plugin, the item a `PluginInUse` conflict
/// reports.
pub use crate::domain::repo::PluginReference;

/// Which aggregate a composite key belongs to, so a deleted identity of one
/// kind never blocks another kind.
pub use crate::domain::repo::ResourceKind;

/// The typed not-found violation of a tenant-scoped lookup miss.
pub(crate) fn not_found(resource: &'static str) -> DomainError {
    DomainError::not_found(
        "id",
        format!("no {resource} with this identifier exists in the caller's tenant"),
    )
}

/// The typed already-exists violation of a taken key.
pub(crate) fn already_exists(field: &'static str, what: &str) -> DomainError {
    DomainError::already_exists(
        field,
        format!("an aggregate with this {what} already exists in the caller's tenant"),
    )
}

/// The composite-key lookup every store resolves its reads with
/// (`cpt-cf-oagw-flow-tenant-scoped-lookup`).
pub(crate) fn lookup<'a, T: TenantScoped>(
    index: &'a HashMap<(Uuid, Uuid), Stored<T>>,
    tenant_id: Uuid,
    id: Uuid,
    resource: &'static str,
) -> Result<&'a Stored<T>, DomainError> {
    // @cpt-begin:cpt-cf-oagw-flow-tenant-scoped-lookup:p1:inst-tl-02
    let found = index.get(&(tenant_id, id));
    // @cpt-end:cpt-cf-oagw-flow-tenant-scoped-lookup:p1:inst-tl-02
    // @cpt-begin:cpt-cf-oagw-flow-tenant-scoped-lookup:p1:inst-tl-03
    let in_scope = found.is_some_and(|stored| stored.aggregate.tenant() == Some(tenant_id));
    // @cpt-end:cpt-cf-oagw-flow-tenant-scoped-lookup:p1:inst-tl-03
    // @cpt-begin:cpt-cf-oagw-flow-tenant-scoped-lookup:p1:inst-tl-05
    // The key is absent from this tenant's index: the resource lives in another
    // tenant, it was deleted, or it never existed.
    let Some(stored) = found.filter(|_| in_scope) else {
        // @cpt-end:cpt-cf-oagw-flow-tenant-scoped-lookup:p1:inst-tl-05
        // @cpt-begin:cpt-cf-oagw-flow-tenant-scoped-lookup:p1:inst-tl-06
        return Err(not_found(resource));
        // @cpt-end:cpt-cf-oagw-flow-tenant-scoped-lookup:p1:inst-tl-06
    };
    // @cpt-begin:cpt-cf-oagw-flow-tenant-scoped-lookup:p1:inst-tl-04
    Ok(stored)
    // @cpt-end:cpt-cf-oagw-flow-tenant-scoped-lookup:p1:inst-tl-04
}

// @cpt-end:cpt-cf-oagw-dod-inmemory-repos:p1:inst-full

#[cfg(test)]
mod tests {
    use super::InMemoryStores;
    use crate::domain::model::{Plugin, Upstream};
    use crate::domain::repo::{UpstreamRepository, contract};
    use uuid::Uuid;

    /// The tenant the minted-identifier and plugin-delete tests work in.
    const MINT_TENANT: Uuid = Uuid::from_u128(0x7f0b_1b2c_3d4e_4f50_8617_8899_aabb_ccdd);
    /// The custom plugin [`a_plugin`] carries.
    const MINT_PLUGIN_ID: Uuid = Uuid::from_u128(0x3f0b_1b2c_3d4e_4f50_8617_8899_aabb_ccdd);

    /// The trait-driven contract suite, instantiated for the in-memory stores
    /// (`cpt-cf-oagw-dod-inmemory-repos`).
    #[test]
    fn the_upstream_store_satisfies_the_repository_contract() {
        contract::upstream_contract(&InMemoryStores::new().upstreams());
    }

    #[test]
    fn the_route_store_satisfies_the_repository_contract() {
        let stores = InMemoryStores::new();
        contract::route_contract(&stores.routes(), &stores.upstreams());
    }

    #[test]
    fn the_plugin_store_satisfies_the_repository_contract() {
        let stores = InMemoryStores::new();
        contract::plugin_contract(&stores.plugins(), &stores.upstreams(), &stores.routes());
    }

    /// The three handles share one state, so a delete issued through one store
    /// is visible to the others.
    #[test]
    fn the_three_stores_share_one_state() {
        use crate::domain::model::{Endpoint, PROTOCOL_HTTP, ServerConfig, Upstream};
        use uuid::Uuid;

        let stores = InMemoryStores::new();
        let upstream = Upstream {
            id: Some(Uuid::from_u128(0x0f0a)),
            tenant_id: Some(Uuid::from_u128(0x7f0a)),
            alias: Some("shared.vendor.com".to_owned()),
            protocol: Some(PROTOCOL_HTTP.to_owned()),
            server: Some(ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: "https".to_owned(),
                    host: Some("api.vendor.com".to_owned()),
                    port: 443,
                }],
            }),
            ..Upstream::default()
        };
        stores.upstreams().insert(&upstream).unwrap();
        assert!(
            stores
                .upstreams()
                .find(Uuid::from_u128(0x7f0a), Uuid::from_u128(0x0f0a))
                .is_ok()
        );
        stores
            .upstreams()
            .delete(Uuid::from_u128(0x7f0a), Uuid::from_u128(0x0f0a))
            .unwrap();
        assert!(
            stores
                .upstreams()
                .find(Uuid::from_u128(0x7f0a), Uuid::from_u128(0x0f0a))
                .is_err()
        );
    }

    /// An enabled, well-formed upstream of [`MINT_TENANT`] with the alias given.
    fn an_upstream(alias: &str) -> Upstream {
        use crate::domain::model::{Endpoint, PROTOCOL_HTTP, ServerConfig};

        Upstream {
            enabled: true,
            tenant_id: Some(MINT_TENANT),
            alias: Some(alias.to_owned()),
            protocol: Some(PROTOCOL_HTTP.to_owned()),
            server: Some(ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: "https".to_owned(),
                    host: Some("api.vendor.com".to_owned()),
                    port: 443,
                }],
            }),
            ..Upstream::default()
        }
    }

    /// A well-formed Starlark plugin of [`MINT_TENANT`].
    fn a_plugin() -> Plugin {
        Plugin {
            id: Some(MINT_PLUGIN_ID),
            tenant_id: Some(MINT_TENANT),
            plugin_type: Some("gts.cf.core.oagw.transform_plugin.v1~".to_owned()),
            name: Some("redact-headers".to_owned()),
            config_schema: Some(serde_json::json!({ "type": "object" })),
            source_code: Some("def apply(ctx):\n    pass\n".to_owned()),
            ..Plugin::default()
        }
    }

    /// An upstream of [`MINT_TENANT`] whose one binding names the custom plugin
    /// [`MINT_PLUGIN_ID`].
    fn an_upstream_bound_to_the_plugin(alias: &str) -> Upstream {
        use crate::domain::model::PluginsConfig;

        let mut bound = an_upstream(alias);
        bound.plugins = Some(PluginsConfig {
            sharing: "private".to_owned(),
            items: vec![format!(
                "gts.cf.core.oagw.transform_plugin.v1~{}",
                MINT_PLUGIN_ID
            )],
        });
        bound
    }

    /// A candidate that carries no identifier is stored under one the store
    /// mints, and that identifier is what the stored view carries back, so no
    /// aggregate is ever keyed by the nil UUID.
    #[test]
    fn an_idless_upstream_insert_is_stored_under_a_minted_identifier() {
        let stores = InMemoryStores::new();
        let stored = stores
            .upstreams()
            .insert(&an_upstream("minted.vendor.com"))
            .unwrap();
        let id = stored
            .id
            .expect("the stored view carries the identifier the store resolved");
        assert!(!id.is_nil(), "the store never keys an aggregate by nil");
        assert!(stored.alias.as_deref() == Some("minted.vendor.com"));
        assert!(
            stores.upstreams().find(MINT_TENANT, id).is_ok(),
            "the minted identifier is the key the composite reads reach"
        );
    }

    /// The unreferenced-plugin delete is reachable through the
    /// `PluginRepository` trait, and it reports a referenced plugin untouched.
    #[test]
    fn the_unreferenced_plugin_delete_is_reachable_through_the_trait() {
        use crate::domain::repo::{PluginReference, PluginRepository, ResourceKind};

        let stores = InMemoryStores::new();
        stores
            .upstreams()
            .insert(&an_upstream_bound_to_the_plugin("payments.vendor.com"))
            .unwrap();
        stores.plugins().insert(&a_plugin()).unwrap();

        let references = stores
            .plugins()
            .delete_plugin_unreferenced(MINT_TENANT, MINT_PLUGIN_ID)
            .unwrap_err();
        assert_eq!(
            references,
            vec![PluginReference {
                kind: ResourceKind::Upstream,
                id: stores
                    .upstreams()
                    .find_by_alias(MINT_TENANT, "payments.vendor.com")
                    .unwrap()
                    .id
                    .expect("present"),
                binding_name: format!("gts.cf.core.oagw.transform_plugin.v1~{}", MINT_PLUGIN_ID),
            }],
            "a still-referenced plugin is left untouched and its references are reported"
        );
        assert!(stores.plugins().find(MINT_TENANT, MINT_PLUGIN_ID).is_ok());

        // With the referencing upstream gone, the same call removes the plugin.
        let upstream_id = stores
            .upstreams()
            .find_by_alias(MINT_TENANT, "payments.vendor.com")
            .unwrap()
            .id
            .expect("present");
        stores.upstreams().delete(MINT_TENANT, upstream_id).unwrap();
        stores
            .plugins()
            .delete_plugin_unreferenced(MINT_TENANT, MINT_PLUGIN_ID)
            .unwrap();
        assert!(stores.plugins().find(MINT_TENANT, MINT_PLUGIN_ID).is_err());

        // A plugin absent from the tenant answers the empty reference set.
        assert!(
            stores
                .plugins()
                .delete_plugin_unreferenced(MINT_TENANT, MINT_PLUGIN_ID)
                .unwrap_err()
                .is_empty()
        );
    }
}
