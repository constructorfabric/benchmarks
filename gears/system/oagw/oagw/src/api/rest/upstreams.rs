//! Upstream Management API routes.
//!
//! Implemented by DECOMPOSITION entry 2.2 (upstream-management): the five
//! CRUD operations for Upstream configuration resources under
//! `/oagw/v1/upstreams`, field validation against
//! `docs/schemas/upstream.v1.schema.json`, alias derivation/enforcement,
//! enable/disable semantics, and tenant-scoped visibility. See
//! `docs/features/upstream-management.md`.
//!
//! ## Tenant hierarchy
//!
//! `cpt-cf-oagw-algo-resolve-tenant-scope`'s ancestor-bind and
//! enabled-cascade branches need to walk the calling tenant's ancestor
//! chain. Entry 2.1's [`OagwState`] carries no tenant-hierarchy data (it
//! only established the `upstreams`/`routes`/`plugins` maps), and this
//! entry's REST handlers receive nothing beyond `Arc<OagwState>` and the
//! request's `SecurityContext` -- there is no `GearCtx`/hub handle here to
//! reach a `TenantResolverClient` (`tenant-resolver-sdk`) through. Rather
//! than invent an ancestor data source this entry does not own, ancestor
//! resolution is expressed as the injectable [`TenantHierarchyProvider`]
//! trait below, defaulting to [`NoTenantHierarchy`] (every tenant is
//! treated as its own root). That default is conservative and safe: with no
//! ancestors, `(tenant_id, alias)` uniqueness and own-tenant visibility
//! (the acceptance criteria that do not require real ancestor data) behave
//! exactly as specified, while the bind/cascade branches simply never
//! trigger. The algorithm itself is fully implemented and exercised by this
//! module's own tests via a test-only provider. See `foreign_file_needs` in
//! this entry's completion report for the wiring a future round would need.

mod handlers;
mod list_query;
mod store_ops;
#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Router};
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::OperationBuilder;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::error::{OagwError, OagwProblem};
use crate::store::OagwState;

const API_TAG: &str = "Upstream Management";
const UPSTREAMS_PATH: &str = "/oagw/v1/upstreams";
const UPSTREAM_BY_ID_PATH: &str = "/oagw/v1/upstreams/{id}";

/// Permission gating an ancestor-alias bind at create time (`DESIGN.md`'s
/// Permissions and Access Control table).
const BIND_PERMISSION: &str = "oagw:upstream:bind";

/// `true` when `ctx`'s token scopes include `permission` (or the
/// unrestricted `"*"` scope).
fn has_permission(ctx: &SecurityContext, permission: &str) -> bool {
    ctx.token_scopes()
        .iter()
        .any(|s| s == "*" || s == permission)
}

/// Ancestor-tenant resolution `cpt-cf-oagw-algo-resolve-tenant-scope`'s
/// create-time bind detection and PUT-time enabled-cascade check depend on.
/// See the module doc comment for why this is injectable rather than backed
/// by a concrete tenant-hierarchy client in this round.
pub(crate) trait TenantHierarchyProvider: fmt::Debug + Send + Sync {
    /// Ancestor tenant ids for `tenant_id`, ordered nearest-parent to root.
    fn ancestors(&self, tenant_id: Uuid) -> Vec<Uuid>;
}

/// Default provider: every tenant is its own root (no ancestors). Safe
/// because it only ever *narrows* which branches can fire -- see the module
/// doc comment.
#[derive(Debug, Default)]
pub(crate) struct NoTenantHierarchy;

impl TenantHierarchyProvider for NoTenantHierarchy {
    fn ancestors(&self, _tenant_id: Uuid) -> Vec<Uuid> {
        Vec::new()
    }
}

/// Unified handler error type: either a catalog [`OagwError`] (`400`
/// validation failures, which do have a documented GTS `type`) or a bare
/// [`OagwProblem`] rendered with `type: "about:blank"` for the `404`/`409`
/// cases `cpt-cf-oagw-dod-error-mapping` documents as carrying no
/// resource-specific GTS identifier.
// Both variants are boxed: `Result<T, ApiError>` is this feature's handler
// return type everywhere, and `clippy::result_large_err` (`-D clippy::perf`)
// flags an unboxed `OagwProblem`/`OagwError` payload as too large to return
// by value on every call site.
#[derive(Debug)]
pub(crate) enum ApiError {
    Oagw(Box<OagwError>),
    Problem(Box<OagwProblem>),
}

impl From<OagwError> for ApiError {
    fn from(e: OagwError) -> Self {
        Self::Oagw(Box::new(e))
    }
}

impl From<OagwProblem> for ApiError {
    fn from(e: OagwProblem) -> Self {
        Self::Problem(Box::new(e))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::Oagw(e) => e.into_response(),
            Self::Problem(p) => p.into_response(),
        }
    }
}

/// Build an RFC 9457 problem with `type: "about:blank"` -- the correct
/// rendering for a status this feature's error catalog defines no
/// resource-specific GTS identifier for, per `cpt-cf-oagw-dod-error-mapping`.
// @cpt-dod:cpt-cf-oagw-dod-error-mapping:p1
fn blank_problem(status: u16, title: &str, detail: impl Into<String>) -> OagwProblem {
    OagwProblem {
        problem_type: "about:blank".to_owned(),
        title: title.to_owned(),
        status,
        detail: detail.into(),
        instance: None,
        upstream_id: None,
        host: None,
        path: None,
        retry_after_seconds: None,
        trace_id: None,
    }
}

/// `404` outcome: id does not exist, or belongs to an ancestor/unrelated
/// tenant -- identical response shape either way
/// (`inst-get-upstream-notfound-return` and siblings).
fn not_found() -> ApiError {
    blank_problem(
        404,
        "Not Found",
        "the requested upstream does not exist or is not visible to the calling tenant",
    )
    .into()
}

/// `409` outcome: `(tenant_id, alias)` collision or a blocked ancestor bind.
fn conflict(detail: impl Into<String>) -> ApiError {
    blank_problem(409, "Conflict", detail).into()
}

/// Registers the five Upstream Management API routes under
/// `/oagw/v1/upstreams`.
// @cpt-dod:cpt-cf-oagw-dod-crud-endpoints:p1
pub(crate) fn register_routes(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    state: Arc<OagwState>,
) -> Router {
    register_routes_with_hierarchy(router, openapi, state, Arc::new(NoTenantHierarchy))
}

/// Same wiring as [`register_routes`], parameterized over the
/// [`TenantHierarchyProvider`] -- production always uses [`NoTenantHierarchy`]
/// (see this module's doc comment); this module's own tests inject a
/// test-only provider to exercise the ancestor-bind/enabled-cascade branches
/// of `cpt-cf-oagw-algo-resolve-tenant-scope` end to end through a real
/// router.
fn register_routes_with_hierarchy(
    router: Router,
    openapi: &dyn OpenApiRegistry,
    state: Arc<OagwState>,
    hierarchy: Arc<dyn TenantHierarchyProvider>,
) -> Router {
    // @cpt-flow:cpt-cf-oagw-flow-create-upstream:p1
    let router = OperationBuilder::post(UPSTREAMS_PATH)
        .operation_id("oagw.upstreams.create")
        .summary("Create an Upstream")
        .description(
            "Create a tenant-scoped Upstream configuration resource, with alias \
             auto-derivation/validation and tenant-scope/ancestor-bind checks.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .handler(handlers::create_upstream)
        .json_response(StatusCode::CREATED, "The created upstream")
        .problem_response(openapi, StatusCode::BAD_REQUEST, "Validation error")
        .problem_response(openapi, StatusCode::CONFLICT, "Alias conflict")
        .register(router, openapi);

    // @cpt-flow:cpt-cf-oagw-flow-list-upstreams:p1
    let router = OperationBuilder::get(UPSTREAMS_PATH)
        .operation_id("oagw.upstreams.list")
        .summary("List Upstreams")
        .description("List the calling tenant's Upstream configuration resources.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .query_param(
            "$filter",
            false,
            "OData filter expression (`field eq value` clauses joined by `and`)",
        )
        .query_param(
            "$select",
            false,
            "Comma-separated list of fields to project",
        )
        .query_param(
            "$orderby",
            false,
            "Comma-separated `field [asc|desc]` clauses",
        )
        .query_param(
            "$top",
            false,
            "Maximum number of items to return (default 50, max 100)",
        )
        .query_param("$skip", false, "Number of items to skip (default 0)")
        .handler(handlers::list_upstreams)
        .json_response(StatusCode::OK, "Matching upstreams")
        .register(router, openapi);

    // @cpt-flow:cpt-cf-oagw-flow-get-upstream:p1
    let router = OperationBuilder::get(UPSTREAM_BY_ID_PATH)
        .operation_id("oagw.upstreams.get")
        .summary("Get an Upstream by id")
        .description(
            "Retrieve a tenant-owned Upstream. `{id}` accepts either the bare UUID or the \
             anonymous GTS form `gts.cf.core.oagw.upstream.v1~{uuid}`.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Bare UUID or gts.cf.core.oagw.upstream.v1~{uuid}")
        .handler(handlers::get_upstream)
        .json_response(StatusCode::OK, "The upstream")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Not found")
        .register(router, openapi);

    // @cpt-flow:cpt-cf-oagw-flow-replace-upstream:p1
    let router = OperationBuilder::put(UPSTREAM_BY_ID_PATH)
        .operation_id("oagw.upstreams.replace")
        .summary("Replace an Upstream")
        .description(
            "Fully replace a tenant-owned Upstream. `alias` is immutable per \
             `cpt-cf-oagw-algo-enforce-alias-update-immutability`.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Bare UUID or gts.cf.core.oagw.upstream.v1~{uuid}")
        .handler(handlers::replace_upstream)
        .json_response(StatusCode::OK, "The updated upstream")
        .problem_response(openapi, StatusCode::BAD_REQUEST, "Validation error")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Not found")
        .register(router, openapi);

    // @cpt-flow:cpt-cf-oagw-flow-delete-upstream:p1
    let router = OperationBuilder::delete(UPSTREAM_BY_ID_PATH)
        .operation_id("oagw.upstreams.delete")
        .summary("Delete an Upstream")
        .description("Permanently delete a tenant-owned Upstream.")
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("id", "Bare UUID or gts.cf.core.oagw.upstream.v1~{uuid}")
        .handler(handlers::delete_upstream)
        .no_content_response(StatusCode::NO_CONTENT, "Deleted")
        .problem_response(openapi, StatusCode::NOT_FOUND, "Not found")
        .register(router, openapi);

    router.layer(Extension(hierarchy)).layer(Extension(state))
}
