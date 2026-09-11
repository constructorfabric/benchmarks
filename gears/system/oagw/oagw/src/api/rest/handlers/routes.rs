//! Route handlers — the five `route` endpoints of DECOMPOSITION §2.2.
//!
//! The order is the upstream half's: the `route` permission of the operation is
//! enforced first, the calling tenant is resolved from the authenticated
//! subject, the body is read last, and the service outcome becomes a
//! representation, a `204`, or a problem document. A route replacement carries
//! no `upstream_id`, because the addressed row is where the reference comes
//! from.

use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::Extension;
use toolkit_security::SecurityContext;

use super::{
    authorize, instance_of, json_response, no_content, parse_body, record_config_change, refused,
    tenant_of,
    unaddressed, CREATE, DELETE, OVERRIDE, READ, SharedState,
};
use crate::api::rest::dto;
use crate::api::rest::params;
use crate::api::rest::problem;
use crate::control_plane::chain;
use crate::control_plane::sharing::OverridePermissions;
use crate::control_plane::validation::ResourceKind;

/// The ancestor chain the calling tenant resolves to, for the hierarchical
/// write decisions.
///
/// A tenant the platform tenant-resolver cannot answer for resolves to no
/// ancestor at all, which is the fail-closed posture of the walk: with no
/// chain, no ancestor row is ever read, copied, or echoed.
async fn ancestors_of(
    state: &SharedState,
    context: &SecurityContext,
    tenant: uuid::Uuid,
) -> Vec<uuid::Uuid> {
    chain::chain_of(state.resolver(), context, tenant)
        .await
        .map(|chain| chain.tenants().to_vec())
        .unwrap_or_default()
}

/// Creates one route: `POST /oagw/v1/routes`.
pub async fn create(
    State(state): State<SharedState>,
    original: OriginalUri,
    context: Option<Extension<SecurityContext>>,
    body: Bytes,
) -> Response {
    let instance = instance_of(&original.0);
    // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-issue
    // The actor's create request carries the route DTO: `upstream_id`,
    // `match`, `priority`, and the optional families.
    // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-issue
    // The route's own `plugins` items ride on the same request; a route binds
    // no auth plugin, so its body carries no `auth` sub-configuration.
    // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-issue
    // @cpt-begin:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-authz
    let checked = authorize(
        &state,
        bearer_of(&context),
        ResourceKind::Route,
        CREATE,
        None,
        &instance,
    )
    .await;
    // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-authz
    let context = match checked {
        Ok(context) => context,
        Err(answer) => return answer,
    };
    let tenant = match tenant_of(context, &instance) {
        Ok(tenant) => tenant,
        Err(answer) => return answer,
    };
    let body = match parse_body(&body, &instance) {
        Ok(body) => body,
        Err(answer) => return answer,
    };
    // @cpt-end:cpt-cf-oagw-flow-route-create:p1:inst-rt-create-issue
    // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-authz
    // The route create permission is the one this operation consumes.
    // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-authz
    // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-return
    match state.service().create_route(tenant, &body) {
        Ok(row) => {
            record_config_change(
                &state,
                crate::domain::observability::EVENT_ROUTE_CREATED,
                context,
                "POST",
                &instance,
                u16::from(StatusCode::CREATED),
                tenant,
            );
            json_response(StatusCode::CREATED, &dto::route(&row))
        }
        Err(error) => refused(&error, &instance),
    }
    // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-return
}

/// Lists the routes of the calling tenant: `GET /oagw/v1/routes`.
pub async fn list(
    State(state): State<SharedState>,
    original: OriginalUri,
    context: Option<Extension<SecurityContext>>,
) -> Response {
    let instance = instance_of(&original.0);
    let checked = authorize(
        &state,
        bearer_of(&context),
        ResourceKind::Route,
        READ,
        None,
        &instance,
    )
    .await;
    let context = match checked {
        Ok(context) => context,
        Err(answer) => return answer,
    };
    let tenant = match tenant_of(context, &instance) {
        Ok(tenant) => tenant,
        Err(answer) => return answer,
    };
    // @cpt-begin:cpt-cf-oagw-dod-list-query-parameters:p1:inst-list-bind-query
    match state
        .service()
        .list_routes(tenant, params::raw_query(&original.0))
    // @cpt-end:cpt-cf-oagw-dod-list-query-parameters:p1:inst-list-bind-query
    {
        Ok(page) => json_response(StatusCode::OK, &dto::route_page(&page)),
        Err(error) => refused(&error, &instance),
    }
}

/// Reads one route: `GET /oagw/v1/routes/{id}`.
pub async fn read(
    State(state): State<SharedState>,
    original: OriginalUri,
    context: Option<Extension<SecurityContext>>,
    Path(id): Path<String>,
) -> Response {
    let instance = instance_of(&original.0);
    // The path identifier is parsed, never answered, before the two gates: a
    // request that cannot state who it is or what it may do is told nothing
    // about the path it named.
    let selector = params::path_id(ResourceKind::Route, &id);
    let checked = authorize(
        &state,
        bearer_of(&context),
        ResourceKind::Route,
        READ,
        selector.as_ref().ok().copied(),
        &instance,
    )
    .await;
    let context = match checked {
        Ok(context) => context,
        Err(answer) => return answer,
    };
    let id = match selector {
        Ok(id) => id,
        Err(error) => return problem::problem_response(&error, &instance),
    };
    let tenant = match tenant_of(context, &instance) {
        Ok(tenant) => tenant,
        Err(answer) => return answer,
    };
    match state.service().read_route(tenant, id) {
        Ok(row) => json_response(StatusCode::OK, &dto::route(&row)),
        Err(error) => refused(&error, &instance),
    }
}

/// Replaces one route: `PUT /oagw/v1/routes/{id}`.
///
/// The `override` permission covers the `enabled` flag, because the ten
/// management paths hold no dedicated enable or disable operation.
pub async fn replace(
    State(state): State<SharedState>,
    original: OriginalUri,
    context: Option<Extension<SecurityContext>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let instance = instance_of(&original.0);
    // The path identifier is parsed, never answered, before the two gates: a
    // request that cannot state who it is or what it may do is told nothing
    // about the path it named.
    let selector = params::path_id(ResourceKind::Route, &id);
    let checked = authorize(
        &state,
        bearer_of(&context),
        ResourceKind::Route,
        OVERRIDE,
        selector.as_ref().ok().copied(),
        &instance,
    )
    .await;
    let context = match checked {
        Ok(context) => context,
        Err(answer) => return answer,
    };
    let id = match selector {
        Ok(id) => id,
        Err(error) => return problem::problem_response(&error, &instance),
    };
    let tenant = match tenant_of(context, &instance) {
        Ok(tenant) => tenant,
        Err(answer) => return answer,
    };
    let body = match parse_body(&body, &instance) {
        Ok(body) => body,
        Err(answer) => return answer,
    };
    // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-issue
    // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-authz
    // The chain and the permission set the override decision consults.
    let ancestors = ancestors_of(&state, context, tenant).await;
    let permissions = OverridePermissions::of(context);
    // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-authz
    // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-issue
    // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-issue
    // A route replacement carries the full replacement of its binding rows.
    // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-issue
    // The binding decision consumes the permission the override flow already
    // checked, so the plugin flow registers no gate of its own here.
    // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-return
    match state
        .service()
        .replace_route_in_chain(tenant, id, &ancestors, &permissions, &body)
    {
        Ok(row) => {
            record_config_change(
                &state,
                crate::domain::observability::EVENT_ROUTE_OVERRIDDEN,
                context,
                "PUT",
                &instance,
                u16::from(StatusCode::OK),
                tenant,
            );
            json_response(StatusCode::OK, &dto::route(&row))
        }
        Err(error) => refused(&error, &instance),
    }
    // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-return
}

/// Deletes one route and its dependent rows: `DELETE /oagw/v1/routes/{id}`.
///
/// The upstream the route was created under is untouched by the deletion.
pub async fn delete(
    State(state): State<SharedState>,
    original: OriginalUri,
    context: Option<Extension<SecurityContext>>,
    Path(id): Path<String>,
) -> Response {
    let instance = instance_of(&original.0);
    // The path identifier is parsed, never answered, before the two gates: a
    // request that cannot state who it is or what it may do is told nothing
    // about the path it named.
    let selector = params::path_id(ResourceKind::Route, &id);
    // @cpt-begin:cpt-cf-oagw-flow-route-delete:p1:inst-rt-del-issue
    // The actor's deletion request carries no body.
    // @cpt-begin:cpt-cf-oagw-flow-route-delete:p1:inst-rt-del-authz
    let checked = authorize(
        &state,
        bearer_of(&context),
        ResourceKind::Route,
        DELETE,
        selector.as_ref().ok().copied(),
        &instance,
    )
    .await;
    // @cpt-end:cpt-cf-oagw-flow-route-delete:p1:inst-rt-del-authz
    let context = match checked {
        Ok(context) => context,
        Err(answer) => return answer,
    };
    let id = match selector {
        Ok(id) => id,
        Err(error) => return problem::problem_response(&error, &instance),
    };
    let tenant = match tenant_of(context, &instance) {
        Ok(tenant) => tenant,
        Err(answer) => return answer,
    };
    // @cpt-end:cpt-cf-oagw-flow-route-delete:p1:inst-rt-del-issue
    match state.service().delete_route(tenant, id) {
        Ok(true) => {
            record_config_change(
                &state,
                crate::domain::observability::EVENT_ROUTE_DELETED,
                context,
                "DELETE",
                &instance,
                u16::from(StatusCode::NO_CONTENT),
                tenant,
            );
            no_content()
        }
        // The service resolved the row before the deletion, so a `false` answer
        // means the row left between the resolution and the write; the answer is
        // the same 404 the resolution would have produced.
        Ok(false) => unaddressed(&instance),
        Err(error) => refused(&error, &instance),
    }
}

/// The authenticated context the extractor carried, or `None` when it carried
/// none.
fn bearer_of(context: &Option<Extension<SecurityContext>>) -> Option<&SecurityContext> {
    context.as_deref()
}
