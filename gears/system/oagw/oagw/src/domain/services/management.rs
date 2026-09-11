//! The management service: the domain half of the ten CRUD flows.
//!
//! The transport (`crate::api::rest`) owns the HTTP half of each flow —
//! authentication, the permission gate, the body parse and the problem mapping —
//! and calls in here with the parsed request and the resolved [`Actor`]. The
//! service owns everything else: model validation, alias derivation and
//! enforcement, the ancestor gates, the identity resolution and the store
//! writes, in the order the flows fix.
//!
//! The store writes are atomic and publish the new snapshot themselves, so a
//! flow either returns the committed record or the mapped rejection.
//!
//! The ancestor chain is the one asynchronous step of a write: the service
//! awaits [`TenantHierarchy::ancestors_of`] before the synchronous sharing gate
//! runs, so a `tenant-resolver` outage fails the write closed instead of
//! pretending the caller has no ancestors.

use std::sync::Arc;

use toolkit_security::SecurityContext;
use tracing::info;
use uuid::Uuid;

use crate::domain::alias;
use crate::domain::error::ManagementError;
use crate::domain::identity;
use crate::domain::model::{Plugin, Route, Timestamp, Upstream};
use crate::domain::plugin::resolver::{
    resolve_definition, validate_plugin_bindings,
};
use crate::domain::query;
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};
use crate::domain::sharing::{Actor, SharingGate, TenantHierarchy};
use crate::domain::validation::{
    build_plugin, validate_plugin, validate_route, validate_upstream, PluginSpec, RouteSpec,
    UpstreamSpec,
};

/// The domain half of the management API.
///
/// One instance lives for the lifetime of the gear, over the store the data
/// plane resolves from and the hierarchy source the gear resolved at startup.
pub struct ManagementService<S: ?Sized> {
    store: Arc<S>,
    hierarchy: Arc<dyn TenantHierarchy>,
}

impl<S: UpstreamRepository + RouteRepository + PluginRepository + ?Sized> ManagementService<S> {
    /// A service over the given store and tenant hierarchy.
    #[must_use]
    pub fn new(store: Arc<S>, hierarchy: Arc<dyn TenantHierarchy>) -> Self {
        Self { store, hierarchy }
    }

    /// The store the service was built over.
    #[must_use]
    pub fn store(&self) -> &Arc<S> {
        &self.store
    }

    // ------------------------------------------------------------------
    // Upstream create (`cpt-cf-oagw-flow-upstream-create`)
    // ------------------------------------------------------------------

    /// `POST /oagw/v1/upstreams`.
    ///
    /// # Errors
    ///
    /// Returns the mapped `400` of a rejected model or a non-derivable alias,
    /// the mapped `409` of a taken `(tenant_id, alias)` key, the mapped `403`
    /// of the bind gate and the mapped `503` of an unreachable hierarchy source.
    pub async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        actor: &Actor,
        spec: &UpstreamSpec,
    ) -> Result<Arc<Upstream>, ManagementError> {
        // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-09
        // The body parsed and the tenant resolved, the write side begins.
        // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-09

        // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-05
        // @cpt-begin:cpt-cf-oagw-dod-upstream-model:p1:inst-full
        let mut candidate =
            validate_upstream(spec, actor.tenant_id, Timestamp::now()).map_err(ManagementError::from)?;
        // @cpt-end:cpt-cf-oagw-dod-upstream-model:p1:inst-full
        // @cpt-begin:cpt-cf-oagw-dod-plugin-binding-validation:p1:inst-full
        self.validate_bindings(&ValidatedRecord::from(&candidate))?;
        // @cpt-end:cpt-cf-oagw-dod-plugin-binding-validation:p1:inst-full
        // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-05

        // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-06
        let alias = alias::resolve_alias(&candidate.server, spec.alias.as_deref())
            .map_err(ManagementError::from)?;
        candidate.id = Uuid::new_v4();
        candidate.alias = alias;
        // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-06

        // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-07
        // A validation or alias failure above is returned to the transport,
        // which maps it to the `400` problem body.
        // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-07

        // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-10
        // The alias key of the calling tenant's own upstreams: a bind to an
        // ancestor alias does not collide with it.
        if self
            .store
            .find_upstream_by_alias(actor.tenant_id, &candidate.alias)
            .is_some()
        {
            return Err(ManagementError::alias_conflict(format!(
                "an upstream with alias `{}` already exists in this tenant",
                candidate.alias
            )));
        }
        // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-10

        // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-11
        let gate = self.gate(ctx, actor.tenant_id).await?;
        let (effective, _constraints) =
            gate.check_upstream_create(self.store.as_ref(), actor, candidate)?;
        // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-11

        // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-12
        // @cpt-begin:cpt-cf-oagw-dod-store-invariants:p1:inst-full
        // The store takes the tenant write lock, checks `UNIQUE (tenant_id,
        // alias)` inside it, writes the record with its tag and plugin rows in
        // one unit and publishes the new snapshot, which is the cache
        // invalidation the data plane sees.
        let record = self.store.insert_upstream(effective)?;
        // @cpt-end:cpt-cf-oagw-dod-store-invariants:p1:inst-full
        // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-12

        // @cpt-begin:cpt-cf-oagw-state-upstream-lifecycle:p1:inst-ust-01
        // `absent` -> `active`: the create passed every gate.
        // @cpt-end:cpt-cf-oagw-state-upstream-lifecycle:p1:inst-ust-01

        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-enb-02
        // `enabled` defaulted to `true` in the validator when the body omitted
        // it, so the stored record carries the effective value.
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-enb-02

        // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-14
        Ok(record)
        // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-14
    }

    // ------------------------------------------------------------------
    // Upstream replace (`cpt-cf-oagw-flow-upstream-replace`)
    // ------------------------------------------------------------------

    /// `PUT /oagw/v1/upstreams/{id}`.
    ///
    /// # Errors
    ///
    /// Returns the mapped `404` of an unresolvable identifier, the mapped `400`
    /// of a changed immutable field or a rejected replacement, the mapped
    /// `403`/`409` of the gates and the mapped `503` of an unreachable hierarchy
    /// source.
    pub async fn replace_upstream(
        &self,
        ctx: &SecurityContext,
        actor: &Actor,
        identifier: &str,
        spec: &UpstreamSpec,
    ) -> Result<Arc<Upstream>, ManagementError> {
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-03
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-04
        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-05
        // @cpt-begin:cpt-cf-oagw-algo-resource-identity:p1:inst-rid-09
        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-enb-04
        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-enb-05
        // The tenant-scoped lookup refuses an ancestor-owned record with a `404`
        // before any field — `enabled` included — is read, so an inherited
        // disabled state cannot be lifted by a descendant.
        let existing = identity::resolve_upstream(self.store.as_ref(), actor.tenant_id, identifier)?;
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-enb-05
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-enb-04
        // @cpt-end:cpt-cf-oagw-algo-resource-identity:p1:inst-rid-09
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-05
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-04
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-03

        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-enb-06
        // The record is the calling tenant's own, so a supplied `enabled` value
        // is accepted.

        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-06
        // `id` and `tenant_id` are immutable and the supplied `alias`, if any,
        // must equal the stored one; `enforce_alias_update` rejects a body that
        // names a different one.
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-06

        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-07
        let mut replacement =
            validate_upstream(spec, actor.tenant_id, Timestamp::now()).map_err(ManagementError::from)?;
        // @cpt-begin:cpt-cf-oagw-dod-plugin-binding-validation:p1:inst-full
        self.validate_bindings(&ValidatedRecord::from(&replacement))?;
        // @cpt-end:cpt-cf-oagw-dod-plugin-binding-validation:p1:inst-full
        let alias = alias::enforce_alias_update(&existing, &replacement.server, spec.alias.as_deref())
            .map_err(ManagementError::from)?;
        replacement.id = existing.id;
        replacement.tenant_id = existing.tenant_id;
        replacement.alias = alias;
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-07

        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-08
        // The ancestor gates are re-validated on every replace: the alias is
        // immutable, so the resolution is the stored record's.
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-08

        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-09
        let gate = self.gate(ctx, actor.tenant_id).await?;
        let (effective, _constraints) = gate.check_upstream_replace(
            self.store.as_ref(),
            actor,
            &existing,
            replacement,
        )?;
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-09

        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-10
        // Omitted optional fields are cleared: the store writes the whole
        // record, so the replacement above is the complete new state.
        let record = self.store.replace_upstream(&existing, effective)?;
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-10

        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-enb-07
        // The `enabled` boolean of the owning tenant's record is stored, and the
        // write published the snapshot the data plane resolves from.
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-enb-07
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-enb-06

        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-enb-08
        // What a disabled upstream does at request time — the `503` and the
        // exclusion of a disabled route from matching — is data-plane behaviour
        // (entry 2.4); management only records the boolean.
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-enb-08

        // @cpt-begin:cpt-cf-oagw-state-upstream-lifecycle:p1:inst-ust-02
        // `active` -> `disabled` when the replace sets `enabled: false`, and
        // @cpt-end:cpt-cf-oagw-state-upstream-lifecycle:p1:inst-ust-02

        // @cpt-begin:cpt-cf-oagw-state-upstream-lifecycle:p1:inst-ust-03
        // `disabled` -> `active` when it sets `enabled: true`; a descendant
        // cannot drive either transition, because an ancestor resource does not
        // resolve here in the first place.
        // @cpt-end:cpt-cf-oagw-state-upstream-lifecycle:p1:inst-ust-03

        // @cpt-begin:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-12
        Ok(record)
        // @cpt-end:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-12
    }

    // ------------------------------------------------------------------
    // Upstream read, list and delete
    // ------------------------------------------------------------------

    /// `GET /oagw/v1/upstreams/{id}`.
    ///
    /// # Errors
    ///
    /// Returns the mapped `400` of a malformed identifier and the mapped `404`
    /// of an identifier this tenant does not hold.
    pub fn get_upstream(
        &self,
        actor: &Actor,
        identifier: &str,
    ) -> Result<Arc<Upstream>, ManagementError> {
        // @cpt-begin:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-03
        // @cpt-begin:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-04
        // The lookup is scoped to the calling tenant before anything else.
        // @cpt-begin:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-05
        // An unresolved or foreign identifier — an ancestor's included — comes
        // back as the `404` problem body.
        identity::resolve_upstream(self.store.as_ref(), actor.tenant_id, identifier)
        // @cpt-end:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-05
        // @cpt-end:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-04
        // @cpt-end:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-03
    }

    /// `GET /oagw/v1/upstreams`.
    ///
    /// # Errors
    ///
    /// Returns the mapped `400` of an unsupported query expression.
    pub fn list_upstreams(&self, actor: &Actor, query_string: &str) -> Result<Vec<serde_json::Value>, ManagementError> {
        let records = self.store.list_upstreams(actor.tenant_id);
        let records = records.iter().map(|record| record.as_ref().clone()).collect::<Vec<_>>();
        // @cpt-begin:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-07
        let query = query::parse(
            query_string,
            query::UPSTREAM_FILTER_FIELDS,
            query::UPSTREAM_FIELDS,
        )?;
        // @cpt-end:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-07

        // @cpt-begin:cpt-cf-oagw-algo-odata-query:p1:inst-oq-09
        // The projection carries the stored representation's fields only; the
        // auth reference stays a `cred://` identifier and no resolved secret
        // value ever enters a response.
        // @cpt-begin:cpt-cf-oagw-algo-odata-query:p1:inst-oq-10
        Ok(query::apply(&records, &query))
        // @cpt-end:cpt-cf-oagw-algo-odata-query:p1:inst-oq-10
        // @cpt-end:cpt-cf-oagw-algo-odata-query:p1:inst-oq-09
    }

    /// `DELETE /oagw/v1/upstreams/{id}`.
    ///
    /// Returns the routes the cascade removed.
    ///
    /// # Errors
    ///
    /// Returns the mapped `400` of a malformed identifier and the mapped `404`
    /// of an identifier this tenant does not hold.
    pub fn delete_upstream(
        &self,
        actor: &Actor,
        identifier: &str,
    ) -> Result<Vec<Arc<Route>>, ManagementError> {
        // @cpt-begin:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-03
        // @cpt-begin:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-04
        // An unresolvable identifier returns `404` before any row is touched.
        // @cpt-begin:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-05
        let existing = identity::resolve_upstream(self.store.as_ref(), actor.tenant_id, identifier)?;
        // @cpt-end:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-05
        // @cpt-end:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-04
        // @cpt-end:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-03

        // @cpt-begin:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-06
        // The identifier resolved inside the calling tenant's key space, so the
        // delete proceeds.

        // @cpt-begin:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-07
        // The store removes the record and its dependent rows — the routes that
        // reference it included — in one atomic write.
        let cascade = self.store.delete_upstream(actor.tenant_id, existing.id)?;
        // @cpt-end:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-07

        // @cpt-begin:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-08
        // The write published the new snapshot and invalidated the consumer
        // caches before returning.
        // @cpt-end:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-08
        // @cpt-end:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-06

        // @cpt-begin:cpt-cf-oagw-state-upstream-lifecycle:p1:inst-ust-04
        // `active` -> `absent`, and with it the routes the cascade removed.
        // @cpt-end:cpt-cf-oagw-state-upstream-lifecycle:p1:inst-ust-04

        // @cpt-begin:cpt-cf-oagw-state-upstream-lifecycle:p1:inst-ust-05
        // `disabled` -> `absent`: a delete is allowed on a disabled upstream and
        // removes it from the lifecycle without re-enabling it first.
        // @cpt-end:cpt-cf-oagw-state-upstream-lifecycle:p1:inst-ust-05

        // @cpt-begin:cpt-cf-oagw-state-route-lifecycle:p1:inst-rst-05
        // A route of the deleted upstream leaves the lifecycle with it: `active`
        // or `disabled` -> `absent`, without its own delete call.
        // @cpt-end:cpt-cf-oagw-state-route-lifecycle:p1:inst-rst-05

        Ok(cascade)
    }

    // ------------------------------------------------------------------
    // Route create (`cpt-cf-oagw-flow-route-create`)
    // ------------------------------------------------------------------

    /// `POST /oagw/v1/routes`.
    ///
    /// # Errors
    ///
    /// Returns the mapped `400` of a rejected model or an unresolvable
    /// `upstream_id` and the mapped `409` of a duplicate match rule.
    pub fn create_route(
        &self,
        actor: &Actor,
        spec: &RouteSpec,
    ) -> Result<Arc<Route>, ManagementError> {
        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rcre-04
        // @cpt-begin:cpt-cf-oagw-algo-route-validate:p1:inst-rval-10
        // @cpt-begin:cpt-cf-oagw-algo-route-validate:p1:inst-rval-11
        // @cpt-begin:cpt-cf-oagw-algo-route-validate:p1:inst-rval-12
        // @cpt-begin:cpt-cf-oagw-dod-route-model:p1:inst-full
        let mut candidate =
            validate_route(spec, actor.tenant_id, Timestamp::now()).map_err(ManagementError::from)?;
        // @cpt-end:cpt-cf-oagw-dod-route-model:p1:inst-full
        // @cpt-begin:cpt-cf-oagw-dod-plugin-binding-validation:p1:inst-full
        self.validate_bindings(&ValidatedRecord::from(&candidate))?;
        // @cpt-end:cpt-cf-oagw-dod-plugin-binding-validation:p1:inst-full
        // @cpt-end:cpt-cf-oagw-algo-route-validate:p1:inst-rval-12
        // @cpt-end:cpt-cf-oagw-algo-route-validate:p1:inst-rval-11
        // @cpt-end:cpt-cf-oagw-algo-route-validate:p1:inst-rval-10
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rcre-04

        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rcre-05
        // @cpt-begin:cpt-cf-oagw-algo-route-validate:p1:inst-rval-09
        // `upstream_id` is resolved inside the calling tenant's key space: an
        // ancestor-owned upstream is not directly addressable and resolves as
        // missing, which the mapping layer renders as `400`.
        let supplied = spec.upstream_id.ok_or_else(|| {
            ManagementError::validation("upstream_id: required".to_string())
        })?;
        let referenced = identity::resolve_upstream(
            self.store.as_ref(),
            actor.tenant_id,
            &supplied.to_string(),
        )
        .map_err(|error| unresolved_upstream(error, supplied))?;
        // @cpt-end:cpt-cf-oagw-algo-route-validate:p1:inst-rval-09
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rcre-05

        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rcre-06
        // A rejected model or an unresolvable reference above is returned to the
        // transport, which renders the `400` naming the offending field.
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rcre-06

        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rcre-08
        // The reference resolved, so the create carries on to the uniqueness
        // check and the store write.
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rcre-08

        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rcre-09
        // The match-uniqueness invariant is the store's: it compares the
        // candidate with the enabled routes of the referenced upstream inside
        // the tenant's write lock.
        candidate.id = Uuid::new_v4();
        candidate.tenant_id = actor.tenant_id;
        candidate.upstream_id = referenced.id;
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rcre-09

        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rcre-10
        let record = self.store.insert_route(candidate)?;
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rcre-10

        // @cpt-begin:cpt-cf-oagw-state-route-lifecycle:p1:inst-rst-01
        // `absent` -> `active`: the create passed validation, the reference
        // resolved in the calling tenant and the match rule is unique.
        // @cpt-end:cpt-cf-oagw-state-route-lifecycle:p1:inst-rst-01

        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rcre-12
        Ok(record)
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rcre-12
    }

    // ------------------------------------------------------------------
    // Route replace (`cpt-cf-oagw-flow-route-replace`)
    // ------------------------------------------------------------------

    /// `PUT /oagw/v1/routes/{id}`.
    ///
    /// # Errors
    ///
    /// Returns the mapped `404` of an unresolvable identifier, the mapped `400`
    /// of a changed immutable field and the mapped `409` of a colliding match.
    pub fn replace_route(
        &self,
        actor: &Actor,
        identifier: &str,
        spec: &RouteSpec,
    ) -> Result<Arc<Route>, ManagementError> {
        // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-03
        // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-04
        // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-05
        let existing = identity::resolve_route(self.store.as_ref(), actor.tenant_id, identifier)?;
        // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-05
        // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-04
        // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-03

        // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-06
        // The identifier resolved inside the calling tenant's key space.

        // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-07
        // The reference is immutable: a supplied `upstream_id` must echo the
        // stored one, and a differing value is `400`.
        if let Some(supplied) = spec.upstream_id
            && supplied != existing.upstream_id
        {
            return Err(ManagementError::identity(
                "upstream_id: is immutable and must echo the stored value".to_string(),
            ));
        }
        // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-07

        // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-08
        let mut replacement =
            validate_route(spec, actor.tenant_id, Timestamp::now()).map_err(ManagementError::from)?;
        // @cpt-begin:cpt-cf-oagw-dod-plugin-binding-validation:p1:inst-full
        self.validate_bindings(&ValidatedRecord::from(&replacement))?;
        // @cpt-end:cpt-cf-oagw-dod-plugin-binding-validation:p1:inst-full
        replacement.id = existing.id;
        replacement.tenant_id = existing.tenant_id;
        replacement.upstream_id = existing.upstream_id;
        // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-08

        // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-09
        // The match rule is re-validated against the enabled routes of the same
        // upstream, so a re-enabled route cannot collide.
        // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-10
        // The store writes the whole replacement — the match rule, the methods,
        // the tags and the plugin rows — so a field omitted from the body is
        // cleared rather than carried over.
        let record = self.store.replace_route(&existing, replacement)?;
        // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-10
        // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-09
        // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-06

        // @cpt-begin:cpt-cf-oagw-state-route-lifecycle:p1:inst-rst-03
        // `disabled` -> `active` only when the revalidated match rule does not
        // collide; otherwise the replace is refused and the route stays as it
        // was.
        // @cpt-end:cpt-cf-oagw-state-route-lifecycle:p1:inst-rst-03

        // @cpt-begin:cpt-cf-oagw-state-route-lifecycle:p1:inst-rst-02
        // `active` -> `disabled` when the replace sets `enabled: false`.
        // @cpt-end:cpt-cf-oagw-state-route-lifecycle:p1:inst-rst-02

        // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-12
        Ok(record)
        // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-12
    }

    // ------------------------------------------------------------------
    // Route read, list and delete
    // ------------------------------------------------------------------

    /// `GET /oagw/v1/routes/{id}`.
    ///
    /// # Errors
    ///
    /// Returns the mapped `400` of a malformed identifier and the mapped `404`
    /// of an identifier this tenant does not hold.
    pub fn get_route(
        &self,
        actor: &Actor,
        identifier: &str,
    ) -> Result<Arc<Route>, ManagementError> {
        // @cpt-begin:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-03
        // @cpt-begin:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-04
        // @cpt-begin:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-05
        identity::resolve_route(self.store.as_ref(), actor.tenant_id, identifier)
        // @cpt-end:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-05
        // @cpt-end:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-04
        // @cpt-end:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-03
    }

    /// `GET /oagw/v1/routes`.
    ///
    /// # Errors
    ///
    /// Returns the mapped `400` of an unsupported query expression.
    pub fn list_routes(&self, actor: &Actor, query_string: &str) -> Result<Vec<serde_json::Value>, ManagementError> {
        let records = self.store.list_routes(actor.tenant_id);
        let records = records.iter().map(|record| record.as_ref().clone()).collect::<Vec<_>>();
        // @cpt-begin:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-07
        let query = query::parse(
            query_string,
            query::ROUTE_FILTER_FIELDS,
            query::ROUTE_FIELDS,
        )?;
        // @cpt-end:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-07

        // @cpt-begin:cpt-cf-oagw-algo-odata-query:p1:inst-oq-09
        // @cpt-begin:cpt-cf-oagw-algo-odata-query:p1:inst-oq-10
        Ok(query::apply(&records, &query))
        // @cpt-end:cpt-cf-oagw-algo-odata-query:p1:inst-oq-10
        // @cpt-end:cpt-cf-oagw-algo-odata-query:p1:inst-oq-09
    }

    /// `DELETE /oagw/v1/routes/{id}`.
    ///
    /// # Errors
    ///
    /// Returns the mapped `400` of a malformed identifier and the mapped `404`
    /// of an identifier this tenant does not hold.
    pub fn delete_route(&self, actor: &Actor, identifier: &str) -> Result<(), ManagementError> {
        // @cpt-begin:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-04
        // The identifier does not resolve outside the calling tenant, so an
        // ancestor-owned route is refused here before the delete is attempted.
        let existing = identity::resolve_route(self.store.as_ref(), actor.tenant_id, identifier)?;
        // @cpt-end:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-04

        // @cpt-begin:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-07
        self.store.delete_route(actor.tenant_id, existing.id)?;
        // @cpt-end:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-07

        // @cpt-begin:cpt-cf-oagw-state-route-lifecycle:p1:inst-rst-04
        // `active` -> `absent`, whatever the stored `enabled` value was.
        // @cpt-end:cpt-cf-oagw-state-route-lifecycle:p1:inst-rst-04
        Ok(())
    }

    /// The gate of the ancestor and sharing-mode rules, holding the caller's
    /// ancestor chain resolved for this request.
    async fn gate(
        &self,
        ctx: &SecurityContext,
        tenant_id: Uuid,
    ) -> Result<SharingGate, ManagementError> {
        let chain = self.hierarchy.ancestors_of(ctx, tenant_id).await?;
        Ok(SharingGate::new(chain))
    }

    /// Validate the plugin bindings an upstream or route write stores
    /// (`cpt-cf-oagw-algo-plugin-binding-validate`).
    ///
    /// Runs after the model validation and before any store write, so a rejected
    /// reference leaves the store untouched (`inst-pbnd-13`).
    ///
    /// # Errors
    ///
    /// Returns the mapped `400` of an unresolvable reference, a catalog-only
    /// identifier, a mismatched plugin type, an `auth` plugin placed in
    /// `plugins.items[]` and a non-contiguous position set.
    fn validate_bindings(
        &self,
        record: &ValidatedRecord<'_>,
    ) -> Result<(), ManagementError> {
        // @cpt-begin:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-03
        // The binding invariants of a binding write: positions contiguous from
        // `0`, `plugin_ref` always stored, `plugin_uuid` stored only for a
        // UUID-backed plugin and matching it when present. The check runs before
        // any row is written, so a rejected invariant leaves the store untouched.
        // @cpt-begin:cpt-cf-oagw-dod-plugin-binding-validation:p1:inst-full
        validate_plugin_bindings(
            self.store.as_ref(),
            record.tenant_id(),
            record
                .plugins()
                .map(|plugins| plugins.items.as_slice())
                .unwrap_or_default(),
            record.auth(),
        )
        .map_err(ManagementError::from)
        // @cpt-end:cpt-cf-oagw-dod-plugin-binding-validation:p1:inst-full
        // @cpt-end:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-03
    }

    // ------------------------------------------------------------------
    // Plugin create (`cpt-cf-oagw-flow-plugin-create`)
    // ------------------------------------------------------------------

    /// `POST /oagw/v1/plugins`.
    ///
    /// # Errors
    ///
    /// Returns the mapped `400` of a rejected definition and the mapped `409`
    /// of a taken `(tenant_id, name)` key.
    pub fn create_plugin(
        &self,
        actor: &Actor,
        spec: &PluginSpec,
    ) -> Result<Arc<Plugin>, ManagementError> {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-05
        // @cpt-begin:cpt-cf-oagw-dod-plugin-model:p1:inst-full
        // The validator classifies the requested `plugin_type` with the
        // identifier-resolution algorithm on the way.
        let contract = validate_plugin(spec).map_err(ManagementError::from)?;
        // @cpt-end:cpt-cf-oagw-dod-plugin-model:p1:inst-full
        // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-05

        // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-06
        // A rejected body is returned to the transport, which maps it to the
        // `400` problem body naming the offending field.
        // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-06

        // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-08
        // @cpt-begin:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-05
        // `id` is a generated UUID and `tenant_id` the calling tenant's; both
        // are stamped here and never changed afterwards, because the definition
        // is immutable.
        // @cpt-begin:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-psta-01
        let candidate = build_plugin(contract, actor.tenant_id, Uuid::new_v4());
        // @cpt-end:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-psta-01
        // @cpt-end:cpt-cf-oagw-algo-plugin-store-write:p1:inst-pstr-05
        // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-08

        // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-10
        // @cpt-begin:cpt-cf-oagw-dod-plugin-store-invariants:p1:inst-full
        // The store takes the tenant write lock, checks `UNIQUE (tenant_id,
        // name)` inside it, writes the row in one unit and publishes the new
        // snapshot, which is the cache invalidation the data plane sees.
        let record = self.store.insert_plugin(candidate)?;
        // @cpt-end:cpt-cf-oagw-dod-plugin-store-invariants:p1:inst-full
        // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-10

        // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-12
        self.log_plugin_write("create", &record, actor, "stored");
        // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-12

        // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-13
        Ok(record)
        // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-13
    }

    // ------------------------------------------------------------------
    // Plugin read, list, source and delete
    // ------------------------------------------------------------------

    /// `GET /oagw/v1/plugins/{id}`.
    ///
    /// # Errors
    ///
    /// Returns the mapped `400` of a malformed identifier and the mapped `404`
    /// of an identifier this tenant does not hold.
    pub fn get_plugin(&self, actor: &Actor, identifier: &str) -> Result<Arc<Plugin>, ManagementError> {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-04
        // @cpt-begin:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-05
        resolve_definition(self.store.as_ref(), actor.tenant_id, identifier)
        // @cpt-end:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-05
        // @cpt-end:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-04
    }

    /// `GET /oagw/v1/plugins`.
    ///
    /// # Errors
    ///
    /// Returns the mapped `400` of an unsupported query expression.
    pub fn list_plugins(&self, actor: &Actor, query_string: &str) -> Result<Vec<serde_json::Value>, ManagementError> {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-03
        // The collection is scoped to the calling tenant before any filtering or
        // paging, so no other tenant's definition can enter the page.
        let records = self.store.list_plugins(actor.tenant_id);
        let records = records.iter().map(|record| record.as_ref().clone()).collect::<Vec<_>>();
        // @cpt-end:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-03

        // @cpt-begin:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-07
        let query = query::parse_plugin_query(query_string)?;
        // @cpt-end:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-07

        // @cpt-begin:cpt-cf-oagw-algo-plugin-list-query:p1:inst-pqry-08
        // @cpt-begin:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-08
        Ok(query::apply(&records, &query))
        // @cpt-end:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-08
        // @cpt-end:cpt-cf-oagw-algo-plugin-list-query:p1:inst-pqry-08
    }

    /// `GET /oagw/v1/plugins/{id}/source`.
    ///
    /// Returns the stored `source_code` verbatim.
    ///
    /// # Errors
    ///
    /// Returns the mapped `400` of a malformed identifier and the mapped `404`
    /// of an identifier this tenant does not hold, a named builtin plugin
    /// included.
    pub fn plugin_source(&self, actor: &Actor, identifier: &str) -> Result<String, ManagementError> {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-source-read:p1:inst-psrc-03
        let record = resolve_definition(self.store.as_ref(), actor.tenant_id, identifier)?;
        // @cpt-end:cpt-cf-oagw-flow-plugin-source-read:p1:inst-psrc-03

        // @cpt-begin:cpt-cf-oagw-flow-plugin-source-read:p1:inst-psrc-06
        // The source is a storage artifact only: it is returned verbatim, with
        // no syntax check, no interpreter and no sandbox on this path
        // (DECOMPOSITION assumption 4).
        Ok(record.source_code.clone())
        // @cpt-end:cpt-cf-oagw-flow-plugin-source-read:p1:inst-psrc-06
    }

    /// `DELETE /oagw/v1/plugins/{id}`.
    ///
    /// # Errors
    ///
    /// Returns the mapped `404` of an identifier this tenant does not hold and
    /// the mapped `409` of a definition a binding or an `auth` reference still
    /// names.
    pub fn delete_plugin(&self, actor: &Actor, identifier: &str) -> Result<(), ManagementError> {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-03
        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-04
        let record = resolve_definition(self.store.as_ref(), actor.tenant_id, identifier)?;
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-04
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-03

        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-06
        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-07
        // @cpt-begin:cpt-cf-oagw-dod-plugin-in-use-conflict:p1:inst-full
        // The scan and the delete run in the store's one write critical section,
        // so no binding can start referencing the definition in between.
        self.store.delete_plugin(actor.tenant_id, &record.id)?;
        // @cpt-end:cpt-cf-oagw-dod-plugin-in-use-conflict:p1:inst-full
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-07
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-06

        // @cpt-begin:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-psta-04
        // `available` -> `absent` when the row is gone; `in_use` -> `absent`
        // when the scan found no live reference any more.
        // @cpt-end:cpt-cf-oagw-state-plugin-lifecycle:p1:inst-psta-04

        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-13
        self.log_plugin_write("delete", &record, actor, "deleted");
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-13

        Ok(())
    }

    /// The structured log line of a plugin write
    /// (`inst-pcre-12`, `inst-pdel-13`).
    ///
    /// The line names the operation, the plugin type, the identifier, the tenant,
    /// the principal and the outcome, and never any credential material: a
    /// definition carries configuration metadata and script source only.
    fn log_plugin_write(&self, operation: &str, record: &Plugin, actor: &Actor, outcome: &str) {
        info!(
            gear = crate::gear::GEAR_NAME,
            operation,
            plugin_type = record.plugin_type.as_str(),
            plugin_id = %record.id,
            tenant = %record.tenant_id,
            principal = %actor.subject_id,
            outcome,
            "oagw plugin write"
        );
    }
}

/// The part of a validated upstream or route the binding validator reads.
enum ValidatedRecord<'a> {
    /// An upstream: its `plugins.items[]` and its scalar `auth` reference.
    Upstream {
        /// Owning tenant, the key space the references resolve in.
        tenant_id: Uuid,
        /// Declared pipeline, if any.
        plugins: Option<&'a crate::domain::model::PluginsConfig>,
        /// Scalar `auth` reference, if any.
        auth: Option<&'a crate::domain::model::AuthConfig>,
    },
    /// A route: its `plugins.items[]` only, an upstream's `auth` being
    /// upstream-level configuration.
    Route {
        /// Owning tenant, the key space the references resolve in.
        tenant_id: Uuid,
        /// Declared pipeline, if any.
        plugins: Option<&'a crate::domain::model::PluginsConfig>,
    },
}

impl ValidatedRecord<'_> {
    /// The key space the references resolve in.
    const fn tenant_id(&self) -> Uuid {
        match self {
            Self::Upstream { tenant_id, .. } | Self::Route { tenant_id, .. } => *tenant_id,
        }
    }

    /// The declared pipeline.
    const fn plugins(&self) -> Option<&crate::domain::model::PluginsConfig> {
        match self {
            Self::Upstream { plugins, .. } | Self::Route { plugins, .. } => *plugins,
        }
    }

    /// The scalar `auth` reference, which only an upstream carries.
    const fn auth(&self) -> Option<&crate::domain::model::AuthConfig> {
        match self {
            Self::Upstream { auth, .. } => *auth,
            Self::Route { .. } => None,
        }
    }
}

impl<'a> From<&'a Upstream> for ValidatedRecord<'a> {
    fn from(record: &'a Upstream) -> Self {
        Self::Upstream {
            tenant_id: record.tenant_id,
            plugins: record.plugins.as_ref(),
            auth: record.auth.as_ref(),
        }
    }
}

impl<'a> From<&'a Route> for ValidatedRecord<'a> {
    fn from(record: &'a Route) -> Self {
        Self::Route {
            tenant_id: record.tenant_id,
            plugins: record.plugins.as_ref(),
        }
    }
}

/// A route create whose reference does not resolve is a `400` naming the field,
/// not the `404` of a read path: the reference is a body field.
fn unresolved_upstream(error: ManagementError, supplied: Uuid) -> ManagementError {
    match error {
        ManagementError::Domain(crate::domain::DomainError::RouteNotFound { .. }) => {
            ManagementError::validation(format!(
                "upstream_id: `{supplied}` does not resolve in this tenant"
            ))
        }
        other => other,
    }
}
