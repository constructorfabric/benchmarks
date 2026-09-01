//! `OperationBuilder` route registration for the
//! `/account-management/v1/tenants/{tenant_id}/conversions*` and
//! `.../child-conversions*` endpoints per
//! `feature-managed-self-managed-modes`. Eight chains in two
//! symmetric families:
//!
//! * **Own (child-side)** — converting tenant's own surface.
//! * **Inbound (parent-side)** — parent surfaces the minimal
//!   cross-barrier projection of conversions inbound to its direct
//!   children.

use axum::Router;
use toolkit::api::OpenApiRegistry;
use toolkit::api::operation_builder::{OperationBuilder, OperationBuilderODataExt};

use crate::api::rest::{dto, handlers};
use crate::domain::conversion::query::ConversionRequestFilterField;

const API_TAG: &str = "Tenant Conversions";

const OWN_COLLECTION_PATH: &str = "/account-management/v1/tenants/{tenant_id}/conversions";
const OWN_ENTRY_PATH: &str = "/account-management/v1/tenants/{tenant_id}/conversions/{request_id}";
const CHILD_COLLECTION_PATH: &str = "/account-management/v1/tenants/{tenant_id}/child-conversions";
const CHILD_ENTRY_PATH: &str =
    "/account-management/v1/tenants/{tenant_id}/child-conversions/{request_id}";

#[allow(
    clippy::too_many_lines,
    reason = "eight OperationBuilder chains in linear sequence (own + child x request / list / get / patch)"
)]
pub(super) fn register_conversions_routes(
    mut router: Router,
    openapi: &dyn OpenApiRegistry,
) -> Router {
    // ====================================================================
    // Own (child-side) — /tenants/{tenant_id}/conversions*
    // ====================================================================

    // POST /tenants/{tenant_id}/conversions
    router = OperationBuilder::post(OWN_COLLECTION_PATH)
        .operation_id("account_management.request_own_conversion")
        .summary("Initiate a conversion request (child-side)")
        .description(
            "Child-side initiation: the converting tenant requests a mode flip. \
             `target_mode` is REQUIRED and MUST be the strict binary inverse of the \
             tenant's current `self_managed` flag -- the service rejects any other value \
             with `code=validation` so a concurrent peer-flip surfaces as a clean envelope \
             rather than a silent implicit-inverse override. An optional `comment` \
             (1..=1000 chars; empty strings rejected) is persisted to `requested_comment`. \
             The `pending_exists` partial-unique-index collision is surfaced with \
             `code=already_exists` (HTTP 409) carrying the existing `request_id` per the \
             AIP-193 duplicate-on-create convention.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("tenant_id", "Converting tenant UUID")
        .json_request::<dto::RequestOwnConversionDto>(openapi, "Conversion initiation payload")
        .handler(handlers::request_own_conversion)
        .json_response_with_schema::<dto::OwnConversionRequestDto>(
            openapi,
            http::StatusCode::CREATED,
            "Conversion request created",
        )
        // 409 (`PendingExists` / `Aborted`) is reachable: the
        // partial-unique-index collision on
        // `ux_conversion_requests_pending` surfaces as
        // `DomainError::PendingExists` -> 409.
        .standard_errors(openapi)
        .problem_response(
            openapi,
            http::StatusCode::SERVICE_UNAVAILABLE,
            "PDP / DB transport failure",
        )
        .register(router, openapi);

    // GET /tenants/{tenant_id}/conversions
    router = OperationBuilder::get(OWN_COLLECTION_PATH)
        .operation_id("account_management.list_own_conversions")
        .summary("List own conversion requests")
        .description(
            "Cursor-paginated listing of the converting tenant's own conversion requests. \
             Returns ALL lifecycle statuses (pending / approved / cancelled / rejected / \
             expired) -- clients narrow to the actionable subset via standard \
             `?$filter=status eq 'pending'`. Effective \
             sort defaults to `(created_at DESC, id ASC)` when no `$orderby` is supplied; \
             `id ASC` is the UNIQUE tiebreaker so cursor re-reads stay deterministic.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("tenant_id", "Converting tenant UUID")
        .query_param_typed(
            "limit",
            false,
            "Maximum number of conversion requests to return",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .handler(handlers::list_own_conversions)
        .json_response_with_schema::<toolkit_odata::Page<dto::OwnConversionRequestDto>>(
            openapi,
            http::StatusCode::OK,
            "Paginated list of own conversion requests",
        )
        .with_odata_filter::<ConversionRequestFilterField>()
        .with_odata_orderby::<ConversionRequestFilterField>()
        .standard_errors(openapi)
        .problem_response(
            openapi,
            http::StatusCode::SERVICE_UNAVAILABLE,
            "PDP / DB transport failure",
        )
        .register(router, openapi);

    // GET /tenants/{tenant_id}/conversions/{request_id}
    router = OperationBuilder::get(OWN_ENTRY_PATH)
        .operation_id("account_management.get_own_conversion")
        .summary("Read a conversion request (child-side)")
        .description(
            "Point read of a single conversion request owned by the URL-bound tenant. A \
             `request_id` whose stored `tenant_id` does NOT match the URL collapses to \
             `not_found` -- the existence channel is uniform with a real not-found miss \
             so callers cannot probe row existence through the error code.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("tenant_id", "Converting tenant UUID")
        .path_param("request_id", "Conversion request UUID")
        .handler(handlers::get_own_conversion)
        .json_response_with_schema::<dto::OwnConversionRequestDto>(
            openapi,
            http::StatusCode::OK,
            "Conversion request",
        )
        .standard_errors(openapi)
        .problem_response(
            openapi,
            http::StatusCode::SERVICE_UNAVAILABLE,
            "PDP / DB transport failure",
        )
        .register(router, openapi);

    // PATCH /tenants/{tenant_id}/conversions/{request_id}
    router = OperationBuilder::patch(OWN_ENTRY_PATH)
        .operation_id("account_management.patch_own_conversion")
        .summary("Resolve a conversion request (child-side)")
        .description(
            "Drive a `pending` row to one of three admissible terminal statuses. The \
             body's `status` field routes to the matching service method: `cancel` is \
             initiator-only, `approve` and `reject` are counterparty-only (initiator gets \
             `code=failed_precondition` with `invalid_actor_for_transition`). Idempotent \
             retries on already-resolved rows surface `code=failed_precondition` with \
             `already_resolved`. `pending` and `expired` are NOT admissible PATCH targets \
             and are rejected at the wire layer with a clean 400 envelope. `comment` is \
             optional (1..=1000 chars) and stamped on the matching per-transition column.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("tenant_id", "Converting tenant UUID")
        .path_param("request_id", "Conversion request UUID")
        .json_request::<dto::ConversionPatchDto>(openapi, "Conversion resolution payload")
        .handler(handlers::patch_own_conversion)
        .json_response_with_schema::<dto::OwnConversionRequestDto>(
            openapi,
            http::StatusCode::OK,
            "Resolved conversion request",
        )
        // 409 (`Aborted` / `AlreadyResolved` / `PendingExists`)
        // surfaces from approve's apply TX retry exhaustion or a
        // peer write that wins the race.
        .standard_errors(openapi)
        .problem_response(
            openapi,
            http::StatusCode::SERVICE_UNAVAILABLE,
            "PDP / DB / types-registry transport failure",
        )
        .register(router, openapi);

    // ====================================================================
    // Inbound (parent-side) — /tenants/{tenant_id}/child-conversions*
    // ====================================================================

    // POST /tenants/{tenant_id}/child-conversions
    router = OperationBuilder::post(CHILD_COLLECTION_PATH)
        .operation_id("account_management.request_child_conversion")
        .summary("Initiate a conversion request on a direct child (parent-side)")
        .description(
            "Parent-side initiation: the URL-bound parent requests a mode flip on one of \
             its direct children. The body carries the child's `child_tenant_id`; the URL \
             binds the parent. `target_mode` follows the same inverse-only contract as \
             [`request_own_conversion`]. The service's URL-vs-row coherence guard surfaces \
             a misrouted call (a `child_tenant_id` whose `parent_id` is not the URL-bound \
             parent) as `code=not_found`. Response surfaces the cross-barrier minimal \
             projection.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("tenant_id", "Parent tenant UUID")
        .json_request::<dto::RequestChildConversionDto>(
            openapi,
            "Parent-side conversion initiation payload",
        )
        .handler(handlers::request_child_conversion)
        .json_response_with_schema::<dto::ChildConversionRequestDto>(
            openapi,
            http::StatusCode::CREATED,
            "Conversion request created",
        )
        .standard_errors(openapi)
        .problem_response(
            openapi,
            http::StatusCode::SERVICE_UNAVAILABLE,
            "PDP / DB transport failure",
        )
        .register(router, openapi);

    // GET /tenants/{tenant_id}/child-conversions
    router = OperationBuilder::get(CHILD_COLLECTION_PATH)
        .operation_id("account_management.list_child_conversions")
        .summary("List inbound child conversion requests (parent-side)")
        .description(
            "Cursor-paginated parent-side inbound listing. Surfaces only the cross-barrier \
             minimal projection per `dod-managed-self-managed-modes-parent-side-minimal- \
             surface`: no child-subtree data (tenant metadata beyond `child_tenant_name`, \
             descendants, user records, resource inventories) leaks across the barrier. \
             Returns ALL lifecycle statuses; clients filter via `?$filter=status eq 'pending'` \
             the same way as the own listing. The listing accepts conversion \
             requests targeting self-managed children whose closure barrier hides the \
             child tenant from the parent -- the dual-consent flows live under the \
             parent's URL authority.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("tenant_id", "Parent tenant UUID")
        .query_param_typed(
            "limit",
            false,
            "Maximum number of conversion requests to return",
            "integer",
        )
        .query_param("cursor", false, "Cursor for pagination")
        .handler(handlers::list_child_conversions)
        .json_response_with_schema::<toolkit_odata::Page<dto::ChildConversionRequestDto>>(
            openapi,
            http::StatusCode::OK,
            "Paginated list of inbound child conversion requests",
        )
        .with_odata_filter::<ConversionRequestFilterField>()
        .with_odata_orderby::<ConversionRequestFilterField>()
        .standard_errors(openapi)
        .problem_response(
            openapi,
            http::StatusCode::SERVICE_UNAVAILABLE,
            "PDP / DB transport failure",
        )
        .register(router, openapi);

    // GET /tenants/{tenant_id}/child-conversions/{request_id}
    router = OperationBuilder::get(CHILD_ENTRY_PATH)
        .operation_id("account_management.get_child_conversion")
        .summary("Read an inbound child conversion request (parent-side)")
        .description(
            "Parent-side point read. Same cross-barrier minimal projection contract as \
             the list endpoint. A `request_id` whose stored `parent_id` does NOT match \
             the URL collapses to `code=not_found`.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("tenant_id", "Parent tenant UUID")
        .path_param("request_id", "Conversion request UUID")
        .handler(handlers::get_child_conversion)
        .json_response_with_schema::<dto::ChildConversionRequestDto>(
            openapi,
            http::StatusCode::OK,
            "Conversion request (cross-barrier minimal projection)",
        )
        .standard_errors(openapi)
        .problem_response(
            openapi,
            http::StatusCode::SERVICE_UNAVAILABLE,
            "PDP / DB transport failure",
        )
        .register(router, openapi);

    // PATCH /tenants/{tenant_id}/child-conversions/{request_id}
    router = OperationBuilder::patch(CHILD_ENTRY_PATH)
        .operation_id("account_management.patch_child_conversion")
        .summary("Resolve an inbound child conversion request (parent-side)")
        .description(
            "Parent-side PATCH dispatcher. Same status-to-method routing as the own \
             PATCH endpoint, constructed with the URL-bound parent caller so the service \
             runs the counterparty-only rule for `approve` / `reject` and the \
             initiator-only rule for `cancel` against the parent side. Response surfaces \
             the cross-barrier minimal projection.",
        )
        .tag(API_TAG)
        .authenticated()
        .no_license_required()
        .path_param("tenant_id", "Parent tenant UUID")
        .path_param("request_id", "Conversion request UUID")
        .json_request::<dto::ConversionPatchDto>(openapi, "Conversion resolution payload")
        .handler(handlers::patch_child_conversion)
        .json_response_with_schema::<dto::ChildConversionRequestDto>(
            openapi,
            http::StatusCode::OK,
            "Resolved conversion request (cross-barrier minimal projection)",
        )
        .standard_errors(openapi)
        .problem_response(
            openapi,
            http::StatusCode::SERVICE_UNAVAILABLE,
            "PDP / DB / types-registry transport failure",
        )
        .register(router, openapi);

    router
}
