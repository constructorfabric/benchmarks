//! Management service for upstreams and routes
//! ([DESIGN.md](../../docs/DESIGN.md) `cpt-cf-oagw-interface-api`).
//!
//! Owns every management-plane rule the phase-1 [`Store`] cannot express on its
//! own: server-generated ids, alias enforcement and per-tenant uniqueness,
//! endpoint-pool consistency, route upstream ownership and match-rule
//! uniqueness, full-replacement `PUT` semantics, enable/disable state, the
//! `OData` list parameters and the tenant-hierarchy alias walk. The REST layer
//! (`crate::api::rest`) is a thin adapter over this type.
//!
//! Every failure is a [`ServiceError`]; the REST boundary maps it onto
//! `CanonicalError`, which the canonical error middleware renders as an RFC 9457
//! `Problem`.

use std::collections::HashSet;
use std::sync::Arc;

use dashmap::DashMap;
use tenant_resolver_sdk::{BarrierMode, GetAncestorsOptions, TenantId, TenantResolverClient};
use toolkit::ClientHub;
use toolkit_canonical_errors::{CanonicalError, resource_error};
use toolkit_gts::gts_id;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::alias;
use crate::domain::error::{OagwAliasConflict, OagwError, OagwValidationError};
use crate::domain::model::{
    Alias, CorsConfig, Protocol, Route, RouteMatch, SharingMode, Tag, Upstream, UpstreamServer,
};
use crate::infra::store::Store;

/// 409 route-conflict GTS instance id: `DESIGN.md` "CRUD Semantics" makes the
/// match rule unique within an upstream, and the phase names
/// `cf.oagw.route.conflict.v1`.
pub const ROUTE_CONFLICT_GTS_ID: &str = gts_id!("cf.core.errors.err.v1~cf.oagw.route.conflict.v1");

/// 409 — a route's match rule is already used within its upstream.
#[resource_error(gts_id!("cf.core.errors.err.v1~cf.oagw.route.conflict.v1~"))]
pub struct OagwRouteConflict;

/// Resource marker of a management upstream (`cf.core.oagw.upstream.v1`).
#[resource_error(gts_id!("cf.core.oagw.upstream.v1~"))]
pub struct UpstreamResource;

/// Resource marker of a management route (`cf.core.oagw.route.v1`).
#[resource_error(gts_id!("cf.core.oagw.route.v1~"))]
pub struct RouteResource;

/// Default page size of a list endpoint (`$top`).
pub const DEFAULT_LIST_TOP: usize = 50;
/// Maximum accepted page size of a list endpoint; a larger `$top` is a 400.
pub const MAX_LIST_TOP: usize = 100;

/// Filterable and sortable fields of the upstream list endpoint.
const UPSTREAM_LIST_FIELDS: &[&str] = &["alias", "enabled", "protocol"];
/// Filterable and sortable fields of the route list endpoint.
const ROUTE_LIST_FIELDS: &[&str] = &["upstream_id", "path"];

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// One rejected request field, rendered as a problem-context field violation.
///
/// The phase's "`invalid_fields` in the problem context" is spelled
/// `field_violations` by the canonical problem model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedField {
    /// Path of the rejected field, in dot notation with JSON array indices
    /// (e.g. `server.endpoints[0].port`).
    pub field: String,
    /// Human-readable, client-safe description of the violation.
    pub description: String,
    /// Canonical reason code (e.g. `REQUIRED`, `INVALID_VALUE`, `IMMUTABLE`).
    pub reason: &'static str,
}

impl RejectedField {
    /// Builds one rejected field.
    fn new(field: &str, description: impl Into<String>, reason: &'static str) -> Self {
        Self {
            field: field.to_owned(),
            description: description.into(),
            reason,
        }
    }
}

/// Error of the management service, mapped onto [`CanonicalError`] at the REST
/// boundary. Statuses follow the error table of `DESIGN.md`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServiceError {
    /// 400 `cf.oagw.validation.error.v1` — payload or query parameter rejected;
    /// `fields` carries the problem's `field_violations`.
    #[error("validation failed: {message}")]
    Validation {
        /// Human-readable, client-safe summary of the violation.
        message: String,
        /// Rejected request fields.
        fields: Vec<RejectedField>,
    },
    /// 404 — no upstream with this id in the calling tenant.
    #[error("upstream '{id}' not found")]
    UpstreamNotFound {
        /// Requested upstream id.
        id: Uuid,
    },
    /// 404 — no route with this id in the calling tenant.
    #[error("route '{id}' not found")]
    RouteNotFound {
        /// Requested route id.
        id: Uuid,
    },
    /// 409 `cf.oagw.alias.conflict.v1` — the alias is taken within the tenant
    /// or claimed by an enforced ancestor upstream.
    #[error("alias '{alias}' conflict: {detail}")]
    AliasConflict {
        /// Conflicting alias.
        alias: String,
        /// Human-readable, client-safe detail of the conflict.
        detail: String,
    },
    /// 409 `cf.oagw.route.conflict.v1` — the match rule is already used.
    #[error("route match rule already in use: {detail}")]
    RouteConflict {
        /// Id of the upstream whose match rule is already used.
        resource: String,
        /// Human-readable, client-safe detail of the conflict.
        detail: String,
    },
    /// 503 — the tenant hierarchy could not be resolved.
    #[error("tenant hierarchy unavailable: {detail}")]
    TenantHierarchy {
        /// Human-readable, client-safe detail of the outage.
        detail: String,
    },
    /// A phase-1 domain error: one row of the `DESIGN.md` error table.
    #[error(transparent)]
    Domain(#[from] OagwError),
}

impl ServiceError {
    /// 400 for a single rejected field.
    #[must_use]
    pub fn invalid(field: &str, description: impl Into<String>, reason: &'static str) -> Self {
        let description = description.into();
        Self::Validation {
            message: format!("{field}: {description}"),
            fields: vec![RejectedField::new(field, description, reason)],
        }
    }

    /// 400 for a missing required field.
    #[must_use]
    pub fn required(field: &str) -> Self {
        Self::invalid(field, "this field is required", "REQUIRED")
    }

    /// 404 for a missing upstream of the calling tenant.
    #[must_use]
    pub fn upstream_not_found(id: Uuid) -> Self {
        Self::UpstreamNotFound { id }
    }

    /// 404 for a missing route of the calling tenant.
    #[must_use]
    pub fn route_not_found(id: Uuid) -> Self {
        Self::RouteNotFound { id }
    }

    /// 409 for an alias that is already taken.
    #[must_use]
    pub fn alias_conflict(alias: &Alias, detail: impl Into<String>) -> Self {
        Self::AliasConflict {
            alias: alias.to_string(),
            detail: detail.into(),
        }
    }

    /// 409 for a match rule that is already used within the upstream.
    #[must_use]
    pub fn route_conflict(upstream_id: Uuid, detail: impl Into<String>) -> Self {
        Self::RouteConflict {
            resource: upstream_id.to_string(),
            detail: detail.into(),
        }
    }
}

impl From<ServiceError> for CanonicalError {
    fn from(error: ServiceError) -> Self {
        match error {
            ServiceError::Validation { message, fields } if fields.is_empty() => {
                OagwValidationError::invalid_argument()
                    .with_format(message)
                    .create()
            }
            ServiceError::Validation { fields, .. } => {
                let mut fields = fields.into_iter();
                let Some(first) = fields.next() else {
                    return OagwValidationError::invalid_argument()
                        .with_format("validation failed")
                        .create();
                };
                let mut builder = OagwValidationError::invalid_argument().with_field_violation(
                    first.field,
                    first.description,
                    first.reason,
                );
                for field in fields {
                    builder =
                        builder.with_field_violation(field.field, field.description, field.reason);
                }
                builder.create()
            }
            ServiceError::UpstreamNotFound { id } => {
                UpstreamResource::not_found(format!("no upstream '{id}' in the calling tenant"))
                    .with_resource(id.to_string())
                    .create()
            }
            ServiceError::RouteNotFound { id } => {
                RouteResource::not_found(format!("no route '{id}' in the calling tenant"))
                    .with_resource(id.to_string())
                    .create()
            }
            ServiceError::AliasConflict { alias, detail } => {
                OagwAliasConflict::already_exists(detail)
                    .with_resource(alias)
                    .create()
            }
            ServiceError::RouteConflict { resource, detail } => {
                OagwRouteConflict::already_exists(detail)
                    .with_resource(resource)
                    .create()
            }
            ServiceError::TenantHierarchy { detail } => CanonicalError::service_unavailable()
                .with_detail(detail)
                .create(),
            ServiceError::Domain(error) => CanonicalError::from(error),
        }
    }
}

// ---------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------

/// Wire-independent create/replace payload of an upstream.
///
/// Configuration blocks (`server`, `auth`, `headers`, `plugins`, `rate_limit`,
/// `cors`) travel as JSON and are validated against the phase-1 domain types, so
/// a malformed block is reported as a canonical 400 naming the field instead of
/// an opaque parser rejection.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UpstreamInput {
    /// Whether the upstream accepts requests; defaults to `true`.
    pub enabled: Option<bool>,
    /// Explicit alias; derivation decides when it is absent.
    pub alias: Option<String>,
    /// Categorization tags; empty when omitted.
    pub tags: Option<Vec<String>>,
    /// Endpoint pool (required).
    pub server: Option<serde_json::Value>,
    /// Upstream protocol (required).
    pub protocol: Option<String>,
    /// Authentication plugin binding.
    pub auth: Option<serde_json::Value>,
    /// Header transformation rules.
    pub headers: Option<serde_json::Value>,
    /// Plugin chain.
    pub plugins: Option<serde_json::Value>,
    /// Rate limiting configuration.
    pub rate_limit: Option<serde_json::Value>,
    /// CORS configuration.
    pub cors: Option<serde_json::Value>,
}

/// Wire-independent create/replace payload of a route.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouteInput {
    /// Categorization tags; empty when omitted.
    pub tags: Option<Vec<String>>,
    /// Owning upstream; required on create and immutable on replace.
    pub upstream_id: Option<Uuid>,
    /// Protocol-scoped matching rules (required).
    pub match_rule: Option<serde_json::Value>,
    /// Plugin chain.
    pub plugins: Option<serde_json::Value>,
    /// Rate limiting configuration.
    pub rate_limit: Option<serde_json::Value>,
    /// CORS configuration.
    pub cors: Option<serde_json::Value>,
}

/// Raw list query parameters, in the spelling used on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListParams {
    /// `$filter`.
    pub filter: Option<String>,
    /// `$select`.
    pub select: Option<String>,
    /// `$orderby`.
    pub orderby: Option<String>,
    /// `$top`.
    pub top: Option<String>,
    /// `$skip`.
    pub skip: Option<String>,
}

/// A parsed `OData` filter comparison (`field op value`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterExpr {
    /// Left-hand side field name.
    pub field: String,
    /// Comparison operator.
    pub op: FilterOp,
    /// Right-hand side literal, quotes stripped.
    pub value: String,
}

/// Comparison operator of a [`FilterExpr`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    /// `eq`.
    Eq,
    /// `ne`.
    Ne,
}

/// A parsed `$orderby` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    /// Field to sort by.
    pub field: String,
    /// `true` when the clause asked for `desc`.
    pub descending: bool,
}

/// Parsed `OData` list query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListQuery {
    /// Parsed `$filter`.
    pub filter: Option<FilterExpr>,
    /// Projected field names of `$select`.
    pub select: Option<Vec<String>>,
    /// Parsed `$orderby`.
    pub orderby: Option<OrderBy>,
    /// Page size (default [`DEFAULT_LIST_TOP`], at most [`MAX_LIST_TOP`]).
    pub top: usize,
    /// Offset of the first returned record.
    pub skip: usize,
}

impl ListQuery {
    /// Parses the wire parameters.
    ///
    /// # Errors
    /// [`ServiceError::Validation`] when `$top`/`$skip` are not non-negative
    /// integers, `$top` exceeds [`MAX_LIST_TOP`], or `$filter`/`$orderby` are
    /// not supported expressions.
    pub fn parse(params: &ListParams) -> Result<Self, ServiceError> {
        Ok(Self {
            filter: params.filter.as_deref().map(parse_filter).transpose()?,
            select: params.select.as_deref().map(projected_fields),
            orderby: params.orderby.as_deref().map(parse_orderby).transpose()?,
            top: match &params.top {
                None => DEFAULT_LIST_TOP,
                Some(raw) => parse_page_field("$top", raw)?,
            },
            skip: match &params.skip {
                None => 0,
                Some(raw) => parse_page_field("$skip", raw)?,
            },
        })
    }
}

/// Parses `$top` / `$skip`.
fn parse_page_field(field: &str, raw: &str) -> Result<usize, ServiceError> {
    let value = raw.trim().parse::<usize>().map_err(|_| {
        ServiceError::invalid(
            field,
            format!("'{raw}' is not a non-negative integer"),
            "INVALID_VALUE",
        )
    })?;
    if field == "$top" && value > MAX_LIST_TOP {
        return Err(ServiceError::invalid(
            field,
            format!("'{value}' exceeds the maximum page size of {MAX_LIST_TOP}"),
            "OUT_OF_RANGE",
        ));
    }
    Ok(value)
}

/// Splits a `$select` list into trimmed, non-empty field names.
fn projected_fields(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// Parses `$filter` into a single `field op value` comparison.
fn parse_filter(raw: &str) -> Result<FilterExpr, ServiceError> {
    let parts: Vec<&str> = raw.split_whitespace().collect();
    let [field, op, value] = parts.as_slice() else {
        return Err(ServiceError::invalid(
            "$filter",
            format!("'{raw}' is not a supported expression: expected `<field> eq|ne <value>`"),
            "INVALID_VALUE",
        ));
    };
    let op = match *op {
        "eq" => FilterOp::Eq,
        "ne" => FilterOp::Ne,
        other => {
            return Err(ServiceError::invalid(
                "$filter",
                format!("unsupported operator '{other}': expected 'eq' or 'ne'"),
                "INVALID_VALUE",
            ));
        }
    };
    Ok(FilterExpr {
        field: (*field).to_owned(),
        op,
        value: value.trim_matches('\'').to_owned(),
    })
}

/// Parses `$orderby` as `field [asc|desc]`.
fn parse_orderby(raw: &str) -> Result<OrderBy, ServiceError> {
    let mut parts = raw.split_whitespace();
    let Some(field) = parts.next() else {
        return Err(ServiceError::invalid(
            "$orderby",
            "must name a field",
            "INVALID_VALUE",
        ));
    };
    let direction = parts.next().unwrap_or("asc");
    if parts.next().is_some() || !matches!(direction, "asc" | "desc") {
        return Err(ServiceError::invalid(
            "$orderby",
            format!("'{raw}' is not a supported ordering: expected `<field> [asc|desc]`"),
            "INVALID_VALUE",
        ));
    }
    Ok(OrderBy {
        field: field.to_owned(),
        descending: direction == "desc",
    })
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// A route with its management-plane enablement state.
///
/// `Route` mirrors `route.v1.schema.json`, which has no `enabled` property, so
/// the management API reports the state separately; the data plane consults it
/// before matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteRecord {
    /// The stored route.
    pub route: Route,
    /// Whether the route participates in matching.
    pub enabled: bool,
}

/// Management service: the CRUD use cases of `DESIGN.md` over the phase-1
/// [`Store`], plus the alias and hierarchy rules the store cannot express.
///
/// No lock is held across an `await`: the only `async` calls resolve the tenant
/// hierarchy before any store access happens.
pub struct Service {
    /// In-memory per-tenant store.
    store: Arc<Store>,
    /// Client hub the tenant hierarchy is resolved through.
    client_hub: Arc<ClientHub>,
    /// Route ids the operator disabled, per tenant (see [`RouteRecord`]).
    disabled_routes: DashMap<Uuid, HashSet<Uuid>>,
}

impl Service {
    /// Creates a service over `store`, resolving the tenant hierarchy through
    /// `client_hub`.
    #[must_use]
    pub fn new(store: Arc<Store>, client_hub: Arc<ClientHub>) -> Self {
        Self {
            store,
            client_hub,
            disabled_routes: DashMap::new(),
        }
    }

    /// The store the service operates on.
    #[must_use]
    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// The caller's tenant chain: the caller's tenant first, then its ancestors
    /// from the direct parent to the root.
    ///
    /// A missing `TenantResolverClient` means the caller's tenant is a leaf.
    async fn ancestor_chain(&self, ctx: &SecurityContext) -> Result<Vec<Uuid>, ServiceError> {
        let tenant = ctx.subject_tenant_id();
        let Some(client) = self.client_hub.try_get::<dyn TenantResolverClient>() else {
            return Ok(vec![tenant]);
        };
        let options = GetAncestorsOptions {
            barrier_mode: BarrierMode::Ignore,
        };
        let response = client
            .get_ancestors(ctx, TenantId(tenant), &options)
            .await
            .map_err(|error| ServiceError::TenantHierarchy {
                detail: error.to_string(),
            })?;
        let mut chain = Vec::with_capacity(response.ancestors.len() + 1);
        chain.push(tenant);
        chain.extend(response.ancestors.iter().map(|ancestor| ancestor.id.0));
        Ok(chain)
    }

    // -- upstreams ----------------------------------------------------------

    /// Creates an upstream owned by the caller's tenant.
    ///
    /// # Errors
    /// [`ServiceError::Validation`] for an invalid payload, 409 for an alias
    /// that is already in use or enforced by an ancestor, and 503 when the
    /// tenant hierarchy cannot be read.
    pub async fn create_upstream(
        &self,
        ctx: &SecurityContext,
        input: UpstreamInput,
    ) -> Result<Upstream, ServiceError> {
        let tenant = ctx.subject_tenant_id();
        let (mut upstream, resolved) = build_upstream(&input)?;
        self.assert_alias_available(ctx, tenant, &resolved, None)
            .await?;
        upstream.id = Some(Uuid::new_v4());
        self.store.put_upstream(tenant, upstream.clone());
        Ok(upstream)
    }

    /// Returns the upstream with `id` owned by `tenant`.
    ///
    /// # Errors
    /// [`ServiceError::UpstreamNotFound`] when the tenant has no such upstream.
    pub fn get_upstream(&self, tenant: Uuid, id: Uuid) -> Result<Upstream, ServiceError> {
        self.store
            .get_upstream(tenant, id)
            .ok_or_else(|| ServiceError::upstream_not_found(id))
    }

    /// Lists the upstreams of `tenant`, filtered, ordered and paged by `query`.
    ///
    /// # Errors
    /// [`ServiceError::Validation`] when `$filter` or `$orderby` names an
    /// unsupported field.
    pub fn list_upstreams(
        &self,
        tenant: Uuid,
        query: &ListQuery,
    ) -> Result<Vec<Upstream>, ServiceError> {
        let mut items = self.store.list_upstreams(tenant);
        apply_filter(
            &mut items,
            query.filter.as_ref(),
            UPSTREAM_LIST_FIELDS,
            upstream_field,
        )?;
        apply_order(
            &mut items,
            query.orderby.as_ref(),
            UPSTREAM_LIST_FIELDS,
            upstream_field,
        )?;
        Ok(items.into_iter().skip(query.skip).take(query.top).collect())
    }

    /// Replaces an upstream: full replacement of every field except `id`,
    /// `tenant_id` and the alias.
    ///
    /// # Errors
    /// [`ServiceError::UpstreamNotFound`] for an unknown id,
    /// [`ServiceError::Validation`] for an invalid payload or an alias
    /// transition the alias matrix rejects, 409 for an enforced ancestor
    /// upstream claiming the alias, and 503 when the hierarchy cannot be read.
    pub async fn replace_upstream(
        &self,
        ctx: &SecurityContext,
        tenant: Uuid,
        id: Uuid,
        input: UpstreamInput,
    ) -> Result<Upstream, ServiceError> {
        let existing = self.get_upstream(tenant, id)?;
        let existing_alias = existing.alias.clone().ok_or_else(|| {
            ServiceError::invalid("alias", "the stored upstream has no alias", "INVALID_VALUE")
        })?;
        let provided = parse_alias(input.alias.as_deref())?;
        let (mut upstream, _) = build_upstream(&input)?;
        upstream.alias = Some(alias::resolve_replacement(
            &existing_alias,
            alias::derive(&existing.server.endpoints).is_some(),
            provided,
            &upstream.server.endpoints,
        )?);
        upstream.id = existing.id;
        // The alias cannot change, so the availability check only guards the
        // ancestor bind constraints; `except` keeps the replaced record out.
        let resolved = upstream.alias.clone().ok_or_else(|| {
            ServiceError::invalid("alias", "the alias could not be determined", "REQUIRED")
        })?;
        self.assert_alias_available(ctx, tenant, &resolved, existing.id)
            .await?;
        self.store.put_upstream(tenant, upstream.clone());
        Ok(upstream)
    }

    /// Deletes the upstream with `id` and every route that references it.
    ///
    /// # Errors
    /// [`ServiceError::UpstreamNotFound`] when the upstream does not belong to
    /// `tenant`.
    pub fn delete_upstream(&self, tenant: Uuid, id: Uuid) -> Result<(), ServiceError> {
        if !self.store.delete_upstream(tenant, id) {
            return Err(ServiceError::upstream_not_found(id));
        }
        for route in self.store.find_routes_by_upstream(tenant, id) {
            let Some(route_id) = route.id else { continue };
            self.store.delete_route(tenant, route_id);
            if let Some(mut disabled) = self.disabled_routes.get_mut(&tenant) {
                disabled.remove(&route_id);
            }
        }
        Ok(())
    }

    /// Enables or disables an upstream; a disabled upstream yields 503 on the
    /// proxy path.
    ///
    /// # Errors
    /// [`ServiceError::UpstreamNotFound`] when the upstream does not belong to
    /// `tenant`.
    pub fn set_upstream_enabled(
        &self,
        tenant: Uuid,
        id: Uuid,
        enabled: bool,
    ) -> Result<Upstream, ServiceError> {
        let mut upstream = self.get_upstream(tenant, id)?;
        upstream.enabled = enabled;
        self.store.put_upstream(tenant, upstream.clone());
        Ok(upstream)
    }

    // -- routes -------------------------------------------------------------

    /// Creates a route owned by the caller's tenant.
    ///
    /// # Errors
    /// [`ServiceError::Validation`] for an invalid payload, 404 when
    /// `upstream_id` does not belong to the caller's tenant (ancestor upstreams
    /// are not addressable), and 409 when the match rule is already used.
    pub fn create_route(&self, tenant: Uuid, input: RouteInput) -> Result<Route, ServiceError> {
        let mut route = build_route(&input)?;
        let upstream_id = route.upstream_id;
        if self.store.get_upstream(tenant, upstream_id).is_none() {
            return Err(ServiceError::upstream_not_found(upstream_id));
        }
        self.assert_match_rule_free(tenant, upstream_id, &route.match_rule, None)?;
        route.id = Some(Uuid::new_v4());
        self.store.put_route(tenant, route.clone());
        Ok(route)
    }

    /// Returns the route with `id` owned by `tenant`, with its enablement.
    ///
    /// # Errors
    /// [`ServiceError::RouteNotFound`] when the route does not belong to
    /// `tenant`.
    pub fn get_route(&self, tenant: Uuid, id: Uuid) -> Result<RouteRecord, ServiceError> {
        let route = self
            .store
            .get_route(tenant, id)
            .ok_or_else(|| ServiceError::route_not_found(id))?;
        Ok(RouteRecord {
            enabled: self.is_route_enabled(tenant, id),
            route,
        })
    }

    /// Lists the routes of `tenant`, filtered, ordered and paged by `query`.
    ///
    /// # Errors
    /// [`ServiceError::Validation`] when `$filter` or `$orderby` names an
    /// unsupported field.
    pub fn list_routes(
        &self,
        tenant: Uuid,
        query: &ListQuery,
    ) -> Result<Vec<RouteRecord>, ServiceError> {
        let mut records: Vec<RouteRecord> = self
            .store
            .list_routes(tenant)
            .into_iter()
            .map(|route| {
                let enabled = route.id.is_some_and(|id| self.is_route_enabled(tenant, id));
                RouteRecord { enabled, route }
            })
            .collect();
        if let Some(filter) = &query.filter {
            assert_filter_field(&filter.field, ROUTE_LIST_FIELDS)?;
            records.retain(|record| route_matches(&record.route, filter));
        }
        apply_order(
            &mut records,
            query.orderby.as_ref(),
            ROUTE_LIST_FIELDS,
            |record, field| route_field(&record.route, field),
        )?;
        Ok(records
            .into_iter()
            .skip(query.skip)
            .take(query.top)
            .collect())
    }

    /// Replaces a route: full replacement of every field except `id` and
    /// `upstream_id`.
    ///
    /// # Errors
    /// [`ServiceError::RouteNotFound`] for an unknown id,
    /// [`ServiceError::Validation`] for an invalid payload or an attempt to
    /// change `upstream_id`, and 409 when the new match rule is already used.
    pub fn replace_route(
        &self,
        tenant: Uuid,
        id: Uuid,
        input: RouteInput,
    ) -> Result<Route, ServiceError> {
        let existing = self
            .store
            .get_route(tenant, id)
            .ok_or_else(|| ServiceError::route_not_found(id))?;
        if let Some(provided) = input.upstream_id
            && provided != existing.upstream_id
        {
            return Err(ServiceError::invalid(
                "upstream_id",
                format!(
                    "'{provided}' does not match the existing upstream '{}': upstream_id is \
                     immutable",
                    existing.upstream_id
                ),
                "IMMUTABLE",
            ));
        }
        let mut route = build_route(&RouteInput {
            upstream_id: Some(existing.upstream_id),
            ..input
        })?;
        self.assert_match_rule_free(tenant, existing.upstream_id, &route.match_rule, Some(id))?;
        route.id = Some(id);
        self.store.put_route(tenant, route.clone());
        Ok(route)
    }

    /// Deletes the route with `id`.
    ///
    /// # Errors
    /// [`ServiceError::RouteNotFound`] when the route does not belong to
    /// `tenant`.
    pub fn delete_route(&self, tenant: Uuid, id: Uuid) -> Result<(), ServiceError> {
        if !self.store.delete_route(tenant, id) {
            return Err(ServiceError::route_not_found(id));
        }
        if let Some(mut disabled) = self.disabled_routes.get_mut(&tenant) {
            disabled.remove(&id);
        }
        Ok(())
    }

    /// Enables or disables a route; a disabled route is excluded from matching.
    ///
    /// # Errors
    /// [`ServiceError::RouteNotFound`] when the route does not belong to
    /// `tenant`.
    pub fn set_route_enabled(
        &self,
        tenant: Uuid,
        id: Uuid,
        enabled: bool,
    ) -> Result<RouteRecord, ServiceError> {
        let record = self.get_route(tenant, id)?;
        {
            let mut disabled = self.disabled_routes.entry(tenant).or_default();
            if enabled {
                disabled.remove(&id);
            } else {
                disabled.insert(id);
            }
        }
        Ok(RouteRecord {
            enabled,
            route: record.route,
        })
    }

    /// `true` when the route participates in matching.
    #[must_use]
    pub fn is_route_enabled(&self, tenant: Uuid, id: Uuid) -> bool {
        self.disabled_routes
            .get(&tenant)
            .is_none_or(|disabled| !disabled.contains(&id))
    }

    // -- alias resolution ---------------------------------------------------

    /// Resolves `alias` along the caller's tenant chain: the closest match
    /// wins, so an exact alias in the caller's tenant beats an inherited one.
    ///
    /// # Errors
    /// [`ServiceError::TenantHierarchy`] when the chain cannot be read.
    pub async fn resolve_upstream(
        &self,
        ctx: &SecurityContext,
        alias: &Alias,
    ) -> Result<Option<Upstream>, ServiceError> {
        let chain = self.ancestor_chain(ctx).await?;
        Ok(alias::closest_match(&chain, |tenant| {
            self.store.find_upstream_by_alias(tenant, alias)
        }))
    }

    /// Rejects an alias that is already used inside `tenant`, or that an
    /// ancestor upstream enforces: shadowing selects the routing target only,
    /// ancestor `enforce` constraints are never bypassed. `except` excludes the
    /// record being replaced from the uniqueness check.
    async fn assert_alias_available(
        &self,
        ctx: &SecurityContext,
        tenant: Uuid,
        alias: &Alias,
        except: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        if let Some(other) = self.store.find_upstream_by_alias(tenant, alias)
            && other.id != except
        {
            return Err(ServiceError::alias_conflict(
                alias,
                format!("alias '{alias}' is already in use in this tenant"),
            ));
        }
        for ancestor in self.ancestor_chain(ctx).await?.iter().skip(1) {
            let Some(ancestor_upstream) = self.store.find_upstream_by_alias(*ancestor, alias)
            else {
                continue;
            };
            if let Some(block) = enforced_block(&ancestor_upstream) {
                return Err(ServiceError::alias_conflict(
                    alias,
                    format!(
                        "ancestor upstream '{alias}' enforces '{block}' and cannot be shadowed"
                    ),
                ));
            }
        }
        Ok(())
    }

    /// Rejects a match rule that another route of the same upstream already
    /// uses; `except` is the id of the route being replaced.
    fn assert_match_rule_free(
        &self,
        tenant: Uuid,
        upstream_id: Uuid,
        rule: &RouteMatch,
        except: Option<Uuid>,
    ) -> Result<(), ServiceError> {
        let key = match_key(rule);
        let conflict = self
            .store
            .find_routes_by_upstream(tenant, upstream_id)
            .into_iter()
            .filter(|route| route.id != except)
            .any(|route| match_key(&route.match_rule) == key);
        if conflict {
            Err(ServiceError::route_conflict(
                upstream_id,
                format!("a route of upstream '{upstream_id}' already matches {key}"),
            ))
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Listing helpers
// ---------------------------------------------------------------------------

/// Applies `$filter` to `items`, rejecting unsupported field names.
fn apply_filter<T>(
    items: &mut Vec<T>,
    filter: Option<&FilterExpr>,
    allowed: &[&str],
    value_of: fn(&T, &str) -> Option<String>,
) -> Result<(), ServiceError> {
    let Some(filter) = filter else {
        return Ok(());
    };
    assert_filter_field(&filter.field, allowed)?;
    items.retain(|item| {
        let Some(value) = value_of(item, &filter.field) else {
            return filter.op == FilterOp::Ne;
        };
        let matched = value.eq_ignore_ascii_case(&filter.value);
        match filter.op {
            FilterOp::Eq => matched,
            FilterOp::Ne => !matched,
        }
    });
    Ok(())
}

/// Applies `$orderby` to `items`, rejecting unsupported field names.
fn apply_order<T>(
    items: &mut [T],
    orderby: Option<&OrderBy>,
    allowed: &[&str],
    key_of: fn(&T, &str) -> Option<String>,
) -> Result<(), ServiceError> {
    let Some(orderby) = orderby else {
        return Ok(());
    };
    assert_filter_field(&orderby.field, allowed)?;
    items.sort_by(|left, right| {
        let ordering = key_of(left, &orderby.field)
            .unwrap_or_default()
            .cmp(&key_of(right, &orderby.field).unwrap_or_default());
        if orderby.descending {
            ordering.reverse()
        } else {
            ordering
        }
    });
    Ok(())
}

/// Rejects a `$filter` / `$orderby` field the resource does not expose.
fn assert_filter_field(field: &str, allowed: &[&str]) -> Result<(), ServiceError> {
    if allowed.contains(&field) {
        Ok(())
    } else {
        Err(ServiceError::invalid(
            "$filter",
            format!(
                "unknown field '{field}': expected one of {}",
                allowed.join(", ")
            ),
            "INVALID_VALUE",
        ))
    }
}

/// The value of an upstream list field, `None` for an unknown field.
fn upstream_field(upstream: &Upstream, field: &str) -> Option<String> {
    match field {
        "alias" => Some(
            upstream
                .alias
                .as_ref()
                .map(Alias::to_string)
                .unwrap_or_default(),
        ),
        "enabled" => Some(upstream.enabled.to_string()),
        "protocol" => Some(upstream.protocol.gts_id().to_owned()),
        _ => None,
    }
}

/// The value of a route list field, `None` for an unknown field or for a
/// gRPC route asked for an HTTP-only field.
fn route_field(route: &Route, field: &str) -> Option<String> {
    match field {
        "upstream_id" => Some(route.upstream_id.to_string()),
        "path" => route.match_rule.http.as_ref().map(|http| http.path.clone()),
        _ => None,
    }
}

/// `true` when `route` satisfies `filter`.
fn route_matches(route: &Route, filter: &FilterExpr) -> bool {
    let Some(value) = route_field(route, &filter.field) else {
        return filter.op == FilterOp::Ne;
    };
    let matched = value.eq_ignore_ascii_case(&filter.value);
    match filter.op {
        FilterOp::Eq => matched,
        FilterOp::Ne => !matched,
    }
}

// ---------------------------------------------------------------------------
// Payload construction
// ---------------------------------------------------------------------------

/// Validates `input` and builds a stored upstream together with its resolved
/// alias; `id` is left to the caller.
///
/// # Errors
/// [`ServiceError::Validation`] naming the first violated field.
fn build_upstream(input: &UpstreamInput) -> Result<(Upstream, Alias), ServiceError> {
    let server = build_server(
        input
            .server
            .as_ref()
            .ok_or_else(|| ServiceError::required("server"))?,
    )?;
    let provided = parse_alias(input.alias.as_deref())?;
    let resolved = alias::resolve_new(provided, &server.endpoints)?;
    let resolved_alias = resolved.alias.clone();
    Ok((
        Upstream {
            id: None,
            enabled: input.enabled.unwrap_or(true),
            alias: Some(resolved_alias.clone()),
            tags: parse_tags(input.tags.as_deref())?,
            server,
            protocol: parse_protocol(input.protocol.as_deref())?,
            auth: parse_block(input.auth.as_ref(), "auth")?,
            headers: parse_block(input.headers.as_ref(), "headers")?,
            plugins: parse_block(input.plugins.as_ref(), "plugins")?,
            rate_limit: parse_block(input.rate_limit.as_ref(), "rate_limit")?,
            cors: build_cors(input.cors.as_ref())?,
        },
        resolved_alias,
    ))
}

/// Validates `input` and builds a stored route; `id` is left to the caller.
///
/// # Errors
/// [`ServiceError::Validation`] naming the first violated field.
fn build_route(input: &RouteInput) -> Result<Route, ServiceError> {
    let upstream_id = input
        .upstream_id
        .ok_or_else(|| ServiceError::required("upstream_id"))?;
    let raw = input
        .match_rule
        .as_ref()
        .ok_or_else(|| ServiceError::required("match"))?;
    let match_rule = parse_block::<RouteMatch>(Some(raw), "match")?
        .ok_or_else(|| ServiceError::required("match"))?;
    match_rule
        .validate()
        .map_err(|error| ServiceError::invalid("match", error.to_string(), "INVALID_VALUE"))?;
    Ok(Route {
        id: None,
        tags: parse_tags(input.tags.as_deref())?,
        upstream_id,
        match_rule,
        plugins: parse_block(input.plugins.as_ref(), "plugins")?,
        rate_limit: parse_block(input.rate_limit.as_ref(), "rate_limit")?,
        cors: build_cors(input.cors.as_ref())?,
    })
}

/// Builds the endpoint pool: shape, hostnames, port range and pool consistency.
///
/// # Errors
/// [`ServiceError::Validation`] naming the violated field.
fn build_server(raw: &serde_json::Value) -> Result<UpstreamServer, ServiceError> {
    // The port range is checked on the raw payload first: the endpoint `port`
    // is a `u16`, so an out-of-range value would otherwise surface as an
    // opaque deserialization error on the whole `server` block.
    if let Some(endpoints) = raw.get("endpoints").and_then(serde_json::Value::as_array) {
        for (index, endpoint) in endpoints.iter().enumerate() {
            let field = format!("server.endpoints[{index}].port");
            let Some(port) = endpoint.get("port").and_then(serde_json::Value::as_i64) else {
                return Err(ServiceError::invalid(
                    &field,
                    "the port must be an integer between 1 and 65535",
                    "INVALID_VALUE",
                ));
            };
            if !(1..=u16::MAX as i64).contains(&port) {
                return Err(ServiceError::invalid(
                    &field,
                    format!("'{port}' is outside the port range 1-65535"),
                    "OUT_OF_RANGE",
                ));
            }
        }
    }
    let server = parse_block::<UpstreamServer>(Some(raw), "server")?
        .ok_or_else(|| ServiceError::required("server"))?;
    server
        .validate()
        .map_err(|error| ServiceError::invalid("server", error.to_string(), "INVALID_VALUE"))?;
    for (index, endpoint) in server.endpoints.iter().enumerate() {
        let field = format!("server.endpoints[{index}]");
        if !alias::is_valid_hostname(&endpoint.host) && !alias::is_ip_literal(&endpoint.host) {
            return Err(ServiceError::invalid(
                &format!("{field}.host"),
                format!(
                    "'{}' is not a valid RFC 1123 hostname or IP address",
                    endpoint.host
                ),
                "INVALID_VALUE",
            ));
        }
        if endpoint.port == 0 {
            return Err(ServiceError::invalid(
                &format!("{field}.port"),
                "the port must be between 1 and 65535",
                "OUT_OF_RANGE",
            ));
        }
    }
    let Some(first) = server.endpoints.first() else {
        return Ok(server);
    };
    if server
        .endpoints
        .iter()
        .any(|endpoint| endpoint.scheme != first.scheme || endpoint.port != first.port)
    {
        return Err(ServiceError::invalid(
            "server.endpoints",
            "every endpoint of a pool must share the same scheme and port",
            "INVALID_VALUE",
        ));
    }
    Ok(server)
}

/// Builds the CORS configuration, rejecting the credentials/wildcard
/// combination the schema forbids.
///
/// # Errors
/// [`ServiceError::Validation`] naming `cors` or `cors.allow_credentials`.
fn build_cors(raw: Option<&serde_json::Value>) -> Result<Option<CorsConfig>, ServiceError> {
    let Some(cors) = parse_block::<CorsConfig>(raw, "cors")? else {
        return Ok(None);
    };
    let wildcard = cors.allowed_origins.iter().any(|origin| origin == "*");
    if cors.allow_credentials && wildcard {
        return Err(ServiceError::invalid(
            "cors.allow_credentials",
            "credentials cannot be combined with the wildcard origin '*'",
            "INVALID_VALUE",
        ));
    }
    Ok(Some(cors))
}

/// Deserializes an optional configuration block, reporting a malformed value as
/// a field violation of `field`.
///
/// # Errors
/// [`ServiceError::Validation`] when the block is present but malformed.
fn parse_block<T: serde::de::DeserializeOwned>(
    raw: Option<&serde_json::Value>,
    field: &str,
) -> Result<Option<T>, ServiceError> {
    match raw {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(raw) => serde_json::from_value::<T>(raw.clone())
            .map(Some)
            .map_err(|error| ServiceError::invalid(field, error.to_string(), "INVALID_VALUE")),
    }
}

/// Validates the tags of a payload against the tag pattern.
///
/// # Errors
/// [`ServiceError::Validation`] naming `tags`.
fn parse_tags(tags: Option<&[String]>) -> Result<Vec<Tag>, ServiceError> {
    let Some(tags) = tags else {
        return Ok(Vec::new());
    };
    tags.iter()
        .map(|tag| {
            Tag::try_new(tag.as_str())
                .map_err(|error| ServiceError::invalid("tags", error.to_string(), "INVALID_VALUE"))
        })
        .collect()
}

/// Validates an explicit alias against the alias pattern.
///
/// # Errors
/// [`ServiceError::Validation`] naming `alias`.
fn parse_alias(raw: Option<&str>) -> Result<Option<Alias>, ServiceError> {
    match raw {
        None => Ok(None),
        Some(raw) => Alias::try_new(raw)
            .map(Some)
            .map_err(|error| ServiceError::invalid("alias", error.to_string(), "INVALID_VALUE")),
    }
}

/// Validates the upstream protocol.
///
/// # Errors
/// [`ServiceError::Validation`] naming `protocol`.
fn parse_protocol(raw: Option<&str>) -> Result<Protocol, ServiceError> {
    let Some(raw) = raw else {
        return Err(ServiceError::required("protocol"));
    };
    serde_json::from_value::<Protocol>(serde_json::Value::String(raw.to_owned())).map_err(|_| {
        ServiceError::invalid(
            "protocol",
            format!(
                "'{raw}' is not an OAGW protocol: expected '{}'",
                Protocol::Http.gts_id()
            ),
            "INVALID_VALUE",
        )
    })
}

/// The name of the first `enforce`d sharing block of `upstream`, if any.
fn enforced_block(upstream: &Upstream) -> Option<String> {
    let blocks = [
        ("auth", upstream.auth.as_ref().map(|block| block.sharing)),
        (
            "plugins",
            upstream.plugins.as_ref().map(|block| block.sharing),
        ),
        (
            "rate_limit",
            upstream.rate_limit.as_ref().map(|block| block.sharing),
        ),
        ("cors", upstream.cors.as_ref().map(|block| block.sharing)),
    ];
    blocks.iter().find_map(|(name, sharing)| {
        (*sharing == Some(SharingMode::Enforce)).then(|| (*name).to_owned())
    })
}

/// Stable identity of a match rule: the `(tenant, upstream, match rule)`
/// uniqueness key of `DESIGN.md` "CRUD Semantics".
#[must_use]
pub fn match_key(rule: &RouteMatch) -> String {
    match (&rule.http, &rule.grpc) {
        (Some(http), _) => {
            let mut methods: Vec<&str> =
                http.methods.iter().map(|method| method.as_str()).collect();
            methods.sort_unstable();
            format!("http {} {}", methods.join(","), http.path)
        }
        (None, Some(grpc)) => format!("grpc {} {}", grpc.service, grpc.method),
        (None, None) => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use std::collections::HashMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use tenant_resolver_sdk::{
        GetAncestorsOptions, GetAncestorsResponse, GetDescendantsOptions, GetDescendantsResponse,
        TenantId, TenantInfo, TenantRef, TenantResolverClient, TenantResolverError, TenantStatus,
    };
    use toolkit::ClientHub;
    use toolkit_canonical_errors::{CanonicalError, Problem};
    use toolkit_security::SecurityContext;

    use super::{
        DEFAULT_LIST_TOP, ListParams, ListQuery, MAX_LIST_TOP, RouteInput, Service, ServiceError,
        UpstreamInput, match_key,
    };
    use crate::domain::model::{
        GrpcMatch, HttpMatch, HttpMethod, PathSuffixMode, Protocol, RouteMatch,
    };
    use crate::infra::store::Store;

    fn ctx(tenant: uuid::Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(uuid::Uuid::now_v7())
            .subject_tenant_id(tenant)
            .build()
            .expect("valid security context")
    }

    fn service() -> Service {
        Service::new(Arc::new(Store::new()), Arc::new(ClientHub::new()))
    }

    /// JSON endpoint pool of a single endpoint.
    fn server(host: &str, port: u16) -> serde_json::Value {
        serde_json::json!({ "endpoints": [{ "scheme": "https", "host": host, "port": port }] })
    }

    fn http_input(alias: Option<&str>, host: &str) -> UpstreamInput {
        UpstreamInput {
            alias: alias.map(ToOwned::to_owned),
            server: Some(server(host, 443)),
            protocol: Some(Protocol::Http.gts_id().to_owned()),
            ..UpstreamInput::default()
        }
    }

    fn ip_input(alias: &str) -> UpstreamInput {
        UpstreamInput {
            alias: Some(alias.to_owned()),
            server: Some(server("10.0.0.1", 443)),
            ..http_input(None, "10.0.0.1")
        }
    }

    fn http_route(upstream_id: uuid::Uuid, path: &str) -> RouteInput {
        RouteInput {
            match_rule: Some(serde_json::json!({
                "http": {
                    "methods": ["GET"],
                    "path": path,
                    "path_suffix_mode": "append",
                }
            })),
            upstream_id: Some(upstream_id),
            ..RouteInput::default()
        }
    }

    /// The wire status the service error maps onto.
    fn status_of(error: &ServiceError) -> u16 {
        Problem::from(CanonicalError::from(error.clone())).status
    }

    fn query(params: ListParams) -> ListQuery {
        ListQuery::parse(&params).expect("list query")
    }

    // -- upstreams ----------------------------------------------------------

    #[tokio::test]
    async fn create_upstream_generates_the_id_and_derives_the_alias() {
        let tenant = uuid::Uuid::new_v4();
        let service = service();

        let created = service
            .create_upstream(&ctx(tenant), http_input(None, "api.openai.com"))
            .await
            .expect("created");
        let id = created.id.expect("server-generated id");
        assert_ne!(id, uuid::Uuid::nil(), "the client cannot set the id");
        assert_eq!(created.alias.expect("alias").as_str(), "api.openai.com");
        assert!(created.enabled);
        assert_eq!(
            service.get_upstream(tenant, id).expect("stored").id,
            Some(id)
        );
    }

    #[tokio::test]
    async fn create_upstream_appends_a_non_standard_port() {
        let tenant = uuid::Uuid::new_v4();
        let service = service();

        let input = UpstreamInput {
            server: Some(server("api.openai.com", 8080)),
            ..http_input(None, "api.openai.com")
        };
        let created = service
            .create_upstream(&ctx(tenant), input)
            .await
            .expect("created");
        assert_eq!(
            created.alias.expect("alias").as_str(),
            "api.openai.com:8080"
        );
    }

    #[tokio::test]
    async fn ip_endpoint_requires_an_explicit_alias() {
        let tenant = uuid::Uuid::new_v4();
        let service = service();

        let input = UpstreamInput {
            server: Some(server("10.0.0.1", 443)),
            ..http_input(None, "10.0.0.1")
        };
        let error = service
            .create_upstream(&ctx(tenant), input)
            .await
            .expect_err("an explicit alias is required");
        assert_eq!(status_of(&error), 400);
        assert!(error.to_string().contains("alias"), "{error}");

        let created = service
            .create_upstream(&ctx(tenant), ip_input("my-service"))
            .await
            .expect("explicit alias accepted");
        assert_eq!(created.alias.expect("alias").as_str(), "my-service");
    }

    #[tokio::test]
    async fn duplicate_alias_is_a_conflict_within_the_tenant_only() {
        let tenant = uuid::Uuid::new_v4();
        let other = uuid::Uuid::new_v4();
        let service = service();

        service
            .create_upstream(&ctx(tenant), http_input(None, "api.openai.com"))
            .await
            .expect("created");

        let error = service
            .create_upstream(&ctx(tenant), ip_input("api.openai.com"))
            .await
            .expect_err("conflict");
        assert_eq!(status_of(&error), 409);

        // The very same alias in another tenant is independent.
        service
            .create_upstream(&ctx(other), ip_input("api.openai.com"))
            .await
            .expect("per-tenant namespace");
    }

    #[tokio::test]
    async fn required_and_malformed_fields_are_reported_per_field() {
        let tenant = uuid::Uuid::new_v4();
        let service = service();

        let error = service
            .create_upstream(&ctx(tenant), UpstreamInput::default())
            .await
            .expect_err("no server");
        assert_eq!(status_of(&error), 400);

        let error = service
            .create_upstream(
                &ctx(tenant),
                UpstreamInput {
                    protocol: None,
                    ..ip_input("no-protocol")
                },
            )
            .await
            .expect_err("no protocol");
        assert_eq!(status_of(&error), 400);
        assert!(error.to_string().contains("protocol"), "{error}");

        let input = UpstreamInput {
            tags: Some(vec!["Open AI".to_owned()]),
            ..ip_input("tagged")
        };
        let error = service
            .create_upstream(&ctx(tenant), input)
            .await
            .expect_err("invalid tag");
        assert_eq!(status_of(&error), 400);
        assert!(error.to_string().contains("tags"), "{error}");
    }

    #[tokio::test]
    async fn cors_wildcard_with_credentials_is_rejected() {
        let tenant = uuid::Uuid::new_v4();
        let service = service();

        let input = UpstreamInput {
            cors: Some(serde_json::json!({
                "enabled": true,
                "allowed_origins": ["*"],
                "allow_credentials": true,
            })),
            ..ip_input("cors-service")
        };
        let error = service
            .create_upstream(&ctx(tenant), input)
            .await
            .expect_err("credentials and wildcard origin");
        assert_eq!(status_of(&error), 400);
        assert!(
            error.to_string().contains("cors.allow_credentials"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn endpoint_pool_must_share_scheme_and_port() {
        let tenant = uuid::Uuid::new_v4();
        let service = service();

        let input = UpstreamInput {
            server: Some(serde_json::json!({
                "endpoints": [
                    { "scheme": "https", "host": "us.vendor.com", "port": 443 },
                    { "scheme": "https", "host": "eu.vendor.com", "port": 8443 },
                ]
            })),
            ..http_input(None, "us.vendor.com")
        };
        let error = service
            .create_upstream(&ctx(tenant), input)
            .await
            .expect_err("mixed pool");
        assert_eq!(status_of(&error), 400);
    }

    #[tokio::test]
    async fn replace_upstream_is_a_full_replacement_with_an_immutable_alias() {
        let tenant = uuid::Uuid::new_v4();
        let service = service();
        let created = service
            .create_upstream(&ctx(tenant), http_input(None, "api.openai.com"))
            .await
            .expect("created");
        let id = created.id.expect("id");

        // Tags are replaced, omitted optional blocks are cleared, alias kept.
        let replacement = UpstreamInput {
            tags: Some(vec!["llm".to_owned()]),
            ..http_input(None, "api.openai.com")
        };
        let replaced = service
            .replace_upstream(&ctx(tenant), tenant, id, replacement)
            .await
            .expect("replaced");
        assert_eq!(replaced.id, Some(id));
        assert_eq!(replaced.alias.expect("alias").as_str(), "api.openai.com");
        assert_eq!(replaced.tags.len(), 1);
        assert!(replaced.cors.is_none());

        // A different derived alias is rejected.
        let error = service
            .replace_upstream(
                &ctx(tenant),
                tenant,
                id,
                http_input(None, "api2.openai.com"),
            )
            .await
            .expect_err("alias change");
        assert_eq!(status_of(&error), 400);

        // Derivable -> non-derivable is rejected even with an explicit alias.
        let to_ip = ip_input("api.openai.com");
        let error = service
            .replace_upstream(&ctx(tenant), tenant, id, to_ip)
            .await
            .expect_err("derivable to non-derivable");
        assert_eq!(status_of(&error), 400);
        assert!(
            error.to_string().contains("delete and re-create"),
            "{error}"
        );

        // Non-derivable -> non-derivable retains the existing alias.
        let created = service
            .create_upstream(&ctx(tenant), ip_input("internal"))
            .await
            .expect("created");
        let kept = service
            .replace_upstream(
                &ctx(tenant),
                tenant,
                created.id.expect("id"),
                ip_input("internal"),
            )
            .await
            .expect("retained");
        assert_eq!(kept.alias.expect("alias").as_str(), "internal");
    }

    #[tokio::test]
    async fn upstream_status_updates_are_persisted_in_the_store() {
        let tenant = uuid::Uuid::new_v4();
        let service = service();
        let created = service
            .create_upstream(&ctx(tenant), http_input(None, "api.openai.com"))
            .await
            .expect("created");
        let id = created.id.expect("id");

        let disabled = service
            .set_upstream_enabled(tenant, id, false)
            .expect("disabled");
        assert!(!disabled.enabled);
        assert!(
            !service.get_upstream(tenant, id).expect("stored").enabled,
            "the store reports the upstream as disabled"
        );

        let enabled = service
            .set_upstream_enabled(tenant, id, true)
            .expect("enabled");
        assert!(enabled.enabled);
        assert!(service.get_upstream(tenant, id).expect("stored").enabled);
    }

    #[tokio::test]
    async fn deleting_an_upstream_cascades_to_its_routes() {
        let tenant = uuid::Uuid::new_v4();
        let service = service();
        let created = service
            .create_upstream(&ctx(tenant), http_input(None, "api.openai.com"))
            .await
            .expect("created");
        let id = created.id.expect("id");
        service
            .create_route(tenant, http_route(id, "/v1/things"))
            .expect("route created");
        assert_eq!(service.store().route_count(tenant), 1);

        service.delete_upstream(tenant, id).expect("deleted");
        assert!(service.store().get_upstream(tenant, id).is_none());
        assert_eq!(service.store().route_count(tenant), 0);
    }

    // -- routes -------------------------------------------------------------

    #[tokio::test]
    async fn route_upstream_must_belong_to_the_calling_tenant() {
        let tenant = uuid::Uuid::new_v4();
        let other = uuid::Uuid::new_v4();
        let service = service();
        let created = service
            .create_upstream(&ctx(tenant), http_input(None, "api.openai.com"))
            .await
            .expect("created");
        let id = created.id.expect("id");

        let error = service
            .create_route(other, http_route(id, "/v1/things"))
            .expect_err("foreign upstream");
        assert_eq!(status_of(&error), 404);

        let error = service
            .create_route(tenant, http_route(uuid::Uuid::new_v4(), "/v1/things"))
            .expect_err("unknown upstream");
        assert_eq!(status_of(&error), 404);
    }

    #[tokio::test]
    async fn duplicate_match_rule_is_a_conflict() {
        let tenant = uuid::Uuid::new_v4();
        let service = service();
        let upstream = service
            .create_upstream(&ctx(tenant), http_input(None, "api.openai.com"))
            .await
            .expect("created");
        let id = upstream.id.expect("id");

        service
            .create_route(tenant, http_route(id, "/v1/things"))
            .expect("created");
        let error = service
            .create_route(tenant, http_route(id, "/v1/things"))
            .expect_err("duplicate match rule");
        assert_eq!(status_of(&error), 409);

        // A different path is a different match rule.
        service
            .create_route(tenant, http_route(id, "/v1/other"))
            .expect("allowed");
    }

    #[tokio::test]
    async fn replace_route_keeps_the_upstream_immutable() {
        let tenant = uuid::Uuid::new_v4();
        let service = service();
        let upstream = service
            .create_upstream(&ctx(tenant), http_input(None, "api.openai.com"))
            .await
            .expect("created");
        let upstream_id = upstream.id.expect("id");
        let route = service
            .create_route(tenant, http_route(upstream_id, "/v1/things"))
            .expect("created");
        let route_id = route.id.expect("id");

        // Same upstream, different path: full replacement of the match rule.
        let replaced = service
            .replace_route(tenant, route_id, http_route(upstream_id, "/v2/things"))
            .expect("replaced");
        assert_eq!(replaced.upstream_id, upstream_id);
        assert_eq!(replaced.match_rule.http.expect("http").path, "/v2/things");

        // A different upstream_id is rejected.
        let other = service
            .create_upstream(&ctx(tenant), http_input(None, "api2.openai.com"))
            .await
            .expect("created");
        let error = service
            .replace_route(
                tenant,
                route_id,
                http_route(other.id.expect("id"), "/v2/things"),
            )
            .expect_err("upstream_id immutable");
        assert_eq!(status_of(&error), 400);
        assert!(error.to_string().contains("immutable"), "{error}");
    }

    #[tokio::test]
    async fn route_payload_is_validated() {
        let tenant = uuid::Uuid::new_v4();
        let service = service();
        let upstream = service
            .create_upstream(&ctx(tenant), http_input(None, "api.openai.com"))
            .await
            .expect("created");
        let id = upstream.id.expect("id");

        let empty_methods = RouteInput {
            match_rule: Some(serde_json::json!({ "http": { "methods": [], "path": "/v1" } })),
            ..http_route(id, "/v1")
        };
        let error = service
            .create_route(tenant, empty_methods)
            .expect_err("empty methods");
        assert_eq!(status_of(&error), 400);

        let both = RouteInput {
            match_rule: Some(serde_json::json!({
                "http": { "methods": ["GET"], "path": "/v1" },
                "grpc": { "service": "s", "method": "m" },
            })),
            ..http_route(id, "/v1")
        };
        let error = service
            .create_route(tenant, both)
            .expect_err("two protocols");
        assert_eq!(status_of(&error), 400);

        let missing = RouteInput {
            match_rule: None,
            ..http_route(id, "/v1")
        };
        let error = service
            .create_route(tenant, missing)
            .expect_err("missing match");
        assert_eq!(status_of(&error), 400);
        assert!(error.to_string().contains("match"), "{error}");
    }

    #[tokio::test]
    async fn route_status_toggles_the_enablement_overlay() {
        let tenant = uuid::Uuid::new_v4();
        let service = service();
        let upstream = service
            .create_upstream(&ctx(tenant), http_input(None, "api.openai.com"))
            .await
            .expect("created");
        let upstream_id = upstream.id.expect("id");
        let route = service
            .create_route(tenant, http_route(upstream_id, "/v1/things"))
            .expect("created");
        let id = route.id.expect("id");

        assert!(service.is_route_enabled(tenant, id));
        let disabled = service
            .set_route_enabled(tenant, id, false)
            .expect("disabled");
        assert!(!disabled.enabled);
        assert!(!service.is_route_enabled(tenant, id));
        assert!(!service.get_route(tenant, id).expect("route").enabled);

        let enabled = service
            .set_route_enabled(tenant, id, true)
            .expect("enabled");
        assert!(enabled.enabled);
        assert!(service.is_route_enabled(tenant, id));
    }

    // -- tenant isolation ---------------------------------------------------

    #[tokio::test]
    async fn tenants_cannot_see_each_others_records() {
        let tenant_a = uuid::Uuid::new_v4();
        let tenant_b = uuid::Uuid::new_v4();
        let service = service();
        let created = service
            .create_upstream(&ctx(tenant_a), http_input(None, "api.openai.com"))
            .await
            .expect("created");
        let id = created.id.expect("id");
        service
            .create_route(tenant_a, http_route(id, "/v1/things"))
            .expect("created");

        // Reads, listings, updates and deletes all stay inside the tenant.
        assert!(service.get_upstream(tenant_b, id).is_err());
        assert!(service.get_route(tenant_b, uuid::Uuid::new_v4()).is_err());
        assert!(
            service
                .list_upstreams(tenant_b, &query(ListParams::default()))
                .expect("listed")
                .is_empty()
        );
        assert!(
            service
                .list_routes(tenant_b, &query(ListParams::default()))
                .expect("listed")
                .is_empty()
        );
        assert!(service.set_upstream_enabled(tenant_b, id, false).is_err());
        assert!(service.delete_upstream(tenant_b, id).is_err());
        assert!(
            service
                .delete_route(tenant_b, uuid::Uuid::new_v4())
                .is_err()
        );
        assert_eq!(service.store().upstream_count(tenant_a), 1);
        assert_eq!(service.store().upstream_count(tenant_b), 0);
    }

    // -- listing ------------------------------------------------------------

    #[tokio::test]
    async fn listing_supports_top_skip_orderby_and_filter() {
        let tenant = uuid::Uuid::new_v4();
        let service = service();
        for host in ["a.openai.com", "b.openai.com", "c.openai.com"] {
            service
                .create_upstream(&ctx(tenant), http_input(None, host))
                .await
                .expect("created");
        }

        let defaults = query(ListParams::default());
        assert_eq!(defaults.top, DEFAULT_LIST_TOP);
        assert_eq!(defaults.skip, 0);
        assert_eq!(
            service
                .list_upstreams(tenant, &defaults)
                .expect("listed")
                .len(),
            3
        );

        let paged = query(ListParams {
            top: Some("2".to_owned()),
            skip: Some("1".to_owned()),
            orderby: Some("alias desc".to_owned()),
            ..ListParams::default()
        });
        let aliases: Vec<String> = service
            .list_upstreams(tenant, &paged)
            .expect("listed")
            .into_iter()
            .map(|upstream| upstream.alias.expect("alias").to_string())
            .collect();
        assert_eq!(aliases, vec!["b.openai.com", "a.openai.com"]);

        let filtered = query(ListParams {
            filter: Some("alias eq 'b.openai.com'".to_owned()),
            ..ListParams::default()
        });
        assert_eq!(
            service
                .list_upstreams(tenant, &filtered)
                .expect("listed")
                .len(),
            1
        );

        let excluded = query(ListParams {
            filter: Some("enabled ne true".to_owned()),
            ..ListParams::default()
        });
        assert!(
            service
                .list_upstreams(tenant, &excluded)
                .expect("listed")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn top_over_the_maximum_is_rejected() {
        let error = ListQuery::parse(&ListParams {
            top: Some((MAX_LIST_TOP + 1).to_string()),
            ..ListParams::default()
        })
        .expect_err("rejected");
        assert_eq!(status_of(&error), 400);

        let at_limit = ListQuery::parse(&ListParams {
            top: Some(MAX_LIST_TOP.to_string()),
            ..ListParams::default()
        })
        .expect("at the limit");
        assert_eq!(at_limit.top, MAX_LIST_TOP);

        let error = ListQuery::parse(&ListParams {
            skip: Some("nope".to_owned()),
            ..ListParams::default()
        })
        .expect_err("rejected");
        assert_eq!(status_of(&error), 400);
    }

    #[tokio::test]
    async fn filter_and_orderby_expressions_are_validated() {
        let tenant = uuid::Uuid::new_v4();
        let service = service();

        // Unknown fields are rejected against the field list of the resource.
        let error = service
            .list_upstreams(
                tenant,
                &ListQuery::parse(&ListParams {
                    filter: Some("bogus eq 'x'".to_owned()),
                    ..ListParams::default()
                })
                .expect("parsed"),
            )
            .expect_err("unknown field");
        assert_eq!(status_of(&error), 400);

        let error = service
            .list_upstreams(
                tenant,
                &ListQuery::parse(&ListParams {
                    orderby: Some("bogus asc".to_owned()),
                    ..ListParams::default()
                })
                .expect("parsed"),
            )
            .expect_err("unknown order field");
        assert_eq!(status_of(&error), 400);

        // Unsupported operators are rejected at parse time.
        let error = ListQuery::parse(&ListParams {
            filter: Some("alias like 'x'".to_owned()),
            ..ListParams::default()
        })
        .expect_err("unsupported operator");
        assert_eq!(status_of(&error), 400);

        let selected = ListQuery::parse(&ListParams {
            select: Some("id, alias".to_owned()),
            ..ListParams::default()
        })
        .expect("parsed");
        assert_eq!(
            selected.select,
            Some(vec!["id".to_owned(), "alias".to_owned()])
        );
    }

    #[test]
    fn match_key_distinguishes_path_methods_and_protocol() {
        let http = |methods: &[HttpMethod], path: &str| RouteMatch {
            http: Some(HttpMatch {
                methods: methods.to_vec(),
                path: path.to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        let grpc = RouteMatch {
            http: None,
            grpc: Some(GrpcMatch {
                service: "s".to_owned(),
                method: "m".to_owned(),
            }),
        };
        assert_eq!(match_key(&http(&[HttpMethod::Get], "/v1")), "http GET /v1");
        assert_eq!(
            match_key(&http(&[HttpMethod::Post, HttpMethod::Get], "/v1")),
            "http GET,POST /v1"
        );
        assert_ne!(
            match_key(&http(&[HttpMethod::Get], "/v1")),
            match_key(&http(&[HttpMethod::Get], "/v2"))
        );
        assert_ne!(
            match_key(&http(&[HttpMethod::Get], "/v1")),
            match_key(&http(&[HttpMethod::Post], "/v1"))
        );
        assert_eq!(match_key(&grpc), "grpc s m");
    }

    // -- hierarchy ----------------------------------------------------------

    /// Tenant resolver returning a fixed parent chain per tenant.
    struct FixedHierarchy {
        parents: HashMap<TenantId, Vec<TenantId>>,
    }

    fn tenant_ref(id: TenantId, parent_id: Option<TenantId>) -> TenantRef {
        TenantRef {
            id,
            status: TenantStatus::Active,
            tenant_type: None,
            parent_id,
            self_managed: false,
        }
    }

    #[async_trait]
    impl TenantResolverClient for FixedHierarchy {
        async fn get_tenant(
            &self,
            _ctx: &SecurityContext,
            id: TenantId,
        ) -> Result<TenantInfo, TenantResolverError> {
            let parent = self
                .parents
                .get(&id)
                .and_then(|chain| chain.first())
                .copied();
            Ok(TenantInfo {
                id,
                name: String::new(),
                status: TenantStatus::Active,
                tenant_type: None,
                parent_id: parent,
                self_managed: false,
            })
        }

        async fn get_root_tenant(
            &self,
            _ctx: &SecurityContext,
        ) -> Result<TenantInfo, TenantResolverError> {
            Err(TenantResolverError::Internal("unused".to_owned()))
        }

        async fn get_tenants(
            &self,
            _ctx: &SecurityContext,
            ids: &[TenantId],
            _options: &tenant_resolver_sdk::GetTenantsOptions,
        ) -> Result<Vec<TenantInfo>, TenantResolverError> {
            Ok(ids
                .iter()
                .copied()
                .map(|id| TenantInfo {
                    id,
                    name: String::new(),
                    status: TenantStatus::Active,
                    tenant_type: None,
                    parent_id: None,
                    self_managed: false,
                })
                .collect())
        }

        async fn get_ancestors(
            &self,
            _ctx: &SecurityContext,
            id: TenantId,
            _options: &GetAncestorsOptions,
        ) -> Result<GetAncestorsResponse, TenantResolverError> {
            let chain = self.parents.get(&id).cloned().unwrap_or_default();
            let parent = chain.first().copied();
            Ok(GetAncestorsResponse {
                tenant: tenant_ref(id, parent),
                ancestors: chain
                    .into_iter()
                    .map(|ancestor| tenant_ref(ancestor, None))
                    .collect(),
            })
        }

        async fn get_descendants(
            &self,
            _ctx: &SecurityContext,
            id: TenantId,
            _options: &GetDescendantsOptions,
        ) -> Result<GetDescendantsResponse, TenantResolverError> {
            Ok(GetDescendantsResponse {
                tenant: tenant_ref(id, None),
                descendants: vec![],
            })
        }

        async fn is_ancestor(
            &self,
            _ctx: &SecurityContext,
            ancestor_id: TenantId,
            descendant_id: TenantId,
            _options: &tenant_resolver_sdk::IsAncestorOptions,
        ) -> Result<bool, TenantResolverError> {
            Ok(self
                .parents
                .get(&descendant_id)
                .is_some_and(|chain| chain.contains(&ancestor_id)))
        }
    }

    async fn service_with_hierarchy(parents: HashMap<TenantId, Vec<TenantId>>) -> Service {
        let hub = Arc::new(ClientHub::new());
        let client: Arc<dyn TenantResolverClient> = Arc::new(FixedHierarchy { parents });
        hub.register(client);
        Service::new(Arc::new(Store::new()), hub)
    }

    #[tokio::test]
    async fn descendant_shadows_an_inherited_ancestor_alias() {
        let root = uuid::Uuid::new_v4();
        let leaf = uuid::Uuid::new_v4();
        let mut parents = HashMap::new();
        parents.insert(TenantId(leaf), vec![TenantId(root)]);
        let service = service_with_hierarchy(parents).await;

        let root_upstream = service
            .create_upstream(&ctx(root), http_input(None, "api.openai.com"))
            .await
            .expect("root upstream");
        let alias = root_upstream.alias.clone().expect("alias");

        // The leaf resolves the ancestor upstream through the chain walk.
        let resolved = service
            .resolve_upstream(&ctx(leaf), &alias)
            .await
            .expect("resolved");
        assert_eq!(resolved.and_then(|upstream| upstream.id), root_upstream.id);

        // The leaf may shadow it: the closest match then wins.
        let shadow = service
            .create_upstream(&ctx(leaf), ip_input("api.openai.com"))
            .await
            .expect("shadowing is allowed");
        let resolved = service
            .resolve_upstream(&ctx(leaf), &alias)
            .await
            .expect("resolved");
        assert_eq!(resolved.and_then(|upstream| upstream.id), shadow.id);

        // The ancestor still sees its own upstream.
        let resolved = service
            .resolve_upstream(&ctx(root), &alias)
            .await
            .expect("resolved");
        assert_eq!(resolved.and_then(|upstream| upstream.id), root_upstream.id);
    }

    #[tokio::test]
    async fn an_enforced_ancestor_upstream_cannot_be_shadowed() {
        let root = uuid::Uuid::new_v4();
        let leaf = uuid::Uuid::new_v4();
        let mut parents = HashMap::new();
        parents.insert(TenantId(leaf), vec![TenantId(root)]);
        let service = service_with_hierarchy(parents).await;

        let input = UpstreamInput {
            rate_limit: Some(serde_json::json!({
                "sharing": "enforce",
                "sustained": { "rate": 10, "window": "second" },
            })),
            ..http_input(None, "api.openai.com")
        };
        let root_upstream = service
            .create_upstream(&ctx(root), input)
            .await
            .expect("created");
        let alias = root_upstream.alias.expect("alias");

        let error = service
            .create_upstream(&ctx(leaf), ip_input(alias.as_str()))
            .await
            .expect_err("enforced");
        assert_eq!(status_of(&error), 409);
        assert!(error.to_string().contains("rate_limit"), "{error}");
    }

    #[tokio::test]
    async fn a_missing_resolver_client_treats_the_tenant_as_a_leaf() {
        let service = service();
        let tenant = uuid::Uuid::new_v4();
        let created = service
            .create_upstream(&ctx(tenant), http_input(None, "api.openai.com"))
            .await
            .expect("created");
        let alias = created.alias.clone().expect("alias");
        assert!(
            service
                .resolve_upstream(&ctx(tenant), &alias)
                .await
                .expect("resolved")
                .is_some()
        );
    }
}
