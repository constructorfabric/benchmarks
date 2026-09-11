//! Tenant-scoped CRUD service tying the [`crate::domain`] validation and
//! alias algorithms to the in-process [`ControlPlaneState`]
//! (`cpt-cf-oagw-dod-create-upstream-endpoint`,
//! `cpt-cf-oagw-dod-list-upstreams-endpoint`,
//! `cpt-cf-oagw-dod-get-upstream-endpoint`,
//! `cpt-cf-oagw-dod-replace-upstream-endpoint`,
//! `cpt-cf-oagw-dod-delete-upstream-endpoint`).
//!
//! `OagwError` (`src/error.rs`) is a plain, builder-chained value
//! (`.with_instance()`, `.with_upstream_id()`, ...) returned unboxed from
//! every fallible function across this crate, including the handler layer;
//! boxing it only in this module would be an inconsistent, purely
//! lint-driven special case, so `clippy::result_large_err` is allowed here.
#![allow(clippy::result_large_err)]

use serde_json::Value;
use uuid::Uuid;

use super::alias::{
    Derivation, ROOT_TENANT_ID, apply_port_suffix, derive_root_alias, normalize_alias,
};
use super::model::{
    Endpoint, MatchConfig, Plugin, PluginSource, Route, ServerConfig, StoredPlugin, Upstream,
    gts_plugin_id, parse_gts_plugin_ref,
};
use super::plugin_resolve::{
    EmptyNamedPluginRegistry, ResolvedPlugin, plugin_references, plugin_view, resolve_plugin_ref,
};
use super::plugin_validate::parse_plugin_request;
use super::query::{
    ListQuery, PLUGIN_ALLOWED_FIELDS, ROUTE_ALLOWED_FIELDS, RawListQuery, UPSTREAM_ALLOWED_FIELDS,
    apply as apply_list_query, validate as validate_query,
};
use super::route_validate::parse_route_request;
use super::validate::parse_request;
use crate::error::OagwError;
use crate::state::{ControlPlaneState, TenantState};

/// Creates a new upstream for `tenant_id`
/// (`cpt-cf-oagw-dod-create-upstream-endpoint`,
/// `cpt-cf-oagw-flow-create-upstream`).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] when the body fails schema
/// validation or alias derivation, [`OagwError::alias_conflict`] when the
/// resolved alias already exists for `tenant_id`.
// @cpt-begin:cpt-cf-oagw-dod-create-upstream-endpoint:p1:inst-create-upstream-service-01
pub fn create_upstream(
    state: &ControlPlaneState,
    tenant_id: Uuid,
    body: Value,
) -> Result<Upstream, OagwError> {
    let request = parse_request(body)?;
    let endpoints = request.server.endpoints.clone();

    // @cpt-begin:cpt-cf-oagw-dod-alias-derivation-normalization:p1:inst-create-upstream-alias-01
    let alias = resolve_create_alias(&endpoints, request.alias.as_deref())?;
    // @cpt-end:cpt-cf-oagw-dod-alias-derivation-normalization:p1:inst-create-upstream-alias-01

    let enabled = request.enabled.unwrap_or(true);

    let tenant = state.tenant(tenant_id);
    // `write_guard` is held from the alias-uniqueness check through the
    // insert below (`BUG1-F-001`, `cpt-cf-oagw-algo-alias-uniqueness-check`):
    // a `DashMap` alone only locks a single key, not this
    // check-a-scan/insert-under-a-different-key sequence, so two concurrent
    // creates deriving the same alias could otherwise both pass the scan and
    // both insert. The alias-uniqueness check runs before the ancestor-disable
    // check (`CODE1-F-002`), matching the FEATURE's step order: a request
    // whose alias is both already taken for this tenant and disabled on an
    // ancestor must see `409`, not `400`.
    let write_guard = tenant.write_guard.lock();
    // @cpt-begin:cpt-cf-oagw-dod-alias-uniqueness:p1:inst-create-upstream-unique-01
    if alias_taken(&tenant, &alias, None) {
        return Err(OagwError::alias_conflict(format!(
            "alias '{alias}' already exists for this tenant"
        )));
    }
    // @cpt-end:cpt-cf-oagw-dod-alias-uniqueness:p1:inst-create-upstream-unique-01

    // @cpt-begin:cpt-cf-oagw-dod-enable-disable:p1:inst-create-upstream-ancestor-01
    if enabled && ancestor_disabled(state, tenant_id, &alias) {
        return Err(OagwError::validation_error(format!(
            "alias '{alias}' is disabled by an ancestor tenant; the new upstream cannot be enabled"
        )));
    }
    // @cpt-end:cpt-cf-oagw-dod-enable-disable:p1:inst-create-upstream-ancestor-01

    let upstream = Upstream {
        id: Uuid::new_v4(),
        enabled,
        alias,
        tags: request.tags,
        server: ServerConfig { endpoints },
        protocol: request.protocol,
        auth: request.auth,
        headers: request.headers,
        plugins: request.plugins,
        rate_limit: request.rate_limit,
        cors: request.cors,
    };
    tenant.upstreams.insert(upstream.id, upstream.clone());
    drop(write_guard);
    // @cpt-begin:cpt-cf-oagw-dod-resolved-config-cache-invalidation:p1:inst-cache-invalidate-create-upstream-01
    state.resolved_cache().invalidate_all();
    // @cpt-end:cpt-cf-oagw-dod-resolved-config-cache-invalidation:p1:inst-cache-invalidate-create-upstream-01
    Ok(upstream)
}
// @cpt-end:cpt-cf-oagw-dod-create-upstream-endpoint:p1:inst-create-upstream-service-01

/// Lists `tenant_id`'s own upstreams, applying `$filter`, `$select`,
/// `$orderby`, `$skip`, and `$top` (`cpt-cf-oagw-dod-list-upstreams-endpoint`).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] when a query parameter is
/// malformed, references an undeclared field, or `$top` exceeds 100.
// @cpt-begin:cpt-cf-oagw-dod-list-upstreams-endpoint:p1:inst-list-upstreams-service-01
pub fn list_upstreams(
    state: &ControlPlaneState,
    tenant_id: Uuid,
    raw_query: &RawListQuery,
) -> Result<Vec<Value>, OagwError> {
    let query: ListQuery = validate_query(raw_query, UPSTREAM_ALLOWED_FIELDS)?;
    let tenant = state.tenant(tenant_id);
    let upstreams: Vec<Upstream> = tenant
        .upstreams
        .iter()
        .map(|entry| entry.value().clone())
        .collect();
    Ok(apply_list_query(&query, upstreams))
}
// @cpt-end:cpt-cf-oagw-dod-list-upstreams-endpoint:p1:inst-list-upstreams-service-01

/// Retrieves a single upstream owned by `tenant_id`
/// (`cpt-cf-oagw-dod-get-upstream-endpoint`).
///
/// # Errors
///
/// Returns [`OagwError::upstream_not_found`] when no upstream with `id`
/// exists for `tenant_id`.
// @cpt-begin:cpt-cf-oagw-dod-get-upstream-endpoint:p1:inst-get-upstream-service-01
pub fn get_upstream(
    state: &ControlPlaneState,
    tenant_id: Uuid,
    id: Uuid,
) -> Result<Upstream, OagwError> {
    let tenant = state.tenant(tenant_id);
    tenant
        .upstreams
        .get(&id)
        .map(|entry| entry.value().clone())
        .ok_or_else(|| {
            OagwError::upstream_not_found(format!("no upstream with id '{id}' for this tenant"))
        })
}
// @cpt-end:cpt-cf-oagw-dod-get-upstream-endpoint:p1:inst-get-upstream-service-01

/// Replaces every field of an existing upstream owned by `tenant_id`
/// (`cpt-cf-oagw-dod-replace-upstream-endpoint`,
/// `cpt-cf-oagw-flow-replace-upstream`).
///
/// # Errors
///
/// Returns [`OagwError::upstream_not_found`] when `id` does not resolve for
/// `tenant_id`, [`OagwError::validation_error`] when the body fails schema
/// validation, the endpoint change would alter the derived alias, or the
/// request would re-enable an ancestor-disabled alias.
// @cpt-begin:cpt-cf-oagw-dod-replace-upstream-endpoint:p1:inst-replace-upstream-service-01
pub fn replace_upstream(
    state: &ControlPlaneState,
    tenant_id: Uuid,
    id: Uuid,
    body: Value,
) -> Result<Upstream, OagwError> {
    let tenant = state.tenant(tenant_id);
    let stored = tenant
        .upstreams
        .get(&id)
        .map(|entry| entry.value().clone())
        .ok_or_else(|| {
            OagwError::upstream_not_found(format!("no upstream with id '{id}' for this tenant"))
        })?;

    let request = parse_request(body)?;
    let endpoints = request.server.endpoints.clone();

    // @cpt-begin:cpt-cf-oagw-dod-alias-immutability:p1:inst-replace-upstream-immutable-01
    let alias = resolve_replace_alias(&stored, &endpoints, request.alias.as_deref())?;
    // @cpt-end:cpt-cf-oagw-dod-alias-immutability:p1:inst-replace-upstream-immutable-01

    let enabled = request.enabled.unwrap_or(true);

    // `write_guard` covers replace's alias handling and the insert below
    // (`BUG1-F-001`): the stored alias is immutable, but the ancestor-disable
    // check reads a value (this tenant's own alias set, indirectly, via
    // `ancestor_disabled`) that a concurrent create/replace under the same
    // tenant could otherwise race with the write that follows it.
    let write_guard = tenant.write_guard.lock();
    // @cpt-begin:cpt-cf-oagw-dod-enable-disable:p1:inst-replace-upstream-ancestor-01
    if enabled && ancestor_disabled(state, tenant_id, &alias) {
        return Err(OagwError::validation_error(format!(
            "alias '{alias}' is disabled by an ancestor tenant; this upstream cannot be re-enabled"
        )));
    }
    // @cpt-end:cpt-cf-oagw-dod-enable-disable:p1:inst-replace-upstream-ancestor-01

    let updated = Upstream {
        id,
        enabled,
        alias,
        tags: request.tags,
        server: ServerConfig { endpoints },
        protocol: request.protocol,
        auth: request.auth,
        headers: request.headers,
        plugins: request.plugins,
        rate_limit: request.rate_limit,
        cors: request.cors,
    };
    tenant.upstreams.insert(id, updated.clone());
    drop(write_guard);
    // @cpt-begin:cpt-cf-oagw-dod-resolved-config-cache-invalidation:p1:inst-cache-invalidate-replace-upstream-01
    state.resolved_cache().invalidate_all();
    // @cpt-end:cpt-cf-oagw-dod-resolved-config-cache-invalidation:p1:inst-cache-invalidate-replace-upstream-01
    Ok(updated)
}
// @cpt-end:cpt-cf-oagw-dod-replace-upstream-endpoint:p1:inst-replace-upstream-service-01

/// Removes an upstream owned by `tenant_id`, cascade-deleting every route
/// bound to it and its own plugin bindings
/// (`cpt-cf-oagw-dod-delete-upstream-endpoint`,
/// `cpt-cf-oagw-dod-route-cascade-delete`,
/// `cpt-cf-oagw-algo-route-cascade-delete`,
/// `cpt-cf-oagw-dod-plugin-lifecycle-tracking`).
///
/// The upstream's own `auth`/`plugins` bindings and every cascade-deleted
/// route's `plugins` bindings are removed as part of removing the upstream
/// and route records themselves — a binding is a field on the `Upstream`/
/// `Route` value, not a separate row, so no further store mutation is
/// needed. `cpt-cf-oagw-feature-plugin-management` completes this clause,
/// deferred by gear-foundation and upstream-management before the plugin
/// entity existed: any custom plugin referenced only by this upstream (or by
/// one of its cascade-deleted routes) is immediately eligible for deletion
/// afterward, since `cpt-cf-oagw-algo-plugin-in-use-detection` scans the
/// live `upstreams`/`routes` maps rather than a separately tracked lifecycle
/// flag.
///
/// # Errors
///
/// Returns [`OagwError::upstream_not_found`] when no upstream with `id`
/// exists for `tenant_id`.
// @cpt-begin:cpt-cf-oagw-dod-delete-upstream-endpoint:p1:inst-delete-upstream-service-01
pub fn delete_upstream(
    state: &ControlPlaneState,
    tenant_id: Uuid,
    id: Uuid,
) -> Result<(), OagwError> {
    let tenant = state.tenant(tenant_id);
    if tenant.upstreams.remove(&id).is_none() {
        return Err(OagwError::upstream_not_found(format!(
            "no upstream with id '{id}' for this tenant"
        )));
    }

    // @cpt-begin:cpt-cf-oagw-dod-route-cascade-delete:p1:inst-cascade-delete-remove-01
    tenant.routes.retain(|_, route| route.upstream_id != id);
    // @cpt-end:cpt-cf-oagw-dod-route-cascade-delete:p1:inst-cascade-delete-remove-01

    // @cpt-begin:cpt-cf-oagw-dod-resolved-config-cache-invalidation:p1:inst-cache-invalidate-delete-upstream-01
    state.resolved_cache().invalidate_all();
    // @cpt-end:cpt-cf-oagw-dod-resolved-config-cache-invalidation:p1:inst-cache-invalidate-delete-upstream-01

    Ok(())
}
// @cpt-end:cpt-cf-oagw-dod-delete-upstream-endpoint:p1:inst-delete-upstream-service-01

/// Resolves the alias for a create request
/// (`cpt-cf-oagw-algo-alias-derivation`): derives it from `endpoints` when
/// possible, otherwise requires and normalizes the supplied `alias`.
fn resolve_create_alias(
    endpoints: &[Endpoint],
    supplied: Option<&str>,
) -> Result<String, OagwError> {
    match derive_root_alias(endpoints) {
        Derivation::Derived(root) => {
            let derived = apply_port_suffix(&root, endpoints);
            if let Some(supplied) = supplied
                && normalize_alias(supplied) != derived
            {
                return Err(OagwError::validation_error(format!(
                    "alias: supplied alias '{supplied}' diverges from the derived alias '{derived}'"
                )));
            }
            Ok(derived)
        }
        Derivation::NotDerivable => match supplied {
            Some(supplied) => Ok(normalize_alias(supplied)),
            None => Err(OagwError::validation_error(
                "alias: an explicit alias is required for IP-addressed or non-derivable endpoints"
                    .to_owned(),
            )),
        },
    }
}

/// Resolves the alias for a replace request via the alias-immutability
/// algorithm (`cpt-cf-oagw-algo-alias-immutability-check`): the stored
/// alias is retained as an idempotent no-op when it is unchanged; any other
/// change is rejected.
fn resolve_replace_alias(
    stored: &Upstream,
    endpoints: &[Endpoint],
    supplied: Option<&str>,
) -> Result<String, OagwError> {
    let recomputed = match derive_root_alias(endpoints) {
        Derivation::Derived(root) => Some(apply_port_suffix(&root, endpoints)),
        Derivation::NotDerivable => None,
    };

    let matches = match supplied {
        Some(alias) => {
            let normalized = normalize_alias(alias);
            recomputed.as_deref() == Some(normalized.as_str())
                || (recomputed.is_none() && normalized == stored.alias)
        }
        None => recomputed.as_deref() == Some(stored.alias.as_str()),
    };

    if matches {
        Ok(stored.alias.clone())
    } else {
        Err(OagwError::validation_error(
            "alias is immutable: this endpoint change would alter the derived alias; delete and \
             re-create the upstream instead"
                .to_owned(),
        ))
    }
}

/// `true` when `alias` is already used by a different upstream owned by the
/// same tenant (`cpt-cf-oagw-algo-alias-uniqueness-check`).
fn alias_taken(tenant: &TenantState, alias: &str, exclude_id: Option<Uuid>) -> bool {
    tenant
        .upstreams
        .iter()
        .any(|entry| entry.value().alias == alias && Some(*entry.key()) != exclude_id)
}

/// `true` when the root tenant (the implicit ancestor stand-in documented on
/// [`ROOT_TENANT_ID`]) has a disabled upstream sharing `alias`.
fn ancestor_disabled(state: &ControlPlaneState, tenant_id: Uuid, alias: &str) -> bool {
    if tenant_id == ROOT_TENANT_ID {
        return false;
    }
    let root = state.tenant(ROOT_TENANT_ID);
    root.upstreams
        .iter()
        .any(|entry| entry.value().alias == alias && !entry.value().enabled)
}

// ---------------------------------------------------------------------------
// Route management (`cpt-cf-oagw-feature-route-management`).
// ---------------------------------------------------------------------------

/// Creates a new route for `tenant_id`
/// (`cpt-cf-oagw-dod-route-schema-validation`,
/// `cpt-cf-oagw-dod-route-crud-endpoints`, `cpt-cf-oagw-flow-create-route`).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] when the body fails schema
/// validation or `upstream_id` does not resolve to an upstream owned by
/// `tenant_id` (`cpt-cf-oagw-dod-upstream-reference-check`), and
/// [`OagwError::route_conflict`] when the candidate route's `path`,
/// `priority`, and method set duplicate another enabled route under the
/// same `upstream_id` (`cpt-cf-oagw-dod-route-conflict-detection`).
// @cpt-begin:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-create-route-service-01
pub fn create_route(
    state: &ControlPlaneState,
    tenant_id: Uuid,
    body: Value,
) -> Result<Route, OagwError> {
    let request = parse_route_request(body, true)?;
    let upstream_id = request
        .upstream_id
        .ok_or_else(|| OagwError::validation_error("upstream_id: field is required".to_owned()))?;

    let tenant = state.tenant(tenant_id);

    // @cpt-begin:cpt-cf-oagw-dod-upstream-reference-check:p1:inst-create-route-upstream-01
    if !tenant.upstreams.contains_key(&upstream_id) {
        return Err(OagwError::validation_error(format!(
            "upstream_id: '{upstream_id}' does not reference an existing upstream owned by this tenant"
        )));
    }
    // @cpt-end:cpt-cf-oagw-dod-upstream-reference-check:p1:inst-create-route-upstream-01

    let match_config = request
        .match_config
        .ok_or_else(|| OagwError::validation_error("match: field is required".to_owned()))?;
    // @cpt-begin:cpt-cf-oagw-dod-enable-disable-fields:p1:inst-create-route-app-fields-01
    let priority = request.priority.unwrap_or(0);
    let enabled = request.enabled.unwrap_or(true);
    // @cpt-end:cpt-cf-oagw-dod-enable-disable-fields:p1:inst-create-route-app-fields-01

    // `write_guard` is held from the conflict-detection scan through the
    // insert below (`BUG1-F-003`): a `DashMap` alone only locks a single key,
    // not a scan of every stored route followed by an insert under a fresh
    // one, so two concurrent creates with the same `upstream_id`, `path`,
    // `priority`, and an intersecting method set could otherwise both pass
    // the scan and both insert. This also serializes against `create_upstream`/
    // `replace_upstream`/`replace_route`/`delete_plugin` on the same tenant,
    // since a route's `plugins` field is a plugin binding
    // (`BUG1-F-004`).
    let write_guard = tenant.write_guard.lock();
    // @cpt-begin:cpt-cf-oagw-dod-route-conflict-detection:p1:inst-create-route-conflict-01
    if conflicts_with_existing(&tenant, upstream_id, &match_config, priority, None) {
        return Err(OagwError::route_conflict(
            "a route with the same path, priority, and an intersecting method set already \
             exists under this upstream"
                .to_owned(),
        ));
    }
    // @cpt-end:cpt-cf-oagw-dod-route-conflict-detection:p1:inst-create-route-conflict-01

    let route = Route {
        id: Uuid::new_v4(),
        upstream_id,
        tags: request.tags,
        match_config,
        plugins: request.plugins,
        rate_limit: request.rate_limit,
        enabled,
        priority,
    };
    tenant.routes.insert(route.id, route.clone());
    drop(write_guard);
    // @cpt-begin:cpt-cf-oagw-dod-resolved-config-cache-invalidation:p1:inst-cache-invalidate-create-route-01
    state.resolved_cache().invalidate_all();
    // @cpt-end:cpt-cf-oagw-dod-resolved-config-cache-invalidation:p1:inst-cache-invalidate-create-route-01
    Ok(route)
}
// @cpt-end:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-create-route-service-01

/// Lists `tenant_id`'s own routes, applying `$filter`, `$select`,
/// `$orderby`, `$skip`, and `$top` (`cpt-cf-oagw-dod-route-list-query-params`,
/// `cpt-cf-oagw-flow-list-routes`).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] when a query parameter is
/// malformed, references an undeclared field, or `$top` exceeds 100.
// @cpt-begin:cpt-cf-oagw-dod-route-list-query-params:p2:inst-list-routes-service-01
pub fn list_routes(
    state: &ControlPlaneState,
    tenant_id: Uuid,
    raw_query: &RawListQuery,
) -> Result<Vec<Value>, OagwError> {
    let query: ListQuery = validate_query(raw_query, ROUTE_ALLOWED_FIELDS)?;
    let tenant = state.tenant(tenant_id);
    let routes: Vec<Route> = tenant
        .routes
        .iter()
        .map(|entry| entry.value().clone())
        .collect();
    Ok(apply_list_query(&query, routes))
}
// @cpt-end:cpt-cf-oagw-dod-route-list-query-params:p2:inst-list-routes-service-01

/// Retrieves a single route owned by `tenant_id`
/// (`cpt-cf-oagw-dod-route-tenant-scoping`, `cpt-cf-oagw-flow-get-route`).
///
/// # Errors
///
/// Returns [`OagwError::route_record_not_found`] when no route with `id`
/// exists for `tenant_id`.
// @cpt-begin:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-get-route-service-01
pub fn get_route(state: &ControlPlaneState, tenant_id: Uuid, id: Uuid) -> Result<Route, OagwError> {
    let tenant = state.tenant(tenant_id);
    tenant
        .routes
        .get(&id)
        .map(|entry| entry.value().clone())
        .ok_or_else(|| {
            OagwError::route_record_not_found(format!("no route with id '{id}' for this tenant"))
        })
}
// @cpt-end:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-get-route-service-01

/// Replaces every field of an existing route owned by `tenant_id`, retaining
/// `upstream_id` when the payload omits it
/// (`cpt-cf-oagw-dod-upstream-id-immutability`,
/// `cpt-cf-oagw-flow-replace-route`).
///
/// # Errors
///
/// Returns [`OagwError::route_record_not_found`] when `id` does not resolve
/// for `tenant_id`, [`OagwError::validation_error`] when the body fails
/// schema validation or supplies an `upstream_id` different from the
/// stored route's, and [`OagwError::route_conflict`] when the replacement
/// would duplicate another enabled route's path, priority, and an
/// intersecting method set under the same `upstream_id`.
// @cpt-begin:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-replace-route-service-01
pub fn replace_route(
    state: &ControlPlaneState,
    tenant_id: Uuid,
    id: Uuid,
    body: Value,
) -> Result<Route, OagwError> {
    let tenant = state.tenant(tenant_id);
    let stored = tenant
        .routes
        .get(&id)
        .map(|entry| entry.value().clone())
        .ok_or_else(|| {
            OagwError::route_record_not_found(format!("no route with id '{id}' for this tenant"))
        })?;

    let request = parse_route_request(body, false)?;

    // @cpt-begin:cpt-cf-oagw-dod-upstream-id-immutability:p1:inst-replace-route-immutable-01
    let upstream_id = match request.upstream_id {
        None => stored.upstream_id,
        Some(supplied) if supplied == stored.upstream_id => supplied,
        Some(_) => {
            return Err(OagwError::validation_error(
                "upstream_id is immutable: the replace payload must omit it or match the stored \
                 value"
                    .to_owned(),
            ));
        }
    };
    // @cpt-end:cpt-cf-oagw-dod-upstream-id-immutability:p1:inst-replace-route-immutable-01

    let match_config = request
        .match_config
        .ok_or_else(|| OagwError::validation_error("match: field is required".to_owned()))?;
    // @cpt-begin:cpt-cf-oagw-dod-enable-disable-fields:p1:inst-replace-route-app-fields-01
    let priority = request.priority.unwrap_or(0);
    let enabled = request.enabled.unwrap_or(true);
    // @cpt-end:cpt-cf-oagw-dod-enable-disable-fields:p1:inst-replace-route-app-fields-01

    // See `create_route`'s `write_guard` comment (`BUG1-F-003`,
    // `BUG1-F-004`): held from the conflict-detection scan through the
    // insert below.
    let write_guard = tenant.write_guard.lock();
    // @cpt-begin:cpt-cf-oagw-dod-route-conflict-detection:p1:inst-replace-route-conflict-01
    if conflicts_with_existing(&tenant, upstream_id, &match_config, priority, Some(id)) {
        return Err(OagwError::route_conflict(
            "a route with the same path, priority, and an intersecting method set already \
             exists under this upstream"
                .to_owned(),
        ));
    }
    // @cpt-end:cpt-cf-oagw-dod-route-conflict-detection:p1:inst-replace-route-conflict-01

    let updated = Route {
        id,
        upstream_id,
        tags: request.tags,
        match_config,
        plugins: request.plugins,
        rate_limit: request.rate_limit,
        enabled,
        priority,
    };
    tenant.routes.insert(id, updated.clone());
    drop(write_guard);
    // @cpt-begin:cpt-cf-oagw-dod-resolved-config-cache-invalidation:p1:inst-cache-invalidate-replace-route-01
    state.resolved_cache().invalidate_all();
    // @cpt-end:cpt-cf-oagw-dod-resolved-config-cache-invalidation:p1:inst-cache-invalidate-replace-route-01
    Ok(updated)
}
// @cpt-end:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-replace-route-service-01

/// Removes a route owned by `tenant_id`
/// (`cpt-cf-oagw-dod-route-crud-endpoints`, `cpt-cf-oagw-flow-delete-route`).
///
/// # Errors
///
/// Returns [`OagwError::route_record_not_found`] when no route with `id`
/// exists for `tenant_id`.
// @cpt-begin:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-delete-route-service-01
pub fn delete_route(state: &ControlPlaneState, tenant_id: Uuid, id: Uuid) -> Result<(), OagwError> {
    let tenant = state.tenant(tenant_id);
    if tenant.routes.remove(&id).is_none() {
        return Err(OagwError::route_record_not_found(format!(
            "no route with id '{id}' for this tenant"
        )));
    }
    // @cpt-begin:cpt-cf-oagw-dod-resolved-config-cache-invalidation:p1:inst-cache-invalidate-delete-route-01
    state.resolved_cache().invalidate_all();
    // @cpt-end:cpt-cf-oagw-dod-resolved-config-cache-invalidation:p1:inst-cache-invalidate-delete-route-01
    Ok(())
}
// @cpt-end:cpt-cf-oagw-dod-route-crud-endpoints:p1:inst-delete-route-service-01

/// `true` when `candidate` (under `upstream_id`, at `priority`) duplicates
/// the path, priority, and an intersecting method set of another *enabled*
/// route already stored under the same `upstream_id`
/// (`cpt-cf-oagw-algo-route-conflict-detection`). Only HTTP-matched routes
/// participate: a `grpc`-matched candidate or stored route has no
/// `path`/`methods` pair to compare, so it can never conflict by this rule.
/// `exclude_id` excludes the route being replaced from the comparison so a
/// route never conflicts with itself.
fn conflicts_with_existing(
    tenant: &TenantState,
    upstream_id: Uuid,
    candidate: &MatchConfig,
    priority: i64,
    exclude_id: Option<Uuid>,
) -> bool {
    let Some((candidate_path, candidate_methods)) = http_match_key(candidate) else {
        return false;
    };

    tenant.routes.iter().any(|entry| {
        if Some(*entry.key()) == exclude_id {
            return false;
        }
        let route = entry.value();
        if route.upstream_id != upstream_id || !route.enabled || route.priority != priority {
            return false;
        }
        let Some((path, methods)) = http_match_key(&route.match_config) else {
            return false;
        };
        path == candidate_path
            && methods
                .iter()
                .any(|method| candidate_methods.contains(method))
    })
}

/// Extracts `(path, methods)` from an HTTP-matched [`MatchConfig`], or
/// `None` when it is `grpc`-matched.
fn http_match_key(match_config: &MatchConfig) -> Option<(&str, &[super::model::RouteMethod])> {
    match_config
        .http
        .as_ref()
        .map(|http| (http.path.as_str(), http.methods.as_slice()))
}

// ---------------------------------------------------------------------------
// Plugin management (`cpt-cf-oagw-feature-plugin-management`).
// ---------------------------------------------------------------------------

/// Creates a new custom plugin for `tenant_id`
/// (`cpt-cf-oagw-dod-plugin-create`, `cpt-cf-oagw-flow-register-plugin`).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] when the body fails validation
/// (`cpt-cf-oagw-dod-plugin-config-schema-validation`), and
/// [`OagwError::plugin_name_conflict`] when `name` already exists for
/// `tenant_id`, regardless of plugin type
/// (`cpt-cf-oagw-dod-plugin-name-uniqueness`).
// @cpt-begin:cpt-cf-oagw-dod-plugin-create:p1:inst-create-plugin-service-01
pub fn create_plugin(
    state: &ControlPlaneState,
    tenant_id: Uuid,
    body: Value,
) -> Result<Plugin, OagwError> {
    let request = parse_plugin_request(body)?;
    let tenant = state.tenant(tenant_id);

    // `write_guard` is held from the name-uniqueness scan through the insert
    // below (`BUG1-F-002`), for the same reason `create_upstream`'s
    // alias-uniqueness check needs it: a `DashMap` alone only locks a single
    // key, not a scan of every stored plugin followed by an insert under a
    // fresh one.
    let write_guard = tenant.write_guard.lock();
    // @cpt-begin:cpt-cf-oagw-dod-plugin-name-uniqueness:p1:inst-create-plugin-unique-01
    if plugin_name_taken(&tenant, &request.name) {
        return Err(OagwError::plugin_name_conflict(format!(
            "a plugin named '{}' already exists for this tenant",
            request.name
        )));
    }
    // @cpt-end:cpt-cf-oagw-dod-plugin-name-uniqueness:p1:inst-create-plugin-unique-01

    let stored = StoredPlugin {
        id: Uuid::new_v4(),
        plugin_type: request.plugin_type,
        name: request.name,
        config_schema: request.config_schema,
        phases: request.phases,
        source_code: request.source_code,
    };
    let view = plugin_view(&stored);
    tenant.plugins.insert(stored.id, stored);
    drop(write_guard);
    // @cpt-begin:cpt-cf-oagw-dod-resolved-config-cache-invalidation:p1:inst-cache-invalidate-create-plugin-01
    state.resolved_cache().invalidate_all();
    // @cpt-end:cpt-cf-oagw-dod-resolved-config-cache-invalidation:p1:inst-cache-invalidate-create-plugin-01
    Ok(view)
}
// @cpt-end:cpt-cf-oagw-dod-plugin-create:p1:inst-create-plugin-service-01

/// Lists `tenant_id`'s own stored plugin definitions, applying `$filter`,
/// `$select`, `$orderby`, `$skip`, and `$top`. Named built-in plugins are
/// never stored, so they are never listed (`cpt-cf-oagw-dod-plugin-list`,
/// `cpt-cf-oagw-flow-list-plugins`).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] when a query parameter is
/// malformed, references an undeclared field, or `$top` exceeds 100.
// @cpt-begin:cpt-cf-oagw-dod-plugin-list:p1:inst-list-plugins-service-01
pub fn list_plugins(
    state: &ControlPlaneState,
    tenant_id: Uuid,
    raw_query: &RawListQuery,
) -> Result<Vec<Value>, OagwError> {
    let query: ListQuery = validate_query(raw_query, PLUGIN_ALLOWED_FIELDS)?;
    let tenant = state.tenant(tenant_id);
    let plugins: Vec<Plugin> = tenant
        .plugins
        .iter()
        .map(|entry| plugin_view(entry.value()))
        .collect();
    Ok(apply_list_query(&query, plugins))
}
// @cpt-end:cpt-cf-oagw-dod-plugin-list:p1:inst-list-plugins-service-01

/// Retrieves a single tenant-owned plugin by its GTS identifier, excluding
/// its source text (`cpt-cf-oagw-dod-plugin-get`,
/// `cpt-cf-oagw-flow-get-plugin`).
///
/// # Errors
///
/// Returns [`OagwError::plugin_record_not_found`] when `gts_ref` does not
/// resolve to a stored plugin owned by `tenant_id`
/// (`cpt-cf-oagw-dod-plugin-identification`).
// @cpt-begin:cpt-cf-oagw-dod-plugin-get:p1:inst-get-plugin-service-01
pub fn get_plugin(
    state: &ControlPlaneState,
    tenant_id: Uuid,
    gts_ref: &str,
) -> Result<Plugin, OagwError> {
    let tenant = state.tenant(tenant_id);
    let id = resolved_custom_id(gts_ref, &tenant)?;
    tenant
        .plugins
        .get(&id)
        .map(|entry| plugin_view(entry.value()))
        .ok_or_else(|| plugin_not_found(gts_ref))
}
// @cpt-end:cpt-cf-oagw-dod-plugin-get:p1:inst-get-plugin-service-01

/// Retrieves the stored source text of a UUID-backed plugin owned by
/// `tenant_id` (`cpt-cf-oagw-dod-plugin-get-source`,
/// `cpt-cf-oagw-flow-get-plugin-source`).
///
/// # Errors
///
/// Returns [`OagwError::plugin_record_not_found`] when `gts_ref` names a
/// named built-in identifier (which carries no stored source) or does not
/// resolve to a stored plugin owned by `tenant_id`.
// @cpt-begin:cpt-cf-oagw-dod-plugin-get-source:p1:inst-get-plugin-source-service-01
pub fn get_plugin_source(
    state: &ControlPlaneState,
    tenant_id: Uuid,
    gts_ref: &str,
) -> Result<PluginSource, OagwError> {
    // @cpt-begin:cpt-cf-oagw-flow-get-plugin-source:p1:inst-get-plugin-source-named-guard-01
    let (_, instance) = parse_gts_plugin_ref(gts_ref).ok_or_else(|| plugin_not_found(gts_ref))?;
    if Uuid::parse_str(instance).is_err() {
        return Err(plugin_not_found(gts_ref));
    }
    // @cpt-end:cpt-cf-oagw-flow-get-plugin-source:p1:inst-get-plugin-source-named-guard-01

    let tenant = state.tenant(tenant_id);
    let id = resolved_custom_id(gts_ref, &tenant)?;
    tenant
        .plugins
        .get(&id)
        .map(|entry| PluginSource {
            id: gts_ref.to_owned(),
            source_code: entry.value().source_code.clone(),
        })
        .ok_or_else(|| plugin_not_found(gts_ref))
}
// @cpt-end:cpt-cf-oagw-dod-plugin-get-source:p1:inst-get-plugin-source-service-01

/// Deletes an unreferenced, tenant-owned plugin
/// (`cpt-cf-oagw-dod-plugin-delete`, `cpt-cf-oagw-flow-delete-plugin`).
///
/// # Errors
///
/// Returns [`OagwError::plugin_record_not_found`] when `gts_ref` does not
/// resolve to a stored plugin owned by `tenant_id`, and
/// [`OagwError::plugin_in_use`] when the plugin is still bound to any
/// upstream or route (`cpt-cf-oagw-algo-plugin-in-use-detection`).
// @cpt-begin:cpt-cf-oagw-dod-plugin-delete:p1:inst-delete-plugin-service-01
pub fn delete_plugin(
    state: &ControlPlaneState,
    tenant_id: Uuid,
    gts_ref: &str,
) -> Result<(), OagwError> {
    let tenant = state.tenant(tenant_id);
    let id = resolved_custom_id(gts_ref, &tenant)?;
    let canonical_ref = tenant
        .plugins
        .get(&id)
        .map(|entry| gts_plugin_id(entry.value().plugin_type, entry.value().id))
        .ok_or_else(|| plugin_not_found(gts_ref))?;

    // `write_guard` is held from the in-use scan through the removal below
    // (`BUG1-F-004`): every mutation that can add a new plugin binding
    // (`create_upstream`, `replace_upstream`, `create_route`,
    // `replace_route`) takes this same per-tenant guard, so a concurrent
    // binding write can no longer land between this scan and the `remove`
    // call below and leave a dangling reference that would otherwise only
    // surface later as a proxy-time 503.
    let write_guard = tenant.write_guard.lock();
    // @cpt-begin:cpt-cf-oagw-algo-plugin-in-use-detection:p2:inst-delete-plugin-inuse-01
    let (upstream_ids, route_ids) = plugin_references(&tenant, &canonical_ref);
    if !upstream_ids.is_empty() || !route_ids.is_empty() {
        return Err(plugin_in_use_error(
            &canonical_ref,
            &upstream_ids,
            &route_ids,
        ));
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-in-use-detection:p2:inst-delete-plugin-inuse-01

    tenant.plugins.remove(&id);
    drop(write_guard);
    // @cpt-begin:cpt-cf-oagw-dod-resolved-config-cache-invalidation:p1:inst-cache-invalidate-delete-plugin-01
    state.resolved_cache().invalidate_all();
    // @cpt-end:cpt-cf-oagw-dod-resolved-config-cache-invalidation:p1:inst-cache-invalidate-delete-plugin-01
    Ok(())
}
// @cpt-end:cpt-cf-oagw-dod-plugin-delete:p1:inst-delete-plugin-service-01

/// Resolves `gts_ref` to a stored, UUID-backed plugin id owned by `tenant`
/// (`cpt-cf-oagw-algo-plugin-ref-resolution`), or a plain not-found error —
/// including when `gts_ref` resolves to a named built-in plugin, since this
/// feature manages custom, stored plugins only
/// (`cpt-cf-oagw-dod-plugin-identification`).
fn resolved_custom_id(gts_ref: &str, tenant: &TenantState) -> Result<Uuid, OagwError> {
    match resolve_plugin_ref(gts_ref, tenant, &EmptyNamedPluginRegistry) {
        Some(ResolvedPlugin::Custom { id, .. }) => Ok(id),
        _ => Err(plugin_not_found(gts_ref)),
    }
}

/// The plain `404` problem document for an unresolved plugin identifier
/// (`cpt-cf-oagw-dod-plugin-get`, `cpt-cf-oagw-dod-plugin-identification`).
/// Never named or shaped after `PluginNotFound` (DESIGN.md's `503` runtime
/// plugin-reference-resolution failure, `cpt-cf-oagw-feature-plugin-runtime`).
fn plugin_not_found(gts_ref: &str) -> OagwError {
    OagwError::plugin_record_not_found(format!("no plugin with id '{gts_ref}' for this tenant"))
}

/// Builds the `409` plugin-in-use conflict document
/// (`cpt-cf-oagw-dod-plugin-delete`, `cpt-cf-oagw-adr-request-routing`),
/// naming every referencing upstream and route as a GTS identifier.
fn plugin_in_use_error(
    canonical_ref: &str,
    upstream_ids: &[Uuid],
    route_ids: &[Uuid],
) -> OagwError {
    let upstream_refs: Vec<String> = upstream_ids
        .iter()
        .map(|id| format!("gts.cf.core.oagw.upstream.v1~{id}"))
        .collect();
    let route_refs: Vec<String> = route_ids
        .iter()
        .map(|id| format!("gts.cf.core.oagw.route.v1~{id}"))
        .collect();
    OagwError::plugin_in_use(format!(
        "plugin is referenced by {} upstream(s) and {} route(s)",
        upstream_refs.len(),
        route_refs.len()
    ))
    .with_plugin_id(canonical_ref.to_owned())
    .with_referenced_by(upstream_refs, route_refs)
}

/// `true` when `name` is already used by a different stored plugin owned by
/// the same tenant, regardless of plugin type
/// (`cpt-cf-oagw-dod-plugin-name-uniqueness`).
fn plugin_name_taken(tenant: &TenantState, name: &str) -> bool {
    tenant
        .plugins
        .iter()
        .any(|entry| entry.value().name == name)
}

#[cfg(test)]
mod tests {
    use super::{
        create_plugin, create_route, create_upstream, delete_plugin, delete_route, delete_upstream,
        get_plugin, get_plugin_source, get_route, get_upstream, list_plugins, list_routes,
        list_upstreams, replace_route, replace_upstream,
    };
    use crate::domain::alias::ROOT_TENANT_ID;
    use crate::domain::query::RawListQuery;
    use crate::state::ControlPlaneState;
    use serde_json::json;
    use uuid::Uuid;

    fn http_upstream_body(host: &str) -> serde_json::Value {
        json!({
            "server": {"endpoints": [{"scheme": "http", "host": host, "port": 80}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        })
    }

    #[test]
    fn create_derives_alias_from_a_single_hostname() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();

        let upstream = create_upstream(
            &state,
            tenant_id,
            http_upstream_body("internal.example.com"),
        )
        .expect("create must succeed");

        assert_eq!(upstream.alias, "internal.example.com");
        assert!(upstream.enabled);
    }

    #[test]
    fn create_rejects_a_duplicate_alias_for_the_same_tenant() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        create_upstream(&state, tenant_id, http_upstream_body("dup.example.com"))
            .expect("first create must succeed");

        let error = create_upstream(&state, tenant_id, http_upstream_body("dup.example.com"))
            .expect_err("duplicate alias must be rejected");
        assert_eq!(error.status(), axum::http::StatusCode::CONFLICT);
    }

    #[test]
    fn create_allows_the_same_alias_across_different_tenants() {
        let state = ControlPlaneState::new();
        create_upstream(
            &state,
            Uuid::new_v4(),
            http_upstream_body("shared.example.com"),
        )
        .expect("tenant a create must succeed");
        create_upstream(
            &state,
            Uuid::new_v4(),
            http_upstream_body("shared.example.com"),
        )
        .expect("tenant b create must succeed");
    }

    #[test]
    fn get_returns_404_for_an_unknown_id() {
        let state = ControlPlaneState::new();
        let error =
            get_upstream(&state, Uuid::new_v4(), Uuid::new_v4()).expect_err("unknown id must 404");
        assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn get_does_not_resolve_another_tenants_upstream() {
        let state = ControlPlaneState::new();
        let owner = Uuid::new_v4();
        let other = Uuid::new_v4();
        let upstream = create_upstream(&state, owner, http_upstream_body("scoped.example.com"))
            .expect("create");

        let error =
            get_upstream(&state, other, upstream.id).expect_err("cross-tenant get must 404");
        assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn delete_removes_the_record_and_a_subsequent_get_404s() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let upstream = create_upstream(&state, tenant_id, http_upstream_body("gone.example.com"))
            .expect("create");

        delete_upstream(&state, tenant_id, upstream.id).expect("delete must succeed");
        assert!(get_upstream(&state, tenant_id, upstream.id).is_err());
    }

    #[test]
    fn delete_returns_404_for_an_unknown_id() {
        let state = ControlPlaneState::new();
        let error = delete_upstream(&state, Uuid::new_v4(), Uuid::new_v4())
            .expect_err("unknown id must 404");
        assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn replace_rejects_an_endpoint_change_that_would_alter_the_alias() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let upstream = create_upstream(&state, tenant_id, http_upstream_body("stable.example.com"))
            .expect("create");

        let error = replace_upstream(
            &state,
            tenant_id,
            upstream.id,
            http_upstream_body("changed.example.com"),
        )
        .expect_err("alias-changing replace must be rejected");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);

        let unchanged = get_upstream(&state, tenant_id, upstream.id).expect("still present");
        assert_eq!(unchanged.alias, "stable.example.com");
    }

    #[test]
    fn replace_accepts_an_idempotent_alias_no_op() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let upstream = create_upstream(
            &state,
            tenant_id,
            http_upstream_body("idempotent.example.com"),
        )
        .expect("create");

        let replaced = replace_upstream(
            &state,
            tenant_id,
            upstream.id,
            http_upstream_body("idempotent.example.com"),
        )
        .expect("same-alias replace must succeed");
        assert_eq!(replaced.alias, "idempotent.example.com");
    }

    #[test]
    fn create_rejects_an_alias_matching_a_disabled_ancestor_upstream() {
        let state = ControlPlaneState::new();
        let mut root_body = http_upstream_body("ancestor.example.com");
        root_body["enabled"] = json!(false);
        create_upstream(&state, ROOT_TENANT_ID, root_body).expect("root create must succeed");

        let error = create_upstream(
            &state,
            Uuid::new_v4(),
            http_upstream_body("ancestor.example.com"),
        )
        .expect_err("create under a disabled ancestor alias must be rejected");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    // CODE1-F-002: when an alias is BOTH already taken for this tenant AND
    // disabled on an ancestor tenant, the alias-uniqueness check must win
    // (409), not the ancestor-disable check (400).
    #[test]
    fn create_returns_conflict_when_alias_is_both_taken_and_ancestor_disabled() {
        let state = ControlPlaneState::new();
        let mut root_body = http_upstream_body("both.example.com");
        root_body["enabled"] = json!(false);
        create_upstream(&state, ROOT_TENANT_ID, root_body).expect("root create must succeed");

        let tenant_id = Uuid::new_v4();
        // Disabled at creation, so it bypasses the ancestor-disable check and
        // seeds the "alias already taken for this tenant" state.
        let mut first_body = http_upstream_body("both.example.com");
        first_body["enabled"] = json!(false);
        create_upstream(&state, tenant_id, first_body)
            .expect("first create (disabled, bypassing the ancestor check) must succeed");

        let error = create_upstream(&state, tenant_id, http_upstream_body("both.example.com"))
            .expect_err("second create must be rejected");
        assert_eq!(
            error.status(),
            axum::http::StatusCode::CONFLICT,
            "the alias-uniqueness check must run before the ancestor-disable check"
        );
    }

    // BUG1-F-001: two concurrent creates deriving the same alias for the same
    // tenant must not both succeed. Genuine OS-thread concurrency (rather
    // than cooperative `tokio` tasks on a single poll) is used so the race
    // window is real, not merely simulated; the invariant checked — exactly
    // one surviving upstream carries the alias — is the one the finding
    // calls out as an acceptable substitute when the underlying interleaving
    // itself can't be forced deterministically.
    #[test]
    fn concurrent_creates_with_the_same_alias_leave_exactly_one_upstream() {
        let state = std::sync::Arc::new(ControlPlaneState::new());
        let tenant_id = Uuid::new_v4();

        // The eager `.collect()` below is load-bearing, not needless: it
        // forces every `std::thread::spawn` call to run immediately, so all
        // 8 threads are genuinely racing before any is joined. Chaining
        // `.map(spawn).map(join)` directly (clippy's usual suggestion) would
        // make this iterator lazy and spawn-then-immediately-join one thread
        // at a time, eliminating the very race window this test exists to
        // exercise (see the `BUG1-F-00*` comment above).
        #[allow(clippy::needless_collect)]
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let state = std::sync::Arc::clone(&state);
                std::thread::spawn(move || {
                    create_upstream(&state, tenant_id, http_upstream_body("race.example.com"))
                })
            })
            .collect();

        let successes = handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("worker thread must not panic"))
            .count();
        assert_eq!(
            successes, 1,
            "exactly one concurrent create must win the alias"
        );

        let tenant = state.tenant(tenant_id);
        let matching = tenant
            .upstreams
            .iter()
            .filter(|entry| entry.value().alias == "race.example.com")
            .count();
        assert_eq!(matching, 1, "exactly one upstream must carry the alias");
    }

    #[test]
    fn replace_rejects_re_enabling_under_a_disabled_ancestor_alias() {
        let state = ControlPlaneState::new();
        let mut root_body = http_upstream_body("shadowed.example.com");
        root_body["enabled"] = json!(false);
        create_upstream(&state, ROOT_TENANT_ID, root_body).expect("root create must succeed");

        let tenant_id = Uuid::new_v4();
        let mut descendant_body = http_upstream_body("shadowed.example.com");
        descendant_body["enabled"] = json!(false);
        let upstream = create_upstream(&state, tenant_id, descendant_body)
            .expect("descendant create (disabled) must succeed");

        let mut re_enable_body = http_upstream_body("shadowed.example.com");
        re_enable_body["enabled"] = json!(true);
        let error = replace_upstream(&state, tenant_id, upstream.id, re_enable_body)
            .expect_err("re-enable under a disabled ancestor alias must be rejected");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn list_honors_top_and_returns_only_the_calling_tenants_upstreams() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        for host in ["a.example.com", "b.example.com", "c.example.com"] {
            create_upstream(&state, tenant_id, http_upstream_body(host)).expect("create");
        }
        create_upstream(
            &state,
            Uuid::new_v4(),
            http_upstream_body("other-tenant.example.com"),
        )
        .expect("other tenant create");

        let raw = RawListQuery {
            top: Some(2),
            ..RawListQuery::default()
        };
        let page = list_upstreams(&state, tenant_id, &raw).expect("list must succeed");
        assert_eq!(page.len(), 2);
    }

    // -----------------------------------------------------------------
    // Route management (`cpt-cf-oagw-feature-route-management`).
    // -----------------------------------------------------------------

    fn http_route_body(upstream_id: Uuid, path: &str, methods: &[&str]) -> serde_json::Value {
        json!({
            "upstream_id": upstream_id,
            "match": {"http": {"methods": methods, "path": path}},
        })
    }

    fn create_test_upstream(state: &ControlPlaneState, tenant_id: Uuid, host: &str) -> Uuid {
        create_upstream(state, tenant_id, http_upstream_body(host))
            .expect("upstream create must succeed")
            .id
    }

    #[test]
    fn create_route_against_an_existing_upstream_succeeds() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let upstream_id = create_test_upstream(&state, tenant_id, "widgets.example.com");

        let route = create_route(
            &state,
            tenant_id,
            http_route_body(upstream_id, "/v1/widgets", &["GET"]),
        )
        .expect("route create must succeed");

        assert_eq!(route.upstream_id, upstream_id);
        assert!(route.enabled);
        assert_eq!(route.priority, 0);
    }

    #[test]
    fn create_route_rejects_an_unknown_upstream_id() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();

        let error = create_route(
            &state,
            tenant_id,
            http_route_body(Uuid::new_v4(), "/v1/widgets", &["GET"]),
        )
        .expect_err("unknown upstream_id must be rejected");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn create_route_rejects_an_upstream_id_owned_by_another_tenant() {
        let state = ControlPlaneState::new();
        let owner = Uuid::new_v4();
        let other = Uuid::new_v4();
        let upstream_id = create_test_upstream(&state, owner, "owned.example.com");

        let error = create_route(
            &state,
            other,
            http_route_body(upstream_id, "/v1/widgets", &["GET"]),
        )
        .expect_err("cross-tenant upstream_id must be rejected");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn create_route_with_intersecting_methods_same_path_and_priority_conflicts() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let upstream_id = create_test_upstream(&state, tenant_id, "conflict.example.com");

        create_route(
            &state,
            tenant_id,
            http_route_body(upstream_id, "/v1/widgets", &["GET", "POST"]),
        )
        .expect("first route create must succeed");

        let error = create_route(
            &state,
            tenant_id,
            http_route_body(upstream_id, "/v1/widgets", &["POST", "PUT"]),
        )
        .expect_err("intersecting method set must conflict");
        assert_eq!(error.status(), axum::http::StatusCode::CONFLICT);
    }

    // BUG1-F-003: two concurrent creates with the same `upstream_id`, `path`,
    // `priority`, and an intersecting method set must not both succeed. See
    // `concurrent_creates_with_the_same_alias_leave_exactly_one_upstream`
    // for why genuine OS threads (rather than `tokio` tasks) are used here.
    #[test]
    fn concurrent_creates_with_a_conflicting_route_leave_exactly_one_route() {
        let state = std::sync::Arc::new(ControlPlaneState::new());
        let tenant_id = Uuid::new_v4();
        let upstream_id = create_test_upstream(&state, tenant_id, "route-race.example.com");

        // The eager `.collect()` below is load-bearing, not needless: it
        // forces every `std::thread::spawn` call to run immediately, so all
        // 8 threads are genuinely racing before any is joined. Chaining
        // `.map(spawn).map(join)` directly (clippy's usual suggestion) would
        // make this iterator lazy and spawn-then-immediately-join one thread
        // at a time, eliminating the very race window this test exists to
        // exercise (see the `BUG1-F-00*` comment above).
        #[allow(clippy::needless_collect)]
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let state = std::sync::Arc::clone(&state);
                std::thread::spawn(move || {
                    create_route(
                        &state,
                        tenant_id,
                        http_route_body(upstream_id, "/v1/race", &["GET"]),
                    )
                })
            })
            .collect();

        let successes = handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("worker thread must not panic"))
            .count();
        assert_eq!(
            successes, 1,
            "exactly one concurrent create must win the route"
        );

        let matching = state
            .tenant(tenant_id)
            .routes
            .iter()
            .filter(|entry| {
                entry.value().upstream_id == upstream_id
                    && entry
                        .value()
                        .match_config
                        .http
                        .as_ref()
                        .is_some_and(|http| http.path == "/v1/race")
            })
            .count();
        assert_eq!(matching, 1, "exactly one route must have been stored");
    }

    #[test]
    fn create_route_same_methods_and_path_but_different_priority_succeeds() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let upstream_id = create_test_upstream(&state, tenant_id, "priority.example.com");

        create_route(
            &state,
            tenant_id,
            http_route_body(upstream_id, "/v1/widgets", &["GET"]),
        )
        .expect("first route create must succeed");

        let mut second_body = http_route_body(upstream_id, "/v1/widgets", &["GET"]);
        second_body["priority"] = json!(10);
        let second = create_route(&state, tenant_id, second_body)
            .expect("a different priority must not conflict");
        assert_eq!(second.priority, 10);
    }

    #[test]
    fn get_route_returns_404_for_an_unknown_id() {
        let state = ControlPlaneState::new();
        let error =
            get_route(&state, Uuid::new_v4(), Uuid::new_v4()).expect_err("unknown id must 404");
        assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn get_route_does_not_resolve_another_tenants_route() {
        let state = ControlPlaneState::new();
        let owner = Uuid::new_v4();
        let other = Uuid::new_v4();
        let upstream_id = create_test_upstream(&state, owner, "scoped-route.example.com");
        let route = create_route(
            &state,
            owner,
            http_route_body(upstream_id, "/v1/widgets", &["GET"]),
        )
        .expect("create");

        let error = get_route(&state, other, route.id).expect_err("cross-tenant get must 404");
        assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn delete_route_removes_the_record_and_a_subsequent_get_404s() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let upstream_id = create_test_upstream(&state, tenant_id, "gone-route.example.com");
        let route = create_route(
            &state,
            tenant_id,
            http_route_body(upstream_id, "/v1/widgets", &["GET"]),
        )
        .expect("create");

        delete_route(&state, tenant_id, route.id).expect("delete must succeed");
        assert!(get_route(&state, tenant_id, route.id).is_err());
    }

    #[test]
    fn delete_route_returns_404_for_an_unknown_id() {
        let state = ControlPlaneState::new();
        let error =
            delete_route(&state, Uuid::new_v4(), Uuid::new_v4()).expect_err("unknown id must 404");
        assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn replace_route_rejects_a_different_upstream_id() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let upstream_id = create_test_upstream(&state, tenant_id, "immutable.example.com");
        let other_upstream_id = create_test_upstream(&state, tenant_id, "other-up.example.com");
        let route = create_route(
            &state,
            tenant_id,
            http_route_body(upstream_id, "/v1/widgets", &["GET"]),
        )
        .expect("create");

        let error = replace_route(
            &state,
            tenant_id,
            route.id,
            http_route_body(other_upstream_id, "/v1/widgets", &["GET"]),
        )
        .expect_err("upstream_id reassignment must be rejected");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn replace_route_omitting_upstream_id_retains_the_stored_value() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let upstream_id = create_test_upstream(&state, tenant_id, "retain.example.com");
        let route = create_route(
            &state,
            tenant_id,
            http_route_body(upstream_id, "/v1/widgets", &["GET"]),
        )
        .expect("create");

        let mut body = json!({
            "match": {"http": {"methods": ["GET", "POST"], "path": "/v1/widgets"}},
        });
        body["enabled"] = json!(false);
        let replaced = replace_route(&state, tenant_id, route.id, body)
            .expect("replace omitting upstream_id must succeed");
        assert_eq!(replaced.upstream_id, upstream_id);
        assert!(!replaced.enabled);
    }

    #[test]
    fn delete_upstream_cascade_deletes_its_routes() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let upstream_id = create_test_upstream(&state, tenant_id, "cascade.example.com");
        let route = create_route(
            &state,
            tenant_id,
            http_route_body(upstream_id, "/v1/widgets", &["GET"]),
        )
        .expect("create");

        delete_upstream(&state, tenant_id, upstream_id).expect("upstream delete must succeed");

        let error = get_route(&state, tenant_id, route.id).expect_err("cascaded route must 404");
        assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn list_routes_honors_top_and_returns_only_the_calling_tenants_routes() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let upstream_id = create_test_upstream(&state, tenant_id, "list-routes.example.com");
        for path in ["/v1/a", "/v1/b", "/v1/c"] {
            create_route(
                &state,
                tenant_id,
                http_route_body(upstream_id, path, &["GET"]),
            )
            .expect("create");
        }
        let other_tenant = Uuid::new_v4();
        let other_upstream = create_test_upstream(&state, other_tenant, "other-list.example.com");
        create_route(
            &state,
            other_tenant,
            http_route_body(other_upstream, "/v1/other", &["GET"]),
        )
        .expect("other tenant create");

        let raw = RawListQuery {
            top: Some(2),
            ..RawListQuery::default()
        };
        let page = list_routes(&state, tenant_id, &raw).expect("list must succeed");
        assert_eq!(page.len(), 2);
    }

    // -----------------------------------------------------------------
    // Plugin management (`cpt-cf-oagw-feature-plugin-management`).
    // -----------------------------------------------------------------

    fn guard_plugin_body(name: &str) -> serde_json::Value {
        json!({
            "plugin_type": "guard",
            "name": name,
            "config_schema": {"type": "object"},
            "phases": ["on_request", "on_response"],
            "source_code": "def on_request(ctx):\n    return ctx.next()",
        })
    }

    // @cpt-begin:cpt-cf-oagw-dod-plugin-create:p1:inst-create-plugin-service-test-01
    #[test]
    fn create_returns_a_plugin_with_a_gts_form_id_matching_its_type() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();

        let plugin = create_plugin(&state, tenant_id, guard_plugin_body("request_validator"))
            .expect("create must succeed");

        assert!(plugin.id.starts_with("gts.cf.core.oagw.guard_plugin.v1~"));
        assert_eq!(plugin.name, "request_validator");
    }
    // @cpt-end:cpt-cf-oagw-dod-plugin-create:p1:inst-create-plugin-service-test-01

    // @cpt-begin:cpt-cf-oagw-dod-plugin-name-uniqueness:p1:inst-create-plugin-unique-test-01
    #[test]
    fn create_rejects_a_duplicate_name_for_the_same_tenant_regardless_of_type() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        create_plugin(&state, tenant_id, guard_plugin_body("dup_plugin"))
            .expect("first create must succeed");

        let mut second_body = guard_plugin_body("dup_plugin");
        second_body["plugin_type"] = json!("transform");
        second_body["phases"] = json!([]);
        let error = create_plugin(&state, tenant_id, second_body)
            .expect_err("duplicate name must be rejected");
        assert_eq!(error.status(), axum::http::StatusCode::CONFLICT);
    }
    // @cpt-end:cpt-cf-oagw-dod-plugin-name-uniqueness:p1:inst-create-plugin-unique-test-01

    #[test]
    fn create_allows_the_same_name_across_different_tenants() {
        let state = ControlPlaneState::new();
        create_plugin(&state, Uuid::new_v4(), guard_plugin_body("shared_name"))
            .expect("tenant a create must succeed");
        create_plugin(&state, Uuid::new_v4(), guard_plugin_body("shared_name"))
            .expect("tenant b create must succeed");
    }

    // BUG1-F-002: two concurrent creates with the same `name` for the same
    // tenant must not both succeed. See
    // `concurrent_creates_with_the_same_alias_leave_exactly_one_upstream`
    // for why genuine OS threads (rather than `tokio` tasks) are used here.
    #[test]
    fn concurrent_creates_with_the_same_plugin_name_leave_exactly_one_plugin() {
        let state = std::sync::Arc::new(ControlPlaneState::new());
        let tenant_id = Uuid::new_v4();

        // The eager `.collect()` below is load-bearing, not needless: it
        // forces every `std::thread::spawn` call to run immediately, so all
        // 8 threads are genuinely racing before any is joined. Chaining
        // `.map(spawn).map(join)` directly (clippy's usual suggestion) would
        // make this iterator lazy and spawn-then-immediately-join one thread
        // at a time, eliminating the very race window this test exists to
        // exercise (see the `BUG1-F-00*` comment above).
        #[allow(clippy::needless_collect)]
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let state = std::sync::Arc::clone(&state);
                std::thread::spawn(move || {
                    create_plugin(&state, tenant_id, guard_plugin_body("race_name"))
                })
            })
            .collect();

        let successes = handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("worker thread must not panic"))
            .count();
        assert_eq!(
            successes, 1,
            "exactly one concurrent create must win the name"
        );

        let matching = state
            .tenant(tenant_id)
            .plugins
            .iter()
            .filter(|entry| entry.value().name == "race_name")
            .count();
        assert_eq!(matching, 1, "exactly one plugin must carry the name");
    }

    // @cpt-begin:cpt-cf-oagw-dod-plugin-get:p1:inst-get-plugin-service-test-01
    #[test]
    fn get_returns_the_plugin_without_source_code() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let created = create_plugin(&state, tenant_id, guard_plugin_body("get_me"))
            .expect("create must succeed");

        let fetched = get_plugin(&state, tenant_id, &created.id).expect("get by id must succeed");
        assert_eq!(fetched.id, created.id);
        assert_eq!(fetched.name, "get_me");
    }
    // @cpt-end:cpt-cf-oagw-dod-plugin-get:p1:inst-get-plugin-service-test-01

    #[test]
    fn get_plugin_returns_404_for_an_unknown_id() {
        let state = ControlPlaneState::new();
        let gts_ref = format!("gts.cf.core.oagw.guard_plugin.v1~{}", Uuid::new_v4());
        let error = get_plugin(&state, Uuid::new_v4(), &gts_ref).expect_err("unknown id must 404");
        assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn get_does_not_resolve_another_tenants_plugin() {
        let state = ControlPlaneState::new();
        let owner = Uuid::new_v4();
        let other = Uuid::new_v4();
        let created = create_plugin(&state, owner, guard_plugin_body("scoped_plugin"))
            .expect("create must succeed");

        let error = get_plugin(&state, other, &created.id).expect_err("cross-tenant get must 404");
        assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
    }

    // @cpt-begin:cpt-cf-oagw-dod-plugin-get-source:p1:inst-get-plugin-source-service-test-01
    #[test]
    fn get_source_returns_the_stored_source_text() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let created = create_plugin(&state, tenant_id, guard_plugin_body("source_me"))
            .expect("create must succeed");

        let source =
            get_plugin_source(&state, tenant_id, &created.id).expect("get source must succeed");
        assert!(source.source_code.contains("on_request"));
    }
    // @cpt-end:cpt-cf-oagw-dod-plugin-get-source:p1:inst-get-plugin-source-service-test-01

    // @cpt-begin:cpt-cf-oagw-dod-plugin-identification:p1:inst-get-plugin-source-named-test-01
    #[test]
    fn get_source_returns_404_for_a_named_built_in_identifier() {
        let state = ControlPlaneState::new();
        let named_ref = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
        let error = get_plugin_source(&state, Uuid::new_v4(), named_ref)
            .expect_err("named plugins carry no stored source");
        assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
    }
    // @cpt-end:cpt-cf-oagw-dod-plugin-identification:p1:inst-get-plugin-source-named-test-01

    // @cpt-begin:cpt-cf-oagw-dod-plugin-delete:p1:inst-delete-plugin-service-test-01
    #[test]
    fn delete_plugin_removes_the_record_and_a_subsequent_get_404s() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let created = create_plugin(&state, tenant_id, guard_plugin_body("delete_me"))
            .expect("create must succeed");

        delete_plugin(&state, tenant_id, &created.id).expect("delete must succeed");
        assert!(get_plugin(&state, tenant_id, &created.id).is_err());
    }
    // @cpt-end:cpt-cf-oagw-dod-plugin-delete:p1:inst-delete-plugin-service-test-01

    #[test]
    fn delete_plugin_returns_404_for_an_unknown_id() {
        let state = ControlPlaneState::new();
        let gts_ref = format!("gts.cf.core.oagw.guard_plugin.v1~{}", Uuid::new_v4());
        let error =
            delete_plugin(&state, Uuid::new_v4(), &gts_ref).expect_err("unknown id must 404");
        assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
    }

    // @cpt-begin:cpt-cf-oagw-dod-plugin-delete:p1:inst-delete-plugin-in-use-test-01
    #[test]
    fn delete_rejects_a_plugin_still_bound_to_an_upstream() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let created = create_plugin(&state, tenant_id, guard_plugin_body("bound_plugin"))
            .expect("create must succeed");

        let mut upstream_body = http_upstream_body("plugin-bound.example.com");
        upstream_body["plugins"] = json!({"items": [created.id]});
        create_upstream(&state, tenant_id, upstream_body).expect("upstream create must succeed");

        let error = delete_plugin(&state, tenant_id, &created.id)
            .expect_err("in-use plugin must not be deletable");
        assert_eq!(error.status(), axum::http::StatusCode::CONFLICT);

        let problem = error.to_problem();
        assert_eq!(problem.context["plugin_id"], created.id);
        assert_eq!(
            problem.context["referenced_by"]["upstreams"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
    }
    // @cpt-end:cpt-cf-oagw-dod-plugin-delete:p1:inst-delete-plugin-in-use-test-01

    // @cpt-begin:cpt-cf-oagw-dod-plugin-lifecycle-tracking:p1:inst-delete-upstream-plugin-binding-test-01
    #[test]
    fn delete_upstream_removes_its_own_plugin_bindings() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        let created = create_plugin(&state, tenant_id, guard_plugin_body("unbind_me"))
            .expect("plugin create must succeed");

        let mut upstream_body = http_upstream_body("unbind.example.com");
        upstream_body["plugins"] = json!({"items": [created.id]});
        let upstream = create_upstream(&state, tenant_id, upstream_body)
            .expect("upstream create must succeed");

        // Still in use: cannot be deleted yet.
        assert!(delete_plugin(&state, tenant_id, &created.id).is_err());

        delete_upstream(&state, tenant_id, upstream.id).expect("upstream delete must succeed");

        // The upstream's plugin binding is gone with it, so the plugin is
        // now deletable.
        delete_plugin(&state, tenant_id, &created.id)
            .expect("plugin must be deletable once its only binding is removed");
    }
    // @cpt-end:cpt-cf-oagw-dod-plugin-lifecycle-tracking:p1:inst-delete-upstream-plugin-binding-test-01

    // BUG1-F-004: a concurrent plugin delete and a concurrent write that
    // removes an upstream's only binding to that same plugin must never
    // leave the plugin removed while a live binding to it still exists.
    //
    // This deliberately races `delete_plugin` against `replace_upstream`
    // dropping the binding, not against a *new* binding being created:
    // `create_upstream`/`create_route` never validate that a referenced
    // plugin id exists (dangling references are a documented, accepted
    // runtime concern — resolved lazily and surfaced as a proxy-time 503,
    // per `crate::domain::service::plugin_in_use_error`'s module doc), so a
    // create racing a delete has two equally legitimate outcomes depending
    // on ordering and asserting a single invariant across both would be
    // asserting behavior this fix never promised. Racing an *unbind* against
    // the delete instead has exactly one consistent outcome regardless of
    // which one the shared per-tenant guard lets run first: either the
    // unbind commits first (the plugin is no longer referenced, so the
    // delete's in-use scan is empty and it succeeds) or the delete commits
    // first (the binding still exists, so the in-use scan finds it and the
    // delete is rejected, leaving the plugin in place for the unbind to
    // proceed against). Either way, the plugin is never removed while a live
    // binding to it survives — the exact guarantee `write_guard` is meant to
    // provide. Repeated across several iterations to exercise the race
    // window; per the finding's guidance, this asserts the invariant rather
    // than forcing one specific interleaving.
    #[test]
    fn concurrent_plugin_delete_and_binding_removal_never_leave_a_dangling_reference() {
        for iteration in 0..20u32 {
            let state = std::sync::Arc::new(ControlPlaneState::new());
            let tenant_id = Uuid::new_v4();
            let created = create_plugin(
                &state,
                tenant_id,
                guard_plugin_body(&format!("race_unbind_{iteration}")),
            )
            .expect("plugin create must succeed");
            let plugin_gts_id = created.id.clone();

            let host = format!("race-unbind-{iteration}.example.com");
            let mut upstream_body = http_upstream_body(&host);
            upstream_body["plugins"] = json!({"items": [plugin_gts_id.clone()]});
            let upstream = create_upstream(&state, tenant_id, upstream_body)
                .expect("bound upstream create must succeed");

            let unbind_state = std::sync::Arc::clone(&state);
            let unbind_host = host.clone();
            let unbind_handle = std::thread::spawn(move || {
                // Omits `plugins` entirely, replacing the binding with none.
                replace_upstream(
                    &unbind_state,
                    tenant_id,
                    upstream.id,
                    http_upstream_body(&unbind_host),
                )
            });

            let delete_state = std::sync::Arc::clone(&state);
            let delete_gts_id = plugin_gts_id.clone();
            let delete_handle =
                std::thread::spawn(move || delete_plugin(&delete_state, tenant_id, &delete_gts_id));

            let _unbind_result = unbind_handle.join().expect("unbind thread must not panic");
            let _delete_result = delete_handle.join().expect("delete thread must not panic");

            let tenant = state.tenant(tenant_id);
            let plugin_still_exists = tenant.plugins.iter().any(|entry| {
                crate::domain::model::gts_plugin_id(entry.value().plugin_type, entry.value().id)
                    == plugin_gts_id
            });
            let (upstream_ids, route_ids) =
                crate::domain::plugin_resolve::plugin_references(&tenant, &plugin_gts_id);
            assert!(
                plugin_still_exists || (upstream_ids.is_empty() && route_ids.is_empty()),
                "iteration {iteration}: plugin was deleted while still referenced by a bound upstream"
            );
        }
    }

    #[test]
    fn list_honors_top_and_returns_only_the_calling_tenants_plugins() {
        let state = ControlPlaneState::new();
        let tenant_id = Uuid::new_v4();
        for name in ["plugin_a", "plugin_b", "plugin_c"] {
            create_plugin(&state, tenant_id, guard_plugin_body(name)).expect("create");
        }
        create_plugin(
            &state,
            Uuid::new_v4(),
            guard_plugin_body("other_tenant_plugin"),
        )
        .expect("other tenant create");

        let raw = RawListQuery {
            top: Some(2),
            ..RawListQuery::default()
        };
        let page = list_plugins(&state, tenant_id, &raw).expect("list must succeed");
        assert_eq!(page.len(), 2);
    }
}
