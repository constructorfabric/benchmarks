//! REST handlers and DTOs of the plugin management API.
//!
//! Thin adapters over the plugin framework
//! ([`crate::infra::plugins`]): they validate the payload, delegate to the
//! per-tenant [`PluginCatalog`](crate::infra::plugins::PluginCatalog) and shape
//! the response. Definitions are immutable, so there is no `PUT`/`PATCH`: a
//! change is a new plugin plus re-binding.
//!
//! The DTOs live next to their handlers because the plugin surface is the only
//! consumer of these shapes and the phase-2 [`dto`](super::dto) module stays
//! untouched.

use std::str::FromStr;
use std::sync::Arc;

use axum::extract::{Extension, Path, Query};
use axum::response::IntoResponse;
use serde_json::Value;
use toolkit::api::canonical_prelude::*;
use toolkit_canonical_errors::{CanonicalError, Http, TransportOverride, resource_error};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::{OagwPluginInUse, OagwValidationError};
use crate::domain::service::{FilterExpr, FilterOp, ListParams, ListQuery};
use crate::infra::plugins::{
    BuiltinCatalog, PluginCatalog, PluginDefinition, PluginInput, PluginRegistry, PluginType,
    PluginUsage,
};
use crate::infra::store::Store;

/// Resource marker of a management plugin (`cf.core.oagw.plugin.v1`).
#[resource_error(gts_id!("cf.core.oagw.plugin.v1~"))]
pub struct PluginResource;

/// 409 status override: `FailedPrecondition` defaults to 400 — the same status
/// class `DESIGN.md`'s error table reports `PluginInUse` with.
const CONFLICT_STATUS: TransportOverride = Http::status_code(409);

/// Filterable and sortable fields of the plugin list endpoint.
const PLUGIN_LIST_FIELDS: &[&str] = &["type", "name"];

/// Permitted characters of a plugin name: the same shape as a tag, so a name
/// stays a stable identifier on the wire.
const NAME_PATTERN: &str = "^[a-z0-9_-]+$";
/// Largest accepted plugin name.
const MAX_NAME_LENGTH: usize = 128;
/// Largest accepted Starlark source.
const MAX_SOURCE_LENGTH: usize = 65_536;

// ---------------------------------------------------------------------------
// DTOs
// ---------------------------------------------------------------------------

/// Create payload of a plugin (`POST` `/oagw/v1/plugins`).
///
/// Every field is `Option` so *all* payload rules are decided in
/// [`validate`] and reported as one canonical 400 with `field_violations`,
/// never as an opaque deserialization error.
#[derive(Debug, Clone, PartialEq, Default)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct PluginRequestDto {
    /// Kind of the plugin: `auth`, `guard` or `transform`.
    #[serde(default)]
    pub plugin_type: Option<String>,
    /// Tenant-unique name of the plugin.
    #[serde(default)]
    pub name: Option<String>,
    /// JSON Schema of the configuration the plugin accepts.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub config_schema: Option<Value>,
    /// Starlark source of the plugin.
    #[serde(default)]
    pub source_code: Option<String>,
}

/// Stored representation of a plugin.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct PluginDto {
    /// Server-generated id.
    pub id: Uuid,
    /// GTS identifier a binding references the plugin by.
    pub plugin_ref: String,
    /// Kind of the plugin: `auth`, `guard` or `transform`.
    pub plugin_type: String,
    /// Tenant-unique name.
    pub name: String,
    /// JSON Schema of the accepted configuration.
    #[schema(value_type = Object)]
    pub config_schema: Value,
    /// Starlark source of the plugin.
    pub source_code: String,
}

impl From<PluginDefinition> for PluginDto {
    fn from(definition: PluginDefinition) -> Self {
        Self {
            id: definition.id,
            plugin_ref: definition.plugin_ref(),
            plugin_type: definition.plugin_type.as_str().to_owned(),
            name: definition.name,
            config_schema: definition.config_schema,
            source_code: definition.source_code,
        }
    }
}

/// `GET /oagw/v1/plugins/{id}/source` response: the plugin document with its
/// Starlark source.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct PluginSourceDto {
    /// Server-generated id.
    pub id: Uuid,
    /// Full GTS identifier a binding references the plugin by.
    pub plugin_ref: String,
    /// Kind of the plugin.
    pub plugin_type: String,
    /// Tenant-unique name.
    pub name: String,
    /// JSON Schema of the accepted configuration.
    #[schema(value_type = Object)]
    pub config_schema: Value,
    /// Starlark source of the plugin.
    pub source_code: String,
}

impl From<PluginDefinition> for PluginSourceDto {
    fn from(definition: PluginDefinition) -> Self {
        Self {
            id: definition.id,
            plugin_ref: definition.plugin_ref(),
            plugin_type: definition.plugin_type.as_str().to_owned(),
            name: definition.name,
            config_schema: definition.config_schema,
            source_code: definition.source_code,
        }
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// A field the create payload rejected.
struct Rejected {
    field: &'static str,
    description: String,
    reason: &'static str,
}

/// Validates the create payload into a [`PluginInput`].
///
/// A name may not shadow a built-in catalog name: `timeout`, `cors`, `logging`
/// and `metrics` are catalog-only identifiers of core Data Plane logic, while
/// `noop`, `apikey`, `required_headers` and `request_id` are the built-ins
/// themselves.
///
/// # Errors
/// A canonical 400 with one `field_violations` entry per rejected field.
fn validate(
    body: PluginRequestDto,
    builtin: &BuiltinCatalog,
) -> Result<PluginInput, CanonicalError> {
    let mut rejected: Vec<Rejected> = Vec::new();

    let plugin_type = match body.plugin_type.as_deref() {
        None => {
            rejected.push(Rejected {
                field: "plugin_type",
                description: "plugin_type is required".to_owned(),
                reason: "REQUIRED",
            });
            None
        }
        Some(raw) => match PluginType::from_str(raw) {
            Ok(plugin_type) => Some(plugin_type),
            Err(error) => {
                rejected.push(Rejected {
                    field: "plugin_type",
                    description: error.to_string(),
                    reason: "INVALID_VALUE",
                });
                None
            }
        },
    };

    let name = match body.name.as_deref() {
        None | Some("") => {
            rejected.push(Rejected {
                field: "name",
                description: "name is required".to_owned(),
                reason: "REQUIRED",
            });
            None
        }
        Some(name) => {
            let well_formed = name.len() <= MAX_NAME_LENGTH
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
            if !well_formed {
                rejected.push(Rejected {
                    field: "name",
                    description: format!(
                        "'{name}' does not match {NAME_PATTERN} (at most {MAX_NAME_LENGTH} \
                         characters)"
                    ),
                    reason: "INVALID_VALUE",
                });
            } else if builtin.is_reserved_name(name) {
                rejected.push(Rejected {
                    field: "name",
                    description: format!(
                        "'{name}' is a reserved built-in plugin name and cannot be redefined"
                    ),
                    reason: "RESERVED_NAME",
                });
            }
            Some(name.to_owned())
        }
    };

    let config_schema = match body.config_schema {
        Some(schema) if schema.is_object() => schema,
        Some(_) => {
            rejected.push(Rejected {
                field: "config_schema",
                description: "config_schema must be a JSON object".to_owned(),
                reason: "INVALID_VALUE",
            });
            Value::Null
        }
        None => Value::Object(serde_json::Map::new()),
    };

    let source_code = match body.source_code.as_deref() {
        None | Some("") => {
            rejected.push(Rejected {
                field: "source_code",
                description: "source_code is required: a plugin is its Starlark source".to_owned(),
                reason: "REQUIRED",
            });
            None
        }
        Some(source) if source.len() > MAX_SOURCE_LENGTH => {
            rejected.push(Rejected {
                field: "source_code",
                description: format!("source_code exceeds {MAX_SOURCE_LENGTH} bytes"),
                reason: "OUT_OF_RANGE",
            });
            None
        }
        Some(source) => Some(source.to_owned()),
    };

    if !rejected.is_empty() {
        return Err(rejection(rejected));
    }
    Ok(PluginInput {
        plugin_type: plugin_type.unwrap_or(PluginType::Transform),
        name: name.unwrap_or_default(),
        config_schema,
        source_code: source_code.unwrap_or_default(),
    })
}

/// Builds the canonical 400 carrying one violation per rejected field.
fn rejection(rejected: Vec<Rejected>) -> CanonicalError {
    let mut rejected = rejected.into_iter();
    let Some(first) = rejected.next() else {
        return OagwValidationError::invalid_argument()
            .with_format("validation failed")
            .create();
    };
    let mut builder = OagwValidationError::invalid_argument().with_field_violation(
        first.field,
        first.description,
        first.reason,
    );
    for field in rejected {
        builder = builder.with_field_violation(field.field, field.description, field.reason);
    }
    builder.create()
}

/// The 400 a list endpoint answers an unsupported field with.
fn unsupported_field(field: &str) -> CanonicalError {
    OagwValidationError::invalid_argument()
        .with_field_violation(
            field,
            format!("'{field}' is not a supported field"),
            "INVALID_VALUE",
        )
        .create()
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// The shared state of the plugin management API.
#[derive(Debug, Clone)]
pub struct PluginApiState {
    /// Per-tenant catalog of custom plugin definitions.
    pub plugins: Arc<PluginCatalog>,
    /// In-process registry, whose built-in catalog a name may not shadow.
    pub registry: Arc<PluginRegistry>,
    /// Store the "still bound" scan reads the upstreams and routes from.
    pub store: Arc<Store>,
}

/// The caller's tenant, or 401 when the request carries no security context.
fn caller(ctx: Option<Extension<SecurityContext>>) -> Result<SecurityContext, CanonicalError> {
    ctx.map(|Extension(ctx)| ctx).ok_or_else(|| {
        CanonicalError::unauthenticated()
            .with_reason("MISSING_SECURITY_CONTEXT")
            .create()
    })
}

/// The definition `id` of the calling tenant, or 404.
fn require_plugin(
    state: &PluginApiState,
    tenant_id: Uuid,
    id: Uuid,
) -> Result<PluginDefinition, CanonicalError> {
    state.plugins.get(tenant_id, id).ok_or_else(|| {
        PluginResource::not_found(format!("no plugin '{id}' in the calling tenant"))
            .with_resource(id.to_string())
            .create()
    })
}

/// The 409 a bound plugin is deleted with: one precondition violation per
/// binding, so the problem context carries the binding list. `None` when the
/// plugin has no binding left, which is not an error.
fn in_use(plugin_ref: &str, usages: &[PluginUsage]) -> Option<CanonicalError> {
    let first = usages.first()?;
    let violation = |usage: &PluginUsage| {
        (
            format!("{}/{}", usage.resource, usage.resource_id),
            usage.to_string(),
            "plugin_binding",
        )
    };
    let (subject, description, kind) = violation(first);
    let mut builder = OagwPluginInUse::failed_precondition()
        .with_resource(plugin_ref)
        .with_override(CONFLICT_STATUS)
        .with_precondition_violation(subject, description, kind);
    for usage in &usages[1..] {
        let (subject, description, kind) = violation(usage);
        builder = builder.with_precondition_violation(subject, description, kind);
    }
    Some(builder.create())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /oagw/v1/plugins`
///
/// Creates an immutable plugin definition owned by the calling tenant and
/// returns 201 with the stored representation and a `Location` header.
///
/// # Errors
/// A canonical `Problem` for an invalid payload (400), a name that shadows a
/// built-in catalog identifier (400), a name already in use (400), or a missing
/// security context (401).
pub async fn create_plugin(
    uri: axum::http::Uri,
    ctx: Option<Extension<SecurityContext>>,
    Extension(state): Extension<Arc<PluginApiState>>,
    Json(body): Json<PluginRequestDto>,
) -> ApiResult<impl IntoResponse> {
    let ctx = caller(ctx)?;
    let input = validate(body, state.registry.catalog())?;
    if state
        .plugins
        .get_by_name(ctx.subject_tenant_id(), &input.name)
        .is_some()
    {
        return Err(rejection(vec![Rejected {
            field: "name",
            description: format!("a plugin named '{}' already exists", input.name),
            reason: "DUPLICATE",
        }]));
    }
    let created = state.plugins.create(ctx.subject_tenant_id(), input);
    let id = created.id.to_string();
    Ok(created_json(PluginDto::from(created), &uri, &id))
}

/// `GET /oagw/v1/plugins`
///
/// # Errors
/// A canonical `Problem` for an unsupported `$filter`, `$orderby` or `$top`
/// (400), or a missing security context (401).
pub async fn list_plugins(
    ctx: Option<Extension<SecurityContext>>,
    Extension(state): Extension<Arc<PluginApiState>>,
    Query(params): Query<PluginListParams>,
) -> ApiResult<impl IntoResponse> {
    let ctx = caller(ctx)?;
    let params = ListParams {
        filter: params.filter,
        select: params.select,
        orderby: params.orderby,
        top: params.top,
        skip: params.skip,
    };
    let query = ListQuery::parse(&params)?;
    if let Some(filter) = &query.filter
        && !PLUGIN_LIST_FIELDS.contains(&filter.field.as_str())
    {
        return Err(unsupported_field(&filter.field));
    }
    if let Some(order) = &query.orderby
        && !PLUGIN_LIST_FIELDS.contains(&order.field.as_str())
    {
        return Err(unsupported_field(&order.field));
    }
    let items: Vec<PluginDto> = state
        .plugins
        .list(ctx.subject_tenant_id())
        .into_iter()
        .map(PluginDto::from)
        .filter(|plugin| matches_filter(&query.filter, plugin))
        .collect();
    let ordered = order_by(items, &query.orderby);
    let body: Vec<serde_json::Value> = ordered
        .into_iter()
        .map(|item| apply_select(item, query.select.as_deref()))
        .skip(query.skip)
        .take(query.top)
        .collect();
    Ok(ok_json(body))
}

/// `OData` list parameters of `GET /oagw/v1/plugins` (`$filter`, `$select`,
/// `$orderby`, `$top`, `$skip`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[toolkit_macros::api_dto(request)]
pub struct PluginListParams {
    /// `$filter` — a single `field eq|ne 'value'` comparison.
    #[serde(default, rename = "$filter")]
    pub filter: Option<String>,
    /// `$select` — comma-separated field projection.
    #[serde(default, rename = "$select")]
    pub select: Option<String>,
    /// `$orderby` — `field [asc|desc]`.
    #[serde(default, rename = "$orderby")]
    pub orderby: Option<String>,
    /// `$top` — page size, 50 by default, at most 100.
    #[serde(default, rename = "$top")]
    pub top: Option<String>,
    /// `$skip` — number of leading items to drop.
    #[serde(default, rename = "$skip")]
    pub skip: Option<String>,
}

/// Applies a single `field eq|ne 'value'` filter to one plugin.
fn matches_filter(filter: &Option<FilterExpr>, plugin: &PluginDto) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    let value = match filter.field.as_str() {
        "type" => plugin.plugin_type.clone(),
        _ => plugin.name.clone(),
    };
    let matched = value == filter.value;
    match filter.op {
        FilterOp::Eq => matched,
        FilterOp::Ne => !matched,
    }
}

/// Sorts the list by the parsed `$orderby` clause, `name` by default.
fn order_by(
    items: Vec<PluginDto>,
    orderby: &Option<crate::domain::service::OrderBy>,
) -> Vec<PluginDto> {
    let Some(order) = orderby else {
        return items;
    };
    let mut items = items;
    items.sort_by(|left, right| {
        let ordering = match order.field.as_str() {
            "type" => left
                .plugin_type
                .cmp(&right.plugin_type)
                .then_with(|| left.name.cmp(&right.name)),
            _ => left.name.cmp(&right.name),
        };
        if order.descending {
            ordering.reverse()
        } else {
            ordering
        }
    });
    items
}

/// `GET /oagw/v1/plugins/{id}`
///
/// # Errors
/// A canonical `Problem` when the plugin does not belong to the calling tenant
/// (404), or when the security context is missing (401).
pub async fn get_plugin(
    ctx: Option<Extension<SecurityContext>>,
    Extension(state): Extension<Arc<PluginApiState>>,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let ctx = caller(ctx)?;
    let definition = require_plugin(&state, ctx.subject_tenant_id(), id)?;
    Ok(ok_json(PluginDto::from(definition)))
}

/// `GET /oagw/v1/plugins/{id}/source`
///
/// Returns the plugin document with its Starlark source as a JSON body.
///
/// # Errors
/// A canonical `Problem` when the plugin does not belong to the calling tenant
/// (404), or when the security context is missing (401).
pub async fn get_plugin_source(
    ctx: Option<Extension<SecurityContext>>,
    Extension(state): Extension<Arc<PluginApiState>>,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let ctx = caller(ctx)?;
    let definition = require_plugin(&state, ctx.subject_tenant_id(), id)?;
    Ok(ok_json(PluginSourceDto::from(definition)))
}

/// `DELETE /oagw/v1/plugins/{id}`
///
/// Deletes an unbound plugin. A plugin still referenced by an upstream or route
/// of the calling tenant is refused with 409 and the binding list in the problem
/// context.
///
/// # Errors
/// A canonical `Problem` when the plugin does not belong to the calling tenant
/// (404), is still bound (409), or the security context is missing (401).
pub async fn delete_plugin(
    ctx: Option<Extension<SecurityContext>>,
    Extension(state): Extension<Arc<PluginApiState>>,
    Path(id): Path<Uuid>,
) -> ApiResult<impl IntoResponse> {
    let ctx = caller(ctx)?;
    let definition = require_plugin(&state, ctx.subject_tenant_id(), id)?;
    let usages = state
        .plugins
        .usages(ctx.subject_tenant_id(), id, &state.store);
    if let Some(error) = in_use(&definition.plugin_ref(), &usages) {
        return Err(error);
    }
    state.plugins.delete(ctx.subject_tenant_id(), id);
    Ok(no_content())
}
