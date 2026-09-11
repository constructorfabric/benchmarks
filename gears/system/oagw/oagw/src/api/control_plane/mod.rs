//! The management (control-plane) surface of the gateway
//! (`cpt-cf-oagw-flow-upstream-crud`, `cpt-cf-oagw-flow-route-crud`,
//! `cpt-cf-oagw-flow-plugin-crud`).
//!
//! One [`ControlPlaneService`] owns the 15 `/oagw/v1` endpoints of §1.1: it
//! hands every body to the DTO boundary of [`dto`], every payload to the domain
//! validation of `cpt-cf-oagw-flow-resource-validation`, every upstream alias
//! decision to the alias contract, every list request to the OData subset of
//! [`list_query`] and every violation to [`conflict`], which decides the status
//! and the GTS type of the response. Nothing here restates a rule another
//! feature owns: the flows of this module are the sequencing of those
//! delegations and the tenant scope they run in.
//!
//! The axum handlers are thin adapters: they read the caller's tenant from
//! `SecurityContext::subject_tenant_id()` through [`tenant_of`], hand the
//! request to the service and render the [`Reply`]. They are `async` because
//! the `Handler` trait of axum requires a future, not because they await
//! anything.

pub mod conflict;
pub mod dto;
pub mod enabled;
pub mod list_query;

use std::sync::Arc;
use std::sync::OnceLock;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::MethodRouter;
use serde_json::Value;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::error::error_response;
use crate::domain::alias::{
    apply_resolved_alias, enforce_alias_update, resolve_alias_for_upstream,
};
use crate::domain::error::{DomainError, OagwError};
use crate::domain::model::{Endpoint, Plugin, Route, Upstream, split_plugin_reference};
use crate::domain::repo::{PluginRepository, RouteRepository, UpstreamRepository};
use crate::domain::validation::{
    UpstreamExistence, validate_plugin_payload, validate_route_payload, validate_upstream_payload,
};
use crate::infra::observability::Telemetry;
use crate::infra::storage::InMemoryStores;

use conflict::Operation;
use dto::{CrudMethod, Resource};
use enabled::EnabledState;
use list_query::Universe;

/// The wrapped path-identifier prefix of an upstream (§1.1): the API-level form
/// of the bare schema identifier the bodies carry.
pub const UPSTREAM_PATH_IDENTIFIER: &str = "gts.cf.core.oagw.upstream.v1~";

/// The wrapped path-identifier prefix of a route (§1.1).
pub const ROUTE_PATH_IDENTIFIER: &str = "gts.cf.core.oagw.route.v1~";

/// The control plane of the gateway: the component that owns the 15 management
/// endpoints, holding the stores they read and write.
///
/// Cloning the service shares the stores, so a resource one handle wrote is
/// visible to every other handle of the same bundle.
#[derive(Debug, Clone, Default)]
pub struct ControlPlaneService {
    stores: InMemoryStores,
    /// The emission facade the observability feature shares, installed once by
    /// the gear's `init()` before any request is served and read-only from then
    /// on. `None` for a control plane no gear constructed, the records then
    /// being simply not produced.
    telemetry: Arc<OnceLock<Arc<Telemetry>>>,
}

/// The response the service decided for one request, before it is rendered to
/// the wire: the success statuses of §1.1 are exactly these variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// `201 Created` with the stored resource.
    Created(Value),
    /// `200 OK` with a stored or replaced resource.
    Ok(Value),
    /// `200 OK` with the Starlark source of a custom plugin.
    Source(String),
    /// `200 OK` with a list page, a bare JSON array with no envelope.
    Page(Vec<Value>),
    /// `204 No Content` with no body.
    Deleted,
}

impl Reply {
    /// The success status the reply is rendered with.
    #[must_use]
    pub const fn status(&self) -> u16 {
        match self {
            Self::Created(_) => 201,
            Self::Ok(_) | Self::Source(_) | Self::Page(_) => 200,
            Self::Deleted => 204,
        }
    }
}

impl ControlPlaneService {
    /// A control plane over `stores`.
    #[must_use]
    pub fn new(stores: InMemoryStores) -> Self {
        Self {
            stores,
            telemetry: Arc::new(OnceLock::new()),
        }
    }

    /// The stores the service reads and writes through.
    #[must_use]
    pub fn stores(&self) -> &InMemoryStores {
        &self.stores
    }

    /// Installs the emission facade the gear's `init()` constructed, once,
    /// before the shell is mounted and before any request is served.
    pub fn set_telemetry(&self, telemetry: Arc<Telemetry>) {
        let _ = self.telemetry.set(telemetry);
    }

    /// The installed facade, when the gear installed one.
    #[must_use]
    pub fn telemetry(&self) -> Option<Arc<Telemetry>> {
        self.telemetry.get().cloned()
    }

    /// Creates an upstream for `tenant_id` from `body`
    /// (`cpt-cf-oagw-flow-upstream-crud`).
    ///
    /// # Errors
    /// Returns the rendered error of the DTO boundary, of the domain
    /// validation, of the delegated alias contract or of the store write.
    pub fn create_upstream(&self, tenant_id: Uuid, body: &[u8]) -> Result<Reply, OagwError> {
        self.upsert_upstream(tenant_id, None, body)
    }

    /// Replaces the upstream `identifier` addresses for `tenant_id` with
    /// `body` (`cpt-cf-oagw-flow-upstream-crud`).
    ///
    /// # Errors
    /// Returns the rendered error of the path resolution, of the DTO boundary,
    /// of the domain validation, of the delegated alias update table or of the
    /// store write.
    pub fn replace_upstream(
        &self,
        tenant_id: Uuid,
        identifier: &str,
        body: &[u8],
    ) -> Result<Reply, OagwError> {
        self.upsert_upstream(tenant_id, Some(identifier), body)
    }

    /// The create and the replace branch of the upstream flow, which the
    /// FEATURE runs as one `IF create … ELSE IF replace …` chain over the same
    /// DTO boundary, the same domain validation and the same store write.
    ///
    /// # Errors
    /// Returns the rendered error of the path resolution, of the DTO boundary,
    /// of the domain validation, of the delegated alias contract or of the
    /// store write.
    fn upsert_upstream(
        &self,
        tenant_id: Uuid,
        identifier: Option<&str>,
        body: &[u8],
    ) -> Result<Reply, OagwError> {
        // The stored upstream a replacement judges its alias transition
        // against, resolved tenant-scoped: a foreign or ancestor resource is
        // indistinguishable from an absent one, which is exactly what a create
        // never resolves at all.
        let stored = identifier
            .map(|identifier| resolve_upstream(&self.stores, tenant_id, identifier))
            .transpose()?;
        let method = if stored.is_some() {
            CrudMethod::Replace
        } else {
            CrudMethod::Create
        };

        // @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-02
        // The body is parsed into the create or the update DTO: the create
        // field set is the upstream schema field set plus the field-set
        // extension, the update DTO declares no `id` and no `tenant_id` field
        // at all.
        // @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-03
        // CATCH the unknown-field and missing-required violations: 400 with the
        // validation-error GTS type, naming every offending field path, before
        // any domain rule runs and with no immutable-field comparison
        // performed.
        let parsed = dto::parse(Resource::Upstream, method, body)
            .map_err(|error| render(&error, &Operation::Other))?;
        let payload = Value::Object(parsed);
        // @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-03
        // @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-02

        // @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-04
        // The payload is handed to the domain validation, which owns every
        // value rule; this flow restates none of them, and the optional blocks
        // a replace payload omits are absent from the aggregate it builds, so
        // the replacement clears them.
        // @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-05
        // CATCH the collected domain violations and report them together.
        let mut upstream = validate_upstream_payload(&payload, tenant_id)
            .map_err(|error| render(&error, &Operation::Other))?;
        // @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-05
        // @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-04

        // The `enabled` machine of `cpt-cf-oagw-state-resource-enabled`: the
        // create-time entry is the state the body carries, the omitted flag
        // taking the `true` default, while a replace runs the two `PUT`
        // transitions, a write of the value the flag already holds being a
        // no-op.
        match stored.as_ref() {
            None => upstream.enabled = EnabledState::on_create(upstream.enabled).as_bool(),
            Some(stored) => {
                if let Some(state) =
                    EnabledState::on_create(stored.enabled).on_put(upstream.enabled)
                {
                    upstream.enabled = state.as_bool();
                }
            }
        }

        match stored.as_ref() {
            // @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-06
            // IF the request is a create, the alias decision is delegated to
            // the alias contract, which derives, normalizes and checks the
            // `(tenant_id, alias)` uniqueness invariant of the caller's tenant.
            None => {
                let endpoints = pool_of(&upstream);
                // @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-07
                // CATCH the per-tenant alias conflict as 409 through the
                // conflict algorithm, and the alias override and missing-alias
                // rejections as the 400 the alias contract reports them.
                let resolved = resolve_alias_for_upstream(
                    &endpoints,
                    upstream.alias.as_deref(),
                    tenant_id,
                    &self.stores.upstreams(),
                )
                .map_err(|error| render(&error, &Operation::UpstreamWrite { tenant_id }))?;
                // @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-07
                // @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-06
                // The alias binding the contract returns is the state
                // `cpt-cf-oagw-state-alias-binding` records with the stored
                // aggregate; the management surface stores the alias the
                // aggregate carries and leaves the binding itself to the alias
                // contract and its store.
                let _ = apply_resolved_alias(&mut upstream, &resolved);
            }
            // @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-08
            // ELSE IF the request is a replace, the replacement is judged by
            // the alias update transition table, which keeps the stored alias
            // on every allowed row and rejects every row that would alter the
            // routing key.
            Some(stored) => {
                let endpoints = pool_of(&upstream);
                // @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-09
                // CATCH the rejected transition as 400 and a conflict raised by
                // the re-check as 409, through the conflict algorithm.
                let decision = enforce_alias_update(
                    stored,
                    &endpoints,
                    upstream.alias.as_deref(),
                    tenant_id,
                    &self.stores.upstreams(),
                )
                .map_err(|error| render(&error, &Operation::UpstreamWrite { tenant_id }))?;
                // @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-09
                // @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-08
                upstream.alias = decision.alias;
            }
        }

        // @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-10
        // On a create or a replace, the upstream is written under the caller's
        // tenant scope with the server-generated UUID that becomes the stored
        // and returned `id` field — the bare schema identifier, whose wrapped
        // `gts.cf.core.oagw.upstream.v1~{uuid}` form is the API-level path
        // identifier of the same aggregate and not a second field of the body.
        // An ancestor upstream that shares the proposed alias is invisible
        // here and is therefore not a conflict, because the management surface
        // performs no ancestor walk.
        upstream.id = Some(match stored.as_ref() {
            None => Uuid::new_v4(),
            Some(stored) => stored.id.unwrap_or_default(),
        });
        let operation = Operation::UpstreamWrite { tenant_id };
        // @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-11
        // CATCH a duplicate key that reaches the store from a path the alias
        // check did not already render.
        let written = match stored.as_ref() {
            None => self.stores.upstreams().insert(&upstream),
            Some(_) => self.stores.upstreams().replace(&upstream),
        }
        .map_err(|error| render(&error, &operation))?;
        // @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-11
        // @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-10

        match stored.as_ref() {
            // @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-12
            // IF the request was a create,
            // @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-13
            // RETURN 201 with the stored resource, carrying the derived alias
            // and the `enabled` default the payload omitted.
            None => Ok(Reply::Created(serialize(&written))),
            // @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-13
            // @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-12
            // @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-14
            // ELSE IF the request was a replace,
            // @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-15
            // RETURN 200 with the replaced resource, the omitted optional
            // blocks cleared with it.
            Some(_) => Ok(Reply::Ok(serialize(&written))),
            // @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-15
            // @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-14
        }
    }

    /// Reads the upstream `identifier` addresses for `tenant_id`
    /// (`cpt-cf-oagw-flow-upstream-crud`).
    ///
    /// # Errors
    /// Returns the rendered not-found error of a miss.
    pub fn get_upstream(&self, tenant_id: Uuid, identifier: &str) -> Result<Reply, OagwError> {
        let stored = resolve_upstream(&self.stores, tenant_id, identifier)?;
        // @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-18
        // RETURN 200 with the stored resource, whose `id` is the bare UUID.
        Ok(Reply::Ok(serialize(&stored)))
        // @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-18
    }

    /// Deletes the upstream `identifier` addresses for `tenant_id`
    /// (`cpt-cf-oagw-flow-upstream-crud`).
    ///
    /// # Errors
    /// Returns the rendered not-found error of a miss.
    pub fn delete_upstream(&self, tenant_id: Uuid, identifier: &str) -> Result<Reply, OagwError> {
        let stored = resolve_upstream(&self.stores, tenant_id, identifier)?;
        // @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-19
        // The cascade of the in-memory repository removes the same tenant's
        // routes and their plugin bindings with the upstream, under the same
        // lock, and the response is 204 with no body.
        self.stores
            .upstreams()
            .delete(tenant_id, stored.id.unwrap_or_default())
            .map_err(|error| render(&error, &Operation::Other))?;
        Ok(Reply::Deleted)
        // @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-19
    }

    /// Lists the upstreams of `tenant_id` under `query`
    /// (`cpt-cf-oagw-flow-upstream-crud`).
    ///
    /// # Errors
    /// Returns the rendered error of an unsupported query parameter or of the
    /// store read.
    pub fn list_upstreams(&self, tenant_id: Uuid, query: &str) -> Result<Reply, OagwError> {
        let universe = Universe::of(Resource::Upstream);
        let items = self
            .stores
            .upstreams()
            .list(tenant_id)
            .map_err(|error| render(&error, &Operation::Other))?;
        let page = list_page(query, &universe, items.iter().map(serialize).collect())?;
        // @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-20
        // RETURN 200 with the JSON array the OData subset built over the
        // calling tenant's upstreams, filtered on the `alias` filter field of
        // §1.5.
        Ok(Reply::Page(page))
        // @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-20
    }

    /// Creates a route for `tenant_id` from `body`
    /// (`cpt-cf-oagw-flow-route-crud`).
    ///
    /// # Errors
    /// Returns the rendered error of the DTO boundary, of the tenant-scoped
    /// upstream lookup, of the domain validation or of the store write.
    pub fn create_route(&self, tenant_id: Uuid, body: &[u8]) -> Result<Reply, OagwError> {
        self.upsert_route(tenant_id, None, body)
    }

    /// Replaces the route `identifier` addresses for `tenant_id` with `body`
    /// (`cpt-cf-oagw-flow-route-crud`).
    ///
    /// # Errors
    /// Returns the rendered error of the path resolution, of the DTO boundary,
    /// of the tenant-scoped upstream lookup, of the domain validation or of the
    /// store write.
    pub fn replace_route(
        &self,
        tenant_id: Uuid,
        identifier: &str,
        body: &[u8],
    ) -> Result<Reply, OagwError> {
        self.upsert_route(tenant_id, Some(identifier), body)
    }

    /// The create and the replace branch of the route flow, which the FEATURE
    /// runs as one chain over the same DTO boundary, the same upstream lookup,
    /// the same domain validation and the same store write.
    ///
    /// # Errors
    /// Returns the rendered error of the path resolution, of the DTO boundary,
    /// of the tenant-scoped upstream lookup, of the domain validation or of the
    /// store write.
    fn upsert_route(
        &self,
        tenant_id: Uuid,
        identifier: Option<&str>,
        body: &[u8],
    ) -> Result<Reply, OagwError> {
        // The stored route a replacement overwrites, resolved tenant-scoped;
        // its `upstream_id` is what the update DTO leaves immutable.
        let stored = identifier
            .map(|identifier| resolve_route(&self.stores, tenant_id, identifier))
            .transpose()?;
        let method = if stored.is_some() {
            CrudMethod::Replace
        } else {
            CrudMethod::Create
        };

        // @cpt-begin:cpt-cf-oagw-flow-route-crud:p1:inst-rc-02
        // The body is parsed into the create or the update DTO: the create
        // field set is the route schema field set plus the `enabled` and
        // `priority` extension, the update DTO declares no `id` and no
        // `upstream_id` field at all.
        // @cpt-begin:cpt-cf-oagw-flow-route-crud:p1:inst-rc-03
        // CATCH the unknown-field and missing-required violations — including a
        // replace body that supplies `upstream_id` — as 400, so such a body
        // never reaches the domain layer.
        let parsed = dto::parse(Resource::Route, method, body)
            .map_err(|error| render(&error, &Operation::Other))?;
        let mut payload = Value::Object(parsed);
        // @cpt-end:cpt-cf-oagw-flow-route-crud:p1:inst-rc-03
        // @cpt-end:cpt-cf-oagw-flow-route-crud:p1:inst-rc-02

        // `upstream_id` is immutable by its absence from the update DTO, so the
        // stored reference is what the payload the domain validates carries; a
        // create body carries the reference the caller supplied.
        if let Some(stored) = stored.as_ref() {
            let Some(upstream_id) = stored.upstream_id else {
                return Err(render(
                    &DomainError::not_found(
                        "upstream_id",
                        "the stored route carries no upstream reference",
                    ),
                    &Operation::Other,
                ));
            };
            payload["upstream_id"] = Value::String(upstream_id.to_string());
        }

        // @cpt-begin:cpt-cf-oagw-flow-route-crud:p1:inst-rc-06
        // IF the request carries an upstream_id, the referenced upstream is
        // looked up through the tenant-scoped `UpstreamRepository`, so only an
        // upstream of the calling tenant is addressable. An identifier that is
        // not a UUID is left to the domain layer, which owns the malformed
        // reference and rejects it 400.
        // @cpt-begin:cpt-cf-oagw-flow-route-crud:p1:inst-rc-07
        // CATCH a lookup that misses — the upstream absent, foreign or
        // ancestor-owned — as 404 with the not-found GTS type, per the
        // unresolvable-upstream_id conformance entry of §1.5.
        let referenced = referenced_upstream(&self.stores, tenant_id, &payload)?;
        // @cpt-end:cpt-cf-oagw-flow-route-crud:p1:inst-rc-07
        // @cpt-end:cpt-cf-oagw-flow-route-crud:p1:inst-rc-06

        // @cpt-begin:cpt-cf-oagw-flow-route-crud:p1:inst-rc-04
        // The payload is handed to the domain validation, which owns the
        // `match` block rules and every other value rule.
        // @cpt-begin:cpt-cf-oagw-flow-route-crud:p1:inst-rc-05
        // CATCH the collected domain violations and report them together.
        let mut route =
            validate_route_payload(&payload, tenant_id, &ReferencedUpstream(referenced))
                .map_err(|error| render(&error, &Operation::Other))?;
        // @cpt-end:cpt-cf-oagw-flow-route-crud:p1:inst-rc-05
        // @cpt-end:cpt-cf-oagw-flow-route-crud:p1:inst-rc-04

        // The `enabled` machine of `cpt-cf-oagw-state-resource-enabled`: the
        // create-time entry is the state the body carries, a replace running
        // the two `PUT` transitions.
        match stored.as_ref() {
            None => route.enabled = EnabledState::on_create(route.enabled).as_bool(),
            Some(stored) => {
                if let Some(state) = EnabledState::on_create(stored.enabled).on_put(route.enabled) {
                    route.enabled = state.as_bool();
                }
            }
        }

        // @cpt-begin:cpt-cf-oagw-flow-route-crud:p1:inst-rc-08
        // On a create or a replace, the route is written under the caller's
        // tenant scope with the server-generated UUID of a create or the
        // identifier the request addressed, the store enforcing the route-match
        // determinism invariant of `cpt-cf-oagw-db-schema` on the write.
        route.id = Some(match stored.as_ref() {
            None => Uuid::new_v4(),
            Some(stored) => stored.id.unwrap_or_default(),
        });
        let operation = Operation::RouteWrite {
            match_key: render_match_key(&route),
        };
        // @cpt-begin:cpt-cf-oagw-flow-route-crud:p1:inst-rc-09
        // CATCH the already-exists violation for a second route with the same
        // `path`, `priority` and `method` under the same upstream as 409.
        let written = match stored.as_ref() {
            None => self.stores.routes().insert(&route),
            Some(_) => self.stores.routes().replace(&route),
        }
        .map_err(|error| render(&error, &operation))?;
        // @cpt-end:cpt-cf-oagw-flow-route-crud:p1:inst-rc-09
        // @cpt-end:cpt-cf-oagw-flow-route-crud:p1:inst-rc-08

        match stored.as_ref() {
            // @cpt-begin:cpt-cf-oagw-flow-route-crud:p1:inst-rc-10
            // IF the request was a create,
            // @cpt-begin:cpt-cf-oagw-flow-route-crud:p1:inst-rc-11
            // RETURN 201 with the stored route, carrying the `enabled` default
            // the payload omitted.
            None => Ok(Reply::Created(serialize(&written))),
            // @cpt-end:cpt-cf-oagw-flow-route-crud:p1:inst-rc-11
            // @cpt-end:cpt-cf-oagw-flow-route-crud:p1:inst-rc-10
            // @cpt-begin:cpt-cf-oagw-flow-route-crud:p1:inst-rc-12
            // ELSE IF the request was a replace,
            // @cpt-begin:cpt-cf-oagw-flow-route-crud:p1:inst-rc-13
            // RETURN 200 with the replaced route, the omitted optional fields
            // cleared with it and `upstream_id` unchanged.
            Some(_) => Ok(Reply::Ok(serialize(&written))),
            // @cpt-end:cpt-cf-oagw-flow-route-crud:p1:inst-rc-13
            // @cpt-end:cpt-cf-oagw-flow-route-crud:p1:inst-rc-12
        }
    }

    /// Reads the route `identifier` addresses for `tenant_id`
    /// (`cpt-cf-oagw-flow-route-crud`).
    ///
    /// # Errors
    /// Returns the rendered not-found error of a miss.
    pub fn get_route(&self, tenant_id: Uuid, identifier: &str) -> Result<Reply, OagwError> {
        let stored = resolve_route(&self.stores, tenant_id, identifier)?;
        // @cpt-begin:cpt-cf-oagw-flow-route-crud:p1:inst-rc-16
        // RETURN 200 with the stored route.
        Ok(Reply::Ok(serialize(&stored)))
        // @cpt-end:cpt-cf-oagw-flow-route-crud:p1:inst-rc-16
    }

    /// Deletes the route `identifier` addresses for `tenant_id`
    /// (`cpt-cf-oagw-flow-route-crud`).
    ///
    /// # Errors
    /// Returns the rendered not-found error of a miss.
    pub fn delete_route(&self, tenant_id: Uuid, identifier: &str) -> Result<Reply, OagwError> {
        let stored = resolve_route(&self.stores, tenant_id, identifier)?;
        // @cpt-begin:cpt-cf-oagw-flow-route-crud:p1:inst-rc-17
        // The plugin bindings of the route go with the route itself, and the
        // response is 204 with no body.
        self.stores
            .routes()
            .delete(tenant_id, stored.id.unwrap_or_default())
            .map_err(|error| render(&error, &Operation::Other))?;
        Ok(Reply::Deleted)
        // @cpt-end:cpt-cf-oagw-flow-route-crud:p1:inst-rc-17
    }

    /// Lists the routes of `tenant_id` under `query`
    /// (`cpt-cf-oagw-flow-route-crud`).
    ///
    /// # Errors
    /// Returns the rendered error of an unsupported query parameter or of the
    /// store read.
    pub fn list_routes(&self, tenant_id: Uuid, query: &str) -> Result<Reply, OagwError> {
        let universe = Universe::of(Resource::Route);
        let items = self
            .stores
            .routes()
            .list(tenant_id)
            .map_err(|error| render(&error, &Operation::Other))?;
        let page = list_page(query, &universe, items.iter().map(serialize).collect())?;
        // @cpt-begin:cpt-cf-oagw-flow-route-crud:p1:inst-rc-18
        // RETURN 200 with the JSON array the OData subset built over the
        // calling tenant's routes, filtered on the `upstream_id` filter field
        // of §1.5.
        Ok(Reply::Page(page))
        // @cpt-end:cpt-cf-oagw-flow-route-crud:p1:inst-rc-18
    }

    /// Creates a custom plugin for `tenant_id` from `body`
    /// (`cpt-cf-oagw-flow-plugin-crud`).
    ///
    /// # Errors
    /// Returns the rendered error of the DTO boundary, of the plugin shape
    /// rules or of the store write.
    pub fn create_plugin(&self, tenant_id: Uuid, body: &[u8]) -> Result<Reply, OagwError> {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-02
        // The body is parsed into the plugin DTO and handed to the plugin rules
        // of the shape validation, which own `plugin_type`, `name` and the
        // Starlark shape.
        // @cpt-begin:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-03
        // CATCH the unknown-field, missing-required and plugin-shape violations
        // and report all violated rules together, as 400.
        let parsed = dto::parse(Resource::Plugin, CrudMethod::Create, body)
            .map_err(|error| render(&error, &Operation::Other))?;
        let mut plugin = validate_plugin_payload(&Value::Object(parsed), tenant_id)
            .map_err(|error| render(&error, &Operation::Other))?;
        // @cpt-end:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-03
        // @cpt-end:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-02

        // @cpt-begin:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-04
        // The plugin is stored under the `(tenant_id, name)` uniqueness
        // invariant of `cpt-cf-oagw-db-schema`, with the server-generated UUID
        // the wrapped path identifier addresses, and 201 is returned with the
        // stored resource.
        plugin.id = Some(Uuid::new_v4());
        let stored = self
            .stores
            .plugins()
            .insert(&plugin)
            .map_err(|error| render(&error, &Operation::Other))?;
        Ok(Reply::Created(serialize(&stored)))
        // @cpt-end:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-04
    }

    /// Deletes the plugin `identifier` addresses for `tenant_id`
    /// (`cpt-cf-oagw-flow-plugin-crud`).
    ///
    /// # Errors
    /// Returns the rendered not-found error of a miss, or the `PluginInUse`
    /// conflict of a plugin an upstream or a route still references.
    pub fn delete_plugin(&self, tenant_id: Uuid, identifier: &str) -> Result<Reply, OagwError> {
        // The identifier is resolved the same way a get resolves it, so an
        // identifier that is not UUID-backed is a miss before any scan runs.
        let id = parse_path_identifier(Resource::Plugin, identifier)
            .ok_or_else(|| identifier_not_found(identifier, "stored plugin"))?;
        // @cpt-begin:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-08
        // ELSE IF the request is a delete, the identifier is resolved the same
        // way and the stored upstreams and routes of the calling tenant are
        // scanned for bindings that still reference it, per the plugin
        // identification model.
        match self.stores.delete_plugin_unreferenced(tenant_id, id) {
            // @cpt-begin:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-10
            // ELSE the reference scan of inst-pc-08 and the delete ran as one
            // atomic store operation, so a binding created after the scan began
            // cannot survive the delete, and the response is 204.
            Ok(()) => Ok(Reply::Deleted),
            // @cpt-end:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-10
            Err(references) => {
                // An empty scan means the plugin is absent from the tenant; a
                // non-empty one means a binding still holds it.
                let operation = if references.is_empty() {
                    Operation::Other
                } else {
                    // @cpt-begin:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-09
                    // IF a binding still references the plugin, the conflict is
                    // rendered through the conflict algorithm, whose
                    // plugin-delete branch selects the existing `PluginInUse`
                    // row with its `plugin_id` field and its `referenced_by`
                    // object; the stored plugin is left untouched.
                    Operation::PluginDelete {
                        plugin_id: identifier.to_owned(),
                        referenced_by: references,
                    }
                    // @cpt-end:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-09
                };
                Err(render(
                    &DomainError::not_found(
                        "id",
                        "no plugin with this identifier exists in the caller's tenant",
                    ),
                    &operation,
                ))
            }
        }
        // @cpt-end:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-08
    }

    /// Reads the plugin `identifier` addresses for `tenant_id`
    /// (`cpt-cf-oagw-flow-plugin-crud`).
    ///
    /// # Errors
    /// Returns the rendered not-found error of a miss.
    pub fn get_plugin(&self, tenant_id: Uuid, identifier: &str) -> Result<Reply, OagwError> {
        let stored = stored_plugin(&self.stores, tenant_id, identifier)?;
        Ok(Reply::Ok(serialize(&stored)))
    }

    /// Reads the Starlark source of the plugin `identifier` addresses
    /// (`cpt-cf-oagw-flow-plugin-crud`).
    ///
    /// # Errors
    /// Returns the rendered not-found error of a miss.
    pub fn plugin_source(&self, tenant_id: Uuid, identifier: &str) -> Result<Reply, OagwError> {
        let stored = stored_plugin(&self.stores, tenant_id, identifier)?;
        // @cpt-begin:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-06
        // IF the identifier is UUID-backed and the stored plugin exists in the
        // calling tenant, the source endpoint returns 200 with the stored
        // Starlark `source_code`, exactly as a get of the same identifier
        // returns 200 with the stored plugin resource.
        Ok(Reply::Source(stored.source_code.unwrap_or_default()))
        // @cpt-end:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-06
    }

    /// Lists the custom plugins of `tenant_id` under `query`
    /// (`cpt-cf-oagw-flow-plugin-crud`).
    ///
    /// # Errors
    /// Returns the rendered error of an unsupported query parameter or of the
    /// store read.
    pub fn list_plugins(&self, tenant_id: Uuid, query: &str) -> Result<Reply, OagwError> {
        let universe = Universe::of(Resource::Plugin);
        let items = self
            .stores
            .plugins()
            .list(tenant_id)
            .map_err(|error| render(&error, &Operation::Other))?;
        let page = list_page(query, &universe, items.iter().map(serialize).collect())?;
        // @cpt-begin:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-11
        // RETURN 200 with the JSON array the OData subset built over the
        // calling tenant's custom plugins, filtered on the DESIGN-named `type`
        // filter field, which addresses the plugin aggregate's `plugin_type`
        // field (§1.5).
        Ok(Reply::Page(page))
        // @cpt-end:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-11
    }
}

// @cpt-begin:cpt-cf-oagw-dod-odata-list:p1:inst-full

/// Parses the query string and applies it to the resources the calling flow
/// read through the tenant-scoped repository, honouring the five parameters of
/// §1.5 and returning the page with no envelope.
///
/// # Errors
/// Returns the rendered error of a malformed or unsupported query parameter.
fn list_page(query: &str, universe: &Universe, items: Vec<Value>) -> Result<Vec<Value>, OagwError> {
    // The items arrive read through the tenant-scoped repository, so the result
    // set the parameters are applied to never holds another tenant's resource —
    // the precondition `inst-lq-08` of `cpt-cf-oagw-algo-list-query` states,
    // whose marker the algorithm module of [`list_query`] carries.
    let parsed =
        list_query::parse(query, universe).map_err(|error| render(&error, &Operation::Other))?;
    Ok(list_query::apply(&parsed, universe, items))
}

// @cpt-end:cpt-cf-oagw-dod-odata-list:p1:inst-full

/// The stored aggregate as the JSON object the response body carries.
fn serialize<T: serde::Serialize>(aggregate: &T) -> Value {
    // The aggregates are plain JSON structs, so the serialization is total.
    serde_json::to_value(aggregate).unwrap_or_default()
}

/// The endpoint pool of an upstream, the input of the alias contract.
fn pool_of(upstream: &Upstream) -> Vec<Endpoint> {
    upstream
        .server
        .as_ref()
        .map(|server| server.endpoints.clone())
        .unwrap_or_default()
}

/// The `(path, priority, method)` match key a route-match conflict names.
fn render_match_key(route: &Route) -> String {
    let Some(match_config) = route.match_config.as_ref() else {
        return format!("priority {}", route.priority);
    };
    if let Some(http) = match_config.http.as_ref() {
        let method = http.methods.first().map(String::as_str).unwrap_or_default();
        return format!(
            "path '{}', priority {}, method '{}'",
            http.path.as_deref().unwrap_or_default(),
            route.priority,
            method
        );
    }
    if let Some(grpc) = match_config.grpc.as_ref() {
        return format!(
            "service '{}', priority {}, method '{}'",
            grpc.service.as_deref().unwrap_or_default(),
            route.priority,
            grpc.method.as_deref().unwrap_or_default()
        );
    }
    format!("priority {}", route.priority)
}

/// Renders one violation through `cpt-cf-oagw-algo-conflict-status`, which
/// decides the status and the GTS type of the response (`inst-cs-01`).
fn render(error: &DomainError, operation: &Operation) -> OagwError {
    conflict::resolve(error, operation)
}

// @cpt-begin:cpt-cf-oagw-dod-tenant-scoping:p1:inst-full

/// The caller's tenant, read from `SecurityContext::subject_tenant_id()`
/// (`inst-uc-01`, `inst-rc-01`, `inst-pc-01`).
///
/// The platform authz middleware has already accepted the caller when a handler
/// runs, and that tenant is the only scope every read and write of the 15
/// endpoints is resolved in.
fn tenant_of(context: Option<Extension<SecurityContext>>) -> Option<Uuid> {
    context.map(|Extension(context)| context.subject_tenant_id())
}

/// The authenticated subject a security context carries, when the platform
/// auth middleware established one: the same context the tenant of the write is
/// read from, never a header the caller controls.
fn principal_of(context: Option<&Extension<SecurityContext>>) -> Option<String> {
    context.map(|extension| extension.0.subject_id().to_string())
}

/// The `AuthenticationFailed` answer of a request that reached a handler
/// without a security context: the existing row, and no new one.
fn unauthenticated() -> OagwError {
    OagwError::authentication_failed(
        "oagw.control_plane: the management surface answers only an authenticated caller \
         carrying a tenant",
    )
}

/// The 404 a management request gets for an identifier no resource of the
/// caller's tenant answers to.
fn identifier_not_found(identifier: &str, kind: &str) -> OagwError {
    OagwError::route_not_found(format!(
        "oagw.routes: '{identifier}' does not address a {kind} of the caller's tenant"
    ))
}

/// Parses the `{id}` path segment of a management request.
///
/// The two identifier forms §1.1 declares have a single referent: the bare
/// server-generated UUID the bodies carry, and the wrapped
/// `gts.cf.core.oagw.{type}.v1~{uuid}` form the path parameter carries. A
/// plugin identifier is a member of the `gts.cf.core.oagw.{type}_plugin.v1~`
/// family, whose instance part is the bare UUID; a named instance is a built-in
/// plugin, which is not persisted and is therefore unresolvable here.
#[must_use]
pub fn parse_path_identifier(resource: Resource, identifier: &str) -> Option<Uuid> {
    if let Ok(uuid) = Uuid::parse_str(identifier) {
        return Some(uuid);
    }
    let instance = match resource {
        Resource::Upstream => identifier.strip_prefix(UPSTREAM_PATH_IDENTIFIER)?,
        Resource::Route => identifier.strip_prefix(ROUTE_PATH_IDENTIFIER)?,
        Resource::Plugin => split_plugin_reference(identifier)?.1,
    };
    Uuid::parse_str(instance).ok()
}

/// The upstream a route payload refers to, as the referential hook of the
/// domain validation sees it: present with its protocol, or absent.
///
/// The flow has already resolved the reference in the caller's tenant scope,
/// which is what makes the hook the answer that lookup produced rather than a
/// second lookup.
#[derive(Debug, Clone)]
struct ReferencedUpstream(Option<Upstream>);

impl UpstreamExistence for ReferencedUpstream {
    fn upstream_exists(&self, _tenant_id: Uuid, upstream_id: Uuid) -> bool {
        self.0
            .as_ref()
            .is_some_and(|upstream| upstream.id == Some(upstream_id))
    }

    fn upstream_protocol(&self, _tenant_id: Uuid, upstream_id: Uuid) -> Option<String> {
        let upstream = self
            .0
            .as_ref()
            .filter(|upstream| upstream.id == Some(upstream_id))?;
        upstream.protocol.clone()
    }
}

// @cpt-end:cpt-cf-oagw-dod-tenant-scoping:p1:inst-full

// @cpt-begin:cpt-cf-oagw-dod-endpoint-registration:p1:inst-full

/// The `MethodRouter` of one management shell path, answering exactly the
/// methods the closed shell declares for it with the handlers of this module.
///
/// # Errors
/// Returns the typed startup error surface for a `(path, method)` pair the
/// closed shell does not name, so a method that is not registered — `PUT
/// /oagw/v1/plugins/{id}` included — is left to the routing layer, which
/// answers it 405.
pub fn control_plane_methods(
    path: &str,
    methods: &[&str],
) -> Result<MethodRouter<Arc<ControlPlaneService>>, OagwError> {
    let mut router = MethodRouter::new();
    for method in methods {
        router = match (path, *method) {
            ("/upstreams", "POST") => router.post(create_upstream),
            ("/upstreams", "GET") => router.get(list_upstreams),
            ("/upstreams/{id}", "GET") => router.get(get_upstream),
            ("/upstreams/{id}", "PUT") => router.put(replace_upstream),
            ("/upstreams/{id}", "DELETE") => router.delete(delete_upstream),
            ("/routes", "POST") => router.post(create_route),
            ("/routes", "GET") => router.get(list_routes),
            ("/routes/{id}", "GET") => router.get(get_route),
            ("/routes/{id}", "PUT") => router.put(replace_route),
            ("/routes/{id}", "DELETE") => router.delete(delete_route),
            ("/plugins", "POST") => router.post(create_plugin),
            ("/plugins", "GET") => router.get(list_plugins),
            ("/plugins/{id}", "GET") => router.get(get_plugin),
            ("/plugins/{id}", "DELETE") => router.delete(delete_plugin),
            ("/plugins/{id}/source", "GET") => router.get(plugin_source),
            other => {
                return Err(OagwError::route_error(format!(
                    "oagw.routes: {other:?} has no control-plane handler in the closed \
                     management shell"
                )));
            }
        };
    }
    Ok(router)
}

/// Renders the reply of a service operation, mapping every rendered error onto
/// the problem+json response the error contract produces.
fn render_reply(reply: Result<Reply, OagwError>) -> Response {
    // @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-21
    // RETURN the response of the matched branch, with every gateway error
    // rendered as application/problem+json carrying X-OAGW-Error-Source:
    // gateway through `cpt-cf-oagw-algo-error-mapping`.
    // @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-21
    // @cpt-begin:cpt-cf-oagw-flow-route-crud:p1:inst-rc-19
    // RETURN the response with the same error rendering.
    // @cpt-end:cpt-cf-oagw-flow-route-crud:p1:inst-rc-19
    // @cpt-begin:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-12
    // RETURN the response with the same error rendering, noting that a
    // `PUT /oagw/v1/plugins/{id}` never reaches this flow, because no such
    // method is registered and the routing layer answers 405.
    // @cpt-end:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-12
    match reply {
        Ok(reply) => render_success(reply),
        Err(error) => error_response(&error),
    }
}

// @cpt-begin:cpt-cf-oagw-dod-audit-event-coverage:p1:inst-full
/// Renders the reply of one mutating management operation and records the
/// configuration-change event it produced.
///
/// Exactly one record is written, and only when the operation succeeded: a
/// rejected write changed no resource and is no configuration change. The
/// record carries the 14 base fields the audit record of the observability
/// feature fixes, the `host` field naming no upstream alias (the resource the
/// operation changed is named by its `path` instead), `error_type` unset and
/// no instrument being incremented by it. The response the caller receives is
/// exactly the one [`render_reply`] would have rendered.
fn render_reply_with_audit(
    service: &ControlPlaneService,
    reply: Result<Reply, OagwError>,
    change: ManagementChange,
) -> Response {
    let Some(telemetry) = service.telemetry() else {
        return render_reply(reply);
    };
    // inst-ob-18: the event is a management configuration change — a create, a
    // replace or a delete of an upstream, route or plugin resource, an enable or
    // a disable of one of them being a replace — and the endpoint,
    // plugin-binding and CORS material that decision carries is nested in the
    // resource the decision is on and is not a surface of its own, so no field
    // of the record names it.
    // @cpt-begin:cpt-cf-oagw-flow-audit-record:p1:inst-ob-18
    if let Ok(ref reply) = reply {
        let status = reply.status();
        // @cpt-begin:cpt-cf-oagw-flow-audit-record:p1:inst-ob-19
        telemetry.audit(
            crate::domain::observability::AuditEventClass::ManagementChange {
                operation: change.operation,
                resource: change.resource,
            },
            crate::domain::observability::AuditFacts {
                request_id: change.request_id,
                tenant_id: Some(change.tenant_id.to_string()),
                principal_id: change.principal_id,
                host: String::new(),
                path: crate::domain::observability::audit_management_path(change.path),
                method: change.method.to_owned(),
                status,
                duration_ms: 0,
                request_size: change.body_len,
                response_size: 0,
                error: None,
            },
        );
        // @cpt-end:cpt-cf-oagw-flow-audit-record:p1:inst-ob-19
    }
    // @cpt-end:cpt-cf-oagw-flow-audit-record:p1:inst-ob-18
    render_reply(reply)
}

/// The identifying fields one mutating management operation carries to the
/// configuration-change record it produces.
struct ManagementChange<'a> {
    /// The operation the flow performed.
    operation: &'static str,
    /// The resource kind the operation changed.
    resource: &'static str,
    /// The HTTP method the handler answered.
    method: &'static str,
    /// The request path, taken as the gear-relative path the record carries.
    path: &'a str,
    /// The tenant the write was scoped to.
    tenant_id: Uuid,
    /// The subject identifier the security context carried.
    principal_id: Option<String>,
    /// The platform trace context the request arrived with.
    request_id: Option<String>,
    /// The size of the received body.
    body_len: u64,
}
// @cpt-end:cpt-cf-oagw-dod-audit-event-coverage:p1:inst-full

/// Renders one successful reply with the success status of §1.1.
fn render_success(reply: Reply) -> Response {
    let status = StatusCode::from_u16(reply.status()).unwrap_or(StatusCode::OK);
    match reply {
        Reply::Deleted => StatusCode::NO_CONTENT.into_response(),
        Reply::Source(source) => {
            let mut response = Response::new(axum::body::Body::from(source));
            *response.status_mut() = status;
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            );
            response
        }
        Reply::Created(value) | Reply::Ok(value) => json_response(status, &value),
        Reply::Page(items) => json_response(status, &Value::Array(items)),
    }
}

/// A JSON response body of the media type the list pages and the resources use.
fn json_response(status: StatusCode, body: &Value) -> Response {
    let bytes = serde_json::to_vec(body).unwrap_or_default();
    let mut response = Response::new(axum::body::Body::from(bytes));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

// @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-01

/// The caller of `POST /oagw/v1/upstreams`
/// (`cpt-cf-oagw-flow-upstream-crud`).
#[allow(clippy::unused_async)]
async fn create_upstream(
    State(service): State<Arc<ControlPlaneService>>,
    context: Option<Extension<SecurityContext>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let principal_id = principal_of(context.as_ref());
    let Some(tenant_id) = tenant_of(context) else {
        return error_response(&unauthenticated());
    };
    render_reply_with_audit(
        &service,
        service.create_upstream(tenant_id, &body),
        ManagementChange {
            operation: "create",
            resource: "upstream",
            method: "POST",
            path: uri.path(),
            tenant_id,
            principal_id,
            request_id: toolkit::api::extract_trace_id(&headers),
            body_len: u64::try_from(body.len()).unwrap_or(u64::MAX),
        },
    )
}

/// The caller of `GET /oagw/v1/upstreams`
/// (`cpt-cf-oagw-flow-upstream-crud`).
#[allow(clippy::unused_async)]
async fn list_upstreams(
    State(service): State<Arc<ControlPlaneService>>,
    context: Option<Extension<SecurityContext>>,
    uri: axum::http::Uri,
) -> Response {
    let Some(tenant_id) = tenant_of(context) else {
        return error_response(&unauthenticated());
    };
    render_reply(service.list_upstreams(tenant_id, uri.query().unwrap_or_default()))
}

/// The caller of `GET /oagw/v1/upstreams/{id}`
/// (`cpt-cf-oagw-flow-upstream-crud`).
#[allow(clippy::unused_async)]
async fn get_upstream(
    State(service): State<Arc<ControlPlaneService>>,
    context: Option<Extension<SecurityContext>>,
    axum::extract::Path(identifier): axum::extract::Path<String>,
) -> Response {
    let Some(tenant_id) = tenant_of(context) else {
        return error_response(&unauthenticated());
    };
    render_reply(service.get_upstream(tenant_id, &identifier))
}

/// The caller of `PUT /oagw/v1/upstreams/{id}`
/// (`cpt-cf-oagw-flow-upstream-crud`).
#[allow(clippy::unused_async)]
async fn replace_upstream(
    State(service): State<Arc<ControlPlaneService>>,
    context: Option<Extension<SecurityContext>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    axum::extract::Path(identifier): axum::extract::Path<String>,
    body: Bytes,
) -> Response {
    let principal_id = principal_of(context.as_ref());
    let Some(tenant_id) = tenant_of(context) else {
        return error_response(&unauthenticated());
    };
    render_reply_with_audit(
        &service,
        service.replace_upstream(tenant_id, &identifier, &body),
        ManagementChange {
            operation: "replace",
            resource: "upstream",
            method: "PUT",
            path: uri.path(),
            tenant_id,
            principal_id,
            request_id: toolkit::api::extract_trace_id(&headers),
            body_len: u64::try_from(body.len()).unwrap_or(u64::MAX),
        },
    )
}

/// The caller of `DELETE /oagw/v1/upstreams/{id}`
/// (`cpt-cf-oagw-flow-upstream-crud`).
#[allow(clippy::unused_async)]
async fn delete_upstream(
    State(service): State<Arc<ControlPlaneService>>,
    context: Option<Extension<SecurityContext>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    axum::extract::Path(identifier): axum::extract::Path<String>,
) -> Response {
    let principal_id = principal_of(context.as_ref());
    let Some(tenant_id) = tenant_of(context) else {
        return error_response(&unauthenticated());
    };
    render_reply_with_audit(
        &service,
        service.delete_upstream(tenant_id, &identifier),
        ManagementChange {
            operation: "delete",
            resource: "upstream",
            method: "DELETE",
            path: uri.path(),
            tenant_id,
            principal_id,
            request_id: toolkit::api::extract_trace_id(&headers),
            body_len: 0,
        },
    )
}

// @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-01

// @cpt-begin:cpt-cf-oagw-flow-route-crud:p1:inst-rc-01

/// The caller of `POST /oagw/v1/routes` (`cpt-cf-oagw-flow-route-crud`).
#[allow(clippy::unused_async)]
async fn create_route(
    State(service): State<Arc<ControlPlaneService>>,
    context: Option<Extension<SecurityContext>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let principal_id = principal_of(context.as_ref());
    let Some(tenant_id) = tenant_of(context) else {
        return error_response(&unauthenticated());
    };
    render_reply_with_audit(
        &service,
        service.create_route(tenant_id, &body),
        ManagementChange {
            operation: "create",
            resource: "route",
            method: "POST",
            path: uri.path(),
            tenant_id,
            principal_id,
            request_id: toolkit::api::extract_trace_id(&headers),
            body_len: u64::try_from(body.len()).unwrap_or(u64::MAX),
        },
    )
}

/// The caller of `GET /oagw/v1/routes` (`cpt-cf-oagw-flow-route-crud`).
#[allow(clippy::unused_async)]
async fn list_routes(
    State(service): State<Arc<ControlPlaneService>>,
    context: Option<Extension<SecurityContext>>,
    uri: axum::http::Uri,
) -> Response {
    let Some(tenant_id) = tenant_of(context) else {
        return error_response(&unauthenticated());
    };
    render_reply(service.list_routes(tenant_id, uri.query().unwrap_or_default()))
}

/// The caller of `GET /oagw/v1/routes/{id}` (`cpt-cf-oagw-flow-route-crud`).
#[allow(clippy::unused_async)]
async fn get_route(
    State(service): State<Arc<ControlPlaneService>>,
    context: Option<Extension<SecurityContext>>,
    axum::extract::Path(identifier): axum::extract::Path<String>,
) -> Response {
    let Some(tenant_id) = tenant_of(context) else {
        return error_response(&unauthenticated());
    };
    render_reply(service.get_route(tenant_id, &identifier))
}

/// The caller of `PUT /oagw/v1/routes/{id}` (`cpt-cf-oagw-flow-route-crud`).
#[allow(clippy::unused_async)]
async fn replace_route(
    State(service): State<Arc<ControlPlaneService>>,
    context: Option<Extension<SecurityContext>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    axum::extract::Path(identifier): axum::extract::Path<String>,
    body: Bytes,
) -> Response {
    let principal_id = principal_of(context.as_ref());
    let Some(tenant_id) = tenant_of(context) else {
        return error_response(&unauthenticated());
    };
    render_reply_with_audit(
        &service,
        service.replace_route(tenant_id, &identifier, &body),
        ManagementChange {
            operation: "replace",
            resource: "route",
            method: "PUT",
            path: uri.path(),
            tenant_id,
            principal_id,
            request_id: toolkit::api::extract_trace_id(&headers),
            body_len: u64::try_from(body.len()).unwrap_or(u64::MAX),
        },
    )
}

/// The caller of `DELETE /oagw/v1/routes/{id}`
/// (`cpt-cf-oagw-flow-route-crud`).
#[allow(clippy::unused_async)]
async fn delete_route(
    State(service): State<Arc<ControlPlaneService>>,
    context: Option<Extension<SecurityContext>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    axum::extract::Path(identifier): axum::extract::Path<String>,
) -> Response {
    let principal_id = principal_of(context.as_ref());
    let Some(tenant_id) = tenant_of(context) else {
        return error_response(&unauthenticated());
    };
    render_reply_with_audit(
        &service,
        service.delete_route(tenant_id, &identifier),
        ManagementChange {
            operation: "delete",
            resource: "route",
            method: "DELETE",
            path: uri.path(),
            tenant_id,
            principal_id,
            request_id: toolkit::api::extract_trace_id(&headers),
            body_len: 0,
        },
    )
}

// @cpt-end:cpt-cf-oagw-flow-route-crud:p1:inst-rc-01

// @cpt-begin:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-01

/// The caller of `POST /oagw/v1/plugins` (`cpt-cf-oagw-flow-plugin-crud`).
#[allow(clippy::unused_async)]
async fn create_plugin(
    State(service): State<Arc<ControlPlaneService>>,
    context: Option<Extension<SecurityContext>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let principal_id = principal_of(context.as_ref());
    let Some(tenant_id) = tenant_of(context) else {
        return error_response(&unauthenticated());
    };
    render_reply_with_audit(
        &service,
        service.create_plugin(tenant_id, &body),
        ManagementChange {
            operation: "create",
            resource: "plugin",
            method: "POST",
            path: uri.path(),
            tenant_id,
            principal_id,
            request_id: toolkit::api::extract_trace_id(&headers),
            body_len: u64::try_from(body.len()).unwrap_or(u64::MAX),
        },
    )
}

/// The caller of `GET /oagw/v1/plugins` (`cpt-cf-oagw-flow-plugin-crud`).
#[allow(clippy::unused_async)]
async fn list_plugins(
    State(service): State<Arc<ControlPlaneService>>,
    context: Option<Extension<SecurityContext>>,
    uri: axum::http::Uri,
) -> Response {
    let Some(tenant_id) = tenant_of(context) else {
        return error_response(&unauthenticated());
    };
    render_reply(service.list_plugins(tenant_id, uri.query().unwrap_or_default()))
}

/// The caller of `GET /oagw/v1/plugins/{id}`
/// (`cpt-cf-oagw-flow-plugin-crud`).
#[allow(clippy::unused_async)]
async fn get_plugin(
    State(service): State<Arc<ControlPlaneService>>,
    context: Option<Extension<SecurityContext>>,
    axum::extract::Path(identifier): axum::extract::Path<String>,
) -> Response {
    let Some(tenant_id) = tenant_of(context) else {
        return error_response(&unauthenticated());
    };
    render_reply(service.get_plugin(tenant_id, &identifier))
}

/// The caller of `DELETE /oagw/v1/plugins/{id}`
/// (`cpt-cf-oagw-flow-plugin-crud`).
#[allow(clippy::unused_async)]
async fn delete_plugin(
    State(service): State<Arc<ControlPlaneService>>,
    context: Option<Extension<SecurityContext>>,
    headers: axum::http::HeaderMap,
    uri: axum::http::Uri,
    axum::extract::Path(identifier): axum::extract::Path<String>,
) -> Response {
    let principal_id = principal_of(context.as_ref());
    let Some(tenant_id) = tenant_of(context) else {
        return error_response(&unauthenticated());
    };
    render_reply_with_audit(
        &service,
        service.delete_plugin(tenant_id, &identifier),
        ManagementChange {
            operation: "delete",
            resource: "plugin",
            method: "DELETE",
            path: uri.path(),
            tenant_id,
            principal_id,
            request_id: toolkit::api::extract_trace_id(&headers),
            body_len: 0,
        },
    )
}

/// The caller of `GET /oagw/v1/plugins/{id}/source`
/// (`cpt-cf-oagw-flow-plugin-crud`).
#[allow(clippy::unused_async)]
async fn plugin_source(
    State(service): State<Arc<ControlPlaneService>>,
    context: Option<Extension<SecurityContext>>,
    axum::extract::Path(identifier): axum::extract::Path<String>,
) -> Response {
    let Some(tenant_id) = tenant_of(context) else {
        return error_response(&unauthenticated());
    };
    render_reply(service.plugin_source(tenant_id, &identifier))
}

// @cpt-end:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-01

// @cpt-end:cpt-cf-oagw-dod-endpoint-registration:p1:inst-full

// @cpt-begin:cpt-cf-oagw-dod-upstream-crud:p1:inst-full

/// Resolves the upstream a read, replace or delete addresses, tenant-scoped.
///
/// # Errors
/// Returns the rendered not-found error of an identifier that addresses no
/// resource of the caller's tenant.
fn resolve_upstream(
    stores: &InMemoryStores,
    tenant_id: Uuid,
    identifier: &str,
) -> Result<Upstream, OagwError> {
    // @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-16
    // The path identifier is resolved against the tenant-scoped store, so a
    // resource owned by another tenant or by an ancestor tenant is
    // indistinguishable from an absent one.
    let id = parse_path_identifier(Resource::Upstream, identifier)
        .ok_or_else(|| identifier_not_found(identifier, "upstream"))?;
    // @cpt-begin:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-17
    // CATCH the miss as 404 with the not-found GTS type and never a 403.
    stores
        .upstreams()
        .find(tenant_id, id)
        .map_err(|error| render(&error, &Operation::Other))
    // @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-17
    // @cpt-end:cpt-cf-oagw-flow-upstream-crud:p1:inst-uc-16
}

// @cpt-end:cpt-cf-oagw-dod-upstream-crud:p1:inst-full

// @cpt-begin:cpt-cf-oagw-dod-route-crud:p1:inst-full

/// Resolves the route a read, replace or delete addresses, tenant-scoped.
///
/// # Errors
/// Returns the rendered not-found error of an identifier that addresses no
/// resource of the caller's tenant.
fn resolve_route(
    stores: &InMemoryStores,
    tenant_id: Uuid,
    identifier: &str,
) -> Result<Route, OagwError> {
    // @cpt-begin:cpt-cf-oagw-flow-route-crud:p1:inst-rc-14
    // The path identifier is resolved against the tenant-scoped store.
    let id = parse_path_identifier(Resource::Route, identifier)
        .ok_or_else(|| identifier_not_found(identifier, "route"))?;
    // @cpt-begin:cpt-cf-oagw-flow-route-crud:p1:inst-rc-15
    // CATCH the miss as 404 with the not-found GTS type and never a 403.
    stores
        .routes()
        .find(tenant_id, id)
        .map_err(|error| render(&error, &Operation::Other))
    // @cpt-end:cpt-cf-oagw-flow-route-crud:p1:inst-rc-15
    // @cpt-end:cpt-cf-oagw-flow-route-crud:p1:inst-rc-14
}

/// Looks the `upstream_id` of a route payload up through the tenant-scoped
/// repository, answering the upstream the reference resolves to.
///
/// An identifier that is not a UUID is left to the domain layer, which owns the
/// malformed reference and rejects it 400, so only a well-formed reference that
/// does not resolve inside the calling tenant is answered here.
///
/// # Errors
/// Returns the rendered not-found error of a reference that misses.
fn referenced_upstream(
    stores: &InMemoryStores,
    tenant_id: Uuid,
    payload: &Value,
) -> Result<Option<Upstream>, OagwError> {
    let reference = payload
        .get("upstream_id")
        .and_then(Value::as_str)
        .and_then(|reference| Uuid::parse_str(reference).ok());
    let Some(upstream_id) = reference else {
        return Ok(None);
    };
    match stores.upstreams().find(tenant_id, upstream_id) {
        Ok(upstream) => Ok(Some(upstream)),
        Err(_) => Err(OagwError::route_not_found(format!(
            "oagw.routes: no upstream '{upstream_id}' exists in the caller's tenant, so the \
             route cannot address it"
        ))),
    }
}

// @cpt-end:cpt-cf-oagw-dod-route-crud:p1:inst-full

// @cpt-begin:cpt-cf-oagw-dod-plugin-crud:p1:inst-full

/// Resolves the custom plugin a get, a delete or a source retrieval addresses.
///
/// # Errors
/// Returns the rendered not-found error of an identifier that is not
/// UUID-backed or that addresses no stored plugin of the caller's tenant.
fn stored_plugin(
    stores: &InMemoryStores,
    tenant_id: Uuid,
    identifier: &str,
) -> Result<Plugin, OagwError> {
    // @cpt-begin:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-05
    // ELSE IF the request is a get or a source retrieval, the path identifier
    // is resolved against the tenant-scoped `PluginRepository`.
    let id = parse_path_identifier(Resource::Plugin, identifier)
        .ok_or_else(|| identifier_not_found(identifier, "stored plugin"))?;
    // @cpt-begin:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-07
    // ELSE, including an identifier whose instance names a built-in plugin and
    // is therefore not persisted, RETURN 404 with the not-found GTS type,
    // never a 403: no tenant-scoped row can answer for a built-in plugin.
    stores
        .plugins()
        .find(tenant_id, id)
        .map_err(|error| render(&error, &Operation::Other))
    // @cpt-end:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-07
    // @cpt-end:cpt-cf-oagw-flow-plugin-crud:p1:inst-pc-05
}

// @cpt-end:cpt-cf-oagw-dod-plugin-crud:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::rest::error::{ERROR_SOURCE_GATEWAY, PROBLEM_JSON, X_OAGW_ERROR_SOURCE};
    use crate::api::rest::route_shell::{MANAGEMENT_SHELL, MOUNT_PREFIX};
    use crate::domain::error::MAPPING_TABLE;
    use crate::domain::model::{PLUGIN_TYPE_IDS, PROTOCOL_HTTP, ServerConfig};
    use axum::Router;
    use axum::body::to_bytes as read_body;
    use axum::http::Request as HttpRequest;
    use axum::middleware;
    use serde_json::json;
    use tower::ServiceExt;

    /// The tenant of the caller every test authenticates by default.
    fn tenant() -> Uuid {
        Uuid::from_u128(0xa11ce)
    }

    /// The tenant a foreign caller belongs to.
    fn other_tenant() -> Uuid {
        Uuid::from_u128(0xb0b)
    }

    /// The subject of a test caller.
    fn subject() -> Uuid {
        Uuid::from_u128(0xcafe)
    }

    /// A test stand-in for the platform authz middleware: the tenant the
    /// request names in `x-test-tenant` becomes the tenant of the
    /// `SecurityContext` the handlers read.
    async fn inject_tenant(
        mut request: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> Response {
        let header_tenant = request
            .headers()
            .get("x-test-tenant")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| Uuid::parse_str(value).ok());
        if let Some(tenant_id) = header_tenant {
            let context = SecurityContext::builder()
                .subject_id(subject())
                .subject_tenant_id(tenant_id)
                .build()
                .expect("a test context carries a subject and a tenant");
            request.extensions_mut().insert(context);
        }
        next.run(request).await
    }

    /// The management surface as the tests reach it: the shell the registration
    /// call mounts, answered by the control-plane handlers over `stores`, with
    /// the test tenant resolver in front of the error layer.
    fn surface(stores: InMemoryStores) -> Router {
        let control_plane = Arc::new(ControlPlaneService::new(stores));
        crate::api::rest::route_shell::mount(
            Router::new(),
            &crate::api::rest::route_shell::MountLedger::new(),
            Arc::clone(&control_plane),
            crate::infra::proxy::pipeline::stub::pipeline(
                control_plane.stores(),
                Arc::new(crate::infra::proxy::pipeline::stub::StubConnector::new()),
            ),
        )
        .expect("the shell names 25 methods and a free prefix")
        .layer(middleware::from_fn(inject_tenant))
    }

    /// Sends one request to the surface and returns the response.
    async fn call(
        router: Router,
        method: &str,
        path: &str,
        tenant_id: Option<Uuid>,
        body: Option<Value>,
    ) -> axum::response::Response {
        let mut builder = HttpRequest::builder().method(method).uri(path);
        if let Some(tenant_id) = tenant_id {
            builder = builder.header("x-test-tenant", tenant_id.to_string());
        }
        let builder = builder.header(header::CONTENT_TYPE, "application/json");
        let request = match body {
            Some(value) => builder
                .body(axum::body::Body::from(value.to_string()))
                .expect("the request is well formed"),
            None => builder
                .body(axum::body::Body::empty())
                .expect("the request is well formed"),
        };
        router.oneshot(request).await.expect("the router answers")
    }

    /// The JSON body of a response.
    async fn json_of(response: axum::response::Response) -> Value {
        let bytes = read_body(response.into_body(), usize::MAX)
            .await
            .expect("the body is readable");
        serde_json::from_slice(&bytes).expect("the body is JSON")
    }

    /// The problem+json assertions every gateway error of the 15 endpoints
    /// satisfies (`inst-cs-09`).
    fn assert_problem(response: &axum::response::Response, status: u16, fragment: &str) {
        assert_eq!(response.status().as_u16(), status, "{fragment}");
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some(PROBLEM_JSON),
            "{fragment}"
        );
        assert_eq!(
            response
                .headers()
                .get(X_OAGW_ERROR_SOURCE)
                .and_then(|value| value.to_str().ok()),
            Some(ERROR_SOURCE_GATEWAY),
            "{fragment}"
        );
    }

    /// A hostname pool the alias contract derives `api.example.com` from.
    fn hostname_pool() -> Value {
        json!({
            "server": {"endpoints": [{"scheme": "https", "host": "api.example.com", "port": 443}]},
            "protocol": PROTOCOL_HTTP
        })
    }

    /// An IP-based pool, whose alias the contract cannot derive.
    fn ip_pool() -> Value {
        json!({
            "server": {"endpoints": [{"scheme": "http", "host": "10.0.0.1", "port": 80}]},
            "protocol": PROTOCOL_HTTP
        })
    }

    /// A route payload under `upstream_id`.
    fn route_body(upstream_id: Uuid, path: &str, priority: i64) -> Value {
        json!({
            "upstream_id": upstream_id.to_string(),
            "match": {"http": {"path": path, "methods": ["GET"]}},
            "priority": priority
        })
    }

    /// A custom Starlark plugin payload.
    fn plugin_body() -> Value {
        json!({
            "plugin_type": PLUGIN_TYPE_IDS[1],
            "name": "tenant-guard",
            "source_code": "def on_request(context):\n    return context\n"
        })
    }

    /// The wrapped path identifier of a stored plugin.
    fn plugin_identifier(id: Uuid) -> String {
        format!("{}{id}", PLUGIN_TYPE_IDS[1])
    }

    /// Stores one upstream in `stores` and returns its bare UUID.
    fn stored_upstream(stores: &InMemoryStores, tenant_id: Uuid, alias: &str) -> Uuid {
        let mut upstream = Upstream {
            server: Some(ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: "https".to_owned(),
                    host: Some("api.vendor.com".to_owned()),
                    port: 443,
                }],
            }),
            protocol: Some(PROTOCOL_HTTP.to_owned()),
            ..Upstream::default()
        };
        upstream.tenant_id = Some(tenant_id);
        let payload = serde_json::to_value(&upstream).expect("the aggregate serializes");
        let mut upstream = validate_upstream_payload(&payload, tenant_id).expect("it is valid");
        upstream.alias = Some(alias.to_owned());
        upstream.id = Some(Uuid::new_v4());
        stores
            .upstreams()
            .insert(&upstream)
            .expect("the upstream is stored")
            .id
            .expect("the stored upstream carries its id")
    }

    /// Creates one resource and returns its status and bare `id` field.
    async fn created_id(response: axum::response::Response) -> (u16, Uuid) {
        let status = response.status().as_u16();
        let body = json_of(response).await;
        let id = body["id"]
            .as_str()
            .and_then(|id| Uuid::parse_str(id).ok())
            .expect("the body carries the bare server-generated UUID");
        (status, id)
    }

    /// The management surface whose audit records a test captures: the same
    /// surface, with the control plane and the pipeline emitting through one
    /// capturing sink, the way `construct_data_plane` wires them.
    fn surface_with_sink(
        stores: InMemoryStores,
    ) -> (Router, Arc<crate::infra::observability::CapturingAuditSink>) {
        let control_plane = Arc::new(ControlPlaneService::new(stores));
        let (telemetry, sink) = crate::infra::proxy::pipeline::stub::telemetry_with_capture();
        control_plane.set_telemetry(Arc::clone(&telemetry));
        let pipeline = crate::infra::proxy::pipeline::stub::pipeline_with_telemetry(
            control_plane.stores(),
            Arc::new(crate::infra::proxy::pipeline::stub::StubConnector::new()),
            crate::infra::proxy::pipeline::ProxyLimits::default(),
            telemetry,
        );
        let router = crate::api::rest::route_shell::mount(
            Router::new(),
            &crate::api::rest::route_shell::MountLedger::new(),
            Arc::clone(&control_plane),
            pipeline,
        )
        .expect("the shell names 25 methods and a free prefix")
        .layer(middleware::from_fn(inject_tenant));
        (router, sink)
    }

    /// Creates one custom plugin and returns its wrapped path identifier.
    async fn stored_plugin_identifier(router: Router) -> String {
        let (status, id) = created_id(
            call(
                router,
                "POST",
                "/oagw/v1/plugins",
                Some(tenant()),
                Some(plugin_body()),
            )
            .await,
        )
        .await;
        assert_eq!(status, 201, "the plugin is stored");
        plugin_identifier(id)
    }

    /// `POST /oagw/v1/upstreams` with a single hostname endpoint and no alias
    /// field returns 201 with the bare UUID `id`, the derived alias and
    /// `enabled: true` (§6).
    #[tokio::test]
    async fn a_hostname_upstream_is_created_with_its_derived_alias() {
        let response = call(
            surface(InMemoryStores::new()),
            "POST",
            "/oagw/v1/upstreams",
            Some(tenant()),
            Some(hostname_pool()),
        )
        .await;

        let (status, id) = created_id(response).await;
        assert_eq!(status, 201, "the create succeeds");
        assert_ne!(id, Uuid::nil(), "the id is server-generated");
    }

    /// The created body carries the derived alias and the `enabled` default
    /// (§6, first criterion, body shape).
    #[tokio::test]
    async fn the_created_upstream_body_carries_the_alias_and_the_enabled_default() {
        let response = call(
            surface(InMemoryStores::new()),
            "POST",
            "/oagw/v1/upstreams",
            Some(tenant()),
            Some(hostname_pool()),
        )
        .await;

        let body = json_of(response).await;
        assert_eq!(
            body["enabled"],
            json!(true),
            "the omitted flag takes the default"
        );
        assert_eq!(body["alias"], json!("api.example.com"), "the derived alias");
        assert!(
            body.get("gts.cf.core.oagw.upstream.v1~").is_none()
                && body["id"]
                    .as_str()
                    .is_some_and(|id| Uuid::parse_str(id).is_ok()),
            "the id field is the bare schema identifier, not the wrapped path form: {body}"
        );
    }

    /// An IP-based pool with no alias field is rejected 400 by the delegated
    /// alias contract, which names the missing alias (§6).
    #[tokio::test]
    async fn a_non_derivable_pool_without_an_alias_is_rejected_by_the_alias_contract() {
        let response = call(
            surface(InMemoryStores::new()),
            "POST",
            "/oagw/v1/upstreams",
            Some(tenant()),
            Some(ip_pool()),
        )
        .await;

        assert_problem(&response, 400, "validation.error");
        let body = json_of(response).await;
        assert_eq!(
            body["type"],
            json!("gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1")
        );
        assert_eq!(body["status"], json!(400));
        assert!(
            body["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("alias")),
            "the alias rejection names the missing alias: {}",
            body["detail"]
        );
    }

    /// A second upstream with the same alias in the same tenant is answered
    /// 409, while the same alias in another tenant is accepted (§6).
    #[tokio::test]
    async fn a_same_tenant_alias_conflicts_and_another_tenant_accepts_it() {
        let stores = InMemoryStores::new();
        let existing = stored_upstream(&stores, tenant(), "payments.vendor.com");
        let router = surface(stores);
        // The pool is IP-based, so the alias the body supplies is the explicit
        // routing key, which is the alias the stored upstream already holds.
        let body = json!({
            "alias": "payments.vendor.com",
            "server": {"endpoints": [{"scheme": "http", "host": "10.0.0.9", "port": 80}]},
            "protocol": PROTOCOL_HTTP
        });

        let response = call(
            router.clone(),
            "POST",
            "/oagw/v1/upstreams",
            Some(tenant()),
            Some(body.clone()),
        )
        .await;
        assert_problem(&response, 409, "validation.error");
        let problem = json_of(response).await;
        assert_eq!(
            problem["status"],
            json!(409),
            "the body carries the override"
        );
        assert_eq!(
            problem["type"],
            json!("gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1")
        );
        assert!(
            problem["detail"].as_str().is_some_and(|detail| {
                detail.contains("payments.vendor.com") && detail.contains(&tenant().to_string())
            }),
            "the detail names the colliding (tenant_id, alias) key: {}",
            problem["detail"]
        );

        // The same alias in another tenant is a different aggregate.
        let response = call(
            router,
            "POST",
            "/oagw/v1/upstreams",
            Some(other_tenant()),
            Some(body),
        )
        .await;
        let (status, other_id) = created_id(response).await;
        assert_eq!(status, 201);
        assert_ne!(other_id, existing);
    }

    /// An alias override is rejected 400 by the alias contract.
    #[tokio::test]
    async fn an_alias_override_is_rejected_by_the_alias_contract() {
        let mut body = hostname_pool();
        body["alias"] = json!("payments.vendor.com");
        let response = call(
            surface(InMemoryStores::new()),
            "POST",
            "/oagw/v1/upstreams",
            Some(tenant()),
            Some(body),
        )
        .await;

        assert_problem(&response, 400, "validation.error");
        let problem = json_of(response).await;
        assert!(
            problem["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("payments.vendor.com")),
            "{}",
            problem["detail"]
        );
    }

    /// `PUT /oagw/v1/upstreams/{id}` clears the optional blocks the body
    /// omits, keeps the alias of the update table, and answers a body carrying
    /// `id` or `tenant_id` with 400 (§6).
    #[tokio::test]
    async fn a_replace_clears_the_omitted_blocks_and_rejects_an_immutable_field() {
        let stores = InMemoryStores::new();
        let id = stored_upstream(&stores, tenant(), "payments.vendor.com");
        let router = surface(stores);
        let replacement = json!({
            "server": {"endpoints": [{"scheme": "https", "host": "api.vendor.com", "port": 443}]},
            "protocol": PROTOCOL_HTTP,
            "tags": ["openai"]
        });

        let response = call(
            router.clone(),
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(tenant()),
            Some(replacement.clone()),
        )
        .await;
        assert_eq!(response.status().as_u16(), 200, "the replace succeeds");
        let body = json_of(response).await;
        assert_eq!(
            body["id"],
            json!(id.to_string()),
            "the addressed identifier"
        );
        assert_eq!(
            body["alias"],
            json!("payments.vendor.com"),
            "the stored alias"
        );
        assert_eq!(body["tags"], json!(["openai"]));
        assert!(
            body.get("cors").is_none()
                && body.get("auth").is_none()
                && body.get("headers").is_none(),
            "the blocks the body omits are cleared: {body}"
        );

        for field in ["id", "tenant_id"] {
            let mut body = replacement.clone();
            body[field] = json!(Uuid::new_v4().to_string());
            let response = call(
                router.clone(),
                "PUT",
                &format!("/oagw/v1/upstreams/{id}"),
                Some(tenant()),
                Some(body),
            )
            .await;
            assert_problem(&response, 400, "validation.error");
            let problem = json_of(response).await;
            assert!(
                problem["detail"]
                    .as_str()
                    .is_some_and(|detail| detail.contains(field)),
                "{field} is reported as an unknown field: {}",
                problem["detail"]
            );
        }
    }

    /// A `PUT` writing `enabled: false` stores and returns the disabled state
    /// of the resource-enabled machine, and a `PUT` writing `true` moves it
    /// back (`inst-en-01`, `inst-en-02`).
    #[tokio::test]
    async fn a_put_moves_the_enabled_state_both_ways() {
        let stores = InMemoryStores::new();
        let id = stored_upstream(&stores, tenant(), "payments.vendor.com");
        let router = surface(stores);
        let replacement = |enabled: bool| {
            json!({
                "server": {"endpoints": [{"scheme": "https", "host": "api.vendor.com", "port": 443}]},
                "protocol": PROTOCOL_HTTP,
                "enabled": enabled
            })
        };

        let response = call(
            router.clone(),
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(tenant()),
            Some(replacement(false)),
        )
        .await;
        let body = json_of(response).await;
        assert_eq!(body["enabled"], json!(false), "inst-en-01");

        let response = call(
            router,
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(tenant()),
            Some(replacement(true)),
        )
        .await;
        let body = json_of(response).await;
        assert_eq!(body["enabled"], json!(true), "inst-en-02");
    }

    /// `GET /oagw/v1/upstreams/{id}` for a foreign identifier returns 404 and
    /// is indistinguishable from an absent one (§6).
    #[tokio::test]
    async fn a_foreign_upstream_is_indistinguishable_from_an_absent_one() {
        let stores = InMemoryStores::new();
        let foreign = stored_upstream(&stores, other_tenant(), "payments.vendor.com");
        let absent = Uuid::new_v4();
        let router = surface(stores);

        for identifier in [foreign.to_string(), absent.to_string()] {
            let response = call(
                router.clone(),
                "GET",
                &format!("/oagw/v1/upstreams/{identifier}"),
                Some(tenant()),
                None,
            )
            .await;
            assert_problem(&response, 404, "route.not_found");
            let problem = json_of(response).await;
            assert_eq!(
                problem["type"],
                json!("gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1")
            );
        }

        // A wrapped identifier of the same aggregate is answered the same way.
        let response = call(
            router,
            "GET",
            &format!("/oagw/v1/upstreams/{}{foreign}", UPSTREAM_PATH_IDENTIFIER),
            Some(tenant()),
            None,
        )
        .await;
        assert_problem(&response, 404, "route.not_found");
    }

    /// A delete of an upstream cascades to its routes, observable through the
    /// management surface (§6).
    #[tokio::test]
    async fn deleting_an_upstream_cascades_to_its_routes() {
        let stores = InMemoryStores::new();
        let upstream_id = stored_upstream(&stores, tenant(), "payments.vendor.com");
        let router = surface(stores);
        let response = call(
            router.clone(),
            "POST",
            "/oagw/v1/routes",
            Some(tenant()),
            Some(route_body(upstream_id, "/v1/chat", 10)),
        )
        .await;
        let (_, route_id) = created_id(response).await;

        let response = call(
            router.clone(),
            "DELETE",
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            Some(tenant()),
            None,
        )
        .await;
        assert_eq!(response.status().as_u16(), 204, "the delete succeeds");

        let response = call(
            router,
            "GET",
            &format!("/oagw/v1/routes/{route_id}"),
            Some(tenant()),
            None,
        )
        .await;
        assert_problem(&response, 404, "route.not_found");
    }

    /// The upstream list answers only the calling tenant, as a bare JSON array
    /// (`inst-uc-20`, `cpt-cf-oagw-dod-tenant-scoping`).
    #[tokio::test]
    async fn the_upstream_list_answers_only_the_calling_tenant() {
        let stores = InMemoryStores::new();
        let mine = stored_upstream(&stores, tenant(), "mine.vendor.com");
        let foreign = stored_upstream(&stores, other_tenant(), "foreign.vendor.com");
        let response = call(
            surface(stores),
            "GET",
            "/oagw/v1/upstreams",
            Some(tenant()),
            None,
        )
        .await;

        assert_eq!(response.status().as_u16(), 200);
        let page = json_of(response).await;
        let items = page.as_array().expect("the page is a bare JSON array");
        assert_eq!(items.len(), 1, "no other tenant's resource: {page}");
        assert_eq!(items[0]["id"], json!(mine.to_string()));
        assert_ne!(items[0]["id"], json!(foreign.to_string()));
    }

    /// A request that reaches a handler without a security context is answered
    /// 401 by the existing `AuthenticationFailed` row.
    #[tokio::test]
    async fn a_request_without_a_security_context_is_answered_401() {
        let response = call(
            surface(InMemoryStores::new()),
            "POST",
            "/oagw/v1/upstreams",
            None,
            Some(hostname_pool()),
        )
        .await;

        assert_problem(&response, 401, "auth.failed");
        let problem = json_of(response).await;
        assert_eq!(
            problem["type"],
            json!("gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1")
        );
    }

    /// `POST /oagw/v1/routes` with an `upstream_id` that does not resolve in
    /// the calling tenant returns 404 (§6), while a malformed one returns 400.
    #[tokio::test]
    async fn a_route_under_an_unresolvable_upstream_is_answered_404() {
        let router = surface(InMemoryStores::new());
        let absent = Uuid::new_v4();

        let response = call(
            router.clone(),
            "POST",
            "/oagw/v1/routes",
            Some(tenant()),
            Some(route_body(absent, "/v1/chat", 10)),
        )
        .await;
        assert_problem(&response, 404, "route.not_found");
        let problem = json_of(response).await;
        assert_eq!(
            problem["type"],
            json!("gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1")
        );

        // A reference into another tenant is answered the same way.
        let foreign_stores = InMemoryStores::new();
        let foreign = stored_upstream(&foreign_stores, other_tenant(), "foreign.vendor.com");
        let response = call(
            router.clone(),
            "POST",
            "/oagw/v1/routes",
            Some(tenant()),
            Some(route_body(foreign, "/v1/chat", 10)),
        )
        .await;
        assert_problem(&response, 404, "route.not_found");

        // A malformed reference is the 400 the domain layer owns (§1.5).
        let mut body = route_body(absent, "/v1/chat", 10);
        body["upstream_id"] = json!("not-a-uuid");
        let response = call(
            router,
            "POST",
            "/oagw/v1/routes",
            Some(tenant()),
            Some(body),
        )
        .await;
        assert_problem(&response, 400, "validation.error");
    }

    /// `PUT /oagw/v1/routes/{id}` with `upstream_id` in the body returns 400,
    /// and the same request without it returns 200 with the reference
    /// unchanged (§6).
    #[tokio::test]
    async fn a_route_replace_leaves_the_upstream_reference_untouched() {
        let stores = InMemoryStores::new();
        let upstream_id = stored_upstream(&stores, tenant(), "payments.vendor.com");
        let router = surface(stores);
        let response = call(
            router.clone(),
            "POST",
            "/oagw/v1/routes",
            Some(tenant()),
            Some(route_body(upstream_id, "/v1/chat", 10)),
        )
        .await;
        let (_, route_id) = created_id(response).await;

        let response = call(
            router.clone(),
            "PUT",
            &format!("/oagw/v1/routes/{route_id}"),
            Some(tenant()),
            Some(route_body(upstream_id, "/v1/other", 20)),
        )
        .await;
        assert_problem(&response, 400, "validation.error");
        let problem = json_of(response).await;
        assert!(
            problem["detail"]
                .as_str()
                .is_some_and(|detail| detail.contains("upstream_id")),
            "{}",
            problem["detail"]
        );

        let mut body = route_body(upstream_id, "/v1/other", 20);
        body.as_object_mut()
            .expect("the body is an object")
            .remove("upstream_id");
        let response = call(
            router,
            "PUT",
            &format!("/oagw/v1/routes/{route_id}"),
            Some(tenant()),
            Some(body),
        )
        .await;
        assert_eq!(response.status().as_u16(), 200);
        let replaced = json_of(response).await;
        assert_eq!(
            replaced["upstream_id"],
            json!(upstream_id.to_string()),
            "the stored reference is what the replacement keeps"
        );
        assert_eq!(replaced["match"]["http"]["path"], json!("/v1/other"));
        assert_eq!(replaced["priority"], json!(20));
    }

    /// A second route with the same `path`, `priority` and `method` under the
    /// same upstream returns 409 naming the match key (§6).
    #[tokio::test]
    async fn a_second_route_with_the_same_match_key_conflicts() {
        let stores = InMemoryStores::new();
        let upstream_id = stored_upstream(&stores, tenant(), "payments.vendor.com");
        let router = surface(stores);
        let body = route_body(upstream_id, "/v1/chat", 10);
        let response = call(
            router.clone(),
            "POST",
            "/oagw/v1/routes",
            Some(tenant()),
            Some(body.clone()),
        )
        .await;
        assert_eq!(response.status().as_u16(), 201, "the first route is stored");

        let response = call(
            router,
            "POST",
            "/oagw/v1/routes",
            Some(tenant()),
            Some(body),
        )
        .await;
        assert_problem(&response, 409, "validation.error");
        let problem = json_of(response).await;
        assert_eq!(problem["status"], json!(409));
        assert_eq!(
            problem["type"],
            json!("gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1")
        );
        assert!(
            problem["detail"].as_str().is_some_and(|detail| {
                detail.contains("/v1/chat") && detail.contains("10") && detail.contains("GET")
            }),
            "the detail names the colliding (path, priority, method) key: {}",
            problem["detail"]
        );
    }

    /// `PUT /oagw/v1/plugins/{id}` is answered 405 by the routing layer,
    /// because no such method is registered (§6).
    #[tokio::test]
    async fn a_put_on_the_plugin_path_is_answered_405() {
        let response = call(
            surface(InMemoryStores::new()),
            "PUT",
            "/oagw/v1/plugins/plugin-1",
            Some(tenant()),
            Some(plugin_body()),
        )
        .await;

        assert_eq!(
            response.status().as_u16(),
            405,
            "the routing layer answers the unregistered method"
        );
        let bytes = read_body(response.into_body(), usize::MAX)
            .await
            .expect("the body is read");
        assert!(
            bytes.is_empty(),
            "no 405 row is invented in the closed mapping table"
        );
        assert_eq!(MAPPING_TABLE.len(), 22, "the mapping table stays closed");
    }

    /// A delete of a plugin an upstream still binds returns 409 with
    /// `plugin_id` and `referenced_by`, and the plugin stays retrievable (§6).
    #[tokio::test]
    async fn a_referenced_plugin_delete_is_answered_409_plugin_in_use() {
        let stores = InMemoryStores::new();
        let router = surface(stores);
        let identifier = stored_plugin_identifier(router.clone()).await;

        // The upstream binds the plugin, so the delete is a conflict.
        let binding = json!({
            "server": {"endpoints": [{"scheme": "https", "host": "api.vendor.com", "port": 443}]},
            "protocol": PROTOCOL_HTTP,
            "plugins": {"items": [identifier.clone()]}
        });
        let response = call(
            router.clone(),
            "POST",
            "/oagw/v1/upstreams",
            Some(tenant()),
            Some(binding),
        )
        .await;
        let (_, holder) = created_id(response).await;

        let response = call(
            router.clone(),
            "DELETE",
            &format!("/oagw/v1/plugins/{identifier}"),
            Some(tenant()),
            None,
        )
        .await;
        assert_problem(&response, 409, "plugin.in_use");
        let problem = json_of(response).await;
        assert_eq!(
            problem["type"],
            json!("gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1")
        );
        assert_eq!(problem["plugin_id"], json!(identifier));
        assert_eq!(
            problem["referenced_by"],
            json!({"upstreams": [holder.to_string()], "routes": []})
        );

        // The stored plugin is untouched and still retrievable.
        let response = call(
            router,
            "GET",
            &format!("/oagw/v1/plugins/{identifier}"),
            Some(tenant()),
            None,
        )
        .await;
        assert_eq!(response.status().as_u16(), 200, "the plugin stays stored");
    }

    /// A delete of a plugin nothing references returns 204, and a following
    /// get returns 404 (§6).
    #[tokio::test]
    async fn an_unreferenced_plugin_delete_removes_it() {
        let router = surface(InMemoryStores::new());
        let identifier = stored_plugin_identifier(router.clone()).await;

        let response = call(
            router.clone(),
            "DELETE",
            &format!("/oagw/v1/plugins/{identifier}"),
            Some(tenant()),
            None,
        )
        .await;
        assert_eq!(response.status().as_u16(), 204, "the delete succeeds");

        let response = call(
            router,
            "GET",
            &format!("/oagw/v1/plugins/{identifier}"),
            Some(tenant()),
            None,
        )
        .await;
        assert_problem(&response, 404, "route.not_found");
    }

    /// The source endpoint returns the stored Starlark source (§6).
    #[tokio::test]
    async fn the_source_endpoint_returns_the_stored_starlark_source() {
        let router = surface(InMemoryStores::new());
        let identifier = stored_plugin_identifier(router.clone()).await;

        let response = call(
            router,
            "GET",
            &format!("/oagw/v1/plugins/{identifier}/source"),
            Some(tenant()),
            None,
        )
        .await;
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("text/plain; charset=utf-8")
        );
        let bytes = read_body(response.into_body(), usize::MAX)
            .await
            .expect("the source is read");
        assert_eq!(
            String::from_utf8_lossy(&bytes),
            "def on_request(context):\n    return context\n"
        );
    }

    /// A built-in named plugin identifier is answered 404 on get, delete and
    /// source, and never 403 (§6).
    #[tokio::test]
    async fn a_built_in_plugin_identifier_is_not_addressable() {
        let router = surface(InMemoryStores::new());
        let built_in = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
        for (method, path) in [
            ("GET", format!("/oagw/v1/plugins/{built_in}")),
            ("DELETE", format!("/oagw/v1/plugins/{built_in}")),
            ("GET", format!("/oagw/v1/plugins/{built_in}/source")),
        ] {
            let response = call(router.clone(), method, &path, Some(tenant()), None).await;
            assert_problem(&response, 404, "route.not_found");
            let problem = json_of(response).await;
            assert_eq!(
                problem["type"],
                json!("gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1")
            );
        }
    }

    /// `GET /oagw/v1/routes?$filter=upstream_id eq '{uuid}'` returns only the
    /// routes of that upstream, with no envelope (§6).
    #[tokio::test]
    async fn the_route_list_filters_by_upstream_id() {
        let stores = InMemoryStores::new();
        let upstream_id = stored_upstream(&stores, tenant(), "payments.vendor.com");
        let other = stored_upstream(&stores, tenant(), "other.vendor.com");
        let router = surface(stores);
        for (upstream, path) in [(upstream_id, "/v1/chat"), (other, "/v1/embed")] {
            let response = call(
                router.clone(),
                "POST",
                "/oagw/v1/routes",
                Some(tenant()),
                Some(route_body(upstream, path, 10)),
            )
            .await;
            assert_eq!(response.status().as_u16(), 201);
        }

        let response = call(
            router,
            "GET",
            &format!("/oagw/v1/routes?$filter=upstream_id%20eq%20'{upstream_id}'"),
            Some(tenant()),
            None,
        )
        .await;
        assert_eq!(response.status().as_u16(), 200);
        let page = json_of(response).await;
        let items = page.as_array().expect("the page is a bare JSON array");
        assert_eq!(
            items.len(),
            1,
            "no envelope, and only the filtered routes: {page}"
        );
        assert_eq!(items[0]["upstream_id"], json!(upstream_id.to_string()));
    }

    /// `$orderby=priority desc` orders the route page, and `alias ne` is
    /// rejected 400 (§6).
    #[tokio::test]
    async fn the_list_orders_by_a_field_and_rejects_an_unsupported_operator() {
        let stores = InMemoryStores::new();
        let upstream_id = stored_upstream(&stores, tenant(), "payments.vendor.com");
        let router = surface(stores);
        for priority in [30, 10, 20] {
            let response = call(
                router.clone(),
                "POST",
                "/oagw/v1/routes",
                Some(tenant()),
                Some(route_body(upstream_id, "/v1/chat", priority)),
            )
            .await;
            assert_eq!(response.status().as_u16(), 201);
        }

        let response = call(
            router.clone(),
            "GET",
            "/oagw/v1/routes?$orderby=priority%20desc",
            Some(tenant()),
            None,
        )
        .await;
        let page = json_of(response).await;
        let priorities: Vec<i64> = page
            .as_array()
            .expect("the page is a bare JSON array")
            .iter()
            .map(|item| item["priority"].as_i64().expect("the priority"))
            .collect();
        assert_eq!(priorities, [30, 20, 10]);

        let mut upstream = hostname_pool();
        upstream["alias"] = json!("api.example.com");
        let response = call(
            router.clone(),
            "POST",
            "/oagw/v1/upstreams",
            Some(tenant()),
            Some(upstream),
        )
        .await;
        assert_eq!(response.status().as_u16(), 201);

        let response = call(
            router,
            "GET",
            "/oagw/v1/upstreams?$filter=alias%20ne%20'api.example.com'",
            Some(tenant()),
            None,
        )
        .await;
        assert_problem(&response, 400, "validation.error");
        let problem = json_of(response).await;
        assert_eq!(
            problem["type"],
            json!("gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1")
        );
    }

    /// `$top` is clamped at 100, defaults to 50, and a malformed `$top` is
    /// rejected 400 (§6).
    #[tokio::test]
    async fn the_top_parameter_is_clamped_and_a_malformed_one_is_rejected() {
        let stores = InMemoryStores::new();
        let upstream_id = stored_upstream(&stores, tenant(), "payments.vendor.com");
        let router = surface(stores);
        for index in 0..120 {
            let response = call(
                router.clone(),
                "POST",
                "/oagw/v1/routes",
                Some(tenant()),
                Some(route_body(upstream_id, &format!("/v1/path-{index}"), index)),
            )
            .await;
            assert_eq!(response.status().as_u16(), 201);
        }

        let response = call(
            router.clone(),
            "GET",
            "/oagw/v1/routes?$top=500",
            Some(tenant()),
            None,
        )
        .await;
        let page = json_of(response).await;
        assert_eq!(
            page.as_array().expect("the page is an array").len(),
            list_query::TOP_MAX,
            "a large top is clamped, not rejected"
        );

        let response = call(
            router.clone(),
            "GET",
            "/oagw/v1/routes",
            Some(tenant()),
            None,
        )
        .await;
        let page = json_of(response).await;
        assert_eq!(
            page.as_array().expect("the page is an array").len(),
            list_query::TOP_DEFAULT,
            "the omitted top takes the default of 50"
        );

        let response = call(
            router,
            "GET",
            "/oagw/v1/routes?$top=abc",
            Some(tenant()),
            None,
        )
        .await;
        assert_problem(&response, 400, "validation.error");
        let problem = json_of(response).await;
        assert_eq!(
            problem["type"],
            json!("gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1")
        );
    }

    /// Every gateway error of the 15 endpoints is problem+json from the
    /// gateway, with the five RFC 9457 standard fields, and the mapping table
    /// stays closed (§6).
    #[tokio::test]
    async fn every_gateway_error_of_the_15_endpoints_is_problem_json() {
        let router = surface(InMemoryStores::new());
        for (path, methods) in MANAGEMENT_SHELL.iter() {
            for method in methods.iter() {
                let full = format!("{MOUNT_PREFIX}{path}");
                let body = method_needs_a_body(method).then_some(hostname_pool());
                let response = call(router.clone(), method, &full, None, body).await;
                let status = response.status().as_u16();
                assert_problem(&response, status, "auth.failed");
                let problem = json_of(response).await;
                for field in ["type", "title", "status", "detail", "instance"] {
                    assert!(
                        problem.get(field).is_some(),
                        "{method} {full}: the problem body carries {field}: {problem}"
                    );
                }
                assert_eq!(
                    problem["type"],
                    json!("gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1")
                );
            }
        }
        assert_eq!(MAPPING_TABLE.len(), 22, "the mapping table stays closed");
    }

    /// The create endpoints carry a body; the read-side endpoints do not.
    fn method_needs_a_body(method: &str) -> bool {
        matches!(method, "POST" | "PUT")
    }

    /// The two identifier forms of §1.1 address one aggregate, and a plugin
    /// identifier is parsed through its family prefix.
    #[test]
    fn the_path_identifiers_of_the_two_forms_address_one_aggregate() {
        let upstream = Uuid::from_u128(0x11);
        assert_eq!(
            parse_path_identifier(Resource::Upstream, &upstream.to_string()),
            Some(upstream)
        );
        assert_eq!(
            parse_path_identifier(
                Resource::Upstream,
                &format!("{UPSTREAM_PATH_IDENTIFIER}{upstream}")
            ),
            Some(upstream)
        );
        let route = Uuid::from_u128(0x12);
        assert_eq!(
            parse_path_identifier(Resource::Route, &format!("{ROUTE_PATH_IDENTIFIER}{route}")),
            Some(route)
        );
        let plugin = Uuid::from_u128(0x13);
        let identifier = format!("{}{plugin}", PLUGIN_TYPE_IDS[2]);
        assert_eq!(
            parse_path_identifier(Resource::Plugin, &identifier),
            Some(plugin)
        );
        for unparseable in [
            "",
            "plugin-1",
            UPSTREAM_PATH_IDENTIFIER,
            "gts.cf.core.oagw.unknown.v1~not-a-uuid",
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
        ] {
            assert_eq!(parse_path_identifier(Resource::Upstream, unparseable), None);
            assert_eq!(parse_path_identifier(Resource::Plugin, unparseable), None);
        }
    }

    /// The success statuses of §1.1 are exactly the five reply variants.
    #[test]
    fn the_reply_variants_carry_the_success_statuses_of_the_contract() {
        assert_eq!(Reply::Created(json!({})).status(), 201);
        assert_eq!(Reply::Ok(json!({})).status(), 200);
        assert_eq!(Reply::Source(String::new()).status(), 200);
        assert_eq!(Reply::Page(Vec::new()).status(), 200);
        assert_eq!(Reply::Deleted.status(), 204);
    }

    /// A `(path, method)` pair the closed shell does not name is refused at
    /// registration, so the routing layer keeps answering it.
    #[test]
    fn a_method_outside_the_shell_is_refused_at_registration() {
        let error = control_plane_methods("/plugins/{id}", &["PUT"]).expect_err("no PUT handler");
        assert_eq!(error.mapping().variant, "RouteError");
        assert!(error.detail().contains("PUT"), "{}", error.detail());
    }

    /// The match key a route-match conflict names.
    #[test]
    fn the_match_key_names_the_path_the_priority_and_the_method() {
        let upstream_id = Uuid::from_u128(0x21);
        let route = validate_route_payload(
            &route_body(upstream_id, "/v1/chat", 10),
            tenant(),
            &ReferencedUpstream(Some(Upstream {
                id: Some(upstream_id),
                protocol: Some(PROTOCOL_HTTP.to_owned()),
                ..Upstream::default()
            })),
        )
        .expect("the payload is valid");
        assert_eq!(
            render_match_key(&route),
            "path '/v1/chat', priority 10, method 'GET'"
        );
    }
    // ------------------------------------------------------------------
    // The configuration-change records the observability feature renders
    // ------------------------------------------------------------------

    /// The records the sink holds, as the `(level, event, status)` triples the
    /// assertions read.
    fn events_of(sink: &Arc<crate::infra::observability::CapturingAuditSink>) -> Vec<String> {
        sink.lines()
            .iter()
            .map(|line| {
                let value: Value =
                    serde_json::from_str(line).expect("each record is one JSON line");
                format!(
                    "{} {} {}",
                    value["level"].as_str().unwrap_or_default(),
                    value["event"].as_str().unwrap_or_default(),
                    value["status"]
                )
            })
            .collect()
    }

    /// §6: exactly one configuration-change record is written per successful
    /// create, replace or delete of an upstream, a route or a plugin resource,
    /// an enable or a disable being a replace, and no record is written for a
    /// read.
    #[tokio::test]
    async fn one_configuration_change_record_is_written_per_management_mutation() {
        let stores = InMemoryStores::new();
        let (router, sink) = surface_with_sink(stores);

        let response = call(
            router.clone(),
            "POST",
            "/oagw/v1/upstreams",
            Some(tenant()),
            Some(hostname_pool()),
        )
        .await;
        let (status, upstream_id) = created_id(response).await;
        assert_eq!(status, 201);
        assert_eq!(
            events_of(&sink),
            vec!["INFO create_upstream 201"],
            "one record, at INFO, for the create"
        );

        let response = call(
            router.clone(),
            "PUT",
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            Some(tenant()),
            Some(json!({
                "server": {"endpoints": [{"scheme": "https", "host": "api.example.com", "port": 443}]},
                "protocol": PROTOCOL_HTTP,
                "enabled": false
            })),
        )
        .await;
        assert_eq!(response.status().as_u16(), 200, "the disable is a replace");
        assert_eq!(
            events_of(&sink),
            vec!["INFO create_upstream 201", "INFO replace_upstream 200"],
            "the disable is one record, at INFO"
        );

        let response = call(
            router.clone(),
            "POST",
            "/oagw/v1/routes",
            Some(tenant()),
            Some(route_body(upstream_id, "/v1/chat", 10)),
        )
        .await;
        let (status, route_id) = created_id(response).await;
        assert_eq!(status, 201);
        assert_eq!(
            events_of(&sink),
            vec![
                "INFO create_upstream 201",
                "INFO replace_upstream 200",
                "INFO create_route 201"
            ]
        );

        let response = call(
            router.clone(),
            "DELETE",
            &format!("/oagw/v1/routes/{route_id}"),
            Some(tenant()),
            None,
        )
        .await;
        assert_eq!(response.status().as_u16(), 204, "the route is deleted");
        assert_eq!(
            events_of(&sink),
            vec![
                "INFO create_upstream 201",
                "INFO replace_upstream 200",
                "INFO create_route 201",
                "INFO delete_route 204"
            ]
        );

        let identifier = stored_plugin_identifier(router.clone()).await;
        assert_eq!(
            events_of(&sink),
            vec![
                "INFO create_upstream 201",
                "INFO replace_upstream 200",
                "INFO create_route 201",
                "INFO delete_route 204",
                "INFO create_plugin 201"
            ],
            "the plugin create is one record"
        );

        let response = call(
            router.clone(),
            "DELETE",
            &format!("/oagw/v1/plugins/{identifier}"),
            Some(tenant()),
            None,
        )
        .await;
        assert_eq!(response.status().as_u16(), 204);
        assert_eq!(
            events_of(&sink),
            vec![
                "INFO create_upstream 201",
                "INFO replace_upstream 200",
                "INFO create_route 201",
                "INFO delete_route 204",
                "INFO create_plugin 201",
                "INFO delete_plugin 204"
            ]
        );

        let response = call(
            router,
            "DELETE",
            &format!("/oagw/v1/upstreams/{upstream_id}"),
            Some(tenant()),
            None,
        )
        .await;
        assert_eq!(response.status().as_u16(), 204);
        assert_eq!(
            events_of(&sink),
            vec![
                "INFO create_upstream 201",
                "INFO replace_upstream 200",
                "INFO create_route 201",
                "INFO delete_route 204",
                "INFO create_plugin 201",
                "INFO delete_plugin 204",
                "INFO delete_upstream 204"
            ],
            "seven mutations, seven records, one each"
        );
    }

    /// §6: a write the surface refuses produces no record at all, and neither
    /// does a read.
    #[tokio::test]
    async fn a_rejected_write_and_a_read_write_no_record() {
        let stores = InMemoryStores::new();
        let id = stored_upstream(&stores, tenant(), "payments.vendor.com");
        let (router, sink) = surface_with_sink(stores);

        let response = call(
            router.clone(),
            "POST",
            "/oagw/v1/upstreams",
            Some(tenant()),
            Some(ip_pool()),
        )
        .await;
        assert_eq!(
            response.status().as_u16(),
            400,
            "the alias is not derivable"
        );
        let response = call(
            router.clone(),
            "PUT",
            &format!("/oagw/v1/upstreams/{id}"),
            Some(tenant()),
            Some(json!({"protocol": PROTOCOL_HTTP, "enabled": true})),
        )
        .await;
        assert_eq!(
            response.status().as_u16(),
            400,
            "a replacement without a server block is refused"
        );
        let response = call(
            router.clone(),
            "DELETE",
            &format!("/oagw/v1/upstreams/{}", Uuid::new_v4()),
            Some(tenant()),
            None,
        )
        .await;
        assert_eq!(response.status().as_u16(), 404);

        let response = call(
            router.clone(),
            "GET",
            "/oagw/v1/upstreams",
            Some(tenant()),
            None,
        )
        .await;
        assert_eq!(response.status().as_u16(), 200, "the read succeeds");

        assert!(
            sink.lines().is_empty(),
            "no record is written for a refused write or a read: {:?}",
            sink.lines()
        );
    }

    /// §6: the record of a configuration change carries the fields DESIGN §4.3
    /// names and nothing of the configuration the decision carried: the
    /// endpoint, plugin-binding and CORS material is nested in the resource the
    /// surface stored, never in the record.
    #[tokio::test]
    async fn a_configuration_change_record_carries_the_closed_field_set() {
        let (router, sink) = surface_with_sink(InMemoryStores::new());
        let response = call(
            router,
            "POST",
            "/oagw/v1/upstreams",
            Some(tenant()),
            Some(hostname_pool()),
        )
        .await;
        assert_eq!(response.status().as_u16(), 201);

        let lines = sink.lines();
        assert_eq!(lines.len(), 1);
        let record: Value = serde_json::from_str(&lines[0]).expect("one JSON line");
        let mut fields = record
            .as_object()
            .map(|object| object.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        fields.sort();
        let expected = [
            "timestamp",
            "level",
            "event",
            "request_id",
            "tenant_id",
            "principal_id",
            "host",
            "path",
            "method",
            "status",
            "duration_ms",
            "request_size",
            "response_size",
            "error_type",
        ];
        let mut sorted_expected = expected.to_vec();
        sorted_expected.sort();
        assert_eq!(
            fields, sorted_expected,
            "the 14 base fields and no field beside them"
        );
        // The members are written in the order the ADR's example fixes, so the
        // position of each key in the line is that order.
        let positions = expected
            .iter()
            .map(|field| {
                lines[0]
                    .find(&format!("\"{field}\":"))
                    .unwrap_or(usize::MAX)
            })
            .collect::<Vec<_>>();
        let mut ordered = positions.clone();
        ordered.sort_unstable();
        assert_eq!(
            positions, ordered,
            "the members are written in the canonical field order: {}",
            lines[0]
        );

        assert_eq!(record["error_type"], Value::Null);
        assert_eq!(record["event"], json!("create_upstream"));
        assert_eq!(record["method"], json!("POST"));
        assert_eq!(record["status"], json!(201));
        assert_eq!(
            record["path"],
            json!("/oagw/v1/upstreams"),
            "the record's path is the gear-relative management path"
        );
        assert_eq!(record["tenant_id"], json!(tenant().to_string()));
        assert_eq!(record["principal_id"], json!(subject().to_string()));
        let serialized = serde_json::to_string(&record).expect("the record serializes");
        assert!(
            !serialized.contains("api.example.com") && !serialized.contains("443"),
            "no endpoint material of the resource enters the record: {serialized}"
        );
    }

    /// §6: `GET /oagw/v1/upstreams/{id}` for an identifier an ancestor tenant
    /// owns is answered 404 with the not-found GTS type, the very answer an
    /// absent identifier gets — the management surface performs no ancestor
    /// walk, so a descendant caller cannot read an ancestor resource by
    /// knowing its identifier.
    #[tokio::test]
    async fn an_ancestor_owned_upstream_is_answered_like_an_absent_one() {
        let stores = InMemoryStores::new();
        // The ancestor holds the resource; the caller below is its descendant.
        let ancestor = stored_upstream(&stores, other_tenant(), "ancestor.vendor.com");
        let absent = Uuid::new_v4();
        let router = surface(stores);

        let response = call(
            router.clone(),
            "GET",
            &format!("/oagw/v1/upstreams/{ancestor}"),
            Some(tenant()),
            None,
        )
        .await;
        assert_problem(&response, 404, "route.not_found");
        let owned = json_of(response).await;
        assert_eq!(
            owned["type"],
            json!("gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1")
        );

        let response = call(
            router,
            "GET",
            &format!("/oagw/v1/upstreams/{absent}"),
            Some(tenant()),
            None,
        )
        .await;
        let absent_problem = json_of(response).await;

        for field in ["type", "title", "status", "detail"] {
            assert_eq!(
                owned[field], absent_problem[field],
                "the ancestor-owned answer carries the {field} of an absent identifier"
            );
        }
    }

    /// §6: `DELETE /oagw/v1/plugins/{id}` for a plugin a route still binds
    /// returns 409 with the referencing route in the `referenced_by.routes`
    /// entry and an empty `upstreams` entry, and the plugin stays retrievable.
    #[tokio::test]
    async fn a_route_bound_plugin_delete_names_the_referencing_route() {
        let stores = InMemoryStores::new();
        let upstream_id = stored_upstream(&stores, tenant(), "payments.vendor.com");
        let router = surface(stores);
        let identifier = stored_plugin_identifier(router.clone()).await;

        let mut body = route_body(upstream_id, "/v1/chat", 10);
        body["plugins"] = json!({"items": [identifier.clone()]});
        let response = call(
            router.clone(),
            "POST",
            "/oagw/v1/routes",
            Some(tenant()),
            Some(body),
        )
        .await;
        let (_, route_id) = created_id(response).await;

        let response = call(
            router.clone(),
            "DELETE",
            &format!("/oagw/v1/plugins/{identifier}"),
            Some(tenant()),
            None,
        )
        .await;
        assert_problem(&response, 409, "plugin.in_use");
        let problem = json_of(response).await;
        assert_eq!(
            problem["type"],
            json!("gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1")
        );
        assert_eq!(problem["plugin_id"], json!(identifier));
        assert_eq!(
            problem["referenced_by"],
            json!({"upstreams": [], "routes": [route_id.to_string()]}),
            "the referencing route is named in the routes entry"
        );

        let response = call(
            router,
            "GET",
            &format!("/oagw/v1/plugins/{identifier}"),
            Some(tenant()),
            None,
        )
        .await;
        assert_eq!(response.status().as_u16(), 200, "the plugin stays stored");
    }

    /// §6: the upstream list clamps `$top=500` at 100 items rather than
    /// rejecting it, answers an omitted `$top` with at most 50, and rejects a
    /// malformed `$top` with the validation GTS type.
    #[tokio::test]
    async fn the_upstream_list_paging_is_clamped_and_a_malformed_top_is_rejected() {
        let stores = InMemoryStores::new();
        for index in 0..(list_query::TOP_MAX + 5) {
            stored_upstream(&stores, tenant(), &format!("tier-{index}.vendor.com"));
        }
        let router = surface(stores);

        let response = call(
            router.clone(),
            "GET",
            "/oagw/v1/upstreams?$top=500",
            Some(tenant()),
            None,
        )
        .await;
        assert_eq!(
            response.status().as_u16(),
            200,
            "a large top is not rejected"
        );
        let page = json_of(response).await;
        assert_eq!(
            page.as_array()
                .expect("the page is a bare JSON array")
                .len(),
            list_query::TOP_MAX,
            "a large top is clamped to the maximum"
        );

        let response = call(
            router.clone(),
            "GET",
            "/oagw/v1/upstreams",
            Some(tenant()),
            None,
        )
        .await;
        let page = json_of(response).await;
        assert_eq!(
            page.as_array().expect("the page is an array").len(),
            list_query::TOP_DEFAULT,
            "the omitted top takes the default of 50"
        );

        let response = call(
            router,
            "GET",
            "/oagw/v1/upstreams?$top=abc",
            Some(tenant()),
            None,
        )
        .await;
        assert_problem(&response, 400, "validation.error");
        let problem = json_of(response).await;
        assert_eq!(
            problem["type"],
            json!("gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1")
        );
    }
}
