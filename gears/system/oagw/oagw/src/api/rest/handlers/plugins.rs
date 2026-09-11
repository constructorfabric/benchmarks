//! Plugin handlers — the five custom-plugin endpoints of DECOMPOSITION §2.4.
//!
//! The three plugin families are three resource types to the enforcer, so the
//! one thing every handler resolves first is the family the request selects:
//! the body's `plugin_type` for a create and the path identifier's prefix for
//! an addressed read or deletion. A list names no family at all, so it is
//! admitted only when the token holds `read` on every arm.

use axum::body::Bytes;
use axum::extract::{OriginalUri, Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::Extension;
use toolkit_security::SecurityContext;

use super::{
    authenticate, authorize_plugin_all, authorize_selector, enforce_plugin, instance_of,
    json_response, no_content, parse_body, record_config_change, refused, tenant_of, unaddressed,
    CREATE, DELETE, READ, SharedState,
};
use crate::api::rest::dto;
use crate::api::rest::params;
use crate::api::rest::problem;
use crate::control_plane::plugin_def;

/// Creates one custom plugin: `POST /oagw/v1/plugins`.
///
/// The body is read before the permission, because it is what selects the arm
/// the permission is enforced against; validation itself still runs after the
/// permission, in the service.
pub async fn create(
    State(state): State<SharedState>,
    original: OriginalUri,
    context: Option<Extension<SecurityContext>>,
    body: Bytes,
) -> Response {
    let instance = instance_of(&original.0);
    // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-issue
    // The actor's create request carries the plugin definition; the handler
    // authenticates before it reads the body, because the body selects the arm
    // and the arm is meaningless without a subject.
    // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-issue
    let context = match authenticate(bearer_of(&context), &instance).await {
        Ok(context) => context,
        Err(answer) => return answer,
    };
    let body = match parse_body(&body, &instance) {
        Ok(body) => body,
        Err(answer) => return answer,
    };
    let Some(family) = plugin_def::declared_family(&body) else {
        // No arm is named by a body whose `plugin_type` selects none, so no
        // single arm may speak for the request: every arm is enforced, and a
        // token that holds none of them is refused before the validation the
        // flow answers with.
        if let Err(answer) = authorize_plugin_all(&state, Some(context), CREATE, &instance).await {
            return answer;
        }
        let tenant = match tenant_of(context, &instance) {
            Ok(tenant) => tenant,
            Err(answer) => return answer,
        };
        return match state.service().create_plugin(tenant, &body) {
            Ok(row) => json_response(StatusCode::CREATED, &dto::plugin(&row)),
            Err(error) => refused(&error, &instance),
        };
    };
    // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-authz
    let enforced = enforce_plugin(
        &state,
        context,
        family,
        CREATE,
        None,
        &instance,
    )
    .await;
    // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pl-create-authz
    if let Err(answer) = enforced {
        return answer;
    }
    let tenant = match tenant_of(context, &instance) {
        Ok(tenant) => tenant,
        Err(answer) => return answer,
    };
    match state.service().create_plugin(tenant, &body) {
        Ok(row) => {
            record_config_change(
                &state,
                crate::domain::observability::EVENT_PLUGIN_CREATED,
                context,
                "POST",
                &instance,
                u16::from(StatusCode::CREATED),
                tenant,
            );
            json_response(StatusCode::CREATED, &dto::plugin(&row))
        }
        Err(error) => refused(&error, &instance),
    }
}

/// Lists the custom plugins of the calling tenant: `GET /oagw/v1/plugins`.
pub async fn list(
    State(state): State<SharedState>,
    original: OriginalUri,
    context: Option<Extension<SecurityContext>>,
) -> Response {
    let instance = instance_of(&original.0);
    // @cpt-begin:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-issue
    // The actor's read request: the list path, or one of the two addressed
    // read paths below.
    // @cpt-end:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-issue
    // @cpt-begin:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-authz
    // A list names no family, so every arm is enforced in turn.
    let checked = authorize_plugin_all(&state, bearer_of(&context), READ, &instance).await;
    // @cpt-end:cpt-cf-oagw-flow-plugin-read-source:p1:inst-pl-read-authz
    let context = match checked {
        Ok(context) => context,
        Err(answer) => return answer,
    };
    let tenant = match tenant_of(context, &instance) {
        Ok(tenant) => tenant,
        Err(answer) => return answer,
    };
    match state
        .service()
        .list_plugins(tenant, params::raw_query(&original.0))
    {
        Ok(page) => json_response(StatusCode::OK, &dto::plugin_page(&page)),
        Err(error) => refused(&error, &instance),
    }
}

/// Reads one custom plugin: `GET /oagw/v1/plugins/{id}`.
pub async fn read(
    State(state): State<SharedState>,
    original: OriginalUri,
    context: Option<Extension<SecurityContext>>,
    Path(id): Path<String>,
) -> Response {
    let instance = instance_of(&original.0);
    // The path identifier is parsed, never answered, before the two gates.
    let selector = params::PluginSelector::parse(&id);
    let checked = authorize_selector(&state, bearer_of(&context), &selector, READ, &instance).await;
    let context = match checked {
        Ok(context) => context,
        Err(answer) => return answer,
    };
    let (_, id) = match selector.addressed() {
        Ok(selector) => selector,
        Err(error) => return problem::problem_response(&error, &instance),
    };
    let tenant = match tenant_of(context, &instance) {
        Ok(tenant) => tenant,
        Err(answer) => return answer,
    };
    match state.service().read_plugin(tenant, id) {
        Ok(row) => json_response(StatusCode::OK, &dto::plugin(&row)),
        Err(error) => refused(&error, &instance),
    }
}

/// Reads the Starlark source of one custom plugin:
/// `GET /oagw/v1/plugins/{id}/source`.
pub async fn source(
    State(state): State<SharedState>,
    original: OriginalUri,
    context: Option<Extension<SecurityContext>>,
    Path(id): Path<String>,
) -> Response {
    let instance = instance_of(&original.0);
    let selector = params::PluginSelector::parse(&id);
    let checked = authorize_selector(&state, bearer_of(&context), &selector, READ, &instance).await;
    let context = match checked {
        Ok(context) => context,
        Err(answer) => return answer,
    };
    let (_, id) = match selector.addressed() {
        Ok(selector) => selector,
        Err(error) => return problem::problem_response(&error, &instance),
    };
    let tenant = match tenant_of(context, &instance) {
        Ok(tenant) => tenant,
        Err(answer) => return answer,
    };
    match state.service().read_plugin_source(tenant, id) {
        Ok(source) => json_response(StatusCode::OK, &dto::plugin_source(&source)),
        Err(error) => refused(&error, &instance),
    }
}

/// Deletes one custom plugin: `DELETE /oagw/v1/plugins/{id}`.
pub async fn delete(
    State(state): State<SharedState>,
    original: OriginalUri,
    context: Option<Extension<SecurityContext>>,
    Path(id): Path<String>,
) -> Response {
    let instance = instance_of(&original.0);
    // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-issue
    // The actor's deletion request carries no body: the path identifier is
    // the only address the operation holds.
    // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-issue
    let selector = params::PluginSelector::parse(&id);
    // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-authz
    let checked =
        authorize_selector(&state, bearer_of(&context), &selector, DELETE, &instance).await;
    // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pl-del-authz
    let context = match checked {
        Ok(context) => context,
        Err(answer) => return answer,
    };
    let (_, id) = match selector.addressed() {
        Ok(selector) => selector,
        Err(error) => return problem::problem_response(&error, &instance),
    };
    let tenant = match tenant_of(context, &instance) {
        Ok(tenant) => tenant,
        Err(answer) => return answer,
    };
    match state.service().delete_plugin(tenant, id) {
        Ok(true) => {
            record_config_change(
                &state,
                crate::domain::observability::EVENT_PLUGIN_DELETED,
                context,
                "DELETE",
                &instance,
                u16::from(StatusCode::NO_CONTENT),
                tenant,
            );
            no_content()
        }
        // The service resolved the row before the deletion, so a `false`
        // answer means the row left between the resolution and the write; the
        // answer is the same 404 the resolution would have produced.
        Ok(false) => unaddressed(&instance),
        Err(error) => refused(&error, &instance),
    }
}

/// The authenticated context the extractor carried, or `None` when it carried
/// none.
fn bearer_of(context: &Option<Extension<SecurityContext>>) -> Option<&SecurityContext> {
    context.as_deref()
}
