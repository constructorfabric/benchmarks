//! The five Upstream Management API handlers -- `cpt-cf-oagw-flow-create-upstream`,
//! `-flow-list-upstreams`, `-flow-get-upstream`, `-flow-replace-upstream`,
//! `-flow-delete-upstream`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query};
use axum::http::StatusCode;
use axum::{Extension, Json};
use uuid::Uuid;

use crate::error::{OagwError, OagwErrorKind};
use crate::model::upstream::{Upstream, alias, ident, validate};
use crate::store::OagwState;

use super::list_query::{self, ListQueryParams};
use super::{
    ApiError, BIND_PERMISSION, TenantHierarchyProvider, conflict, has_permission, not_found,
    store_ops,
};
use toolkit_security::SecurityContext;

fn parse_json_body(body: &[u8]) -> Result<serde_json::Value, ApiError> {
    serde_json::from_slice(body).map_err(|e| {
        ApiError::from(OagwError::new(
            OagwErrorKind::ValidationError,
            format!("invalid JSON body: {e}"),
        ))
    })
}

fn validation_error(detail: impl Into<String>) -> ApiError {
    ApiError::from(OagwError::new(OagwErrorKind::ValidationError, detail))
}

/// `POST /oagw/v1/upstreams` -- `cpt-cf-oagw-flow-create-upstream`.
// @cpt-begin:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-request
// @cpt-begin:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-auth
pub(super) async fn create_upstream(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Extension(hierarchy): Extension<Arc<dyn TenantHierarchyProvider>>,
    body: Bytes,
) -> Result<(StatusCode, Json<Upstream>), ApiError> {
    // @cpt-end:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-auth
    // @cpt-end:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-request
    let value = parse_json_body(&body)?;

    // @cpt-begin:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-validate
    let draft = match validate::validate_upstream_body(&value) {
        // @cpt-begin:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-validate-ok
        Ok(draft) => draft,
        // @cpt-end:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-validate-ok
        // @cpt-begin:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-validate-fail
        Err(e) => {
            // @cpt-begin:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-validate-fail-return
            return Err(validation_error(e.0));
            // @cpt-end:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-validate-fail-return
        } // @cpt-end:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-validate-fail
    };
    // @cpt-end:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-validate

    // @cpt-begin:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-alias
    // @cpt-begin:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-alias-ok
    let resolved_alias = match alias::resolve_alias(&draft.server.endpoints, draft.alias.as_deref())
    {
        Ok(a) => a,
        // @cpt-end:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-alias-ok
        // @cpt-begin:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-alias-fail
        // @cpt-begin:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-alias-fail-return
        Err(e) => return Err(validation_error(e.0)),
        // @cpt-end:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-alias-fail-return
        // @cpt-end:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-alias-fail
    };
    // @cpt-end:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-alias

    let tenant_id = ctx.subject_tenant_id();

    // @cpt-begin:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-scope
    // @cpt-begin:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-branch
    // @cpt-begin:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-own-lookup
    // @cpt-begin:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-conflict
    if store_ops::find_by_alias_for_tenant(&state, tenant_id, &resolved_alias).is_some() {
        // @cpt-end:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-own-lookup
        // @cpt-begin:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-own-conflict
        // @cpt-begin:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-own-conflict-return
        // @cpt-begin:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-conflict-return
        return Err(conflict(
            "an upstream with this alias already exists for the calling tenant",
        ));
        // @cpt-end:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-conflict-return
        // @cpt-end:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-own-conflict-return
        // @cpt-end:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-own-conflict
    }
    // @cpt-end:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-conflict

    // @cpt-begin:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-walk-branch
    // @cpt-begin:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-ancestor-walk
    // @cpt-begin:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-bind-denied
    let ancestor_upstream =
        store_ops::find_ancestor_upstream(&state, hierarchy.as_ref(), tenant_id, &resolved_alias);
    // @cpt-end:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-ancestor-walk
    // @cpt-end:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-walk-branch
    // @cpt-begin:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-no-ancestor
    // @cpt-begin:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-no-ancestor-return
    // @cpt-begin:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-ancestor-found
    // @cpt-begin:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-bind-blocked
    if ancestor_upstream.is_some() && !has_permission(&ctx, BIND_PERMISSION) {
        // @cpt-end:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-no-ancestor-return
        // @cpt-end:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-no-ancestor
        // @cpt-begin:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-bind-blocked-return
        // @cpt-begin:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-bind-denied-return
        return Err(conflict(
            "ancestor upstream shares this alias and is not bindable from this tenant",
        ));
        // @cpt-end:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-bind-denied-return
        // @cpt-end:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-bind-blocked-return
    }
    // @cpt-end:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-bind-blocked
    // @cpt-begin:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-bind-allowed
    // @cpt-begin:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-bind-allowed-return
    // An ancestor found with bind permission granted (or no ancestor at
    // all) falls through to the persist branch below: an ordinary create,
    // or a tenant-local bind record referencing the shared alias.
    // @cpt-end:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-bind-allowed-return
    // @cpt-end:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-bind-allowed
    // @cpt-end:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-ancestor-found
    // @cpt-end:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-bind-denied
    // @cpt-end:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-create-branch
    // @cpt-end:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-scope

    // @cpt-begin:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-persist-branch
    // @cpt-begin:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-persist
    let mut record = draft;
    record.id = Some(Uuid::new_v4());
    record.alias = Some(resolved_alias);
    record.tenant_id = tenant_id;
    state
        .store
        .upstreams()
        .insert(record.id.unwrap_or_default(), Arc::new(record.clone()));
    // @cpt-end:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-persist
    // @cpt-end:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-persist-branch

    // @cpt-begin:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-return
    Ok((StatusCode::CREATED, Json(record)))
    // @cpt-end:cpt-cf-oagw-flow-create-upstream:p1:inst-create-upstream-return
}

/// `GET /oagw/v1/upstreams` -- `cpt-cf-oagw-flow-list-upstreams`.
// @cpt-dod:cpt-cf-oagw-dod-list-query-params:p1
// @cpt-begin:cpt-cf-oagw-flow-list-upstreams:p1:inst-list-upstreams-request
// @cpt-begin:cpt-cf-oagw-flow-list-upstreams:p1:inst-list-upstreams-auth
pub(super) async fn list_upstreams(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Query(params): Query<ListQueryParams>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    // @cpt-end:cpt-cf-oagw-flow-list-upstreams:p1:inst-list-upstreams-auth
    // @cpt-end:cpt-cf-oagw-flow-list-upstreams:p1:inst-list-upstreams-request
    let tenant_id = ctx.subject_tenant_id();

    // @cpt-begin:cpt-cf-oagw-flow-list-upstreams:p1:inst-list-upstreams-query
    let mut items: Vec<serde_json::Value> = store_ops::list_for_tenant(&state, tenant_id)
        .iter()
        .map(|up| serde_json::to_value(up.as_ref()).unwrap_or(serde_json::Value::Null))
        .collect();

    if let Some(filter) = params.filter.as_deref() {
        items = list_query::apply_filter(items, filter).map_err(validation_error)?;
    }
    if let Some(orderby) = params.orderby.as_deref() {
        items = list_query::apply_orderby(items, orderby);
    }
    // @cpt-end:cpt-cf-oagw-flow-list-upstreams:p1:inst-list-upstreams-query

    // @cpt-begin:cpt-cf-oagw-flow-list-upstreams:p1:inst-list-upstreams-parse
    let page = list_query::parse_page(params.top.as_deref(), params.skip.as_deref())
        .map_err(validation_error)?;
    // @cpt-end:cpt-cf-oagw-flow-list-upstreams:p1:inst-list-upstreams-parse

    let items: Vec<serde_json::Value> = items.into_iter().skip(page.skip).take(page.top).collect();
    let items = match params.select.as_deref() {
        Some(select) => list_query::apply_select(items, select),
        None => items,
    };

    // @cpt-begin:cpt-cf-oagw-flow-list-upstreams:p1:inst-list-upstreams-return
    Ok(Json(items))
    // @cpt-end:cpt-cf-oagw-flow-list-upstreams:p1:inst-list-upstreams-return
}

/// `GET /oagw/v1/upstreams/{id}` -- `cpt-cf-oagw-flow-get-upstream`.
// @cpt-begin:cpt-cf-oagw-flow-get-upstream:p1:inst-get-upstream-request
// @cpt-begin:cpt-cf-oagw-flow-get-upstream:p1:inst-get-upstream-auth
pub(super) async fn get_upstream(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(raw_id): Path<String>,
) -> Result<Json<Upstream>, ApiError> {
    // @cpt-end:cpt-cf-oagw-flow-get-upstream:p1:inst-get-upstream-auth
    // @cpt-end:cpt-cf-oagw-flow-get-upstream:p1:inst-get-upstream-request
    // @cpt-begin:cpt-cf-oagw-flow-get-upstream:p1:inst-get-upstream-scope
    let id = ident::normalize_upstream_path_id(&raw_id).ok_or_else(not_found)?;
    // @cpt-end:cpt-cf-oagw-flow-get-upstream:p1:inst-get-upstream-scope
    let tenant_id = ctx.subject_tenant_id();

    // @cpt-begin:cpt-cf-oagw-flow-get-upstream:p1:inst-get-upstream-query
    // Representative instance of `cpt-cf-oagw-algo-resolve-tenant-scope`'s
    // non-create ("get"/"list"/"replace"/"delete") visibility branch: every
    // predicate here is `tenant_id = calling tenant` only
    // (`inst-scope-visibility-restrict`); `replace_upstream`/`delete_upstream`
    // apply the identical `store_ops::find_by_id_for_tenant` lookup below.
    // @cpt-begin:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-visibility-branch
    // @cpt-begin:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-visibility-restrict
    match store_ops::find_by_id_for_tenant(&state, tenant_id, id) {
        // @cpt-end:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-visibility-restrict
        // @cpt-begin:cpt-cf-oagw-flow-get-upstream:p1:inst-get-upstream-notfound
        // @cpt-begin:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-visibility-foreign
        None => {
            // @cpt-end:cpt-cf-oagw-flow-get-upstream:p1:inst-get-upstream-notfound
            // @cpt-begin:cpt-cf-oagw-flow-get-upstream:p1:inst-get-upstream-notfound-return
            // @cpt-begin:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-visibility-foreign-return
            Err(not_found())
            // @cpt-end:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-visibility-foreign-return
            // @cpt-end:cpt-cf-oagw-flow-get-upstream:p1:inst-get-upstream-notfound-return
        }
        // @cpt-end:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-visibility-foreign
        // @cpt-begin:cpt-cf-oagw-flow-get-upstream:p1:inst-get-upstream-found
        // @cpt-begin:cpt-cf-oagw-flow-get-upstream:p1:inst-get-upstream-return
        // @cpt-begin:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-visibility-own
        // @cpt-begin:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-visibility-own-return
        Some(upstream) => Ok(Json((*upstream).clone())),
        // @cpt-end:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-visibility-own-return
        // @cpt-end:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-visibility-own
        // @cpt-end:cpt-cf-oagw-flow-get-upstream:p1:inst-get-upstream-return
        // @cpt-end:cpt-cf-oagw-flow-get-upstream:p1:inst-get-upstream-found
    }
    // @cpt-end:cpt-cf-oagw-algo-resolve-tenant-scope:p1:inst-scope-visibility-branch
    // @cpt-end:cpt-cf-oagw-flow-get-upstream:p1:inst-get-upstream-query
}

/// `PUT /oagw/v1/upstreams/{id}` -- `cpt-cf-oagw-flow-replace-upstream`.
// @cpt-begin:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-request
// @cpt-begin:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-auth
pub(super) async fn replace_upstream(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Extension(hierarchy): Extension<Arc<dyn TenantHierarchyProvider>>,
    Path(raw_id): Path<String>,
    body: Bytes,
) -> Result<Json<Upstream>, ApiError> {
    // @cpt-end:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-auth
    // @cpt-end:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-request
    // @cpt-begin:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-scope
    let id = ident::normalize_upstream_path_id(&raw_id).ok_or_else(not_found)?;
    // @cpt-end:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-scope
    let tenant_id = ctx.subject_tenant_id();

    // @cpt-begin:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-notfound
    let Some(existing) = store_ops::find_by_id_for_tenant(&state, tenant_id, id) else {
        // @cpt-begin:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-notfound-return
        return Err(not_found());
        // @cpt-end:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-notfound-return
    };
    // @cpt-end:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-notfound
    // @cpt-begin:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-found

    let value = parse_json_body(&body)?;

    // @cpt-begin:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-validate
    let draft = match validate::validate_upstream_body(&value) {
        // @cpt-begin:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-validate-ok
        Ok(draft) => draft,
        // @cpt-end:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-validate-ok
        // @cpt-begin:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-validate-fail
        Err(e) => {
            // @cpt-begin:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-validate-fail-return
            return Err(validation_error(e.0));
            // @cpt-end:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-validate-fail-return
        } // @cpt-end:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-validate-fail
    };
    // @cpt-end:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-validate

    let existing_alias = existing.alias.clone().unwrap_or_default();
    // @cpt-begin:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-alias-check
    let resolved_alias = match alias::enforce_update(
        &existing_alias,
        &existing.server.endpoints,
        &draft.server.endpoints,
        draft.alias.as_deref(),
    ) {
        Ok(a) => a,
        // @cpt-begin:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-alias-fail
        Err(e) => {
            // @cpt-begin:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-alias-fail-return
            return Err(validation_error(e.0));
            // @cpt-end:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-alias-fail-return
        } // @cpt-end:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-alias-fail
    };
    // @cpt-end:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-alias-check

    // @cpt-dod:cpt-cf-oagw-dod-enable-disable:p1
    // @cpt-begin:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-cascade-blocked
    // @cpt-begin:cpt-cf-oagw-state-upstream-enabled-lifecycle:p2:inst-state-disabled-reenable-blocked
    if draft.enabled
        && store_ops::ancestor_disabled(&state, hierarchy.as_ref(), tenant_id, &resolved_alias)
    {
        // @cpt-begin:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-cascade-blocked-return
        return Err(validation_error(
            "a descendant cannot re-enable an ancestor-disabled upstream",
        ));
        // @cpt-end:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-cascade-blocked-return
    }
    // @cpt-end:cpt-cf-oagw-state-upstream-enabled-lifecycle:p2:inst-state-disabled-reenable-blocked
    // @cpt-end:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-cascade-blocked

    // @cpt-begin:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-persist-branch
    // @cpt-begin:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-persist
    // @cpt-begin:cpt-cf-oagw-state-upstream-enabled-lifecycle:p2:inst-state-enabled-to-disabled
    // @cpt-begin:cpt-cf-oagw-state-upstream-enabled-lifecycle:p2:inst-state-disabled-to-enabled
    let mut updated = draft;
    updated.id = Some(id);
    updated.alias = Some(resolved_alias);
    updated.tenant_id = tenant_id;
    state
        .store
        .upstreams()
        .insert(id, Arc::new(updated.clone()));
    // @cpt-end:cpt-cf-oagw-state-upstream-enabled-lifecycle:p2:inst-state-disabled-to-enabled
    // @cpt-end:cpt-cf-oagw-state-upstream-enabled-lifecycle:p2:inst-state-enabled-to-disabled
    // @cpt-end:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-persist
    // @cpt-end:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-persist-branch

    // @cpt-begin:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-return
    Ok(Json(updated))
    // @cpt-end:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-return
    // @cpt-end:cpt-cf-oagw-flow-replace-upstream:p1:inst-replace-upstream-found
}

/// `DELETE /oagw/v1/upstreams/{id}` -- `cpt-cf-oagw-flow-delete-upstream`.
// @cpt-begin:cpt-cf-oagw-flow-delete-upstream:p1:inst-delete-upstream-request
// @cpt-begin:cpt-cf-oagw-flow-delete-upstream:p1:inst-delete-upstream-auth
pub(super) async fn delete_upstream(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(ctx): Extension<SecurityContext>,
    Path(raw_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    // @cpt-end:cpt-cf-oagw-flow-delete-upstream:p1:inst-delete-upstream-auth
    // @cpt-end:cpt-cf-oagw-flow-delete-upstream:p1:inst-delete-upstream-request
    // @cpt-begin:cpt-cf-oagw-flow-delete-upstream:p1:inst-delete-upstream-scope
    let id = ident::normalize_upstream_path_id(&raw_id).ok_or_else(not_found)?;
    // @cpt-end:cpt-cf-oagw-flow-delete-upstream:p1:inst-delete-upstream-scope
    let tenant_id = ctx.subject_tenant_id();

    // @cpt-begin:cpt-cf-oagw-flow-delete-upstream:p1:inst-delete-upstream-notfound
    if store_ops::find_by_id_for_tenant(&state, tenant_id, id).is_none() {
        // @cpt-begin:cpt-cf-oagw-flow-delete-upstream:p1:inst-delete-upstream-notfound-return
        return Err(not_found());
        // @cpt-end:cpt-cf-oagw-flow-delete-upstream:p1:inst-delete-upstream-notfound-return
    }
    // @cpt-end:cpt-cf-oagw-flow-delete-upstream:p1:inst-delete-upstream-notfound

    // @cpt-begin:cpt-cf-oagw-flow-delete-upstream:p1:inst-delete-upstream-found
    // @cpt-begin:cpt-cf-oagw-flow-delete-upstream:p1:inst-delete-upstream-persist
    state.store.upstreams().remove(&id);
    // @cpt-end:cpt-cf-oagw-flow-delete-upstream:p1:inst-delete-upstream-persist
    // @cpt-end:cpt-cf-oagw-flow-delete-upstream:p1:inst-delete-upstream-found

    // @cpt-begin:cpt-cf-oagw-flow-delete-upstream:p1:inst-delete-upstream-return
    Ok(StatusCode::NO_CONTENT)
    // @cpt-end:cpt-cf-oagw-flow-delete-upstream:p1:inst-delete-upstream-return
}
