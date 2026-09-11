//! Upstream handlers — the five `upstream` endpoints of DECOMPOSITION §2.2.
//!
//! Each handler realizes one step of the FEATURE's flows on the wire and hands
//! the rest to the management service: the permission of the operation is
//! enforced first, the calling tenant is resolved from the authenticated
//! subject, the body is read last, and the service outcome becomes a
//! representation, a `204`, or a problem document.

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
/// A tenant the platform tenant-resolver cannot answer for — a missing client,
/// a failed call, an unordered answer — resolves to no ancestor at all, which
/// is the fail-closed posture of the walk: with no chain, no ancestor row is
/// ever read, copied, or echoed, and every family the body carries is decided
/// `own`. The chain arrives ordered, calling tenant first.
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

/// Creates one upstream: `POST /oagw/v1/upstreams`.
pub async fn create(
    State(state): State<SharedState>,
    original: OriginalUri,
    context: Option<Extension<SecurityContext>>,
    body: Bytes,
) -> Response {
    let instance = instance_of(&original.0);
    // @cpt-begin:cpt-cf-oagw-flow-config-change-logged:p1:inst-cc-issue
    // The actor issues the management write — a create here, and the same
    // shape a replacement, an `enabled` change, or a delete takes on the
    // upstream, route, and plugin paths.
    // @cpt-end:cpt-cf-oagw-flow-config-change-logged:p1:inst-cc-issue
    // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-issue
    // The actor's create request carries the upstream DTO; the handler answers
    // it in the order: permission, tenant, body, service.
    // @cpt-begin:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-issue
    // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-issue
    // The same request carries the `plugins` sub-object and, for an upstream,
    // the `auth` sub-configuration the plugin flow resolves and writes; no
    // plugin path of its own is issued.
    // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-issue
    // The same request reaches the hierarchical flow: whether it is an
    // ordinary create or a bind against an ancestor's alias is decided after
    // the alias is derived, by the service.
    // @cpt-end:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-issue
    // @cpt-begin:cpt-cf-oagw-flow-config-change-logged:p1:inst-cc-api
    // The API is the management path `cpt-cf-oagw-feature-control-plane-config`
    // registers: the platform middleware authenticates the bearer token and
    // resolves the calling tenant and subject, and the handler below enforces
    // the resource kind's permission.
    // @cpt-end:cpt-cf-oagw-flow-config-change-logged:p1:inst-cc-api
    // @cpt-begin:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-authz
    let checked = authorize(
        &state,
        bearer_of(&context),
        ResourceKind::Upstream,
        CREATE,
        None,
        &instance,
    )
    .await;
    // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-authz
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
    // @cpt-end:cpt-cf-oagw-flow-upstream-create:p1:inst-us-create-issue
    // @cpt-begin:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-authz
    // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-authz
    // The parent resource's management permission is the one this operation
    // consumes: the plugin flow registers no path and consumes no permission
    // of its own.
    let ancestors = ancestors_of(&state, context, tenant).await;
    let permissions = OverridePermissions::of(context);
    // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-authz
    // @cpt-end:cpt-cf-oagw-flow-bind-ancestor-upstream:p1:inst-bind-authz
    // @cpt-begin:cpt-cf-oagw-flow-config-change-logged:p1:inst-cc-write
    // The write itself — the validation, the one transaction, and the Control
    // Plane cache flush — is `cpt-cf-oagw-feature-control-plane-config`'s act
    // and is issued here through the management service.
    // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-return
    match state
        .service()
        .create_upstream_in_chain(tenant, &ancestors, &permissions, &body)
    {
        // @cpt-begin:cpt-cf-oagw-flow-config-change-logged:p1:inst-cc-completed-if
        Ok(row) => {
            record_config_change(
                &state,
                crate::domain::observability::EVENT_UPSTREAM_CREATED,
                context,
                "POST",
                &instance,
                u16::from(StatusCode::CREATED),
                tenant,
            );
            json_response(StatusCode::CREATED, &dto::upstream(&row))
        }
        // @cpt-begin:cpt-cf-oagw-flow-config-change-logged:p1:inst-cc-completed-else
        // @cpt-begin:cpt-cf-oagw-flow-config-change-logged:p1:inst-cc-refused
        // No configuration-change record is written for a refused write: the
        // management path answers the refusal itself, with the correlation
        // identifier, and logs neither a request body nor a configuration
        // value.
        Err(error) => refused(&error, &instance),
        // @cpt-end:cpt-cf-oagw-flow-config-change-logged:p1:inst-cc-refused
        // @cpt-end:cpt-cf-oagw-flow-config-change-logged:p1:inst-cc-completed-else
        // @cpt-end:cpt-cf-oagw-flow-config-change-logged:p1:inst-cc-completed-if
        // @cpt-begin:cpt-cf-oagw-flow-config-change-logged:p1:inst-cc-return
        // RETURN the handler's response unchanged: this flow mutates no
        // status, no header, and no body of it.
        // @cpt-end:cpt-cf-oagw-flow-config-change-logged:p1:inst-cc-return
    }
    // @cpt-end:cpt-cf-oagw-flow-config-change-logged:p1:inst-cc-write
    // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-return
}

/// Lists the upstreams of the calling tenant: `GET /oagw/v1/upstreams`.
pub async fn list(
    State(state): State<SharedState>,
    original: OriginalUri,
    context: Option<Extension<SecurityContext>>,
) -> Response {
    let instance = instance_of(&original.0);
    // @cpt-begin:cpt-cf-oagw-flow-config-read-list:p1:inst-read-issue
    // The actor's read request: one of the four read paths, with an `{id}`
    // path parameter for a single read and OData query parameters for a list.
    // @cpt-begin:cpt-cf-oagw-flow-config-read-list:p1:inst-read-authz
    let checked = authorize(
        &state,
        bearer_of(&context),
        ResourceKind::Upstream,
        READ,
        None,
        &instance,
    )
    .await;
    // @cpt-end:cpt-cf-oagw-flow-config-read-list:p1:inst-read-authz
    let context = match checked {
        Ok(context) => context,
        Err(answer) => return answer,
    };
    // @cpt-end:cpt-cf-oagw-flow-config-read-list:p1:inst-read-issue
    let tenant = match tenant_of(context, &instance) {
        Ok(tenant) => tenant,
        Err(answer) => return answer,
    };
    // @cpt-dod:cpt-cf-oagw-dod-list-query-parameters:p1
    match state
        .service()
        .list_upstreams(tenant, params::raw_query(&original.0))
    {
        Ok(page) => json_response(StatusCode::OK, &dto::upstream_page(&page)),
        Err(error) => refused(&error, &instance),
    }
}

/// Reads one upstream: `GET /oagw/v1/upstreams/{id}`.
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
    let selector = params::path_id(ResourceKind::Upstream, &id);
    let checked = authorize(
        &state,
        bearer_of(&context),
        ResourceKind::Upstream,
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
    match state.service().read_upstream(tenant, id) {
        Ok(row) => json_response(StatusCode::OK, &dto::upstream(&row)),
        Err(error) => refused(&error, &instance),
    }
}

/// Replaces one upstream: `PUT /oagw/v1/upstreams/{id}`.
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
    let selector = params::path_id(ResourceKind::Upstream, &id);
    // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-issue
    // The actor's operation against `/oagw/v1/upstreams/{id}`: a replacement
    // when the request carries a body, a deletion when it carries none.
    // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-issue
    // A replacement carries the full replacement of the parent's binding rows,
    // so a body that omits the `plugins` sub-object unlinks every plugin.
    // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-issue
    // @cpt-begin:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-authz
    // @cpt-begin:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-authz
    let checked = authorize(
        &state,
        bearer_of(&context),
        ResourceKind::Upstream,
        OVERRIDE,
        selector.as_ref().ok().copied(),
        &instance,
    )
    .await;
    // @cpt-end:cpt-cf-oagw-flow-enable-disable:p1:inst-en-dis-authz
    // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-authz
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
    // @cpt-end:cpt-cf-oagw-flow-upstream-replace-delete:p1:inst-us-rw-issue
    // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-issue
    // @cpt-begin:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-authz
    // The chain and the permission set the override decision consults.
    let ancestors = ancestors_of(&state, context, tenant).await;
    let permissions = OverridePermissions::of(context);
    // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-authz
    // @cpt-end:cpt-cf-oagw-flow-override-inherited-field:p1:inst-ovr-issue
    // The binding decision consumes the permission the ancestor flow already
    // checked, so the plugin flow registers no gate of its own here.
    // @cpt-begin:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-return
    match state
        .service()
        .replace_upstream_in_chain(tenant, id, &ancestors, &permissions, &body)
    {
        Ok(row) => {
            record_config_change(
                &state,
                crate::domain::observability::EVENT_UPSTREAM_OVERRIDDEN,
                context,
                "PUT",
                &instance,
                u16::from(StatusCode::OK),
                tenant,
            );
            json_response(StatusCode::OK, &dto::upstream(&row))
        }
        Err(error) => refused(&error, &instance),
    }
    // @cpt-end:cpt-cf-oagw-flow-bind-plugins:p1:inst-bind-return
}

/// Deletes one upstream and its cascaded rows: `DELETE /oagw/v1/upstreams/{id}`.
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
    let selector = params::path_id(ResourceKind::Upstream, &id);
    let checked = authorize(
        &state,
        bearer_of(&context),
        ResourceKind::Upstream,
        DELETE,
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
    match state.service().delete_upstream(tenant, id) {
        Ok(true) => {
            record_config_change(
                &state,
                crate::domain::observability::EVENT_UPSTREAM_DELETED,
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
