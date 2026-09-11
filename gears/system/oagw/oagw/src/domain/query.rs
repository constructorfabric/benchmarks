//! `GET /oagw/v1/upstreams` and `GET /oagw/v1/routes` list query parameters
//! (`cpt-cf-oagw-dod-list-upstreams-endpoint`,
//! `cpt-cf-oagw-dod-route-list-query-params`).
//!
//! Supports the documented `$filter`, `$select`, `$orderby`, `$top`, and
//! `$skip` parameters (DESIGN.md's "List Query Parameters" table). This is a
//! narrow, purpose-built parser rather than a general `OData` engine: `$filter`
//! accepts a single `field eq value` clause and `$orderby` a single
//! `field [asc|desc]` clause, each validated against the calling entity's
//! declared field set ([`UPSTREAM_ALLOWED_FIELDS`] or
//! [`ROUTE_ALLOWED_FIELDS`]), which is enough to satisfy this feature's
//! flows (`cpt-cf-oagw-flow-list-upstreams`, `cpt-cf-oagw-flow-list-routes`)
//! without duplicating a full `OData` implementation. [`apply`] is generic
//! over any `Serialize` entity so both upstream and route listing reuse the
//! same filter/order/paginate/select pipeline.
//!
//! See `crate::domain::service`'s module doc for why
//! `clippy::result_large_err` is allowed here: `OagwError` is returned
//! unboxed everywhere in this crate, including the handler layer.
#![allow(clippy::result_large_err)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::OagwError;

/// Every field name a `$filter`, `$select`, or `$orderby` clause may
/// reference on `GET /oagw/v1/upstreams`.
pub const UPSTREAM_ALLOWED_FIELDS: &[&str] = &[
    "id",
    "enabled",
    "alias",
    "tags",
    "server",
    "protocol",
    "auth",
    "headers",
    "plugins",
    "rate_limit",
    "cors",
];

/// Every field name a `$filter`, `$select`, or `$orderby` clause may
/// reference on `GET /oagw/v1/routes`
/// (`cpt-cf-oagw-dod-route-list-query-params`).
pub const ROUTE_ALLOWED_FIELDS: &[&str] = &[
    "id",
    "upstream_id",
    "tags",
    "match",
    "plugins",
    "rate_limit",
    "enabled",
    "priority",
];

/// Every field name a `$filter`, `$select`, or `$orderby` clause may
/// reference on `GET /oagw/v1/plugins` (`cpt-cf-oagw-dod-plugin-list`).
pub const PLUGIN_ALLOWED_FIELDS: &[&str] =
    &["id", "plugin_type", "name", "config_schema", "phases"];

/// Default page size when `$top` is omitted.
const DEFAULT_TOP: u64 = 50;

/// Maximum accepted `$top` value.
const MAX_TOP: u64 = 100;

/// The raw, wire-level query-string bindings.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct RawListQuery {
    #[serde(rename = "$filter")]
    pub filter: Option<String>,
    #[serde(rename = "$select")]
    pub select: Option<String>,
    #[serde(rename = "$orderby")]
    pub orderby: Option<String>,
    #[serde(rename = "$top")]
    pub top: Option<u64>,
    #[serde(rename = "$skip")]
    pub skip: Option<u64>,
}

/// A validated `field eq value` filter clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterClause {
    pub field: String,
    pub value: String,
}

/// A validated `field [asc|desc]` order clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderClause {
    pub field: String,
    pub descending: bool,
}

/// A validated, defaulted list-query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListQuery {
    pub filter: Option<FilterClause>,
    pub select: Option<Vec<String>>,
    pub orderby: Option<OrderClause>,
    pub top: u64,
    pub skip: u64,
}

/// Validates a raw list-query against `allowed_fields`, defaulting `$top`
/// to 50 and `$skip` to 0.
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] when `$filter`/`$select`/
/// `$orderby` reference a field outside `allowed_fields`, `$filter`'s
/// operator is not `eq`, or `$top` exceeds 100.
pub fn validate(raw: &RawListQuery, allowed_fields: &[&str]) -> Result<ListQuery, OagwError> {
    let filter = raw
        .filter
        .as_deref()
        .map(|clause| parse_filter(clause, allowed_fields))
        .transpose()?;
    let select = raw
        .select
        .as_deref()
        .map(|clause| parse_select(clause, allowed_fields))
        .transpose()?;
    let orderby = raw
        .orderby
        .as_deref()
        .map(|clause| parse_orderby(clause, allowed_fields))
        .transpose()?;
    let top = raw.top.unwrap_or(DEFAULT_TOP);
    if top > MAX_TOP {
        return Err(OagwError::validation_error(format!(
            "$top: must not exceed {MAX_TOP}"
        )));
    }
    Ok(ListQuery {
        filter,
        select,
        orderby,
        top,
        skip: raw.skip.unwrap_or(0),
    })
}

fn known_field(allowed_fields: &[&str], field: &str) -> bool {
    allowed_fields.contains(&field)
}

fn parse_filter(raw: &str, allowed_fields: &[&str]) -> Result<FilterClause, OagwError> {
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    let [field, op, value] = tokens.as_slice() else {
        return Err(OagwError::validation_error(
            "$filter: expected a single 'field eq value' clause".to_owned(),
        ));
    };
    if *op != "eq" {
        return Err(OagwError::validation_error(format!(
            "$filter: unsupported operator '{op}'"
        )));
    }
    if !known_field(allowed_fields, field) {
        return Err(OagwError::validation_error(format!(
            "$filter: unknown field '{field}'"
        )));
    }
    let value = value.trim_matches('\'').to_owned();
    Ok(FilterClause {
        field: (*field).to_owned(),
        value,
    })
}

fn parse_select(raw: &str, allowed_fields: &[&str]) -> Result<Vec<String>, OagwError> {
    let fields: Vec<String> = raw.split(',').map(str::trim).map(str::to_owned).collect();
    for field in &fields {
        if !known_field(allowed_fields, field) {
            return Err(OagwError::validation_error(format!(
                "$select: unknown field '{field}'"
            )));
        }
    }
    Ok(fields)
}

fn parse_orderby(raw: &str, allowed_fields: &[&str]) -> Result<OrderClause, OagwError> {
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    let (field, descending) = match tokens.as_slice() {
        [field] | [field, "asc"] => (*field, false),
        [field, "desc"] => (*field, true),
        _ => {
            return Err(OagwError::validation_error(
                "$orderby: expected 'field' or 'field asc|desc'".to_owned(),
            ));
        }
    };
    if !known_field(allowed_fields, field) {
        return Err(OagwError::validation_error(format!(
            "$orderby: unknown field '{field}'"
        )));
    }
    Ok(OrderClause {
        field: field.to_owned(),
        descending,
    })
}

/// Applies `filter`, `orderby`, `skip`/`top`, and `select` (in that order)
/// to `items`, returning each surviving record as a JSON value (projected
/// down to the selected fields, when `$select` was given). Generic over any
/// `Serialize` entity so both `Upstream` and `Route` listing reuse this one
/// pipeline.
#[must_use]
pub fn apply<T: Serialize>(query: &ListQuery, items: Vec<T>) -> Vec<Value> {
    let mut values: Vec<Value> = items
        .into_iter()
        .map(|item| serde_json::to_value(item).unwrap_or(Value::Null))
        .collect();

    if let Some(filter) = &query.filter {
        values.retain(|value| field_matches(value, &filter.field, &filter.value));
    }
    if let Some(order) = &query.orderby {
        values.sort_by(|a, b| compare_field(a, b, &order.field));
        if order.descending {
            values.reverse();
        }
    }

    let skip = usize::try_from(query.skip).unwrap_or(usize::MAX);
    let top = usize::try_from(query.top).unwrap_or(usize::MAX);
    let page: Vec<Value> = values.into_iter().skip(skip).take(top).collect();

    match &query.select {
        Some(fields) => page.iter().map(|value| project(value, fields)).collect(),
        None => page,
    }
}

fn field_matches(value: &Value, field: &str, target: &str) -> bool {
    match value.get(field) {
        Some(Value::String(s)) => s == target,
        Some(Value::Bool(b)) => target.parse::<bool>().is_ok_and(|t| *b == t),
        // A non-string, non-bool value (number, array, object, null) has no
        // borrowed textual form to compare against `target` without
        // allocating; this is not a hot path.
        #[allow(clippy::cmp_owned)]
        Some(other) => other.to_string() == target,
        None => false,
    }
}

/// Orders two entities by `field`, comparing by the field's own JSON type
/// rather than its string form (`BUG1-F-005`): numbers compare numerically,
/// booleans by `false < true`, and strings lexically. A `$orderby` clause
/// comparing `priority: 10` against `priority: 2` as text would otherwise
/// put `10` before `2`. Any other JSON shape (array, object, or a
/// missing/`null` field on one side) falls back to the string form, which
/// is the closest total order available for those cases.
fn compare_field(a: &Value, b: &Value, field: &str) -> std::cmp::Ordering {
    compare_values(a.get(field), b.get(field))
}

fn compare_values(a: Option<&Value>, b: Option<&Value>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(Value::Number(a_num)), Some(Value::Number(b_num))) => compare_numbers(a_num, b_num),
        (Some(Value::Bool(a_bool)), Some(Value::Bool(b_bool))) => a_bool.cmp(b_bool),
        (Some(Value::String(a_str)), Some(Value::String(b_str))) => a_str.cmp(b_str),
        _ => {
            let a_key = a.map(Value::to_string).unwrap_or_default();
            let b_key = b.map(Value::to_string).unwrap_or_default();
            a_key.cmp(&b_key)
        }
    }
}

fn compare_numbers(a: &serde_json::Number, b: &serde_json::Number) -> std::cmp::Ordering {
    if let (Some(a_int), Some(b_int)) = (a.as_i64(), b.as_i64()) {
        return a_int.cmp(&b_int);
    }
    let a_float = a.as_f64().unwrap_or(0.0);
    let b_float = b.as_f64().unwrap_or(0.0);
    a_float.total_cmp(&b_float)
}

fn project(value: &Value, fields: &[String]) -> Value {
    let Some(object) = value.as_object() else {
        return value.clone();
    };
    let mut projected = serde_json::Map::new();
    for field in fields {
        if let Some(field_value) = object.get(field) {
            projected.insert(field.clone(), field_value.clone());
        }
    }
    Value::Object(projected)
}

#[cfg(test)]
mod tests {
    use super::{RawListQuery, UPSTREAM_ALLOWED_FIELDS, apply, validate};
    use crate::domain::model::{Endpoint, Protocol, Scheme, ServerConfig, Upstream};
    use uuid::Uuid;

    fn sample_upstream(alias: &str) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            enabled: true,
            alias: alias.to_owned(),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Https,
                    host: alias.to_owned(),
                    port: Some(443),
                }],
            },
            protocol: Protocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    #[test]
    fn defaults_top_to_50_and_skip_to_0() {
        let query = validate(&RawListQuery::default(), UPSTREAM_ALLOWED_FIELDS)
            .expect("defaults must validate");
        assert_eq!(query.top, 50);
        assert_eq!(query.skip, 0);
    }

    #[test]
    fn rejects_top_above_100() {
        let raw = RawListQuery {
            top: Some(101),
            ..RawListQuery::default()
        };
        assert!(validate(&raw, UPSTREAM_ALLOWED_FIELDS).is_err());
    }

    #[test]
    fn rejects_an_unknown_filter_field() {
        let raw = RawListQuery {
            filter: Some("bogus eq 'x'".to_owned()),
            ..RawListQuery::default()
        };
        assert!(validate(&raw, UPSTREAM_ALLOWED_FIELDS).is_err());
    }

    #[test]
    fn rejects_an_unknown_select_field() {
        let raw = RawListQuery {
            select: Some("bogus".to_owned()),
            ..RawListQuery::default()
        };
        assert!(validate(&raw, UPSTREAM_ALLOWED_FIELDS).is_err());
    }

    #[test]
    fn top_and_skip_bound_the_returned_page() {
        let query = validate(
            &RawListQuery {
                top: Some(1),
                ..RawListQuery::default()
            },
            UPSTREAM_ALLOWED_FIELDS,
        )
        .expect("must validate");
        let upstreams = vec![
            sample_upstream("a.example.com"),
            sample_upstream("b.example.com"),
        ];
        let page = apply(&query, upstreams);
        assert_eq!(page.len(), 1);
    }

    // BUG1-F-005: `$orderby` must sort numeric fields numerically, not as
    // their string form (which would put `10` before `2`).
    #[test]
    fn orderby_sorts_priority_numerically_not_lexically() {
        use super::ROUTE_ALLOWED_FIELDS;
        use serde_json::json;

        let query = validate(
            &RawListQuery {
                orderby: Some("priority".to_owned()),
                ..RawListQuery::default()
            },
            ROUTE_ALLOWED_FIELDS,
        )
        .expect("must validate");
        let items = vec![json!({"priority": 10}), json!({"priority": 2})];
        let page = apply(&query, items);
        assert_eq!(page[0]["priority"], json!(2));
        assert_eq!(page[1]["priority"], json!(10));
    }

    #[test]
    fn filter_by_alias_narrows_the_page() {
        let query = validate(
            &RawListQuery {
                filter: Some("alias eq 'b.example.com'".to_owned()),
                ..RawListQuery::default()
            },
            UPSTREAM_ALLOWED_FIELDS,
        )
        .expect("must validate");
        let upstreams = vec![
            sample_upstream("a.example.com"),
            sample_upstream("b.example.com"),
        ];
        let page = apply(&query, upstreams);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0]["alias"], "b.example.com");
    }
}
