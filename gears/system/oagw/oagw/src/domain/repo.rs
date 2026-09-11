//! Repository traits and the resource lifecycle of the `oagw` gear.
//!
//! The traits are the tenant-scoped persistence boundary of the domain layer
//! (`cpt-cf-oagw-dod-repository-traits`): every lookup and write is scoped by a
//! tenant identifier, so no method can address a resource across tenants. The
//! in-memory stores of `src/infra/storage/` implement them; a SQL backend can
//! replace those without changing a caller, because nothing at this boundary
//! names a storage technology.

// @cpt-begin:cpt-cf-oagw-dod-repository-traits:p1:inst-full

use crate::domain::error::{DomainError, Violation, ViolationKind};
use crate::domain::model::{Plugin, Route, Upstream};
use uuid::Uuid;

/// The feature-local lifecycle of a stored resource
/// (`cpt-cf-oagw-state-resource-lifecycle`).
///
/// `Draft` is the not-yet-stored state between validation and the repository
/// write, whose only exit is the `enabled` flag of the aggregate; `Plugin`
/// carries no `enabled` field and enters `Active` directly when it is stored.
/// `Deleted` is terminal, and the lifecycle is in-process only: a restart
/// begins with an empty store, so no state survives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ResourceLifecycle {
    /// The aggregate is validated but not stored yet.
    Draft,
    /// The aggregate is stored with `enabled: true`.
    Active,
    /// The aggregate is stored disabled, so a route is excluded from matching
    /// and proxying to an upstream is rejected.
    Disabled,
    /// The aggregate was deleted; terminal for the in-memory store.
    Deleted,
}

impl ResourceLifecycle {
    /// The state of an aggregate that has not been written yet.
    #[must_use]
    pub const fn initial() -> Self {
        Self::Draft
    }

    /// The state a validated aggregate enters when it is written
    /// (`inst-rl-01` for `enabled: true`, `inst-rl-02` for `enabled: false`).
    #[must_use]
    pub fn from_enabled(enabled: bool) -> Self {
        if enabled {
            // `Draft` to `Active` (`inst-rl-01`).
            return Self::Active;
        }
        // `Draft` to `Disabled`, so a resource created disabled never passes
        // through `Active` (`inst-rl-02`).
        Self::Disabled
    }

    /// Whether the resource takes part in proxy request handling.
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }

    /// Whether the resource was deleted, which is terminal.
    #[must_use]
    pub const fn is_deleted(self) -> bool {
        matches!(self, Self::Deleted)
    }

    /// The closed state name, for diagnostics and tests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "Draft",
            Self::Active => "Active",
            Self::Disabled => "Disabled",
            Self::Deleted => "Deleted",
        }
    }

    /// Applies `to` to `self` per the state machine of FEATURE §4.
    ///
    /// Returns `Some` with the reached state, or `None` when the transition is
    /// refused and the state stays unchanged: `Draft` never reaches `Deleted`
    /// directly, `Disabled` never returns to `Draft`, and no transition leaves
    /// `Deleted`. Reaching the state the resource already occupies is a no-op,
    /// so re-setting the enable flag on an `Active` resource keeps it `Active`.
    #[must_use]
    pub fn transition(self, to: Self) -> Option<Self> {
        if self == to {
            return Some(self);
        }
        if to == Self::Draft || self == Self::Deleted {
            return None;
        }
        match (self, to) {
            // @cpt-begin:cpt-cf-oagw-state-resource-lifecycle:p1:inst-rl-01
            (Self::Draft, Self::Active) => Some(to),
            // @cpt-end:cpt-cf-oagw-state-resource-lifecycle:p1:inst-rl-01
            // @cpt-begin:cpt-cf-oagw-state-resource-lifecycle:p1:inst-rl-02
            (Self::Draft, Self::Disabled) => Some(to),
            // @cpt-end:cpt-cf-oagw-state-resource-lifecycle:p1:inst-rl-02
            // @cpt-begin:cpt-cf-oagw-state-resource-lifecycle:p1:inst-rl-03
            (Self::Active, Self::Disabled) => Some(to),
            // @cpt-end:cpt-cf-oagw-state-resource-lifecycle:p1:inst-rl-03
            // @cpt-begin:cpt-cf-oagw-state-resource-lifecycle:p1:inst-rl-04
            (Self::Disabled, Self::Active) => Some(to),
            // @cpt-end:cpt-cf-oagw-state-resource-lifecycle:p1:inst-rl-04
            // @cpt-begin:cpt-cf-oagw-state-resource-lifecycle:p1:inst-rl-05
            (Self::Active, Self::Deleted) => Some(to),
            // @cpt-end:cpt-cf-oagw-state-resource-lifecycle:p1:inst-rl-05
            // @cpt-begin:cpt-cf-oagw-state-resource-lifecycle:p1:inst-rl-06
            (Self::Disabled, Self::Deleted) => Some(to),
            // @cpt-end:cpt-cf-oagw-state-resource-lifecycle:p1:inst-rl-06
            _ => None,
        }
    }
}

impl std::fmt::Display for ResourceLifecycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which aggregate a composite key belongs to, so a deleted identity of one
/// kind never blocks another kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceKind {
    /// An upstream.
    Upstream,
    /// A route.
    Route,
    /// A plugin.
    Plugin,
}

/// One still-live reference to a plugin, the item a `PluginInUse` conflict
/// reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginReference {
    /// The kind of aggregate that holds the binding.
    pub kind: ResourceKind,
    /// The identifier of that aggregate, rendered as a bare UUID.
    pub id: Uuid,
    /// The binding name the reference is made under.
    pub binding_name: String,
}

/// The tenant context of an aggregate handed to a write path.
///
/// The write paths of the repository traits take the tenant from the aggregate
/// itself — the caller stamps it from its tenant context — and validation
/// rejects an aggregate that carries none, so this is the fallback of last
/// resort that keeps the tenant scope unforgeable.
pub(crate) fn tenant_scope(tenant_id: Option<Uuid>) -> Result<Uuid, DomainError> {
    tenant_id.map_or_else(
        || {
            Err(DomainError::from_violation(Violation::new(
                ViolationKind::MalformedUuid,
                "tenant_id",
                "the caller's tenant context is required",
            )))
        },
        Ok,
    )
}

/// Tenant-scoped persistence of an [`Upstream`] aggregate.
///
/// The composite key is `(tenant_id, id)` and the alias is unique per
/// `(tenant_id, alias)` (`cpt-cf-oagw-db-schema`), so a second upstream with the
/// same alias in the same tenant is a duplicate, while the same alias in another
/// tenant is a different aggregate.
pub trait UpstreamRepository: Send + Sync {
    /// Stores a validated upstream.
    ///
    /// # Errors
    /// Returns every validation violation of the candidate, or `already-exists`
    /// when its `(tenant_id, id)` or `(tenant_id, alias)` key is taken.
    fn insert(&self, upstream: &Upstream) -> Result<Upstream, DomainError>;

    /// Replaces a stored upstream wholesale, clearing the optional fields the
    /// candidate omits.
    ///
    /// # Errors
    /// Returns the validation violations, or `not-found` when the composite key
    /// is absent from the tenant.
    fn replace(&self, upstream: &Upstream) -> Result<Upstream, DomainError>;

    // @cpt-begin:cpt-cf-oagw-flow-tenant-scoped-lookup:p1:inst-tl-01
    /// Looks an upstream up by the composite key `(tenant_id, id)`, restricted
    /// to the caller's tenant.
    ///
    /// # Errors
    /// Returns `not-found` when the key is absent from this tenant's index, so
    /// a resource of another or of an ancestor tenant is indistinguishable from
    /// an absent one.
    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError>;
    // @cpt-end:cpt-cf-oagw-flow-tenant-scoped-lookup:p1:inst-tl-01

    /// Looks an upstream up by `(tenant_id, alias)`, the unique-alias index of
    /// `cpt-cf-oagw-db-schema`.
    ///
    /// # Errors
    /// Returns `not-found` when no upstream of the tenant carries the alias.
    fn find_by_alias(&self, tenant_id: Uuid, alias: &str) -> Result<Upstream, DomainError>;

    /// Every upstream stored for the tenant, in insertion order, the read seam
    /// the management list endpoints apply their OData subset to.
    ///
    /// # Errors
    /// Returns `not-found` never: an empty tenant is an empty list, not a
    /// missing resource.
    fn list(&self, tenant_id: Uuid) -> Result<Vec<Upstream>, DomainError>;

    /// Sets the enable flag of a stored upstream, the `Active`/`Disabled`
    /// transition of `cpt-cf-oagw-fr-enable-disable`.
    ///
    /// # Errors
    /// Returns `not-found` when the key is absent from the tenant.
    fn set_enabled(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        enabled: bool,
    ) -> Result<Upstream, DomainError>;

    /// The lifecycle state of a stored upstream.
    ///
    /// # Errors
    /// Returns `not-found` when the key is absent from the tenant, which also
    /// covers an upstream that lives in another tenant.
    fn lifecycle(&self, tenant_id: Uuid, id: Uuid) -> Result<ResourceLifecycle, DomainError>;

    /// Removes an upstream and cascades the removal to its routes.
    ///
    /// # Errors
    /// Returns `not-found` when the key is absent from the tenant.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Upstream, DomainError>;
}

/// Tenant-scoped persistence of a [`Route`] aggregate.
///
/// Routes are owned by an upstream of the same tenant, so every write checks
/// the `upstream_id` reference against the caller's tenant store.
pub trait RouteRepository: Send + Sync {
    /// Stores a validated route.
    ///
    /// # Errors
    /// Returns the validation violations, `already-exists` for a taken
    /// composite key or for a route that breaks the route-match determinism
    /// invariant, and `not-found` when `upstream_id` names no upstream of the
    /// tenant.
    fn insert(&self, route: &Route) -> Result<Route, DomainError>;

    /// Replaces a stored route wholesale.
    ///
    /// # Errors
    /// Returns the validation violations, `already-exists` for a determinism
    /// conflict, or `not-found` when the composite key is absent.
    fn replace(&self, route: &Route) -> Result<Route, DomainError>;

    /// Looks a route up by the composite key `(tenant_id, id)`.
    ///
    /// # Errors
    /// Returns `not-found` when the key is absent from the tenant.
    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError>;

    /// Lists the routes of one upstream of the tenant, in the deterministic
    /// order the route-match invariant needs: priority descending, then id.
    ///
    /// # Errors
    /// Returns `not-found` when the upstream is absent from the tenant.
    fn list_by_upstream(
        &self,
        tenant_id: Uuid,
        upstream_id: Uuid,
    ) -> Result<Vec<Route>, DomainError>;

    /// Every route stored for the tenant, in insertion order, the read seam the
    /// management list endpoint applies its OData subset to.
    ///
    /// # Errors
    /// Returns `not-found` never: an empty tenant is an empty list, not a
    /// missing resource.
    fn list(&self, tenant_id: Uuid) -> Result<Vec<Route>, DomainError>;

    /// Sets the enable flag of a stored route.
    ///
    /// # Errors
    /// Returns `not-found` when the key is absent from the tenant, or
    /// `already-exists` when enabling the route creates a determinism conflict.
    fn set_enabled(&self, tenant_id: Uuid, id: Uuid, enabled: bool) -> Result<Route, DomainError>;

    /// The lifecycle state of a stored route.
    ///
    /// # Errors
    /// Returns `not-found` when the key is absent from the tenant.
    fn lifecycle(&self, tenant_id: Uuid, id: Uuid) -> Result<ResourceLifecycle, DomainError>;

    /// Removes a route and, with it, its plugin bindings.
    ///
    /// # Errors
    /// Returns `not-found` when the key is absent from the tenant.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Route, DomainError>;
}

/// Tenant-scoped persistence of a [`Plugin`] aggregate.
///
/// A stored plugin is a Starlark custom plugin; its natural key besides the
/// composite one is `(tenant_id, name)`, the plugin uniqueness invariant of
/// `cpt-cf-oagw-db-schema`.
pub trait PluginRepository: Send + Sync {
    /// Stores a validated plugin, which enters `Active` directly from `Draft`
    /// because a plugin carries no `enabled` field.
    ///
    /// # Errors
    /// Returns the validation violations, or `already-exists` when its
    /// composite key or its `(tenant_id, name)` key is taken.
    fn insert(&self, plugin: &Plugin) -> Result<Plugin, DomainError>;

    /// Replaces a stored plugin wholesale.
    ///
    /// # Errors
    /// Returns the validation violations, or `not-found` when the composite key
    /// is absent.
    fn replace(&self, plugin: &Plugin) -> Result<Plugin, DomainError>;

    /// Looks a plugin up by the composite key `(tenant_id, id)`.
    ///
    /// # Errors
    /// Returns `not-found` when the key is absent from the tenant.
    fn find(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError>;

    /// Looks a plugin up by `(tenant_id, name)`.
    ///
    /// # Errors
    /// Returns `not-found` when no plugin of the tenant carries the name.
    fn find_by_name(&self, tenant_id: Uuid, name: &str) -> Result<Plugin, DomainError>;

    /// Every plugin stored for the tenant, in insertion order, the read seam
    /// the management list endpoint applies its OData subset to.
    ///
    /// # Errors
    /// Returns `not-found` never: an empty tenant is an empty list, not a
    /// missing resource.
    fn list(&self, tenant_id: Uuid) -> Result<Vec<Plugin>, DomainError>;

    /// The lifecycle state of a stored plugin.
    ///
    /// # Errors
    /// Returns `not-found` when the key is absent from the tenant.
    fn lifecycle(&self, tenant_id: Uuid, id: Uuid) -> Result<ResourceLifecycle, DomainError>;

    /// Removes a plugin and every binding that still references it.
    ///
    /// # Errors
    /// Returns `not-found` when the key is absent from the tenant.
    fn delete(&self, tenant_id: Uuid, id: Uuid) -> Result<Plugin, DomainError>;

    /// Removes a plugin that no upstream and no route of the tenant still
    /// references, atomically.
    ///
    /// The reference check and the removal are one store operation, so a
    /// concurrent write cannot slip a binding in between them; a plugin that is
    /// still referenced is left untouched and the references are reported.
    ///
    /// # Errors
    /// Returns the still-live references; the set is empty when the plugin is
    /// absent from the tenant, which the caller turns into the not-found
    /// response.
    fn delete_plugin_unreferenced(
        &self,
        tenant_id: Uuid,
        plugin_id: Uuid,
    ) -> Result<(), Vec<PluginReference>>;
}

/// The trait-driven contract suite of the three repositories.
///
/// Written once against the traits, it is instantiated by the in-memory stores
/// of `src/infra/storage/` and is the suite any other implementation of the
/// traits has to pass.
#[cfg(test)]
pub(crate) mod contract {
    use super::{PluginRepository, ResourceLifecycle, RouteRepository, UpstreamRepository};
    use crate::domain::error::{DomainError, ViolationKind};
    use crate::domain::model::{
        Endpoint, GrpcMatch, HttpMatch, MatchConfig, PROTOCOL_HTTP, Plugin, PluginsConfig, Route,
        ServerConfig, Upstream,
    };
    use uuid::Uuid;

    /// A named builtin plugin, referenced by its GTS identifier only.
    const BUILTIN_PLUGIN: &str =
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
    /// The Starlark plugin family, the prefix of a custom plugin's GTS ref.
    const CUSTOM_PLUGIN_FAMILY: &str = "gts.cf.core.oagw.transform_plugin.v1~";

    const TENANT: Uuid = Uuid::from_u128(0x7f0a_1b2c_3d4e_4f50_8617_8899_aabb_ccdd);
    const OTHER_TENANT: Uuid = Uuid::from_u128(0x1f0a_1b2c_3d4e_4f50_8617_8899_aabb_ccdd);
    const UPSTREAM_ID: Uuid = Uuid::from_u128(0x0f0a_1b2c_3d4e_4f50_8617_8899_aabb_ccdd);
    const ROUTE_ID: Uuid = Uuid::from_u128(0x2f0a_1b2c_3d4e_4f50_8617_8899_aabb_ccdd);
    const SIBLING_ID: Uuid = Uuid::from_u128(0x4f0a_1b2c_3d4e_4f50_8617_8899_aabb_ccdd);
    const PLUGIN_ID: Uuid = Uuid::from_u128(0x3f0a_1b2c_3d4e_4f50_8617_8899_aabb_ccdd);
    /// A second upstream of `TENANT`, of the gRPC protocol.
    const GRPC_UPSTREAM_ID: Uuid = Uuid::from_u128(0x5f0a_1b2c_3d4e_4f50_8617_8899_aabb_ccdd);

    fn kind_of(error: &DomainError) -> Option<ViolationKind> {
        error.kind()
    }

    fn is_not_found(error: &DomainError) -> bool {
        kind_of(error) == Some(ViolationKind::NotFound)
    }

    /// An enabled, well-formed upstream of `TENANT`.
    fn upstream(alias: &str) -> Upstream {
        Upstream {
            id: Some(UPSTREAM_ID),
            tenant_id: Some(TENANT),
            enabled: true,
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

    /// An enabled, well-formed HTTP route of `TENANT` under `UPSTREAM_ID`.
    fn route(id: Uuid, path: &str, priority: i64, enabled: bool) -> Route {
        Route {
            id: Some(id),
            tenant_id: Some(TENANT),
            enabled,
            priority,
            upstream_id: Some(UPSTREAM_ID),
            match_config: Some(MatchConfig {
                http: Some(HttpMatch {
                    methods: vec!["GET".to_owned()],
                    path: Some(path.to_owned()),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: "append".to_owned(),
                }),
                grpc: None,
            }),
            ..Route::default()
        }
    }

    /// An enabled, well-formed gRPC route of `TENANT` under the gRPC upstream.
    fn grpc_route(id: Uuid, service: &str, priority: i64) -> Route {
        Route {
            id: Some(id),
            tenant_id: Some(TENANT),
            enabled: true,
            priority,
            upstream_id: Some(GRPC_UPSTREAM_ID),
            match_config: Some(MatchConfig {
                http: None,
                grpc: Some(GrpcMatch {
                    service: Some(service.to_owned()),
                    method: Some("Send".to_owned()),
                }),
            }),
            ..Route::default()
        }
    }

    /// A well-formed Starlark plugin of `TENANT`.
    fn plugin(name: &str) -> Plugin {
        Plugin {
            id: Some(PLUGIN_ID),
            tenant_id: Some(TENANT),
            plugin_type: Some(CUSTOM_PLUGIN_FAMILY.to_owned()),
            name: Some(name.to_owned()),
            config_schema: Some(serde_json::json!({ "type": "object" })),
            source_code: Some("def apply(ctx):\n    pass\n".to_owned()),
            ..Plugin::default()
        }
    }

    /// The `UpstreamRepository` contract.
    pub fn upstream_contract(repo: &dyn UpstreamRepository) {
        // A validated upstream stored with `enabled: true` is `Active`, and the
        // stored view is returned to the caller (`inst-mr-11`).
        let stored = repo.insert(&upstream("payments.vendor.com")).unwrap();
        assert_eq!(stored.id, Some(UPSTREAM_ID));
        assert_eq!(
            repo.lifecycle(TENANT, UPSTREAM_ID),
            Ok(ResourceLifecycle::Active)
        );

        // A second upstream with the same id or the same alias is a duplicate
        // (`inst-mr-06`, `inst-mr-07`).
        let duplicate = repo.insert(&upstream("payments.vendor.com")).unwrap_err();
        assert_eq!(
            kind_of(&duplicate),
            Some(ViolationKind::AlreadyExists),
            "{duplicate}"
        );
        let mut same_id = upstream("other.vendor.com");
        same_id.id = Some(UPSTREAM_ID);
        assert_eq!(
            kind_of(&repo.insert(&same_id).unwrap_err()),
            Some(ViolationKind::AlreadyExists)
        );

        // The composite lookup answers within the tenant and is silent about
        // other tenants (`inst-tl-05`, `inst-tl-06`).
        assert_eq!(
            repo.find(TENANT, UPSTREAM_ID).unwrap().alias.as_deref(),
            Some("payments.vendor.com")
        );
        let foreign = repo.find(OTHER_TENANT, UPSTREAM_ID).unwrap_err();
        assert!(is_not_found(&foreign), "{foreign}");
        let missing = repo.find(TENANT, Uuid::nil()).unwrap_err();
        assert!(is_not_found(&missing), "{missing}");
        assert_eq!(
            foreign.field(),
            missing.field(),
            "a foreign resource is indistinguishable from an absent one"
        );

        // The (tenant id, alias) lookup of the unique-alias index.
        assert_eq!(
            repo.find_by_alias(TENANT, "payments.vendor.com")
                .unwrap()
                .id,
            Some(UPSTREAM_ID)
        );
        assert!(is_not_found(
            &repo.find_by_alias(TENANT, "nope.vendor.com").unwrap_err()
        ));
        assert!(is_not_found(
            &repo
                .find_by_alias(OTHER_TENANT, "payments.vendor.com")
                .unwrap_err()
        ));

        // Replacement is wholesale: the optional blocks the candidate omits are
        // cleared, and storing with `enabled: false` enters `Disabled`
        // (`inst-rl-02`).
        let mut bare = upstream("payments.vendor.com");
        bare.enabled = false;
        let replaced = repo.replace(&bare).unwrap();
        assert!(
            replaced.auth.is_none() && replaced.cors.is_none() && replaced.rate_limit.is_none()
        );
        assert_eq!(
            repo.lifecycle(TENANT, UPSTREAM_ID),
            Ok(ResourceLifecycle::Disabled)
        );

        // The enable/disable flag moves the aggregate between `Active` and
        // `Disabled` without a re-store (`inst-rl-03`, `inst-rl-04`).
        let disabled = repo.set_enabled(TENANT, UPSTREAM_ID, false).unwrap();
        assert!(!disabled.enabled);
        assert_eq!(
            repo.lifecycle(TENANT, UPSTREAM_ID),
            Ok(ResourceLifecycle::Disabled)
        );
        assert!(is_not_found(
            &repo
                .set_enabled(OTHER_TENANT, UPSTREAM_ID, false)
                .unwrap_err()
        ));
        let re_enabled = repo.set_enabled(TENANT, UPSTREAM_ID, true).unwrap();
        assert!(re_enabled.enabled);
        assert_eq!(
            repo.lifecycle(TENANT, UPSTREAM_ID),
            Ok(ResourceLifecycle::Active)
        );

        // An invalid candidate is refused before any mutation is applied
        // (`inst-mr-03`), so nothing is stored under its key.
        let invalid = upstream(&"a".repeat(254));
        let error = repo.insert(&invalid).unwrap_err();
        assert_eq!(
            kind_of(&error),
            Some(ViolationKind::MalformedAlias),
            "{error}"
        );
        assert!(is_not_found(&repo.find(TENANT, Uuid::nil()).unwrap_err()));

        // Deletion removes the aggregate, is reported as the `Deleted` state and
        // is terminal (`inst-rl-05`).
        let deleted = repo.delete(TENANT, UPSTREAM_ID).unwrap();
        assert_eq!(deleted.alias.as_deref(), Some("payments.vendor.com"));
        assert!(is_not_found(&repo.find(TENANT, UPSTREAM_ID).unwrap_err()));
        assert_eq!(
            repo.lifecycle(TENANT, UPSTREAM_ID),
            Ok(ResourceLifecycle::Deleted)
        );
        assert!(is_not_found(&repo.delete(TENANT, UPSTREAM_ID).unwrap_err()));
        assert!(is_not_found(
            &repo.delete(OTHER_TENANT, UPSTREAM_ID).unwrap_err()
        ));
    }

    /// The `RouteRepository` contract; `upstreams` carries the upstream the
    /// routes of `TENANT` reference.
    pub fn route_contract(routes: &dyn RouteRepository, upstreams: &dyn UpstreamRepository) {
        upstreams.insert(&upstream("payments.vendor.com")).unwrap();
        let mut grpc_upstream = upstream("rpc.vendor.com");
        grpc_upstream.id = Some(GRPC_UPSTREAM_ID);
        grpc_upstream.protocol = Some(crate::domain::model::PROTOCOL_GRPC.to_owned());
        upstreams.insert(&grpc_upstream).unwrap();

        // A route whose upstream exists in the caller's tenant is stored.
        let stored = routes
            .insert(&route(ROUTE_ID, "/v1/chat", 10, true))
            .unwrap();
        assert_eq!(stored.upstream_id(), Some(UPSTREAM_ID));
        assert_eq!(
            routes.lifecycle(TENANT, ROUTE_ID),
            Ok(ResourceLifecycle::Active)
        );

        // The `upstream_id` reference is resolved in the caller's tenant only,
        // and reads as not-found (`inst-mr-03`).
        let sibling =
            |index: u128| Uuid::from_u128(0x4f0a_0000_0000_0000_0000_0000_0000_0000 + index);
        let mut foreign = route(sibling(1), "/v1/chat", 10, true);
        foreign.upstream_id = Some(Uuid::nil());
        let error = routes.insert(&foreign).unwrap_err();
        assert_eq!(error.field(), "upstream_id", "{error}");
        assert!(is_not_found(&error), "{error}");

        // The route-match determinism invariant (`inst-mr-10`): a second
        // enabled route under the same upstream with the same path prefix, the
        // same priority and a shared method conflicts ...
        let conflict = routes
            .insert(&route(sibling(1), "/v1/chat", 10, true))
            .unwrap_err();
        assert_eq!(
            kind_of(&conflict),
            Some(ViolationKind::AlreadyExists),
            "{conflict}"
        );
        // ... while a different priority, a different path and a disabled
        // candidate are all accepted.
        routes
            .insert(&route(sibling(2), "/v1/chat", 5, true))
            .unwrap();
        routes
            .insert(&route(sibling(3), "/v1/other", 10, true))
            .unwrap();
        routes
            .insert(&route(sibling(4), "/v1/chat", 10, false))
            .unwrap();

        // A gRPC match keys the same invariant on (service, method, priority),
        // against the upstream of the gRPC protocol it matches.
        let grpc = grpc_route(sibling(5), "vendor.chat.v1.ChatService", 10);
        routes.insert(&grpc).unwrap();
        let grpc_conflict = grpc_route(sibling(6), "vendor.chat.v1.ChatService", 10);
        assert_eq!(
            kind_of(&routes.insert(&grpc_conflict).unwrap_err()),
            Some(ViolationKind::AlreadyExists)
        );

        // Tenant-scoped reads.
        assert_eq!(routes.find(TENANT, ROUTE_ID).unwrap().priority, 10);
        assert!(is_not_found(
            &routes.find(OTHER_TENANT, ROUTE_ID).unwrap_err()
        ));
        assert_eq!(
            routes.list_by_upstream(TENANT, UPSTREAM_ID).unwrap().len(),
            4,
            "the routes of the gRPC upstream are not listed"
        );
        assert_eq!(
            routes
                .list_by_upstream(TENANT, GRPC_UPSTREAM_ID)
                .unwrap()
                .len(),
            1
        );
        assert!(is_not_found(
            &routes
                .list_by_upstream(OTHER_TENANT, UPSTREAM_ID)
                .unwrap_err()
        ));

        // The enable flag participates in the invariant.
        let enabled = routes.set_enabled(TENANT, ROUTE_ID, true).unwrap();
        assert!(enabled.enabled);
        assert!(is_not_found(
            &routes
                .set_enabled(OTHER_TENANT, ROUTE_ID, true)
                .unwrap_err()
        ));

        // Deletion is terminal and scoped (`inst-rl-05`).
        routes.delete(TENANT, ROUTE_ID).unwrap();
        assert!(is_not_found(&routes.find(TENANT, ROUTE_ID).unwrap_err()));
        assert_eq!(
            routes.lifecycle(TENANT, ROUTE_ID),
            Ok(ResourceLifecycle::Deleted)
        );

        // The upstream delete cascade (`inst-mr-12`): removing an upstream
        // removes its routes, so a tenant-scoped lookup by that upstream id
        // returns not-found and no route in the tenant's store still carries
        // the deleted upstream id.
        upstreams.delete(TENANT, UPSTREAM_ID).unwrap();
        assert_eq!(
            upstreams.lifecycle(TENANT, UPSTREAM_ID),
            Ok(ResourceLifecycle::Deleted)
        );
        assert!(is_not_found(
            &upstreams.find(TENANT, UPSTREAM_ID).unwrap_err()
        ));
        assert!(is_not_found(
            &routes.list_by_upstream(TENANT, UPSTREAM_ID).unwrap_err()
        ));
        for route_id in [ROUTE_ID, sibling(2), sibling(3), sibling(4)] {
            assert!(is_not_found(&routes.find(TENANT, route_id).unwrap_err()));
        }
        assert!(
            routes.find(TENANT, sibling(5)).is_ok(),
            "the routes of another upstream are untouched by the cascade"
        );
    }

    /// The `PluginRepository` contract; `upstreams` and `routes` receive the
    /// bindings a plugin deletion cascades away (`inst-mr-12`).
    pub fn plugin_contract(
        plugins: &dyn PluginRepository,
        upstreams: &dyn UpstreamRepository,
        routes: &dyn RouteRepository,
    ) {
        let mut bound = upstream("payments.vendor.com");
        bound.plugins = Some(PluginsConfig {
            sharing: "private".to_owned(),
            items: vec![
                BUILTIN_PLUGIN.to_owned(),
                format!("{CUSTOM_PLUGIN_FAMILY}{PLUGIN_ID}"),
            ],
        });
        upstreams.insert(&bound).unwrap();
        let mut with_binding = route(ROUTE_ID, "/v1/chat", 10, true);
        with_binding.plugins = Some(PluginsConfig {
            sharing: "private".to_owned(),
            items: vec![BUILTIN_PLUGIN.to_owned()],
        });
        routes.insert(&with_binding).unwrap();

        // A stored plugin enters `Active` directly from `Draft`: it carries no
        // `enabled` field.
        plugins.insert(&plugin("redact-headers")).unwrap();
        assert_eq!(
            plugins.lifecycle(TENANT, PLUGIN_ID),
            Ok(ResourceLifecycle::Active)
        );

        // Both uniqueness keys are enforced per tenant (`inst-mr-06`).
        let duplicate = plugins.insert(&plugin("redact-headers")).unwrap_err();
        assert_eq!(
            kind_of(&duplicate),
            Some(ViolationKind::AlreadyExists),
            "{duplicate}"
        );
        let mut same_name = plugin("redact-headers");
        same_name.id = Some(SIBLING_ID);
        assert_eq!(
            kind_of(&plugins.insert(&same_name).unwrap_err()),
            Some(ViolationKind::AlreadyExists)
        );

        // The natural key is scoped: the same name in another tenant is free.
        let mut foreign_tenant = plugin("redact-headers");
        foreign_tenant.tenant_id = Some(OTHER_TENANT);
        plugins.insert(&foreign_tenant).unwrap();

        // Tenant-scoped reads, by composite key and by natural key.
        assert_eq!(
            plugins.find(TENANT, PLUGIN_ID).unwrap().name.as_deref(),
            Some("redact-headers")
        );
        assert_eq!(
            plugins.find_by_name(TENANT, "redact-headers").unwrap().id,
            Some(PLUGIN_ID)
        );
        assert!(is_not_found(
            &plugins.find_by_name(TENANT, "nope").unwrap_err()
        ));
        assert_eq!(
            plugins
                .find_by_name(OTHER_TENANT, "redact-headers")
                .unwrap()
                .tenant_id,
            Some(OTHER_TENANT),
            "the natural key is scoped: the same name in another tenant is free"
        );

        // Replacement is wholesale.
        let mut described = plugin("redact-headers");
        described.description = Some("removes response headers".to_owned());
        let replaced = plugins.replace(&described).unwrap();
        assert_eq!(
            replaced.description.as_deref(),
            Some("removes response headers")
        );

        // Deletion cascades to every binding that still references the plugin.
        plugins.delete(TENANT, PLUGIN_ID).unwrap();
        assert!(is_not_found(&plugins.find(TENANT, PLUGIN_ID).unwrap_err()));
        assert!(is_not_found(
            &plugins.delete(TENANT, PLUGIN_ID).unwrap_err()
        ));
        assert!(
            plugins.find(OTHER_TENANT, PLUGIN_ID).is_ok(),
            "a deletion is scoped: the plugin of another tenant is untouched"
        );
        let upstream = upstreams.find(TENANT, UPSTREAM_ID).unwrap();
        assert_eq!(
            upstream.plugins.as_ref().unwrap().items,
            vec![BUILTIN_PLUGIN.to_owned()],
            "the upstream binding of the deleted plugin is removed"
        );
        let route = routes.find(TENANT, ROUTE_ID).unwrap();
        assert!(
            route
                .plugins
                .as_ref()
                .unwrap()
                .items
                .contains(&BUILTIN_PLUGIN.to_owned()),
            "a named binding is untouched by the cascade"
        );
    }
}

/// The closed state machine of `cpt-cf-oagw-state-resource-lifecycle`: the six
/// declared transitions are accepted, and every other one — `Draft` to
/// `Deleted`, `Disabled` back to `Draft`, any transition out of `Deleted` — is
/// refused and leaves the state unchanged.
#[cfg(test)]
mod lifecycle_tests {
    use super::ResourceLifecycle;
    use super::ResourceLifecycle::{Active, Deleted, Disabled, Draft};

    #[test]
    fn the_lifecycle_accepts_only_the_declared_transitions() {
        let accepted = [
            (Draft, Active),
            (Draft, Disabled),
            (Active, Disabled),
            (Disabled, Active),
            (Active, Deleted),
            (Disabled, Deleted),
        ];
        for (from, to) in accepted {
            assert_eq!(from.transition(to), Some(to), "{from:?} to {to:?}");
        }
        let refused = [
            (Draft, Deleted),
            (Active, Draft),
            (Disabled, Draft),
            (Deleted, Draft),
            (Deleted, Active),
            (Deleted, Disabled),
        ];
        for (from, to) in refused {
            assert_eq!(from.transition(to), None, "{from:?} to {to:?}");
        }
        // Reaching the state the resource already occupies is a no-op, `Deleted`
        // included, since that transition never leaves `Deleted`.
        assert_eq!(Deleted.transition(Deleted), Some(Deleted));
        assert_eq!(Draft, ResourceLifecycle::initial());
    }
}

// @cpt-end:cpt-cf-oagw-dod-repository-traits:p1:inst-full
