//! Management service — the only caller of the store.
//!
//! One method per endpoint operation, each running the FEATURE's step order for
//! its flow: validate the body, resolve the tenant-scoped row, apply the
//! cross-row check, write in one transaction, flush the Control Plane cache
//! before the response is produced. The service takes and returns domain types,
//! `serde_json::Value`, and `Uuid`; no `axum`/`http` type appears here.
//!
//! A store failure is never a catalogue variant: it becomes
//! [`ServiceError::Storage`], which the API layer answers with the platform's
//! RFC 9457 500 problem shape.

use std::sync::Arc;

use serde_json::Value;
use uuid::Uuid;

use crate::control_plane::alias_derive;
use crate::control_plane::bind;
use crate::control_plane::cache::{ControlPlaneCache, DeletionObservers, RateLimitCleanup};
use crate::control_plane::chain;
use crate::control_plane::match_uniqueness;
use crate::control_plane::odata::{self, ListKind, Page};
use crate::control_plane::plugin_def;
use crate::control_plane::replace;
use crate::control_plane::scoping;
use crate::control_plane::shadow;
use crate::control_plane::sharing::{self, Decisions, OverridePermissions, Refusal};
use crate::control_plane::validation::{ResourceKind, Validator, WriteKind};
use crate::domain::alias::Alias;
use crate::domain::effective::{AncestorBinding, Family};
use crate::domain::error::{DomainError, ErrorKind};
use crate::control_plane::binding;
use crate::domain::plugin::Plugin;
use crate::domain::plugin_contract::NamedPluginRegistry;
use crate::domain::route::Route;
use crate::domain::upstream::Upstream;
use crate::gts;
use crate::store::{
    BindingWrite, OagwStore, PluginGcReport, PluginRow, RouteRow, StoreError, UpstreamRow,
};

/// Why one operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceError {
    /// A failure of the flow's own: a foundation catalogue row answers it.
    Domain(DomainError),
    /// A descendant override permission the calling token does not hold.
    ///
    /// This is deliberately not a [`DomainError`] variant: no row of the OAGW
    /// error catalogue answers it, and the API layer answers it with a bare
    /// 403 problem answer whose detail names neither the family the body
    /// carried nor the mode any ancestor declared.
    Forbidden {
        /// The resource kind the refused operation addressed.
        resource: ResourceKind,
        /// The `oagw:upstream:*` permission the token does not hold, when the
        /// refused family names one; the CORS family names none and the
        /// sharing mode alone governs it.
        permission: Option<&'static str>,
    },
    /// A persistence failure: no catalogue row answers it, and the API layer
    /// answers it with the platform's RFC 9457 500 problem shape.
    Storage { reason: String },
}

impl ServiceError {
    /// The catalogue row a `Domain` failure answers with.
    ///
    /// # Panics
    ///
    /// Never: the caller checks `is_storage` first.
    #[must_use]
    pub fn domain(&self) -> &DomainError {
        match self {
            Self::Domain(error) => error,
            Self::Forbidden { .. } | Self::Storage { .. } => {
                unreachable!("a refused permission or a storage failure carries no catalogue row")
            }
        }
    }

    /// Whether the failure is a persistence failure.
    #[must_use]
    pub const fn is_storage(&self) -> bool {
        matches!(self, Self::Storage { .. })
    }

    /// The permission a refused descendant override names, when its family
    /// carries one.
    #[must_use]
    pub const fn permission(&self) -> Option<&'static str> {
        match self {
            Self::Forbidden { permission, .. } => *permission,
            Self::Domain(_) | Self::Storage { .. } => None,
        }
    }
}

impl From<DomainError> for ServiceError {
    fn from(error: DomainError) -> Self {
        Self::Domain(error)
    }
}

impl From<StoreError> for ServiceError {
    fn from(error: StoreError) -> Self {
        match error {
            // The same 409 the pre-write check produces, when the store's own
            // batch check is the one that observed it.
            StoreError::AliasConflict => Self::Domain(alias_conflict()),
            StoreError::MatchConflict {
                colliding_route_id,
            } => Self::Domain(conflict(colliding_route_id)),
            // The same 400 naming `name` the pre-write duplicate check
            // produces, when the store's own batch check is the one that
            // observed it.
            StoreError::PluginNameConflict => Self::Domain(plugin_def::name_taken()),
            StoreError::Invariant { reason } => Self::Storage { reason },
        }
    }
}

/// The effective lifecycle state of one row as the calling tenant observes it.
///
/// `Enabled` is the initial state: a successfully persisted row is immediately
/// meaningful. Deletion is not a state; it removes the row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lifecycle {
    /// The row is available for proxy routing.
    Enabled,
    /// The row is not available for proxy routing.
    Disabled,
}

impl Lifecycle {
    /// The state a written `enabled` flag puts the row in.
    #[must_use]
    pub const fn of(enabled: bool) -> Self {
        if enabled {
            Self::Enabled
        } else {
            Self::Disabled
        }
    }

    /// Whether the row is effectively enabled for a descendant tenant.
    ///
    /// A contributing ancestor row that is disabled keeps the descendant
    /// effectively disabled without any write reaching the descendant row; the
    /// hierarchy walk that resolves that ancestor state belongs to the
    /// hierarchical configuration feature, which passes it in here.
    #[must_use]
    pub const fn effective(self, ancestor_disabled: bool) -> bool {
        // @cpt-begin:cpt-cf-oagw-state-config-lifecycle:p1:inst-state-ancestor-guard
        // The guard is structural: no descendant write can reach an ancestor
        // row, so an ancestor `Disabled` state wins over a descendant `Enabled`
        // one and the descendant stays effectively disabled.
        !ancestor_disabled && matches!(self, Self::Enabled)
        // @cpt-end:cpt-cf-oagw-state-config-lifecycle:p1:inst-state-ancestor-guard
    }
}

/// The management half of the gear.
pub struct ManagementService {
    store: Arc<OagwStore>,
    validator: Validator,
    cache: Arc<ControlPlaneCache>,
    observers: Arc<DeletionObservers>,
    plugins: NamedPluginRegistry,
}

impl ManagementService {
    /// Builds the service over an empty store and the compiled validators.
    ///
    /// # Errors
    ///
    /// Returns the compilation error of the four request-body validators.
    #[allow(clippy::result_large_err)]
    pub fn new(
        store: Arc<OagwStore>,
        config: &crate::config::OagwConfig,
        cache: Arc<ControlPlaneCache>,
    ) -> Result<Self, DomainError> {
        Ok(Self {
            store,
            validator: Validator::compile(config)?,
            cache,
            observers: Arc::new(DeletionObservers::new()),
            plugins: NamedPluginRegistry::with_builtins(),
        })
    }

    /// The store the service owns, for the read paths the API layer needs.
    #[must_use]
    pub fn store(&self) -> &OagwStore {
        &self.store
    }

    /// The cache the write path advances.
    #[must_use]
    pub fn cache(&self) -> &ControlPlaneCache {
        &self.cache
    }

    /// Registers the rate-limit cleanup the deletion seam notifies.
    pub fn register_deletion_observer(&self, observer: Arc<dyn RateLimitCleanup>) {
        self.observers.register(observer);
    }

    /// Resolves and validates the plugin bindings one parent body carries,
    /// through `cpt-cf-oagw-algo-binding-validate`, and answers the write set
    /// the parent's single-transaction write applies.
    ///
    /// # Errors
    ///
    /// Returns the validation error that names every failing item with its
    /// position and reason, which is indistinguishable from any other
    /// validation failure of the parent write.
    #[allow(clippy::result_large_err)]
    fn binding_write(
        &self,
        tenant_id: Uuid,
        body: &Value,
        is_upstream: bool,
    ) -> Result<BindingWrite, ServiceError> {
        // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-validate
        // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-fail-if
        // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-fail-return
        // Every reference is resolved and every item validated before the
        // parent write is attempted, so a body that names an unresolvable
        // plugin writes no parent row and no binding row.
        let write = binding::validate(
            &self.store,
            tenant_id,
            &self.plugins,
            body,
            is_upstream,
            crate::store::unix_now(),
        )
        .map_err(ServiceError::from)?;
        // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-fail-return
        // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-fail-else
        // ELSE every item resolved and every check held, and the write set the
        // routine built rides on the parent's single transaction.
        // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-fail-else
        // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-fail-if
        Ok(write)
        // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-validate
    }

    /// Creates one upstream over the ancestor chain the caller obtained:
    /// `POST /oagw/v1/upstreams`.
    ///
    /// The same steps as [`Self::create_upstream`], with the hierarchical bind
    /// decision of `cpt-cf-oagw-flow-bind-ancestor-upstream` between the alias
    /// derivation and the write: a create whose normalized alias matches an
    /// ancestor's upstream is a bind requiring `oagw:upstream:bind`, answered
    /// 201 with the descendant's own row rather than 409. The chain arrives
    /// ordered, calling tenant first; an empty chain is the ordinary create.
    ///
    /// # Errors
    ///
    /// Returns a validation error when the body is not valid or the endpoint
    /// set derives no alias, the `AliasConflict` row when another upstream of
    /// the calling tenant holds the alias, the 403 of a missing descendant
    /// override permission and the 400 of an `enforce` family, and a storage
    /// failure when the walk could not be ordered or the write could not be
    /// applied.
    #[allow(clippy::result_large_err)]
    pub fn create_upstream_in_chain(
        &self,
        tenant_id: Uuid,
        ancestors: &[Uuid],
        permissions: &OverridePermissions,
        body: &Value,
    ) -> Result<UpstreamRow, ServiceError> {
        // @cpt-begin:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-validate
        // The body is validated and the alias derived before the walk runs, by
        // the same two routines the ordinary create uses.
        // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-parent-validate
        // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-validate
        let validated = self
            .validator
            .validate_upstream(WriteKind::Create, body)
            .map_err(ServiceError::from)?;
        // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-validate
        // The shipped schema confirms the `plugins` envelope and its
        // `sharing` enum; the shape of the items that envelope carries is the
        // plugin routine's answer, not this schema's.
        // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-parent-validate
        let mut upstream = validated.value;

        let plugin_write = self.binding_write(tenant_id, body, true)?;

        // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-alias
        // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-alias-if
        // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-alias-return
        let alias =
            alias_derive::resolve(&upstream.server.endpoints, upstream.alias.as_deref(), None)
                .map_err(ServiceError::from)?;
        // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-alias-return
        // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-alias-if
        // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-alias-else
        // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-alias-continue
        upstream.alias = Some(alias.to_string());
        // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-alias-continue
        // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-alias-else
        // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-alias
        // @cpt-end:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-validate
        upstream.id = Uuid::new_v4();

        // @cpt-begin:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-own-scope
        // The calling tenant's own scope is confirmed first: a duplicate answers
        // 409 before the walk runs, so an ancestor's alias can never be
        // mistaken for a same-tenant conflict.
        if self.store.upstream_by_alias(tenant_id, &alias).is_some() {
            return Err(ServiceError::Domain(alias_conflict()));
        }
        // @cpt-end:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-own-scope

        // @cpt-begin:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-walk
        let bindings = self.ancestor_bindings(tenant_id, ancestors, &alias)?;
        // @cpt-end:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-walk

        // @cpt-begin:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-noancestor-if
        let decided = if bindings.is_empty() {
            // @cpt-begin:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-noancestor
            // No ancestor holds the alias: the operation is an ordinary create
            // and no permission beyond `create` is consumed.
            Ok(Decisions {
                families: Vec::new(),
            })
            // @cpt-end:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-noancestor
        } else {
            // @cpt-end:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-noancestor-if
            // @cpt-begin:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-ancestor-else
            // @cpt-begin:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-perm-if
            // The bind itself needs `oagw:upstream:bind`, and it needs it
            // whatever the body carries: the permission check runs before
            // every per-family sharing check, so a caller that lacks the
            // permission never learns which families the ancestor enforces.
            if !permissions.holds(gts::PERMISSION_BIND) {
                // @cpt-begin:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-perm-return
                return Err(ServiceError::Forbidden {
                    resource: ResourceKind::Upstream,
                    permission: Some(gts::PERMISSION_BIND),
                });
                // @cpt-end:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-perm-return
            }
            // @cpt-end:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-perm-if
            // @cpt-begin:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-decide
            // Every family the body carries is decided against the ancestor's
            // per-family sharing modes and the calling tenant's permission set.
            sharing::decide(&bindings, &carried_families(&upstream), permissions)
            // @cpt-end:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-decide
            // @cpt-end:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-ancestor-else
        };

        // @cpt-begin:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-write-else
        // ELSE every family the body carries is writable, so the bind routine
        // records the binding the walk resolved and produces the write set for
        // the descendant's own row; a refusal it returns is answered below and
        // writes no row.
        // @cpt-end:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-write-else
        // @cpt-begin:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-write
        let write_set = match bind::write_set(tenant_id, bindings.len(), upstream, &decided) {
            Ok(write_set) => write_set,
            // @cpt-begin:cpt-cf-oagw-algo-sharing-mode-decision:p1:inst-decide-refusal-if
            Err(Refusal::Permission { family }) => {
                // @cpt-begin:cpt-cf-oagw-algo-sharing-mode-decision:p1:inst-decide-refusal-return
                // A per-family sharing check refused the override after the
                // bind permission was confirmed, so the decision's refusal is
                // returned and no row is written.
                return Err(ServiceError::Forbidden {
                    resource: ResourceKind::Upstream,
                    permission: family.override_permission(),
                });
                // @cpt-end:cpt-cf-oagw-algo-sharing-mode-decision:p1:inst-decide-refusal-return
            }
            // @cpt-end:cpt-cf-oagw-algo-sharing-mode-decision:p1:inst-decide-refusal-if
            // @cpt-begin:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-enforce-if
            Err(Refusal::Enforced { family }) => {
                // @cpt-begin:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-enforce-return
                return Err(ServiceError::Domain(enforced_family_error(family)));
                // @cpt-end:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-enforce-return
            }
            // @cpt-end:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-enforce-if
        };
        // @cpt-end:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-write

        // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-scope
        // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-write
        // The binding rows and the two auth plugin identity columns are
        // written by the same batch the parent row is, so a parent that fails
        // writes no binding and a binding that fails writes no parent.
        let written = match self.store.insert_upstream_with_bindings(
            write_set.tenant_id,
            &write_set.value,
            &plugin_write,
        ) {
            // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-conflict-if
            Err(StoreError::AliasConflict) => {
                // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-conflict-return
                return Err(ServiceError::Domain(alias_conflict()));
                // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-conflict-return
            }
            // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-conflict-if
            // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-else
            outcome => outcome.map_err(ServiceError::from)?,
            // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-else
        };
        // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-write
        // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-scope

        self.cache.flush_for(tenant_id);
        // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-return
        // @cpt-begin:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-return
        // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-return
        // The ancestor's rows are unchanged by the operation.
        Ok(written)
        // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-return
        // @cpt-end:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-return
        // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-return
    }

    /// Creates one upstream: `POST /oagw/v1/upstreams`.
    ///
    /// The ordinary create: no ancestor chain is consulted, so no bind is ever
    /// performed and no permission beyond `create` is consumed. The
    /// hierarchical form is [`Self::create_upstream_in_chain`].
    ///
    /// # Errors
    ///
    /// Returns a validation error when the body is not valid or the endpoint
    /// set derives no alias, the `AliasConflict` row when another upstream of
    /// the calling tenant holds the alias, and a storage failure when the
    /// write could not be applied.
    #[allow(clippy::result_large_err)]
    pub fn create_upstream(
        &self,
        tenant_id: Uuid,
        body: &Value,
    ) -> Result<UpstreamRow, ServiceError> {
        self.create_upstream_in_chain(tenant_id, &[], &OverridePermissions::none(), body)
    }

    /// Reads one upstream: `GET /oagw/v1/upstreams/{id}`.
    ///
    /// # Errors
    ///
    /// Returns the 404 row when no row of the calling tenant matches.
    #[allow(clippy::result_large_err)]
    pub fn read_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<UpstreamRow, ServiceError> {
        // @cpt-begin:cpt-cf-oagw-flow-config-read-list:p1:inst-read-scope
        let row =
            scoping::resolve_upstream(&self.store, tenant_id, id).map_err(ServiceError::from)?;
        // @cpt-end:cpt-cf-oagw-flow-config-read-list:p1:inst-read-scope
        // @cpt-begin:cpt-cf-oagw-flow-config-read-list:p1:inst-read-single-else
        // ELSE the operation is a single read: the one row the predicate
        // resolved, with its dependent tag rows already attached.
        // @cpt-begin:cpt-cf-oagw-flow-config-read-list:p1:inst-read-single
        // The resolved row already carries its dependent tag rows.
        // @cpt-end:cpt-cf-oagw-flow-config-read-list:p1:inst-read-single
        // @cpt-end:cpt-cf-oagw-flow-config-read-list:p1:inst-read-single-else
        // @cpt-begin:cpt-cf-oagw-flow-config-read-list:p1:inst-read-return
        Ok(row)
        // @cpt-end:cpt-cf-oagw-flow-config-read-list:p1:inst-read-return
    }

    /// Lists the upstreams of the calling tenant: `GET /oagw/v1/upstreams`.
    ///
    /// # Errors
    ///
    /// Returns a validation error naming the offending parameter.
    #[allow(clippy::result_large_err)]
    pub fn list_upstreams(
        &self,
        tenant_id: Uuid,
        query: &str,
    ) -> Result<Page<UpstreamRow>, ServiceError> {
        // @cpt-begin:cpt-cf-oagw-flow-config-read-list:p1:inst-read-list-if
        // @cpt-begin:cpt-cf-oagw-flow-config-read-list:p1:inst-read-list
        let parsed = odata::parse(ResourceKind::Upstream.into(), query).map_err(ServiceError::from)?;
        let scan = scoping::list_upstreams(&self.store, tenant_id);
        // @cpt-end:cpt-cf-oagw-flow-config-read-list:p1:inst-read-list
        // @cpt-end:cpt-cf-oagw-flow-config-read-list:p1:inst-read-list-if
        // @cpt-begin:cpt-cf-oagw-flow-config-read-list:p1:inst-read-return
        Ok(odata::apply_upstream(&parsed, scan))
        // @cpt-end:cpt-cf-oagw-flow-config-read-list:p1:inst-read-return
    }

    /// Replaces one upstream over the ancestor chain the caller obtained:
    /// `PUT /oagw/v1/upstreams/{id}`.
    ///
    /// The same steps as [`Self::replace_upstream`], with the sharing-mode
    /// decision of `cpt-cf-oagw-flow-override-inherited-field` between the
    /// write set and the write: a family the ancestor marks `enforce` answers
    /// 400 and an `inherit` family whose override permission the token does
    /// not hold answers 403, in that order, and neither writes the row. The
    /// chain arrives ordered, calling tenant first; an empty chain decides
    /// every family `own`.
    ///
    /// The same steps serve the enable/disable flow: there is no dedicated
    /// enable or disable operation, the flag travels on the replacement body.
    ///
    /// # Errors
    ///
    /// Returns the 404 row when no row of the calling tenant matches, a
    /// validation error when the body is not valid or states a different
    /// identifier, the `AliasConflict` row when the replacement endpoints
    /// derive a different alias, the 403 of a missing descendant override
    /// permission and the 400 of an `enforce` family, and a storage failure
    /// when the walk could not be ordered or the write could not be applied.
    #[allow(clippy::result_large_err)]
    pub fn replace_upstream_in_chain(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        ancestors: &[Uuid],
        permissions: &OverridePermissions,
        body: &Value,
    ) -> Result<UpstreamRow, ServiceError> {
        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-issue
        // A replacement may carry an explicit `enabled` value; the ten
        // management paths hold no dedicated enable or disable operation.
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-issue

        // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-scope
        // The row is resolved by identifier and calling tenant only, so an
        // ancestor's row can never satisfy the predicate and a descendant can
        // never address one.
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-scope
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-scope-if
        // The same resolution answers the enable/disable flow: a non-match is
        // the only reason a descendant cannot re-enable an ancestor-disabled
        // resource through this API.
        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-scope
        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-scope-if
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-scope-return
        // RETURN 404: a nonexistent identifier and a foreign-owned one answer
        // the same way, so the endpoint discloses nothing about other tenants'
        // resources.
        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-scope-return
        // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-scope-if
        // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-scope-return
        let stored =
            scoping::resolve_upstream(&self.store, tenant_id, id).map_err(ServiceError::from)?;
        // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-scope-return
        // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-scope-if
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-scope-return
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-scope-return
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-scope-if
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-scope-if
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-scope-else
        // ELSE the row resolved under the calling tenant's scope.
        // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-scope-else
        // ELSE the tenant's own row resolved, and it is the only row the
        // replacement can address.
        // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-scope-else
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-scope-else
        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-scope-else
        // ELSE the row resolved under the calling tenant's scope.
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-scope-else
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-scope
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-scope
        // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-scope

        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-scope-else
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-load
        // The stored row and its dependent tag rows are the diff baseline.
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-load

        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-put-if
        // IF the operation is a replacement; the deletion branch is
        // [`Self::delete_upstream`].
        // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-validate
        // The replacement body is validated and the write set built for the
        // tenant's own row before the chain walk and the sharing-mode decision
        // run against it.
        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-validate
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-validate
        let validated = self
            .validator
            .validate_upstream(WriteKind::Replacement, body)
            .map_err(ServiceError::from)?;
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-validate
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-validate
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-diff
        // The routine recomputes the derived alias from the replacement
        // endpoints, confirms the immutable fields, and builds the write set.
        // @cpt-dod:cpt-cf-oagw-dod-full-replacement-put:p1
        let carried = carried_families(&validated.value);
        let write_set = replace::upstream_diff(&stored, validated.stated_id, validated.value)
            .map_err(ServiceError::from)?;
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-diff

        // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-parent-validate
        // The replacement body's plugin items and auth identity are resolved
        // after the parent's own validation, and before any row is written.
        // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-parent-validate
        let plugin_write = self.binding_write(tenant_id, body, true)?;
        // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-validate

        // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-decide
        // The walk resolves the ancestor binding the row participates in, from
        // the stored row's immutable alias, and the sharing-mode decision
        // evaluates every family the body carries against it. The families are
        // read off the validated body, not off the write set: a family the
        // body omits is written by nobody and takes no part in the decision.
        let bindings = self.upstream_ancestor_bindings(tenant_id, ancestors, &stored)?;
        let decided = sharing::decide(&bindings, &carried, permissions);
        // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-decide

        // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-perm-if
        if let Err(Refusal::Permission { family }) = &decided {
            // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-perm-return
            // The permission check precedes every per-family sharing check, so
            // the tenant uses the ancestor's value as-is and never learns which
            // families the ancestor enforces.
            return Err(ServiceError::Forbidden {
                resource: ResourceKind::Upstream,
                permission: family.override_permission(),
            });
            // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-perm-return
        }
        // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-perm-if

        // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-enforce-if
        if let Err(Refusal::Enforced { family }) = &decided {
            // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-enforce-return
            // This 400 is reached only once the permission check above has
            // passed; the stored row is left unchanged.
            return Err(ServiceError::Domain(enforced_family_error(*family)));
            // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-enforce-return
        }
        // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-enforce-if

        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-alias-if
        // The routine returned 409 when the recomputed alias differs from the
        // stored one: the alias is immutable across updates and the stored
        // alias is left unchanged.
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-alias-if
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-alias-else
        // ELSE the recomputed alias equals the stored one and the write set
        // applies.
        // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-write-else
        // ELSE every family the body carries is writable: the write set applies
        // to the tenant's own row, no ancestor row is written, and the
        // inherited tags stay in the effective set whatever the body's tag list
        // holds.
        // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-write-else
        // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-write
        if !write_set.changed {
            // A write set that changes nothing is applied anyway, so the cache
            // flush still runs and the response carries the stored row.
            self.cache.flush_for(write_set.tenant_id);
            return Ok(stored);
        }
        // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-write
        // The full replacement of the binding rows is written by the same
        // batch the parent row is, so a body that omits the `plugins`
        // sub-object unlinks every plugin in one commit.
        let written = self
            .store
            .replace_upstream_with_bindings(
                write_set.tenant_id,
                write_set.id,
                &write_set.value,
                &plugin_write,
            )
            .map_err(ServiceError::from)?;
        // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-write
        // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-write
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-alias-else
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-put-if

        // @cpt-dod:cpt-cf-oagw-dod-enable-disable:p1
        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-write
        self.cache.flush_for(tenant_id);
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-write
        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-off-if
        let _state = Lifecycle::of(written.upstream.enabled);
        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-off
        // A written `false` puts the row in `Disabled` for its owner and for
        // every descendant tenant without any further write.
        // @cpt-begin:cpt-cf-oagw-state-config-lifecycle:p1:inst-state-disable
        // The `Enabled` to `Disabled` transition of the owning tenant's write.
        // @cpt-end:cpt-cf-oagw-state-config-lifecycle:p1:inst-state-disable
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-off
        // @cpt-begin:cpt-cf-oagw-state-config-lifecycle:p1:inst-state-ancestor-disable
        // The ancestor-driven `Enabled` to `Disabled` transition reaches this
        // row through no write: it is observed only from the descendant's side,
        // and the hierarchy walk that detects it belongs to
        // `cpt-cf-oagw-feature-hierarchical-config`.
        // @cpt-end:cpt-cf-oagw-state-config-lifecycle:p1:inst-state-ancestor-disable
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-off-if
        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-on-else
        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-on
        // @cpt-begin:cpt-cf-oagw-state-config-lifecycle:p1:inst-state-enable
        // The `Disabled` to `Enabled` transition holds only where no
        // contributing ancestor row is disabled.
        // @cpt-end:cpt-cf-oagw-state-config-lifecycle:p1:inst-state-enable
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-on
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-on-else
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-scope-else
        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-return
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-return
        // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-return
        Ok(written)
        // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-return
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-return
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-return
    }

    /// Replaces one upstream: `PUT /oagw/v1/upstreams/{id}`.
    ///
    /// The ordinary replacement: no ancestor chain is consulted, so every
    /// family the body carries is decided `own`. The hierarchical form is
    /// [`Self::replace_upstream_in_chain`].
    ///
    /// # Errors
    ///
    /// Returns the 404 row when no row of the calling tenant matches, a
    /// validation error when the body is not valid or states a different
    /// identifier, the `AliasConflict` row when the replacement endpoints
    /// derive a different alias, and a storage failure when the write could
    /// not be applied.
    #[allow(clippy::result_large_err)]
    pub fn replace_upstream(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        body: &Value,
    ) -> Result<UpstreamRow, ServiceError> {
        self.replace_upstream_in_chain(tenant_id, id, &[], &OverridePermissions::none(), body)
    }

    /// Deletes one upstream and, by cascade, its routes: `DELETE
    /// /oagw/v1/upstreams/{id}`.
    ///
    /// # Errors
    ///
    /// Returns the 404 row when no row of the calling tenant matches, and a
    /// storage failure when the deletion could not be applied.
    #[allow(clippy::result_large_err)]
    pub fn delete_upstream(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, ServiceError> {
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-delete-else
        // ELSE the operation is a deletion: the row resolves under the calling
        // tenant's scope, then the row and its cascaded dependents leave in one
        // transaction.
        scoping::resolve_upstream(&self.store, tenant_id, id)
            .map_err(ServiceError::from)?;
        let deleted = self
            .store
            .delete_upstream(tenant_id, id)
            .map_err(ServiceError::from)?;
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-delete-else

        self.cache.flush_for(tenant_id);
        // A successful deletion notifies the registered rate-limit cleanup; a
        // failed one notifies nothing.
        self.observers.upstream_deleted(tenant_id, id);
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-return
        Ok(deleted)
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-return
    }

    /// Creates one route: `POST /oagw/v1/routes`.
    ///
    /// # Errors
    ///
    /// Returns a validation error when the body is not valid or references an
    /// upstream the calling tenant does not own, the `MatchConflict` row when
    /// another enabled route holds the match key, and a storage failure when
    /// the write could not be applied.
    #[allow(clippy::result_large_err)]
    pub fn create_route(&self, tenant_id: Uuid, body: &Value) -> Result<RouteRow, ServiceError> {
        // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-parent-validate
        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-validate
        let validated = self
            .validator
            .validate_route(WriteKind::Create, body)
            .map_err(ServiceError::from)?;
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-validate
        // The shipped schema confirms the `plugins` envelope; the item shape
        // is the plugin routine's answer.
        // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-parent-validate
        let mut route = validated.value;

        // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-validate
        // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-fail-if
        // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-fail-return
        // A route binds no auth plugin, so its body's `auth` member is a
        // schema failure the validation above already answered, and the
        // routine only records the rule here.
        let plugin_write = self.binding_write(tenant_id, body, false)?;
        // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-fail-return
        // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-fail-else
        // ELSE the write set carries one binding row per resolvable item, at
        // the positions the body submitted.
        // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-fail-else
        // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-fail-if
        // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-validate

        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-resolve
        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-resolve-if
        scoping::resolve_referenced_upstream(&self.store, tenant_id, route.upstream_id)
            .map_err(ServiceError::from)?;
        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-resolve-return
        // An unresolvable `upstream_id` was answered 400 above: an
        // ancestor-owned upstream is not directly addressable as a route
        // target, so a route can only be created under a target the caller
        // owns.
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-resolve-return
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-resolve-if
        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-resolve-else
        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-resolve-continue
        // ELSE continue with the resolved upstream.
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-resolve-continue
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-resolve-else
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-resolve

        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-unique
        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-unique-if
        match_uniqueness::confirm_route(&self.store, tenant_id, &route, None)
            .map_err(ServiceError::from)?;
        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-unique-return
        // A collision was answered 409 above, naming the colliding route, and
        // no row was written.
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-unique-return
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-unique-if
        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-unique-else
        // ELSE no other enabled route of that upstream holds the match key.
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-unique-else
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-unique

        route.id = Uuid::new_v4();
        // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-write
        // The route's binding rows are written by the same batch its own row
        // is, at the positions the body submitted.
        let written = self
            .store
            .insert_route_with_bindings(tenant_id, &route, &plugin_write)
            .map_err(ServiceError::from)?;
        // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-write
        self.cache.flush_for(tenant_id);
        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-return
        // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-return
        Ok(written)
        // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-return
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-return
    }

    /// Reads one route: `GET /oagw/v1/routes/{id}`.
    ///
    /// # Errors
    ///
    /// Returns the 404 row when no row of the calling tenant matches.
    #[allow(clippy::result_large_err)]
    pub fn read_route(&self, tenant_id: Uuid, id: Uuid) -> Result<RouteRow, ServiceError> {
        // @cpt-begin:cpt-cf-oagw-flow-config-read-list:p1:inst-read-scope
        let row = scoping::resolve_route(&self.store, tenant_id, id).map_err(ServiceError::from)?;
        // @cpt-end:cpt-cf-oagw-flow-config-read-list:p1:inst-read-scope
        // @cpt-begin:cpt-cf-oagw-flow-config-read-list:p1:inst-read-return
        Ok(row)
        // @cpt-end:cpt-cf-oagw-flow-config-read-list:p1:inst-read-return
    }

    /// Lists the routes of the calling tenant: `GET /oagw/v1/routes`.
    ///
    /// # Errors
    ///
    /// Returns a validation error naming the offending parameter.
    #[allow(clippy::result_large_err)]
    pub fn list_routes(&self, tenant_id: Uuid, query: &str) -> Result<Page<RouteRow>, ServiceError> {
        // @cpt-begin:cpt-cf-oagw-flow-config-read-list:p1:inst-read-list-if
        // @cpt-begin:cpt-cf-oagw-flow-config-read-list:p1:inst-read-list
        let parsed = odata::parse(ResourceKind::Route.into(), query).map_err(ServiceError::from)?;
        let scan = scoping::list_routes(&self.store, tenant_id);
        // @cpt-end:cpt-cf-oagw-flow-config-read-list:p1:inst-read-list
        // @cpt-end:cpt-cf-oagw-flow-config-read-list:p1:inst-read-list-if
        // @cpt-begin:cpt-cf-oagw-flow-config-read-list:p1:inst-read-return
        Ok(odata::apply_route(&parsed, scan))
        // @cpt-end:cpt-cf-oagw-flow-config-read-list:p1:inst-read-return
    }

    /// Replaces one route over the ancestor chain the caller obtained:
    /// `PUT /oagw/v1/routes/{id}`.
    ///
    /// The same steps as [`Self::replace_route`], with the sharing-mode
    /// decision of `cpt-cf-oagw-flow-override-inherited-field` on the families
    /// a route carries. A route participates in the hierarchy through its
    /// upstream's alias, so the ancestors the walk resolves for that alias —
    /// and the routes those upstreams hold — are the row's ancestors; a route
    /// carries no `auth` family, so there is no inherited auth configuration
    /// for it to override.
    ///
    /// # Errors
    ///
    /// Returns the 404 row when no row of the calling tenant matches, a
    /// validation error when the body is not valid or states a different
    /// identifier or upstream reference, the `MatchConflict` row when another
    /// enabled route holds the match key, the 403 of a missing descendant
    /// override permission and the 400 of an `enforce` family, and a storage
    /// failure when the walk could not be ordered or the write could not be
    /// applied.
    #[allow(clippy::result_large_err)]
    pub fn replace_route_in_chain(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        ancestors: &[Uuid],
        permissions: &OverridePermissions,
        body: &Value,
    ) -> Result<RouteRow, ServiceError> {
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-scope-if
        let stored =
            scoping::resolve_route(&self.store, tenant_id, id).map_err(ServiceError::from)?;
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-scope-if
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-load
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-validate
        let validated = self
            .validator
            .validate_route(WriteKind::Replacement, body)
            .map_err(ServiceError::from)?;
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-validate
        // The DoD scope marker for `cpt-cf-oagw-dod-full-replacement-put` is
        // declared once per file, at the upstream replacement.
        let carried = carried_route_families(&validated.value);
        let write_set =
            replace::route_diff(&self.store, &stored, validated.stated_id, validated.value)
                .map_err(ServiceError::from)?;
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-load

        // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-parent-validate
        // The replacement body's plugin items are resolved after the parent's
        // own validation, and before any row is written.
        // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-parent-validate
        let plugin_write = self.binding_write(tenant_id, body, false)?;

        // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-decide
        // The walk resolves the ancestor routes the row participates in,
        // through its upstream's alias, and the sharing-mode decision
        // evaluates every family the body carries against them.
        let bindings = self.route_ancestor_bindings(tenant_id, ancestors, &stored)?;
        let decided = sharing::decide(&bindings, &carried, permissions);
        // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-decide

        // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-perm-if
        if let Err(Refusal::Permission { family }) = &decided {
            // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-perm-return
            return Err(ServiceError::Forbidden {
                resource: ResourceKind::Route,
                permission: family.override_permission(),
            });
            // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-perm-return
        }
        // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-perm-if

        // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-enforce-if
        if let Err(Refusal::Enforced { family }) = &decided {
            // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-enforce-return
            return Err(ServiceError::Domain(enforced_family_error(*family)));
            // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-enforce-return
        }
        // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-enforce-if

        // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-write-else
        // ELSE every family the body carries is writable: the write set applies
        // to the tenant's own row, no ancestor row is written, and the
        // inherited tags stay in the effective set whatever the body's tag list
        // holds.
        // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-write-else
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-put-if
        // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-write
        if !write_set.changed {
            // A write set that changes nothing is applied anyway, so the cache
            // flush still runs and the response carries the stored row.
            self.cache.flush_for(write_set.tenant_id);
            return Ok(stored);
        }
        let written = self
            .store
            .replace_route_with_bindings(
                write_set.tenant_id,
                write_set.id,
                &write_set.value,
                &plugin_write,
            )
            .map_err(ServiceError::from)?;
        self.cache.flush_for(write_set.tenant_id);
        // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-write
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-put-if
        // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-return
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-return
        // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-return
        Ok(written)
        // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-return
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-return
        // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-return
    }

    /// Replaces one route: `PUT /oagw/v1/routes/{id}`.
    ///
    /// The ordinary replacement: no ancestor chain is consulted, so every
    /// family the body carries is decided `own`. The hierarchical form is
    /// [`Self::replace_route_in_chain`]. The route's `enabled` flag travels on
    /// the replacement body, exactly as an upstream's does.
    ///
    /// # Errors
    ///
    /// Returns the 404 row when no row of the calling tenant matches, a
    /// validation error when the body is not valid or states a different
    /// identifier or upstream reference, the `MatchConflict` row when another
    /// enabled route holds the match key, and a storage failure when the write
    /// could not be applied.
    #[allow(clippy::result_large_err)]
    pub fn replace_route(
        &self,
        tenant_id: Uuid,
        id: Uuid,
        body: &Value,
    ) -> Result<RouteRow, ServiceError> {
        self.replace_route_in_chain(tenant_id, id, &[], &OverridePermissions::none(), body)
    }

    /// Deletes one route and its dependent rows: `DELETE /oagw/v1/routes/{id}`.
    ///
    /// # Errors
    ///
    /// Returns the 404 row when no row of the calling tenant matches, and a
    /// storage failure when the deletion could not be applied.
    #[allow(clippy::result_large_err)]
    pub fn delete_route(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, ServiceError> {
        // @cpt-begin:cpt-cf-oagw-flow-route-delete:p1:inst-rt-del-scope-if
        // @cpt-begin:cpt-cf-oagw-flow-route-delete:p1:inst-rt-del-scope
        scoping::resolve_route(&self.store, tenant_id, id)
            .map_err(ServiceError::from)?;
        // @cpt-end:cpt-cf-oagw-flow-route-delete:p1:inst-rt-del-scope
        // @cpt-begin:cpt-cf-oagw-flow-route-delete:p1:inst-rt-del-scope-return
        // A row that did not resolve was answered 404 above; the two causes are
        // deliberately indistinguishable.
        // @cpt-end:cpt-cf-oagw-flow-route-delete:p1:inst-rt-del-scope-return
        // @cpt-end:cpt-cf-oagw-flow-route-delete:p1:inst-rt-del-scope-if
        // @cpt-begin:cpt-cf-oagw-flow-route-delete:p1:inst-rt-del-scope-else
        let deleted = self
            .store
            .delete_route(tenant_id, id)
            .map_err(ServiceError::from)?;
        // @cpt-end:cpt-cf-oagw-flow-route-delete:p1:inst-rt-del-scope-else

        self.cache.flush_for(tenant_id);
        // A successful deletion notifies the registered rate-limit cleanup; a
        // failed one notifies nothing.
        self.observers.route_deleted(tenant_id, id);
        // @cpt-begin:cpt-cf-oagw-flow-route-delete:p1:inst-rt-del-return
        Ok(deleted)
        // @cpt-end:cpt-cf-oagw-flow-route-delete:p1:inst-rt-del-return
    }

    /// Creates one custom plugin: `POST /oagw/v1/plugins`.
    ///
    /// The body selects the permission arm before any validation runs, so the
    /// family is what the handler authorized against. The source is stored
    /// verbatim and never parsed or executed here: the create flow's contract
    /// is the declared fields alone.
    ///
    /// # Errors
    ///
    /// Returns a validation error naming every failing property, the same
    /// validation error naming `name` when another plugin of the calling
    /// tenant holds the name, and a storage failure when the write could not
    /// be applied.
    #[allow(clippy::result_large_err)]
    pub fn create_plugin(&self, tenant_id: Uuid, body: &Value) -> Result<PluginRow, ServiceError> {
        // The registry check and the accumulated property validation are
        // `cpt-cf-oagw-algo-plugin-contract-registry`, which the create flow
        // calls before any row is written.
        let validated = plugin_def::validate_plugin(body).map_err(ServiceError::from)?;
        let plugin = Plugin {
            id: Uuid::new_v4(),
            tenant_id,
            plugin_type: validated.family.as_str().to_owned(),
            name: validated.name,
            description: validated.description,
            config_schema: validated.config_schema,
            phases: validated.phases,
            source_code: validated.source_code,
            // `last_used_at` stays unset until the data plane first resolves
            // the plugin, and `gc_eligible_at` stays unset until the lifecycle
            // moves the row.
            last_used_at: None,
            gc_eligible_at: None,
        };

        // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-dup
        let taken = self
            .store
            .list_plugins(tenant_id)
            .iter()
            .any(|row| row.plugin.name == plugin.name);
        // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-dup
        // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-dup-if
        if taken {
            // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-dup-return
            // RETURN 400 naming `name`; the calling tenant's catalogue holds
            // another plugin of the same name and no row is written.
            return Err(ServiceError::from(plugin_def::name_taken()));
            // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-dup-return
        }
        // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-dup-if
        // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-dup-else
        let written = self
            .store
            .insert_plugin(tenant_id, &plugin)
            .map_err(ServiceError::from)?;
        // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-dup-else

        self.cache.flush_for(tenant_id);
        // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-return
        Ok(written)
        // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-return
    }

    /// Reads one custom plugin: `GET /oagw/v1/plugins/{id}`.
    ///
    /// # Errors
    ///
    /// Returns the 404 row when no row of the calling tenant matches, which
    /// is also the answer for a named built-in plugin, which has no row.
    #[allow(clippy::result_large_err)]
    pub fn read_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<PluginRow, ServiceError> {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-scope
        let resolved = self.store.get_plugin(tenant_id, id);
        // @cpt-end:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-scope
        // @cpt-begin:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-404-if
        let Some(row) = resolved else {
            // @cpt-begin:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-404-return
            // RETURN 404: a missing identifier, a foreign one, and a named
            // plugin without a row are deliberately indistinguishable.
            return Err(ServiceError::from(scoping::path_miss()));
            // @cpt-end:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-404-return
        };
        // @cpt-end:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-404-if
        // @cpt-begin:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-404-else
        // @cpt-begin:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-assemble
        // The row as stored: the configuration schema and the source are
        // carried verbatim and never re-rendered.
        // @cpt-end:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-assemble
        // @cpt-end:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-404-else
        // @cpt-begin:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-return
        Ok(row)
        // @cpt-end:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-return
    }

    /// Lists the custom plugins of the calling tenant:
    /// `GET /oagw/v1/plugins`.
    ///
    /// # Errors
    ///
    /// Returns a validation error naming the offending parameter.
    #[allow(clippy::result_large_err)]
    pub fn list_plugins(&self, tenant_id: Uuid, query: &str) -> Result<Page<PluginRow>, ServiceError> {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-scope
        let parsed = odata::parse(ListKind::Plugin, query).map_err(ServiceError::from)?;
        let scan = self.store.list_plugins(tenant_id);
        // @cpt-end:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-scope
        // @cpt-begin:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-return
        Ok(odata::apply_plugin(&parsed, scan))
        // @cpt-end:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-return
    }

    /// Reads the Starlark source of one custom plugin:
    /// `GET /oagw/v1/plugins/{id}/source`.
    ///
    /// # Errors
    ///
    /// Returns the 404 row when no row of the calling tenant matches.
    #[allow(clippy::result_large_err)]
    pub fn read_plugin_source(&self, tenant_id: Uuid, id: Uuid) -> Result<String, ServiceError> {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-scope
        let resolved = self.store.get_plugin(tenant_id, id);
        // @cpt-end:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-scope
        // @cpt-begin:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-404-if
        let Some(row) = resolved else {
            // @cpt-begin:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-404-return
            return Err(ServiceError::from(scoping::path_miss()));
            // @cpt-end:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-404-return
        };
        // @cpt-end:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-404-if
        // @cpt-begin:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-404-else
        // @cpt-begin:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-assemble
        // The source path returns the stored source alone and no other member
        // of the row.
        // @cpt-end:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-assemble
        // @cpt-end:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-404-else
        // @cpt-begin:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-return
        Ok(row.plugin.source_code)
        // @cpt-end:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-return
    }

    /// Deletes one custom plugin: `DELETE /oagw/v1/plugins/{id}`.
    ///
    /// The reference scan runs before the write and answers 409 when any
    /// binding row or upstream `auth_plugin_uuid` still carries the plugin.
    ///
    /// # Errors
    ///
    /// Returns the 404 row when no row of the calling tenant matches, the 409
    /// in-use variant when a reference remains, and a storage failure when the
    /// deletion could not be applied.
    #[allow(clippy::result_large_err)]
    pub fn delete_plugin(&self, tenant_id: Uuid, id: Uuid) -> Result<bool, ServiceError> {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-scope
        let resolved = self.store.get_plugin(tenant_id, id);
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-scope
        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-404-if
        let Some(_) = resolved else {
            // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-404-return
            // RETURN 404, indistinguishable between the three causes.
            return Err(ServiceError::from(scoping::path_miss()));
            // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-404-return
        };
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-404-if
        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-404-else
        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-inuse
        // @cpt-dod:cpt-cf-oagw-dod-plugin-inuse-gc:p1
        let in_use = self.store.plugin_in_use(tenant_id, id);
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-inuse
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-404-else

        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-inuse-if
        // @cpt-begin:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-state-inuse-guard
        // @cpt-begin:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-delete-if
        if in_use {
            // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-inuse-return
            // @cpt-begin:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-delete-return
            // RETURN 409 PluginInUse: the row and its gc_eligible_at are left
            // exactly as the reference scan found them.
            return Err(ServiceError::from(plugin_def::plugin_in_use()));
            // @cpt-end:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-delete-return
            // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-inuse-return
        }
        // @cpt-end:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-delete-if
        // @cpt-end:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-state-inuse-guard
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-inuse-if
        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-inuse-else
        // @cpt-begin:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-delete-else
        // @cpt-begin:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-continue
        let deleted = self
            .store
            .delete_plugin(tenant_id, id)
            .map_err(ServiceError::from)?;
        // @cpt-end:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-continue
        // @cpt-end:cpt-cf-oagw-algo-plugin-inuse-gc:p1:inst-inuse-delete-else
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-inuse-else

        self.cache.flush_for(tenant_id);
        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-return
        Ok(deleted)
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-return
    }

    /// Runs one pass of the periodic garbage-collection job of §1.4.
    ///
    /// The pass marks every custom row whose reference set is empty and which
    /// carries no marking, then deletes the rows whose TTL has elapsed and
    /// whose reference set is still empty when it runs. The cache is flushed
    /// for the rows the pass removed, so a resolution no longer answers for a
    /// row the store no longer holds.
    ///
    /// # Errors
    ///
    /// Returns the storage failure when the pass could not be applied, which
    /// leaves every row exactly as it was.
    #[allow(clippy::result_large_err)]
    pub fn run_plugin_garbage_collection(&self) -> Result<PluginGcReport, ServiceError> {
        self.run_plugin_garbage_collection_at(crate::store::unix_now())
    }

    /// Runs one pass of the job at an explicit instant, which the tests use to
    /// place a row on either side of the TTL.
    ///
    /// # Errors
    ///
    /// Returns the storage failure the pass answers, which leaves every row
    /// exactly as it was.
    #[allow(clippy::result_large_err)]
    pub fn run_plugin_garbage_collection_at(&self, now: u64) -> Result<PluginGcReport, ServiceError> {
        let report = self.store.run_plugin_garbage_collection(now);
        if !report.collected.is_empty() {
            self.cache.flush();
        }
        Ok(report)
    }

    /// The ancestor bindings the chain holds for one upstream row, at a depth
    /// greater than the calling tenant's.
    ///
    /// The row participates in the hierarchy through its own immutable alias,
    /// which the walk matches against the chain.
    #[allow(clippy::result_large_err)]
    fn upstream_ancestor_bindings(
        &self,
        tenant_id: Uuid,
        ancestors: &[Uuid],
        row: &UpstreamRow,
    ) -> Result<Vec<AncestorBinding>, ServiceError> {
        let Some(alias) = row
            .upstream
            .alias
            .as_deref()
            .and_then(|alias| Alias::parse(alias).ok())
        else {
            return Ok(Vec::new());
        };
        self.ancestor_bindings(tenant_id, ancestors, &alias)
    }

    /// The ancestor bindings the chain holds for one normalized alias, at a
    /// depth greater than the calling tenant's.
    ///
    /// An unordered or cyclic chain is a storage failure: the operation fails
    /// closed with the platform 500 problem shape and writes nothing, exactly
    /// as an unavailable chain does. The per-element reads are the
    /// tenant-scoped reads `cpt-cf-oagw-algo-tenant-chain-walk` issues; a
    /// tenant whose rows the calling tenant cannot read is never a candidate.
    #[allow(clippy::result_large_err)]
    fn ancestor_bindings(
        &self,
        tenant_id: Uuid,
        ancestors: &[Uuid],
        alias: &Alias,
    ) -> Result<Vec<AncestorBinding>, ServiceError> {
        let candidates = chain::walk_candidates(&self.store, tenant_id, ancestors, alias)
            .map_err(|error| ServiceError::Storage {
                reason: error.to_string(),
            })?;
        Ok(candidates
            .iter()
            .filter(|candidate| candidate.depth > 0)
            .map(|candidate| AncestorBinding {
                tenant_id: candidate.tenant_id,
                depth: candidate.depth,
                upstream_id: candidate.upstream_id,
                enabled: candidate.enabled,
                contributed: shadow::contributed(&candidate.row.upstream),
            })
            .collect())
    }

    /// The ancestor route bindings one route row participates in.
    ///
    /// A route inherits through its upstream's alias: the ancestor upstreams
    /// the walk matches on that alias are the ones whose routes a resolution
    /// reads, so every route those upstreams hold is an ancestor of the row.
    /// An owning upstream the store cannot resolve — which a validated route
    /// reference cannot produce — contributes no ancestor, because no ancestor
    /// value can then be obtained for the row.
    #[allow(clippy::result_large_err)]
    fn route_ancestor_bindings(
        &self,
        tenant_id: Uuid,
        ancestors: &[Uuid],
        row: &RouteRow,
    ) -> Result<Vec<AncestorBinding>, ServiceError> {
        let Some(owner) = self.store.get_upstream(tenant_id, row.route.upstream_id) else {
            return Ok(Vec::new());
        };
        let Some(alias) = owner
            .upstream
            .alias
            .as_deref()
            .and_then(|alias| Alias::parse(alias).ok())
        else {
            return Ok(Vec::new());
        };
        let candidates = chain::walk_candidates(&self.store, tenant_id, ancestors, &alias)
            .map_err(|error| ServiceError::Storage {
                reason: error.to_string(),
            })?;
        let mut bindings = Vec::new();
        for candidate in candidates.iter().filter(|candidate| candidate.depth > 0) {
            for route in self
                .store
                .routes_of_upstream(candidate.tenant_id, candidate.upstream_id)
            {
                bindings.push(AncestorBinding {
                    tenant_id: candidate.tenant_id,
                    depth: candidate.depth,
                    upstream_id: candidate.upstream_id,
                    enabled: !matches!(route.route.enabled, Some(false)),
                    contributed: shadow::route_contributed(&route.route),
                });
            }
        }
        Ok(bindings)
    }
}

/// The 409 the alias uniqueness constraint answers with.
fn alias_conflict() -> DomainError {
    // @cpt-dod:cpt-cf-oagw-dod-alias-derivation:p1
    DomainError::gateway(
        ErrorKind::AliasConflict,
        "another upstream of the calling tenant already holds the alias",
    )
}

/// The sharing-bearing families one upstream body carries, in the order the
/// decision table reads them.
///
/// `tags` carries no sharing field and never reaches the decision; a family
/// the body omits is written by nobody and takes no part in it either.
fn carried_families(value: &Upstream) -> Vec<Family> {
    let mut carried = Vec::new();
    if value.auth.is_some() {
        carried.push(Family::Auth);
    }
    if value.rate_limit.is_some() {
        carried.push(Family::RateLimit);
    }
    if value.plugins.is_some() {
        carried.push(Family::Plugins);
    }
    if value.cors.is_some() {
        carried.push(Family::Cors);
    }
    carried
}

/// The sharing-bearing families one route body carries.
///
/// A route carries no `auth` family, so there is no inherited auth
/// configuration for a descendant route to override, and `tags` carries no
/// sharing field.
fn carried_route_families(value: &Route) -> Vec<Family> {
    let mut carried = Vec::new();
    if value.rate_limit.is_some() {
        carried.push(Family::RateLimit);
    }
    if value.plugins.is_some() {
        carried.push(Family::Plugins);
    }
    if value.cors.is_some() {
        carried.push(Family::Cors);
    }
    carried
}

/// The 400 an `enforce` refusal answers with: a validation error naming the
/// family the ancestor enforces, carrying no request-body value and no other
/// detail of the ancestor's configuration.
fn enforced_family_error(family: Family) -> DomainError {
    DomainError::gateway(
        ErrorKind::ValidationError,
        format!(
            "the {} family is enforced by an ancestor and cannot be set",
            family_label(family)
        ),
    )
}

/// The schema-member name of one family, which is how a refusal names it.
const fn family_label(family: Family) -> &'static str {
    match family {
        Family::Auth => "auth",
        Family::RateLimit => "rate_limit",
        Family::Plugins => "plugins",
        Family::Cors => "cors",
    }
}

/// The 409 the match uniqueness constraint answers with, naming the route that
/// holds the key. The detail carries an identifier, never a body value.
fn conflict(holder: Uuid) -> DomainError {
    DomainError::gateway(
        ErrorKind::MatchConflict,
        format!("route {holder} already holds this match rule"),
    )
}

/// The anonymous GTS instance identifier of one upstream row.
#[must_use]
pub fn upstream_instance(id: Uuid) -> String {
    gts::gts_instance(gts::UPSTREAM_TYPE, id)
}

/// The anonymous GTS instance identifier of one route row.
#[must_use]
pub fn route_instance(id: Uuid) -> String {
    gts::gts_instance(gts::ROUTE_TYPE, id)
}
