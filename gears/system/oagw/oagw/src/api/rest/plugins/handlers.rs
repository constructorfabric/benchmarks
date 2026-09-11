//! Plugin Management API handlers
//! (`cpt-cf-oagw-feature-plugin-management`, 2.4).

use std::sync::Arc;

use axum::extract::{Path, Query};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Serialize;
use serde_json::Value;
use toolkit_canonical_errors::problem::APPLICATION_PROBLEM_JSON;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::error::{OagwError, OagwErrorKind};
use crate::model::plugin::{
    Plugin, PluginLookupError, PluginReferences, PluginType, count_plugin_references,
    lookup_plugin_for_management, mark_gc_eligibility, plugin_gts_ref,
};
use crate::store::OagwState;

use super::dto::{PluginListQuery, PluginResponse};

const DEFAULT_PAGE_SIZE: usize = 50;
const MAX_PAGE_SIZE: usize = 100;

fn validation_error(detail: impl Into<String>) -> Response {
    OagwError::new(OagwErrorKind::ValidationError, detail.into()).into_response()
}

/// Minimal RFC 9457 envelope with **no `type` field** -- an absent `type`
/// means `about:blank` per RFC 9457, satisfying this feature's documented
/// `404` contract ("status and RFC 9457 envelope only, no GTS `type`
/// asserted"): neither the data-plane `PluginNotFound` nor the proxy-time
/// `RouteNotFound` identifier is ever asserted from these handlers.
#[derive(Debug, Serialize)]
struct PlainNotFoundProblem {
    title: &'static str,
    status: u16,
    detail: String,
}

fn not_found_response(detail: impl Into<String>) -> Response {
    let problem = PlainNotFoundProblem {
        title: "Not Found",
        status: StatusCode::NOT_FOUND.as_u16(),
        detail: detail.into(),
    };
    let body = serde_json::to_vec(&problem).unwrap_or_default();
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, APPLICATION_PROBLEM_JSON)],
        body,
    )
        .into_response()
}

/// `409 PluginInUse` envelope carrying the documented `plugin_id` and
/// `referenced_by` extension fields (`cpt-cf-oagw-dod-plugin-in-use-protection`,
/// `ADR-0001` §"Plugin Deletion Behavior"). `error::OagwError` cannot render
/// this shape -- its `OagwProblem` has no `plugin_id`/`referenced_by`
/// fields, and this feature does not modify `error.rs` -- so this feature
/// owns its own minimal envelope for this one response.
#[derive(Debug, Serialize)]
struct ReferencedBy {
    upstreams: Vec<String>,
    routes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct PluginInUseProblem {
    #[serde(rename = "type")]
    problem_type: &'static str,
    title: &'static str,
    status: u16,
    detail: String,
    plugin_id: String,
    referenced_by: ReferencedBy,
}

fn plugin_in_use_response(plugin_ref: &str, references: &PluginReferences) -> Response {
    let problem = PluginInUseProblem {
        problem_type: "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1",
        title: "Plugin In Use",
        status: StatusCode::CONFLICT.as_u16(),
        detail: format!(
            "Plugin is referenced by {} upstream(s) and {} route(s)",
            references.upstreams.len(),
            references.routes.len()
        ),
        plugin_id: plugin_ref.to_owned(),
        referenced_by: ReferencedBy {
            upstreams: references.upstreams.clone(),
            routes: references.routes.clone(),
        },
    };
    let body = serde_json::to_vec(&problem).unwrap_or_default();
    (
        StatusCode::CONFLICT,
        [(header::CONTENT_TYPE, APPLICATION_PROBLEM_JSON)],
        body,
    )
        .into_response()
}

/// Parsed, validated `POST /oagw/v1/plugins` request body.
struct ParsedCreateRequest {
    plugin_type: PluginType,
    name: String,
    description: Option<String>,
    config_schema: Option<Value>,
    source_code: String,
}

/// Validate the raw JSON body for `POST /oagw/v1/plugins`
/// (`inst-plugin-create-validate`).
///
/// # Errors
///
/// Returns a human-readable validation-failure message when `plugin_type`
/// is missing/invalid, `name`/`source_code` is missing or empty, or
/// `config_schema` is present but not a JSON object.
fn parse_create_request(body: &Value) -> Result<ParsedCreateRequest, String> {
    let plugin_type = body
        .get("plugin_type")
        .and_then(Value::as_str)
        .and_then(PluginType::from_url_segment)
        .ok_or_else(|| "plugin_type must be one of 'auth', 'guard', or 'transform'".to_owned())?;

    let name = body
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| "name is required and must be a non-empty string".to_owned())?
        .to_owned();

    let source_code = body
        .get("source_code")
        .and_then(Value::as_str)
        .filter(|source| !source.is_empty())
        .ok_or_else(|| "source_code is required and must be a non-empty string".to_owned())?
        .to_owned();

    let description = body
        .get("description")
        .and_then(Value::as_str)
        .map(str::to_owned);

    let config_schema = match body.get("config_schema") {
        None | Some(Value::Null) => None,
        Some(Value::Object(_)) => body.get("config_schema").cloned(),
        Some(_) => return Err("config_schema must be a well-formed JSON Schema object".to_owned()),
    };

    Ok(ParsedCreateRequest {
        plugin_type,
        name,
        description,
        config_schema,
        source_code,
    })
}

/// `POST /oagw/v1/plugins` -- `cpt-cf-oagw-flow-plugin-create`.
// @cpt-flow:cpt-cf-oagw-flow-plugin-create:p1
// @cpt-dod:cpt-cf-oagw-dod-plugin-gts-issuance:p1
// @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-recv
// @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-validate
pub async fn create_plugin(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(security_context): Extension<SecurityContext>,
    Json(body): Json<Value>,
) -> Response {
    let tenant_id = security_context.subject_tenant_id();

    let request = match parse_create_request(&body) {
        Ok(request) => request,
        // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-if-invalid
        // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-400
        Err(detail) => return validation_error(detail),
        // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-400
        // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-if-invalid
    };
    // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-validate
    // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-recv

    // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-else-valid
    // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-name-check
    let name_collision = state
        .store
        .plugins()
        .iter()
        .any(|entry| entry.tenant_id == Some(tenant_id) && entry.name == request.name);
    // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-name-check
    if name_collision {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-if-collide
        // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-400-name
        return validation_error(format!(
            "a plugin named '{}' already exists for this tenant",
            request.name
        ));
        // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-400-name
        // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-if-collide
    }

    // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-else-noncollide
    // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-issue-gts
    let id = Uuid::new_v4();
    // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-issue-gts
    // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-gc-mark
    let gc_eligible_at = mark_gc_eligibility(None, 0);
    // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-gc-mark

    let plugin = Plugin {
        id: Some(id),
        tenant_id: Some(tenant_id),
        plugin_type: request.plugin_type,
        name: request.name,
        description: request.description,
        config_schema: request.config_schema,
        source_code: request.source_code,
        last_used_at: None,
        gc_eligible_at,
    };

    // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-insert
    state.store.plugins().insert(id, Arc::new(plugin.clone()));
    // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-insert
    // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-else-noncollide
    // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-else-valid

    // @cpt-begin:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-201
    (StatusCode::CREATED, Json(PluginResponse::from(&plugin))).into_response()
    // @cpt-end:cpt-cf-oagw-flow-plugin-create:p1:inst-plugin-create-201
}

/// Parse a `field eq 'value'` (or unquoted `value`) OData-lite `$filter`
/// expression against the `name`/`plugin_type` fields only -- a minimal but
/// functioning subset of `inst-plugin-list-odata`, not the full OData v4
/// filter grammar.
fn parse_equality_filter(filter: &str) -> Option<(String, String)> {
    let mut parts = filter.splitn(3, ' ');
    let field = parts.next()?.trim();
    let operator = parts.next()?.trim();
    let value = parts.next()?.trim();
    if operator != "eq" || field.is_empty() || value.is_empty() {
        return None;
    }
    Some((field.to_owned(), value.trim_matches('\'').to_owned()))
}

fn matches_filter(plugin: &Plugin, field: &str, value: &str) -> bool {
    match field {
        "name" => plugin.name == value,
        "plugin_type" => plugin.plugin_type.url_segment() == value,
        _ => false,
    }
}

/// Project each `PluginResponse` down to `fields` only, for `$select`.
fn apply_select(items: &[PluginResponse], fields_csv: &str) -> Vec<Value> {
    let fields: Vec<&str> = fields_csv
        .split(',')
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .collect();
    items
        .iter()
        .map(|item| {
            let value = serde_json::to_value(item).unwrap_or(Value::Null);
            match value {
                Value::Object(map) => Value::Object(
                    map.into_iter()
                        .filter(|(key, _)| fields.contains(&key.as_str()))
                        .collect(),
                ),
                other => other,
            }
        })
        .collect()
}

/// `GET /oagw/v1/plugins` -- `cpt-cf-oagw-flow-plugin-list`.
// @cpt-flow:cpt-cf-oagw-flow-plugin-list:p1
// @cpt-dod:cpt-cf-oagw-dod-plugin-tenant-scope:p1
// @cpt-begin:cpt-cf-oagw-flow-plugin-list:p1:inst-plugin-list-recv
pub async fn list_plugins(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(security_context): Extension<SecurityContext>,
    Query(query): Query<PluginListQuery>,
) -> Response {
    let tenant_id = security_context.subject_tenant_id();

    // @cpt-begin:cpt-cf-oagw-flow-plugin-list:p1:inst-plugin-list-query
    let mut plugins: Vec<Arc<Plugin>> = state
        .store
        .plugins()
        .iter()
        .filter(|entry| entry.tenant_id == Some(tenant_id))
        .map(|entry| Arc::clone(entry.value()))
        .collect();
    // @cpt-end:cpt-cf-oagw-flow-plugin-list:p1:inst-plugin-list-query
    // @cpt-end:cpt-cf-oagw-flow-plugin-list:p1:inst-plugin-list-recv
    plugins.sort_by_key(|plugin| plugin.id);

    // @cpt-begin:cpt-cf-oagw-flow-plugin-list:p1:inst-plugin-list-odata
    if let Some(filter) = query.filter.as_deref() {
        let Some((field, value)) = parse_equality_filter(filter) else {
            return validation_error(
                "unsupported $filter expression; only 'field eq value' is supported",
            );
        };
        plugins.retain(|plugin| matches_filter(plugin, &field, &value));
    }

    let skip = query
        .skip
        .as_deref()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(0);
    let top = query
        .top
        .as_deref()
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE);

    let page: Vec<PluginResponse> = plugins
        .into_iter()
        .skip(skip)
        .take(top)
        .map(|plugin| PluginResponse::from(plugin.as_ref()))
        .collect();

    let response = match query.select.as_deref() {
        Some(select) => Json(apply_select(&page, select)).into_response(),
        None => Json(page).into_response(),
    };
    // @cpt-end:cpt-cf-oagw-flow-plugin-list:p1:inst-plugin-list-odata

    // @cpt-begin:cpt-cf-oagw-flow-plugin-list:p1:inst-plugin-list-200
    response
    // @cpt-end:cpt-cf-oagw-flow-plugin-list:p1:inst-plugin-list-200
}

/// `GET /oagw/v1/plugins/{id}` -- `cpt-cf-oagw-flow-plugin-get`.
// @cpt-flow:cpt-cf-oagw-flow-plugin-get:p1
// @cpt-begin:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-recv
pub async fn get_plugin(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(security_context): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Response {
    let tenant_id = security_context.subject_tenant_id();
    // @cpt-begin:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-parse
    // @cpt-begin:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-else-uuid
    // @cpt-begin:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-query
    let lookup = lookup_plugin_for_management(&id, tenant_id, state.store.plugins());
    // @cpt-end:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-query
    // @cpt-end:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-else-uuid
    // @cpt-end:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-parse
    // @cpt-end:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-recv

    match lookup {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-else-found
        // @cpt-begin:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-200
        Ok(plugin) => Json(PluginResponse::from(plugin.as_ref())).into_response(),
        // @cpt-end:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-200
        // @cpt-end:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-else-found
        // @cpt-begin:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-if-malformed
        // @cpt-begin:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-400
        Err(PluginLookupError::Malformed) => {
            validation_error("id must be a UUID or a valid anonymous GTS plugin identifier")
        }
        // @cpt-end:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-400
        // @cpt-end:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-if-malformed
        // @cpt-begin:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-if-named
        // @cpt-begin:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-404-named
        // @cpt-begin:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-if-notfound
        // @cpt-begin:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-404
        Err(PluginLookupError::NotFound) => not_found_response("plugin not found"),
        // @cpt-end:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-404
        // @cpt-end:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-if-notfound
        // @cpt-end:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-404-named
        // @cpt-end:cpt-cf-oagw-flow-plugin-get:p1:inst-plugin-get-if-named
    }
}

/// `GET /oagw/v1/plugins/{id}/source` -- `cpt-cf-oagw-flow-plugin-get-source`.
// @cpt-flow:cpt-cf-oagw-flow-plugin-get-source:p1
// @cpt-begin:cpt-cf-oagw-flow-plugin-get-source:p1:inst-plugin-source-recv
// @cpt-begin:cpt-cf-oagw-flow-plugin-get-source:p1:inst-plugin-source-lookup
pub async fn get_plugin_source(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(security_context): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Response {
    let tenant_id = security_context.subject_tenant_id();
    let lookup = lookup_plugin_for_management(&id, tenant_id, state.store.plugins());
    // @cpt-end:cpt-cf-oagw-flow-plugin-get-source:p1:inst-plugin-source-lookup
    // @cpt-end:cpt-cf-oagw-flow-plugin-get-source:p1:inst-plugin-source-recv

    match lookup {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-get-source:p1:inst-plugin-source-else-found
        // @cpt-begin:cpt-cf-oagw-flow-plugin-get-source:p1:inst-plugin-source-200
        Ok(plugin) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            plugin.source_code.clone(),
        )
            .into_response(),
        // @cpt-end:cpt-cf-oagw-flow-plugin-get-source:p1:inst-plugin-source-200
        // @cpt-end:cpt-cf-oagw-flow-plugin-get-source:p1:inst-plugin-source-else-found
        // @cpt-begin:cpt-cf-oagw-flow-plugin-get-source:p1:inst-plugin-source-if-notfound
        // @cpt-begin:cpt-cf-oagw-flow-plugin-get-source:p1:inst-plugin-source-error
        Err(PluginLookupError::Malformed) => {
            validation_error("id must be a UUID or a valid anonymous GTS plugin identifier")
        }
        Err(PluginLookupError::NotFound) => not_found_response("plugin not found"),
        // @cpt-end:cpt-cf-oagw-flow-plugin-get-source:p1:inst-plugin-source-error
        // @cpt-end:cpt-cf-oagw-flow-plugin-get-source:p1:inst-plugin-source-if-notfound
    }
}

/// `DELETE /oagw/v1/plugins/{id}` -- `cpt-cf-oagw-flow-plugin-delete`.
// @cpt-flow:cpt-cf-oagw-flow-plugin-delete:p1
// @cpt-dod:cpt-cf-oagw-dod-plugin-in-use-protection:p1
// @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-recv
// @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-lookup
pub async fn delete_plugin(
    Extension(state): Extension<Arc<OagwState>>,
    Extension(security_context): Extension<SecurityContext>,
    Path(id): Path<String>,
) -> Response {
    let tenant_id = security_context.subject_tenant_id();
    let lookup = lookup_plugin_for_management(&id, tenant_id, state.store.plugins());
    // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-lookup
    // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-recv

    let plugin = match lookup {
        Ok(plugin) => plugin,
        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-if-notfound
        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-error
        Err(PluginLookupError::Malformed) => {
            return validation_error(
                "id must be a UUID or a valid anonymous GTS plugin identifier",
            );
        }
        Err(PluginLookupError::NotFound) => return not_found_response("plugin not found"),
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-error
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-if-notfound
    };

    // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-else-found
    // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-refcount
    let plugin_uuid = plugin.id.unwrap_or_default();
    let plugin_ref = plugin_gts_ref(plugin.plugin_type, plugin_uuid);
    let references = count_plugin_references(
        &plugin_ref,
        Some(plugin_uuid),
        tenant_id,
        state.store.upstreams(),
        state.store.routes(),
    );
    // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-refcount

    if references.count() > 0 {
        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-if-inuse
        // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-409
        return plugin_in_use_response(&plugin_ref, &references);
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-409
        // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-if-inuse
    }

    // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-else-unused
    // Re-run the GC mark phase at this decision point per
    // `cpt-cf-oagw-dod-plugin-gc-bookkeeping`; the row is deleted
    // immediately afterward regardless of the resulting value.
    let _ = mark_gc_eligibility(plugin.gc_eligible_at.as_deref(), 0);

    // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-execute
    // @cpt-begin:cpt-cf-oagw-state-plugin-lifecycle:p2:inst-lifecycle-unreferenced-to-deleted
    state.store.plugins().remove(&plugin_uuid);
    // @cpt-end:cpt-cf-oagw-state-plugin-lifecycle:p2:inst-lifecycle-unreferenced-to-deleted
    // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-execute
    // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-else-unused
    // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-else-found

    // @cpt-begin:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-204
    StatusCode::NO_CONTENT.into_response()
    // @cpt-end:cpt-cf-oagw-flow-plugin-delete:p1:inst-plugin-delete-204
}
