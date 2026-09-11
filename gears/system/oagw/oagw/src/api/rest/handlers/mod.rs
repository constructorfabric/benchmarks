//! REST handlers of the OAGW gear.
//!
//! Feature 1 left this module intentionally empty; entry 2.2 fills it with the
//! management surface. Every handler of
//! `cpt-cf-oagw-interface-api`'s management surface lives
//! here: the two collections under `/oagw/v1/upstreams` and `/oagw/v1/routes`,
//! each with `POST` (create), `GET` (read and list), `PUT` (replace) and
//! `DELETE`. The handlers are deliberately thin: they enforce the permission of
//! the addressed resource type, hand the parsed body to
//! [`ManagementService`](crate::domain::services::management::ManagementService)
//! and map every outcome through the single mapping layer of entry 2.1.
//!
//! The transport holds no record state of its own: the gear's store is the only
//! place a management write lands, so the response body is always the stored
//! representation.

use std::sync::Arc;

use axum::extract::{FromRequest, FromRequestParts, OriginalUri, Path, RawQuery, Request, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::de::DeserializeOwned;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::ErrorContext;
use crate::api::rest::error::OagwProblem;
use crate::domain::error::{DomainError, ManagementError};
use crate::domain::services::management::ManagementService;
use crate::domain::sharing::{
    Actor, FlatHierarchy, TenantHierarchy, PERM_ROUTE_CREATE, PERM_ROUTE_DELETE,
    PERM_ROUTE_OVERRIDE, PERM_ROUTE_READ, PERM_UPSTREAM_CREATE, PERM_UPSTREAM_DELETE,
    PERM_UPSTREAM_OVERRIDE, PERM_UPSTREAM_READ,
};
use crate::domain::validation::{RouteSpec, UpstreamSpec};
use crate::infra::storage::OagwStore;

/// Plugin definition endpoints of entry 2.3.
pub mod plugins;

/// Router state of the management endpoints.
///
/// Holds the domain service over the gear's store; `Clone` is cheap because the
/// service itself sits behind an `Arc`.
#[derive(Clone)]
pub struct ManagementState {
    service: Arc<ManagementService<OagwStore>>,
}

impl ManagementState {
    /// Build the state over `store` and the tenant-hierarchy port.
    #[must_use]
    pub fn new(store: Arc<OagwStore>, hierarchy: Arc<dyn TenantHierarchy>) -> Self {
        Self {
            service: Arc::new(ManagementService::new(store, hierarchy)),
        }
    }

    /// Build the state with the single-tenant fallback hierarchy.
    ///
    /// The fallback of a deployment with no in-process hierarchy source: the
    /// gear reports no ancestors, so no bind or sharing gate fires and every
    /// record is its tenant's own. The gear selects it explicitly and logs the
    /// reason at startup instead of assuming it silently.
    #[must_use]
    pub fn single_tenant(store: Arc<OagwStore>) -> Self {
        Self::new(store, Arc::new(FlatHierarchy))
    }

    /// The domain service the handlers delegate to.
    #[must_use]
    pub fn service(&self) -> &ManagementService<OagwStore> {
        &self.service
    }
}

// @cpt-begin:cpt-cf-oagw-dod-tenant-scoping:p1:inst-full
/// The authenticated caller of a management request.
///
/// The host api-gateway authenticates the Bearer token and injects the resolved
/// [`SecurityContext`]; this extractor turns it into the domain [`Actor`] and
/// rejects a request that reaches the gear without one. The tenant read here is
/// the key of every subsequent lookup, so a caller without a resolvable tenant
/// never reaches the store.
pub struct Caller {
    actor: Actor,
    subject: Uuid,
    context: SecurityContext,
}

impl Caller {
    /// Require `permission` for the addressed resource type.
    ///
    /// # Errors
    ///
    /// Returns the mapped `403` when the token scopes do not grant it.
    pub fn require(&self, permission: &str) -> Result<(), ManagementError> {
        self.actor.require(permission)
    }

    /// The domain actor the handlers hand to the service.
    #[must_use]
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// The authenticated subject, for tracing only.
    #[must_use]
    pub fn subject(&self) -> Uuid {
        self.subject
    }

    /// The security context the hierarchy source is queried with.
    ///
    /// The ancestor chain of [`Self::actor`]'s tenant is a call to
    /// tenant-resolver, which authorizes the caller again; the context that
    /// authenticated this request is the one that travels with it.
    #[must_use]
    pub fn context(&self) -> &SecurityContext {
        &self.context
    }
}

impl<S> FromRequestParts<S> for Caller
where
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-03
        // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rcre-03
        // @cpt-begin:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-03
        // @cpt-begin:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-03
        // The calling tenant comes from the security context the host resolved
        // from the Bearer token. A request without one — including the
        // anonymous context the host injects for an unauthenticated call — has
        // no key space to look in and is rejected before any store access.
        let Some(context) = parts.extensions.get::<SecurityContext>().cloned() else {
            return Err(unauthenticated(
                "no authenticated caller on a management request",
                request_path(parts),
            ));
        };
        let tenant_id = context.subject_tenant_id();
        if tenant_id.is_nil() {
            return Err(unauthenticated(
                "the security context carries no resolvable tenant",
                request_path(parts),
            ));
        }
        // @cpt-end:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-03
        // @cpt-end:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-03
        // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rcre-03
        // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-03

        Ok(Self {
            actor: Actor::new(
                tenant_id,
                context.subject_id(),
                context.token_scopes().to_vec(),
            ),
            subject: context.subject_id(),
            context,
        })
    }
}
// @cpt-end:cpt-cf-oagw-dod-tenant-scoping:p1:inst-full

/// A JSON request body bound to a management DTO.
///
/// Wraps axum's JSON extractor so a body that fails to bind — a malformed
/// document, a wrong content type or an unknown property, which the DTOs reject
/// per the schema's `additionalProperties: false` — comes back as the canonical
/// problem document instead of axum's plain-text rejection.
pub struct Body<T> {
    /// The bound request value.
    pub value: T,
}

impl<T, S> FromRequest<S> for Body<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = Response;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        // Taken before the extractor consumes the request: the path a problem
        // document names, which is the full mount path when the host mounted
        // the gear under its root.
        let path = request
            .extensions()
            .get::<OriginalUri>()
            .map(path_of)
            .unwrap_or_else(|| request.uri().path().to_owned());
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(Self { value }),
            Err(rejection) => {
                // A wrong content type is a transport-level `415`; every other
                // rejection — malformed JSON, a type mismatch or an unknown
                // property, which the DTOs reject per the schema's
                // `additionalProperties: false` — is a client error the error
                // contract reports as `400`.
                let status = match rejection.status() {
                    StatusCode::UNSUPPORTED_MEDIA_TYPE => StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    _ => StatusCode::BAD_REQUEST,
                };
                Err(OagwProblem::new(
                    &DomainError::ValidationError {
                        detail: rejection.body_text(),
                    },
                    &ErrorContext::for_request(path),
                )
                .with_status(status.as_u16())
                .into_response())
            }
        }
    }
}

// @cpt-begin:cpt-cf-oagw-dod-error-contract:p1:inst-full
/// Map a management failure through the entry-2.1 mapping layer.
///
/// The single mapping layer produces the body format, the GTS `type`
/// identifier and the `gateway` classification; the handler only supplies the
/// request path, which becomes the problem document's `instance`.
pub(crate) fn mapped(error: &ManagementError, path: &str) -> Response {
    OagwProblem::new_management(error, &ErrorContext::for_request(path)).into_response()
}

/// The `401` of a request the security context cannot attribute to a tenant.
///
/// The context is built here, from the path the extractor still holds, because
/// a rejection raised before a handler runs has no request path of its own.
fn unauthenticated(detail: &str, path: String) -> Response {
    mapped(
        &ManagementError::Domain(DomainError::AuthenticationFailed {
            detail: detail.to_owned(),
        }),
        &path,
    )
}

/// The request path a problem document names.
///
/// `OriginalUri` carries the full request path, so the problem document's
/// `instance` and `path` extension field name `/oagw/v1/...` even though the
/// nested router stripped the mount prefix; a request rejected before the
/// router ran falls back to the stripped path it was built with.
fn request_path(parts: &Parts) -> String {
    parts
        .extensions
        .get::<OriginalUri>()
        .map(path_of)
        .unwrap_or_else(|| parts.uri.path().to_owned())
}

/// The request path a problem document names for a handler.
fn path_of(uri: &OriginalUri) -> String {
    uri.0.path().to_owned()
}
// @cpt-end:cpt-cf-oagw-dod-error-contract:p1:inst-full

/// The `201` response of a successful create.
pub(crate) fn created<T: serde::Serialize>(record: &T) -> Response {
    (StatusCode::CREATED, Json(record)).into_response()
}

/// The `204` of a successful delete.
pub(crate) fn no_content() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

/// The `200` of a read, a list or a replace.
pub(crate) fn ok<T: serde::Serialize>(body: &T) -> Response {
    (StatusCode::OK, Json(body)).into_response()
}

// @cpt-begin:cpt-cf-oagw-dod-upstream-endpoints:p1:inst-full
/// `POST /oagw/v1/upstreams` (`cpt-cf-oagw-flow-upstream-create`).
pub async fn create_upstream(
    State(state): State<ManagementState>,
    caller: Caller,
    uri: OriginalUri,
    body: Body<UpstreamSpec>,
) -> Response {
    let path = path_of(&uri);

    // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-01
    // The create request reached the endpoint with its body.
    // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-01

    // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-04
    // The body is bound to the create DTO, whose `deny_unknown_fields` rejects
    // a property the schema does not declare (`additionalProperties: false`).
    let spec = body.value;
    // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-04

    // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-enb-09
    // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-02
    // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-enb-01
    // The create may carry `enabled`; the recorded default is applied by the
    // domain, and the response reports the effective value.
    let permitted = caller.require(PERM_UPSTREAM_CREATE);
    // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-enb-01
    // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-02

    // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-08
    // A rejected body — a validation failure, a missing permission or a taken
    // alias — is mapped through the entry-2.1 mapping layer and returned as the
    // problem body of the error table (`400`, `403` or `409`).
    if let Err(error) = permitted {
        return mapped(&error, &path);
    }
    match state
        .service()
        .create_upstream(caller.context(), caller.actor(), &spec)
        .await
    {
        Ok(record) => created(record.as_ref()),
        Err(error) => mapped(&error, &path),
    }
    // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-ucre-08
    // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-enb-09
}

/// `GET /oagw/v1/upstreams/{id}`
/// (`cpt-cf-oagw-flow-management-list-query`).
///
/// A request that targets a single identifier resolves that record of the
/// calling tenant; an unresolved or foreign identifier is `404`.
pub async fn get_upstream(
    State(state): State<ManagementState>,
    caller: Caller,
    Path(identifier): Path<String>,
    uri: OriginalUri,
) -> Response {
    let path = path_of(&uri);

    // @cpt-begin:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-01
    // @cpt-begin:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-02
    // The `read` permission of the addressed resource type.
    let permitted = caller.require(PERM_UPSTREAM_READ);
    // @cpt-end:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-02
    // @cpt-end:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-01

    // @cpt-begin:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-04
    // The request targets a single identifier: the permission gate and the
    // resolution below both run on that identifier only, so an unresolved or
    // foreign identifier is reported as `404` by the domain service.
    // @cpt-end:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-04

    if let Err(error) = permitted {
        return mapped(&error, &path);
    }

    match state.service().get_upstream(caller.actor(), &identifier) {
        Ok(record) => ok(record.as_ref()),
        Err(error) => mapped(&error, &path),
    }
}

/// `GET /oagw/v1/upstreams` (`cpt-cf-oagw-flow-management-list-query`).
pub async fn list_upstreams(
    State(state): State<ManagementState>,
    caller: Caller,
    uri: OriginalUri,
    RawQuery(query): RawQuery,
) -> Response {
    let path = path_of(&uri);

    // @cpt-begin:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-06
    // The request lists the tenant's collection, so the query parameters are
    // translated with the OData query translation.
    if let Err(error) = caller.require(PERM_UPSTREAM_READ) {
        return mapped(&error, &path);
    }
    // @cpt-end:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-06

    // @cpt-begin:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-08
    // `200` with the page projected onto `$select` when given, and the full
    // stored representation otherwise.
    match state
        .service()
        .list_upstreams(caller.actor(), query.as_deref().unwrap_or_default())
    {
        Ok(page) => ok(&page),
        Err(error) => mapped(&error, &path),
    }
    // @cpt-end:cpt-cf-oagw-flow-management-list-query:p1:inst-lst-08
}

/// `PUT /oagw/v1/upstreams/{id}` (`cpt-cf-oagw-flow-upstream-replace`).
pub async fn replace_upstream(
    State(state): State<ManagementState>,
    caller: Caller,
    Path(identifier): Path<String>,
    uri: OriginalUri,
    body: Body<UpstreamSpec>,
) -> Response {
    let path = path_of(&uri);

    // @cpt-begin:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-01
    // The replacement body is bound to the same DTO the create flow uses; the
    // stored `alias`, `id` and `tenant_id` are the only members it may not
    // change.
    let spec = body.value;
    // @cpt-end:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-01

    // @cpt-begin:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-02
    // The `override` permission of the upstream resource type.
    if let Err(error) = caller.require(PERM_UPSTREAM_OVERRIDE) {
        return mapped(&error, &path);
    }
    // @cpt-end:cpt-cf-oagw-flow-upstream-replace:p1:inst-urep-02

    match state
        .service()
        .replace_upstream(caller.context(), caller.actor(), &identifier, &spec)
        .await
    {
        // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-enb-09
        Ok(record) => ok(record.as_ref()),
        Err(error) => mapped(&error, &path),
        // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-enb-09
    }
}

/// `DELETE /oagw/v1/upstreams/{id}`
/// (`cpt-cf-oagw-flow-upstream-route-delete`).
pub async fn delete_upstream(
    State(state): State<ManagementState>,
    caller: Caller,
    Path(identifier): Path<String>,
    uri: OriginalUri,
) -> Response {
    let path = path_of(&uri);

    // @cpt-begin:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-01
    // The delete request carries only the identifier of the record to remove.
    // @cpt-end:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-01

    // @cpt-begin:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-02
    // The `delete` permission of the addressed resource type.
    if let Err(error) = caller.require(PERM_UPSTREAM_DELETE) {
        return mapped(&error, &path);
    }
    // @cpt-end:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-02

    match state.service().delete_upstream(caller.actor(), &identifier) {
        Ok(cascade) => {
            // @cpt-begin:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-09
            // `204` with no body, whether or not the delete cascaded routes.
            let _ = cascade;
            no_content()
            // @cpt-end:cpt-cf-oagw-flow-upstream-route-delete:p1:inst-del-09
        }
        Err(error) => mapped(&error, &path),
    }
}
// @cpt-end:cpt-cf-oagw-dod-upstream-endpoints:p1:inst-full

// @cpt-begin:cpt-cf-oagw-dod-route-endpoints:p1:inst-full
/// `POST /oagw/v1/routes` (`cpt-cf-oagw-flow-route-create`).
pub async fn create_route(
    State(state): State<ManagementState>,
    caller: Caller,
    uri: OriginalUri,
    body: Body<RouteSpec>,
) -> Response {
    let path = path_of(&uri);

    // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rcre-01
    // The create request reached the endpoint with its body.
    // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rcre-01

    // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rcre-02
    // The `create` permission of the route resource type.
    if let Err(error) = caller.require(PERM_ROUTE_CREATE) {
        return mapped(&error, &path);
    }
    // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rcre-02

    match state.service().create_route(caller.actor(), &body.value) {
        Ok(record) => created(record.as_ref()),
        Err(error) => {
            // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rcre-07
            // A rejected model or an unresolvable `upstream_id` is mapped
            // through the entry-2.1 mapping layer, naming the offending field.
            mapped(&error, &path)
            // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rcre-07
        }
    }
}

/// `GET /oagw/v1/routes/{id}`
/// (`cpt-cf-oagw-flow-management-list-query`).
pub async fn get_route(
    State(state): State<ManagementState>,
    caller: Caller,
    Path(identifier): Path<String>,
    uri: OriginalUri,
) -> Response {
    let path = path_of(&uri);

    if let Err(error) = caller.require(PERM_ROUTE_READ) {
        return mapped(&error, &path);
    }

    match state.service().get_route(caller.actor(), &identifier) {
        Ok(record) => ok(record.as_ref()),
        Err(error) => mapped(&error, &path),
    }
}

/// `GET /oagw/v1/routes` (`cpt-cf-oagw-flow-management-list-query`).
pub async fn list_routes(
    State(state): State<ManagementState>,
    caller: Caller,
    uri: OriginalUri,
    RawQuery(query): RawQuery,
) -> Response {
    let path = path_of(&uri);

    if let Err(error) = caller.require(PERM_ROUTE_READ) {
        return mapped(&error, &path);
    }

    match state
        .service()
        .list_routes(caller.actor(), query.as_deref().unwrap_or_default())
    {
        Ok(page) => ok(&page),
        Err(error) => mapped(&error, &path),
    }
}

/// `PUT /oagw/v1/routes/{id}` (`cpt-cf-oagw-flow-route-replace`).
pub async fn replace_route(
    State(state): State<ManagementState>,
    caller: Caller,
    Path(identifier): Path<String>,
    uri: OriginalUri,
    body: Body<RouteSpec>,
) -> Response {
    let path = path_of(&uri);

    // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-01
    // The replacement body is bound to the route create DTO; `upstream_id` is
    // immutable, so a body echoing the stored reference is accepted and a
    // differing one is `400`.
    let spec = body.value;
    // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-01

    // @cpt-begin:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-02
    // The `override` permission of the route resource type.
    if let Err(error) = caller.require(PERM_ROUTE_OVERRIDE) {
        return mapped(&error, &path);
    }
    // @cpt-end:cpt-cf-oagw-flow-route-replace:p1:inst-rrep-02

    match state
        .service()
        .replace_route(caller.actor(), &identifier, &spec)
    {
        Ok(record) => ok(record.as_ref()),
        Err(error) => mapped(&error, &path),
    }
}

/// `DELETE /oagw/v1/routes/{id}`
/// (`cpt-cf-oagw-flow-upstream-route-delete`).
pub async fn delete_route(
    State(state): State<ManagementState>,
    caller: Caller,
    Path(identifier): Path<String>,
    uri: OriginalUri,
) -> Response {
    let path = path_of(&uri);

    if let Err(error) = caller.require(PERM_ROUTE_DELETE) {
        return mapped(&error, &path);
    }

    match state.service().delete_route(caller.actor(), &identifier) {
        Ok(()) => no_content(),
        Err(error) => mapped(&error, &path),
    }
}
// @cpt-end:cpt-cf-oagw-dod-route-endpoints:p1:inst-full

/// Register the management endpoints on the mounted subtree.
///
/// The paths are mount-relative: the nested router sees the request path with
/// [`MOUNT_ROOT`](crate::api::rest::routes::MOUNT_ROOT) stripped, while the
/// OpenAPI operations carry the absolute `/oagw/v1/...` paths.
pub fn management_router(state: ManagementState) -> axum::Router {
    axum::Router::new()
        .route(
            "/upstreams",
            axum::routing::post(create_upstream).get(list_upstreams),
        )
        .route(
            "/upstreams/{id}",
            axum::routing::get(get_upstream)
                .put(replace_upstream)
                .delete(delete_upstream),
        )
        .route(
            "/routes",
            axum::routing::post(create_route).get(list_routes),
        )
        .route(
            "/routes/{id}",
            axum::routing::get(get_route)
                .put(replace_route)
                .delete(delete_route),
        )
        .route(
            "/plugins",
            axum::routing::post(plugins::create_plugin).get(plugins::list_plugins),
        )
        // No `PUT` on `/plugins/{id}`: a plugin definition is immutable
        // (`cpt-cf-oagw-dod-plugin-immutability`), so a replace request resolves
        // to the router's method-not-allowed response.
        .route(
            "/plugins/{id}",
            axum::routing::get(plugins::get_plugin).delete(plugins::delete_plugin),
        )
        .route(
            "/plugins/{id}/source",
            axum::routing::get(plugins::plugin_source),
        )
        .with_state(state)
}
