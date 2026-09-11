//! Plugin definition endpoints (entry 2.3).
//!
//! The five endpoints of `cpt-cf-oagw-dod-plugin-endpoints`: the create, the
//! list, the read, the delete and the source read of a custom plugin
//! definition. There is deliberately no `PUT`: a definition is immutable for
//! its lifetime (`cpt-cf-oagw-dod-plugin-immutability`), so a replace request
//! resolves to the router's method-not-allowed response and never to a stored
//! mutation, and no other plugin endpoint is registered.
//!
//! The handlers are as thin as the entry-2.2 ones: they gate the caller on the
//! permission string the plugin type derives
//! (`gts.cf.core.oagw.{type}_plugin.v1~:{create,read,delete}`), hand the request
//! to [`ManagementService`](crate::domain::services::management::ManagementService)
//! and map every outcome through the single mapping layer of entry 2.1. The
//! source read is the one response that is not JSON: the stored `source_code`
//! goes out verbatim as `text/plain; charset=utf-8`, with no envelope around it
//! and never as an active-content type
//! (`cpt-cf-oagw-dod-plugin-source-endpoint`).

use axum::extract::{OriginalUri, Path, RawQuery, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::api::rest::handlers::{created, mapped, no_content, ok, Body, Caller, ManagementState};
use crate::domain::plugin::PluginType;
use crate::domain::validation::PluginSpec;

/// Media type of the verbatim source response (`inst-psrc-07`).
///
/// A non-HTML, non-executable type: the stored source is never served as an
/// active-content vector.
const SOURCE_MEDIA_TYPE: &str = "text/plain; charset=utf-8";

/// The permission strings of one action over the three plugin types.
fn permissions(action: &str) -> Vec<String> {
    PluginType::all()
        .iter()
        .map(|plugin_type| plugin_type.permission(action))
        .collect()
}

/// The request path a problem document names, taken from `OriginalUri`.
fn path_of(uri: &OriginalUri) -> String {
    uri.0.path().to_owned()
}

// @cpt-begin:cpt-cf-oagw-dod-plugin-endpoints:p1:inst-full
/// `POST /oagw/v1/plugins` (`cpt-cf-oagw-flow-plugin-create`).
pub async fn create_plugin(
    State(state): State<ManagementState>,
    caller: Caller,
    uri: OriginalUri,
    body: Body<PluginSpec>,
) -> Response {
    let path = path_of(&uri);

    // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-01
    // The create request reached the endpoint with its body.
    // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-01

    // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-04
    // The body is bound to the create DTO, whose `deny_unknown_fields` rejects
    // a property the schema does not declare.
    let spec = body.value;
    // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-04

    // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-02
    // The `create` permission of the requested `plugin_type`. A body whose type
    // the catalog does not declare has no permission to derive, so it falls
    // through to the validator, which reports the `400` the flow names.
    if let Some(plugin_type) = spec.plugin_type.as_deref().and_then(PluginType::parse)
        && let Err(error) = caller.require(&plugin_type.permission("create"))
    {
        return mapped(&error, &path);
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-02

    // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-03
    // The calling tenant is resolved by the `Caller` extractor, which rejects a
    // request the security context cannot attribute to a tenant before this
    // handler runs.
    // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-03

    match state.service().create_plugin(caller.actor(), &spec) {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-13
        Ok(record) => created(record.as_ref()),
        // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-13
        // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-07
        Err(error) => mapped(&error, &path),
        // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-pcre-07
    }
}

/// `GET /oagw/v1/plugins/{id}` (`cpt-cf-oagw-flow-plugin-list-read`).
pub async fn get_plugin(
    State(state): State<ManagementState>,
    caller: Caller,
    Path(identifier): Path<String>,
    uri: OriginalUri,
) -> Response {
    let path = path_of(&uri);

    // @cpt-begin:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-01
    // The read request reached the endpoint with its bearer token.
    // @cpt-end:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-01

    // @cpt-begin:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-02
    // The family gate runs before any store access; the gate of the
    // definition's own type runs once the resolution below has named it.
    if let Err(error) = caller.actor().require_any(&permissions("read")) {
        return mapped(&error, &path);
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-02

    // @cpt-begin:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-04
    // @cpt-begin:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-05
    // The request targets a single identifier, classified by the
    // identifier-resolution algorithm inside the domain service.
    match state.service().get_plugin(caller.actor(), &identifier) {
        Ok(record) => {
            if let Err(error) = caller.require(&record.plugin_type.permission("read")) {
                return mapped(&error, &path);
            }
            // @cpt-begin:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-10
            ok(record.as_ref())
            // @cpt-end:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-10
        }
        Err(error) => mapped(&error, &path),
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-05
    // @cpt-end:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-04
}

/// `GET /oagw/v1/plugins` (`cpt-cf-oagw-flow-plugin-list-read`).
pub async fn list_plugins(
    State(state): State<ManagementState>,
    caller: Caller,
    uri: OriginalUri,
    RawQuery(query): RawQuery,
) -> Response {
    let path = path_of(&uri);

    // @cpt-begin:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-01
    // The list request reached the endpoint with its bearer token.
    // @cpt-end:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-01

    // @cpt-begin:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-02
    // A list request carries no `plugin_type` of its own, so the family gate
    // is the gate of the collection.
    if let Err(error) = caller.actor().require_any(&permissions("read")) {
        return mapped(&error, &path);
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-02

    // @cpt-begin:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-06
    // The `ELSE` of the single-identifier branch: no `{id}` in the path, so the
    // query parameters are translated and the collection is returned.
    // @cpt-begin:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-09
    // The projection falls back to the full stored representation, which carries
    // no credential material: a definition holds configuration metadata and
    // script source only, and no resolved secret value exists to leak.
    // @cpt-end:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-09

    // @cpt-begin:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-07
    // The query parameters are translated with the plugin list query; an
    // unsupported expression is returned as `400`.
    match state
        .service()
        .list_plugins(caller.actor(), query.as_deref().unwrap_or_default())
    {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-10
        Ok(page) => ok(&page),
        // @cpt-end:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-10
        Err(error) => mapped(&error, &path),
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-07
    // @cpt-end:cpt-cf-oagw-flow-plugin-list-read:p1:inst-plst-06
}

/// `DELETE /oagw/v1/plugins/{id}` (`cpt-cf-oagw-flow-plugin-delete`).
pub async fn delete_plugin(
    State(state): State<ManagementState>,
    caller: Caller,
    Path(identifier): Path<String>,
    uri: OriginalUri,
) -> Response {
    let path = path_of(&uri);

    // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-01
    // The delete request reached the endpoint carrying only the identifier of
    // the definition to remove.
    // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-01

    // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-02
    // The family gate runs before any store access; the gate of the
    // definition's own type runs once the resolution below has named it.
    if let Err(error) = caller.actor().require_any(&permissions("delete")) {
        return mapped(&error, &path);
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-02

    // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-03
    // The definition is resolved first, so the `delete` permission of its own
    // `plugin_type` can be required; the in-use scan and the delete themselves
    // run inside the domain service, in one critical section.
    // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-04
    // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-05
    // A named builtin plugin and a catalog-only identifier have no row, so the
    // resolution reports them missing like any other unknown identifier.
    let record = match state.service().get_plugin(caller.actor(), &identifier) {
        Ok(record) => record,
        Err(error) => return mapped(&error, &path),
    };
    // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-05
    // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-04
    if let Err(error) = caller.require(&record.plugin_type.permission("delete")) {
        return mapped(&error, &path);
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-03

    // The delete request carries only the identifier of the definition to
    // remove.
    match state.service().delete_plugin(caller.actor(), &identifier) {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-14
        Ok(()) => no_content(),
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-14
        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-09
        Err(error) => mapped(&error, &path),
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-pdel-09
    }
}

/// `GET /oagw/v1/plugins/{id}/source`
/// (`cpt-cf-oagw-flow-plugin-source-read`).
pub async fn plugin_source(
    State(state): State<ManagementState>,
    caller: Caller,
    Path(identifier): Path<String>,
    uri: OriginalUri,
) -> Response {
    let path = path_of(&uri);

    // @cpt-begin:cpt-cf-oagw-flow-plugin-source-read:p1:inst-psrc-01
    // The source read request reached the endpoint for an existing definition.
    // @cpt-end:cpt-cf-oagw-flow-plugin-source-read:p1:inst-psrc-01

    // @cpt-begin:cpt-cf-oagw-flow-plugin-source-read:p1:inst-psrc-02
    // The read permission the read flow requires, gated the same way.
    if let Err(error) = caller.actor().require_any(&permissions("read")) {
        return mapped(&error, &path);
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-source-read:p1:inst-psrc-02

    // @cpt-begin:cpt-cf-oagw-flow-plugin-source-read:p1:inst-psrc-03
    // The identifier is classified and looked up inside the calling tenant's
    // key space by the domain service, which also names the type the read
    // permission is derived from.
    // @cpt-begin:cpt-cf-oagw-flow-plugin-source-read:p1:inst-psrc-04
    // A named builtin plugin is among the identifiers that resolve to no stored
    // definition, so it is reported missing like any other unknown identifier.
    let record = match state.service().get_plugin(caller.actor(), &identifier) {
        Ok(record) => record,
        Err(error) => return mapped(&error, &path),
    };
    // @cpt-end:cpt-cf-oagw-flow-plugin-source-read:p1:inst-psrc-04
    if let Err(error) = caller.require(&record.plugin_type.permission("read")) {
        return mapped(&error, &path);
    }

    // The stored source, read from the row the resolution returned.
    match state.service().plugin_source(caller.actor(), &identifier) {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-source-read:p1:inst-psrc-07
        // The stored source, verbatim and with no JSON envelope around it: the
        // body is the `source_code` the store holds, byte for byte.
        Ok(source) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, SOURCE_MEDIA_TYPE)],
            source,
        )
            .into_response(),
        // @cpt-end:cpt-cf-oagw-flow-plugin-source-read:p1:inst-psrc-07
        // @cpt-begin:cpt-cf-oagw-flow-plugin-source-read:p1:inst-psrc-05
        Err(error) => mapped(&error, &path),
        // @cpt-end:cpt-cf-oagw-flow-plugin-source-read:p1:inst-psrc-05
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-source-read:p1:inst-psrc-03

    // @cpt-begin:cpt-cf-oagw-flow-plugin-source-read:p1:inst-psrc-08
    // Executing that source is out of scope on this path: no interpreter, no
    // sandbox and no network call is invoked here (DECOMPOSITION assumption 4).
    // @cpt-end:cpt-cf-oagw-flow-plugin-source-read:p1:inst-psrc-08
}
// @cpt-end:cpt-cf-oagw-dod-plugin-endpoints:p1:inst-full
