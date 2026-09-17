//! Wire DTOs, id normalisation and the `OData` list-parameter binding.

use std::collections::HashMap;

use axum::extract::{FromRequestParts, Query};
use axum::http::request::Parts;
use serde::Deserialize;
use uuid::Uuid;

use super::error::ProblemResponse;
use crate::domain::error::DomainError;
use crate::domain::model::{Plugin, PluginType, Route, Upstream};

/// Maximum page size accepted on list endpoints.
pub const MAX_PAGE_SIZE: u64 = 100;
/// Page size used when `$top` is absent.
pub const DEFAULT_PAGE_SIZE: u64 = 50;

/// Create/replace payload for an upstream: the resource shape documented in
/// `docs/schemas/upstream.v1.schema.json`, with `alias` hoisted out of the
/// spec because it is an identity field rather than configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct UpstreamRequest {
    /// Optional explicit alias (see the alias derivation contract).
    #[serde(default)]
    pub alias: Option<String>,
    /// Configuration payload.
    #[serde(flatten)]
    pub spec: crate::domain::model::UpstreamSpec,
}

/// Create/replace payload for a route (`docs/schemas/route.v1.schema.json`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct RouteRequest {
    /// Target upstream, as a GTS identifier or a bare UUID.
    pub upstream_id: String,
    /// Configuration payload.
    #[serde(flatten)]
    pub spec: crate::domain::model::RouteSpec,
}

/// Create payload for a custom plugin.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PluginRequest {
    /// Human-readable name.
    pub name: String,
    /// Plugin kind (`auth`, `guard` or `transform`).
    #[serde(rename = "type")]
    pub kind: PluginType,
    /// Optional JSON Schema for the plugin configuration.
    #[serde(default)]
    pub config_schema: Option<serde_json::Value>,
    /// Starlark source.
    #[serde(default)]
    pub source_code: String,
}

/// `GET /plugins/{id}/source` response body.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PluginSourceResponse {
    /// Plugin identifier in GTS form.
    pub id: String,
    /// Starlark source of the plugin.
    pub source: String,
}

/// A resource id as presented on the wire: `gts.cf.core.oagw.*.v1~<uuid>` or
/// a bare UUID.
#[must_use]
pub fn parse_resource_id(raw: &str) -> Option<Uuid> {
    if let Ok(id) = Uuid::parse_str(raw) {
        return Some(id);
    }
    Uuid::parse_str(raw.rsplit_once('~')?.1).ok()
}

/// Format an id as its GTS identifier.
#[must_use]
pub fn format_resource_id(base: &str, id: Uuid) -> String {
    crate::ids::format_id(base, id)
}

/// `OData` list parameters bound off the query string (`DESIGN.md` §3.3).
///
/// Only the spellings the contract documents are accepted (`$filter`,
/// `$select`, `$orderby`, `$top`, `$skip`); unknown `$`-prefixed keys are
/// rejected so a typo is reported instead of silently ignored.
#[derive(Debug, Clone, Default)]
pub struct ListParams(pub ListParamsBody);

/// Field-level list parameters.
#[derive(Debug, Clone, Default)]
pub struct ListParamsBody {
    /// `field eq value` terms, in order.
    pub filter: Vec<(String, String)>,
    /// Fields to project, lowercased.
    pub select: Option<Vec<String>>,
    /// Order-by clauses as `(field, descending)` pairs.
    pub orderby: Vec<(String, bool)>,
    /// Page size (default 50, capped at 100).
    pub top: u64,
    /// Offset.
    pub skip: u64,
}

impl ListParams {
    /// Project and paginate a list of already-filtered items.
    ///
    /// # Errors
    ///
    /// Returns a validation error when `$select` names an unknown field.
    pub fn apply<T: serde::Serialize>(
        &self,
        items: Vec<T>,
        allowed_fields: &[&str],
    ) -> Result<Vec<serde_json::Value>, DomainError> {
        self.0.apply(items, allowed_fields)
    }
}

impl ListParamsBody {
    /// Project and paginate a list of already-filtered items.
    ///
    /// # Errors
    ///
    /// Returns a validation error when `$select` names an unknown field.
    pub fn apply<T: serde::Serialize>(
        &self,
        items: Vec<T>,
        allowed_fields: &[&str],
    ) -> Result<Vec<serde_json::Value>, DomainError> {
        if let Some(selected) = &self.select {
            for field in selected {
                if !allowed_fields.contains(&field.as_str()) {
                    return Err(DomainError::validation(format!(
                        "$select contains an unknown field: `{field}`"
                    )));
                }
            }
        }
        let mut projected: Vec<serde_json::Value> = items
            .into_iter()
            .map(|item| {
                let mut value = serde_json::to_value(&item).unwrap_or(serde_json::Value::Null);
                if let Some(selected) = &self.select {
                    let mut kept = serde_json::Map::new();
                    if let Some(object) = value.as_object_mut() {
                        for field in selected {
                            for (key, val) in object.iter() {
                                if key.eq_ignore_ascii_case(field) {
                                    kept.insert(key.clone(), val.clone());
                                }
                            }
                        }
                    }
                    value = serde_json::Value::Object(kept);
                }
                value
            })
            .collect();
        let orderby = self.orderby.clone();
        projected.sort_by(|a, b| {
            for (field, descending) in &orderby {
                let ordering = json_cmp(
                    a.get(field.as_str()).unwrap_or(&serde_json::Value::Null),
                    b.get(field.as_str()).unwrap_or(&serde_json::Value::Null),
                );
                if ordering != std::cmp::Ordering::Equal {
                    return if *descending {
                        ordering.reverse()
                    } else {
                        ordering
                    };
                }
            }
            std::cmp::Ordering::Equal
        });
        Ok(projected
            .into_iter()
            .skip(usize::try_from(self.skip).unwrap_or(usize::MAX))
            .take(usize::try_from(self.top).unwrap_or(usize::MAX))
            .collect())
    }
}

/// Total order over the JSON values used by `$orderby`.
fn json_cmp(a: &serde_json::Value, b: &serde_json::Value) -> std::cmp::Ordering {
    use serde_json::Value;
    match (a, b) {
        (Value::String(a), Value::String(b)) => a.cmp(b),
        (Value::Number(a), Value::Number(b)) => a
            .as_f64()
            .zip(b.as_f64())
            .map_or(std::cmp::Ordering::Equal, |(a, b)| {
                a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
            }),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        _ => std::cmp::Ordering::Equal,
    }
}

const ACCEPTED_QUERY_OPTIONS: [&str; 5] = ["$filter", "$select", "$orderby", "$top", "$skip"];

/// Extract [`ListParams`] from the request query string.
impl<S: Send + Sync> FromRequestParts<S> for ListParams {
    type Rejection = ProblemResponse;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        /// Lift a domain error into the extractor's rejection type.
        fn problem(error: DomainError) -> ProblemResponse {
            error.into()
        }
        let pairs: HashMap<String, String> =
            Query::<HashMap<String, String>>::try_from_uri(&parts.uri)
                .map_err(|_| problem(DomainError::validation("query string is not valid")))?
                .0;
        for key in pairs.keys() {
            if key.starts_with('$') && !ACCEPTED_QUERY_OPTIONS.contains(&key.as_str()) {
                return Err(problem(DomainError::validation(format!(
                    "unsupported OData query option: `{key}`"
                ))));
            }
        }
        Ok(Self(ListParamsBody {
            filter: pairs
                .get("$filter")
                .map(String::as_str)
                .map(parse_filter)
                .transpose()
                .map_err(problem)?
                .unwrap_or_default(),
            select: pairs
                .get("$select")
                .map(|raw| {
                    raw.split(',')
                        .map(str::trim)
                        .filter(|field| !field.is_empty())
                        .map(str::to_ascii_lowercase)
                        .collect::<Vec<_>>()
                })
                .filter(|fields| !fields.is_empty()),
            orderby: pairs
                .get("$orderby")
                .map(String::as_str)
                .map(parse_orderby)
                .transpose()
                .map_err(problem)?
                .unwrap_or_default(),
            top: match pairs.get("$top").map(String::as_str) {
                None => DEFAULT_PAGE_SIZE,
                Some(raw) => raw.parse::<u64>().map_err(|_| {
                    problem(DomainError::validation(
                        "$top must be a non-negative integer",
                    ))
                })?,
            }
            .min(MAX_PAGE_SIZE),
            skip: match pairs.get("$skip").map(String::as_str) {
                None => 0,
                Some(raw) => raw.parse::<u64>().map_err(|_| {
                    problem(DomainError::validation(
                        "$skip must be a non-negative integer",
                    ))
                })?,
            },
        }))
    }
}

/// Parse an `OData` filter into `field eq value` terms.
///
/// # Errors
///
/// Returns a validation error for anything that is not a conjunction of
/// equality terms.
pub fn parse_filter(raw: &str) -> Result<Vec<(String, String)>, DomainError> {
    let mut terms = Vec::new();
    for term in raw.split(" and ").map(str::trim).filter(|t| !t.is_empty()) {
        let Some((field, value)) = term.split_once(" eq ") else {
            return Err(DomainError::validation(format!(
                "$filter only supports `field eq value` terms joined by `and`: `{term}`"
            )));
        };
        let value = value.trim().trim_matches('\'').to_owned();
        terms.push((field.trim().to_ascii_lowercase(), value));
    }
    Ok(terms)
}

/// Parse `$orderby` into `(field, descending)` clauses.
///
/// # Errors
///
/// Returns a validation error for a malformed clause.
pub fn parse_orderby(raw: &str) -> Result<Vec<(String, bool)>, DomainError> {
    raw.split(',')
        .map(str::trim)
        .filter(|clause| !clause.is_empty())
        .map(|clause| match clause.split_once(' ') {
            Some((field, direction)) if direction.eq_ignore_ascii_case("desc") => {
                Ok((field.trim().to_ascii_lowercase(), true))
            }
            Some((field, direction)) if direction.eq_ignore_ascii_case("asc") => {
                Ok((field.trim().to_ascii_lowercase(), false))
            }
            Some((_, direction)) => Err(DomainError::validation(format!(
                "$orderby direction must be `asc` or `desc`: `{direction}`"
            ))),
            None => Ok((clause.to_ascii_lowercase(), false)),
        })
        .collect()
}

/// Apply the `alias`, `enabled`, `protocol`, `type` and `upstream_id` filter
/// terms the contract documents, by serialising each item once and matching
/// on the JSON projection.
pub fn matches_filter<T: serde::Serialize>(item: &T, filter: &[(String, String)]) -> bool {
    let value = serde_json::to_value(item).unwrap_or(serde_json::Value::Null);
    filter.iter().all(|(field, expected)| {
        let actual = field
            .split('.')
            .try_fold(&value, |acc, segment| acc.get(segment));
        match actual {
            Some(serde_json::Value::String(actual)) => actual.eq_ignore_ascii_case(expected),
            Some(serde_json::Value::Bool(actual)) => {
                *actual == expected.parse::<bool>().unwrap_or(!actual)
            }
            Some(serde_json::Value::Number(actual)) => {
                expected.parse::<i64>() == Ok(actual.as_i64().unwrap_or_default())
            }
            _ => false,
        }
    })
}

/// Visible form of an upstream on the wire.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct UpstreamDto {
    /// GTS identifier of the upstream.
    pub id: String,
    /// Owning tenant.
    pub tenant_id: String,
    /// Normalized routing key.
    pub alias: String,
    /// Whether the alias was operator-supplied.
    pub alias_explicit: bool,
    /// Creation timestamp.
    pub created_at: u64,
    /// Last modification timestamp.
    pub updated_at: u64,
    /// The rest of the resource, as submitted.
    #[serde(flatten)]
    pub spec: crate::domain::model::UpstreamSpec,
}

impl From<Upstream> for UpstreamDto {
    fn from(record: Upstream) -> Self {
        Self {
            id: format_resource_id(crate::ids::UPSTREAM_TYPE, record.id),
            tenant_id: record.tenant_id.to_string(),
            alias: record.alias,
            alias_explicit: record.alias_explicit,
            created_at: record.created_at,
            updated_at: record.updated_at,
            spec: record.spec,
        }
    }
}

/// Visible form of a route on the wire.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct RouteDto {
    /// GTS identifier of the route.
    pub id: String,
    /// Owning tenant.
    pub tenant_id: String,
    /// Target upstream, in GTS form.
    pub upstream_id: String,
    /// Creation timestamp.
    pub created_at: u64,
    /// Last modification timestamp.
    pub updated_at: u64,
    /// The rest of the resource, as submitted.
    #[serde(flatten)]
    pub spec: crate::domain::model::RouteSpec,
}

impl From<Route> for RouteDto {
    fn from(record: Route) -> Self {
        Self {
            id: format_resource_id(crate::ids::ROUTE_TYPE, record.id),
            tenant_id: record.tenant_id.to_string(),
            upstream_id: format_resource_id(crate::ids::UPSTREAM_TYPE, record.upstream_id),
            created_at: record.created_at,
            updated_at: record.updated_at,
            spec: record.spec,
        }
    }
}

/// Visible form of a plugin on the wire.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PluginDto {
    /// GTS identifier of the plugin.
    pub id: String,
    /// Owning tenant.
    pub tenant_id: String,
    /// Human-readable name.
    pub name: String,
    /// Plugin kind.
    #[serde(rename = "type")]
    pub kind: PluginType,
    /// Optional JSON Schema for the plugin configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    /// Creation timestamp.
    pub created_at: u64,
    /// Last modification timestamp.
    pub updated_at: u64,
}

impl From<Plugin> for PluginDto {
    fn from(record: Plugin) -> Self {
        Self {
            id: format_resource_id(plugin_base(record.kind), record.id),
            tenant_id: record.tenant_id.to_string(),
            name: record.name,
            kind: record.kind,
            config_schema: record.config_schema,
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

/// GTS type id base for a plugin kind.
#[must_use]
pub fn plugin_base(kind: PluginType) -> &'static str {
    kind.base_type()
}
