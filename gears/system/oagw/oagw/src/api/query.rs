//! OData-style list query handling shared by the three list endpoints
//! (`DESIGN.md` §"List Query Parameters").
//!
//! `$top`, `$skip`, `$orderby`, `$select`, and `$filter` are bound here rather
//! than through the platform's `OData` extractor, whose paging model is cursor
//! based: the OAGW surface documents offset paging (`$top` / `$skip`) and a
//! `field eq value` filter, so the binding lives with the gear that specifies
//! it.
//!
//! Everything is resolved against a resource's own top-level fields, read off
//! the model's serialized shape, so the accepted names cannot drift from what
//! the endpoints return: `$select` may name any top-level field, while
//! `$orderby` and `$filter` are restricted to the scalar ones — sorting or
//! comparing an object member is meaningless.
//!
//! A malformed value is a 400 problem naming the offending parameter, never a
//! silent default: a caller whose `$top=abc` is answered with the default page
//! would otherwise read page one as the whole collection. Query keys other
//! than the five documented ones are left alone here, and a repeated option is
//! resolved to the value the caller sent last.

use std::cmp::Ordering;
use std::sync::OnceLock;

use axum::http::Uri;
use serde::Serialize;
use serde_json::Value;

use crate::domain::error::{ErrorKind, OagwError};
use crate::domain::model::{CustomPlugin, Route, Upstream};

/// Page size assumed when a list request asks for none.
pub const DEFAULT_TOP: usize = 50;
/// Largest page size a caller may ask for.
pub const MAX_TOP: usize = 100;

/// The top-level fields one resource serializes, split into the ones that may
/// be selected and the (smaller) scalar subset that may be ordered or filtered.
#[derive(Debug, Clone, Default)]
pub struct ListFields {
    /// Every top-level member the resource serializes.
    all: Vec<String>,
    /// The members holding a string, number, or boolean.
    scalar: Vec<String>,
}

impl ListFields {
    /// Read the field list off a default instance of the model.
    ///
    /// The serialized shape is the contract, so it — not a second hand-written
    /// list — is what the query parameters are validated against.
    #[must_use]
    pub fn of<T: Serialize>(model: &T) -> Self {
        let shape = serde_json::to_value(model)
            .unwrap_or_else(|_| Value::Object(serde_json::Map::default()));
        let Some(map) = shape.as_object() else {
            return Self::default();
        };
        let all: Vec<String> = map.keys().cloned().collect();
        let scalar: Vec<String> = all
            .iter()
            .filter(|field| map.get(field.as_str()).is_some_and(is_scalar))
            .cloned()
            .collect();
        Self { all, scalar }
    }

    /// Whether `field` is one of the resource's top-level members.
    #[must_use]
    pub fn is_field(&self, field: &str) -> bool {
        self.all.iter().any(|known| known == field)
    }

    /// Whether `field` is a top-level scalar, i.e. orderable and filterable.
    #[must_use]
    pub fn is_scalar(&self, field: &str) -> bool {
        self.scalar.iter().any(|known| known == field)
    }

    /// The top-level fields, in serialization order.
    #[must_use]
    pub fn all(&self) -> &[String] {
        &self.all
    }
}

/// Whether a serialized value holds a string, number, or boolean.
fn is_scalar(value: &Value) -> bool {
    matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_))
}

/// The fields of an [`Upstream`] a list query may address.
#[must_use]
pub fn upstream_fields() -> &'static ListFields {
    static FIELDS: OnceLock<ListFields> = OnceLock::new();
    FIELDS.get_or_init(|| ListFields::of(&Upstream::default()))
}

/// The fields of a [`Route`] a list query may address.
#[must_use]
pub fn route_fields() -> &'static ListFields {
    static FIELDS: OnceLock<ListFields> = OnceLock::new();
    FIELDS.get_or_init(|| ListFields::of(&Route::default()))
}

/// The fields of a [`CustomPlugin`] a list query may address.
#[must_use]
pub fn plugin_fields() -> &'static ListFields {
    static FIELDS: OnceLock<ListFields> = OnceLock::new();
    FIELDS.get_or_init(|| ListFields::of(&CustomPlugin::default()))
}

/// One list request's parsed query options.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// `field eq value` a row must satisfy.
    filter: Option<(String, String)>,
    /// Field to sort by, and whether the order is descending.
    order: Option<(String, bool)>,
    /// Top-level fields a row is projected down to.
    select: Vec<String>,
    /// Maximum number of rows returned.
    top: usize,
    /// Rows discarded before the page is cut.
    skip: usize,
}

/// A 400 problem naming the query parameter that carried the bad value.
fn bad_param(name: &str, detail: impl Into<String>) -> OagwError {
    OagwError::new(ErrorKind::Validation, detail).with_context("field", serde_json::json!(name))
}

/// The problem for a parameter naming a field the resource does not expose.
fn unknown_field(parameter: &str, field: &str, fields: &ListFields) -> OagwError {
    bad_param(
        parameter,
        format!(
            "unknown field '{field}' in {parameter}; the resource exposes {}",
            fields.all().join(", ")
        ),
    )
}

impl ListQuery {
    /// Parse the query options of `uri` against the fields `fields` advertises.
    ///
    /// # Errors
    ///
    /// Returns a 400 problem for a non-integer or oversized `$top`, a negative
    /// or non-integer `$skip`, an unknown or multi-clause `$orderby`, an
    /// unknown `$select` field, and an unparseable `$filter` expression.
    pub fn parse(uri: &Uri, fields: &ListFields) -> Result<Self, OagwError> {
        // The page size starts at the documented default, so an absent `$top`
        // is a 50-row page rather than an empty one.
        let mut query = Self {
            top: DEFAULT_TOP,
            ..Self::default()
        };
        for (key, value) in pairs(uri) {
            match key.as_str() {
                "$top" => query.top = parse_page_size(&value)?,
                "$skip" => query.skip = parse_offset(&value)?,
                "$orderby" => query.order = parse_orderby(&value, fields)?,
                "$select" => query.select = parse_select(&value, fields)?,
                "$filter" => query.filter = parse_filter(&value, fields)?,
                // A key the OAGW list endpoints do not document is not theirs
                // to police: a caller may address the transport's own
                // parameters, and an unknown `$`-prefixed option belongs to the
                // platform's query conventions.
                _ => {}
            }
        }
        Ok(query)
    }

    /// Filter, order, page, and project `rows`.
    ///
    /// The order is fixed — filter, then sort, then cut the page, then project
    /// — so a page stays stable regardless of what a caller projected away.
    #[must_use]
    pub fn apply(&self, rows: Vec<Value>, fields: &ListFields) -> Vec<Value> {
        let selected: Vec<Value> = rows
            .into_iter()
            .filter(|row| {
                self.filter
                    .as_ref()
                    .is_none_or(|(field, literal)| row_matches(row, field, literal))
            })
            .collect();
        let ordered = match &self.order {
            None => selected,
            Some((field, descending)) => {
                let mut ordered = selected;
                ordered.sort_by(|left, right| {
                    let ordering = compare_values(field_of(left, field), field_of(right, field));
                    if *descending {
                        ordering.reverse()
                    } else {
                        ordering
                    }
                });
                ordered
            }
        };
        ordered
            .into_iter()
            .skip(self.skip)
            .take(self.top)
            .map(|row| project(&row, &self.select, fields))
            .collect()
    }
}

/// The decoded query pairs of `uri`, in wire order.
fn pairs(uri: &Uri) -> Vec<(String, String)> {
    uri.query()
        .map(|raw| {
            form_urlencoded::parse(raw.as_bytes())
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a non-negative integer query value.
fn parse_offset(raw: &str) -> Result<usize, OagwError> {
    raw.trim().parse::<usize>().map_err(|_| {
        bad_param(
            "$skip",
            format!("$skip '{raw}' is not a non-negative integer"),
        )
    })
}

/// Parse a page size: a non-negative integer of at most [`MAX_TOP`].
fn parse_page_size(raw: &str) -> Result<usize, OagwError> {
    let value = raw.trim().parse::<usize>().map_err(|_| {
        bad_param(
            "$top",
            format!("$top '{raw}' is not a non-negative integer"),
        )
    })?;
    if value > MAX_TOP {
        // The documented ceiling: a caller asking for more is told so rather
        // than being served a page the endpoint will not produce.
        return Err(bad_param("$top", format!("$top must not exceed {MAX_TOP}")));
    }
    Ok(value)
}

/// Parse `$orderby`: one top-level scalar field, optionally ` asc`/` desc`.
fn parse_orderby(raw: &str, fields: &ListFields) -> Result<Option<(String, bool)>, OagwError> {
    let clause = raw.trim();
    if clause.is_empty() {
        return Ok(None);
    }
    let mut tokens = clause.split_whitespace();
    let Some(field) = tokens.next() else {
        return Ok(None);
    };
    let direction = tokens.collect::<Vec<_>>();
    let descending = match direction.as_slice() {
        [] | ["asc" | "ASC"] => false,
        ["desc" | "DESC"] => true,
        _ => {
            return Err(bad_param(
                "$orderby",
                format!(
                    "$orderby '{clause}' must be a single field, optionally followed by 'asc' or 'desc'"
                ),
            ));
        }
    };
    if !fields.is_scalar(field) {
        return Err(unknown_field("$orderby", field, fields));
    }
    Ok(Some((field.to_owned(), descending)))
}

/// Parse `$select`: a comma-separated list of top-level fields.
fn parse_select(raw: &str, fields: &ListFields) -> Result<Vec<String>, OagwError> {
    let mut selected: Vec<String> = Vec::new();
    for name in raw.split(',') {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        if !fields.is_field(name) {
            return Err(unknown_field("$select", name, fields));
        }
        if !selected.iter().any(|known| known == name) {
            selected.push(name.to_owned());
        }
    }
    Ok(selected)
}

/// Parse `$filter`: a single `field eq value` comparison.
fn parse_filter(raw: &str, fields: &ListFields) -> Result<Option<(String, String)>, OagwError> {
    let expression = raw.trim();
    if expression.is_empty() {
        return Ok(None);
    }
    let malformed = || {
        bad_param(
            "$filter",
            format!("$filter '{expression}' must be a 'field eq value' comparison"),
        )
    };
    let Some((field, rest)) = expression.split_once(char::is_whitespace) else {
        return Err(malformed());
    };
    let field = field.trim();
    // The comparison operator is delimited from its value by whitespace:
    // `alias eq 'a b'` keeps its value whole, and `alias eqal x` is a malformed
    // expression rather than an `eq` with a stray tail.
    let Some(comparison) = rest.trim_start().strip_prefix("eq") else {
        return Err(bad_param(
            "$filter",
            format!("$filter '{expression}' supports only the 'eq' comparison"),
        ));
    };
    if !comparison.is_empty() && !comparison.starts_with(char::is_whitespace) {
        return Err(malformed());
    }
    let literal = unquote(comparison.trim());
    if field.is_empty() || literal.is_empty() {
        return Err(malformed());
    }
    if !fields.is_scalar(field) {
        return Err(unknown_field("$filter", field, fields));
    }
    Ok(Some((field.to_owned(), literal.to_owned())))
}

/// Strip one pair of surrounding quotes from a filter literal.
fn unquote(literal: &str) -> &str {
    for quote in ['\'', '"'] {
        if let Some(unquoted) = literal.strip_prefix(quote)
            && let Some(inner) = unquoted.strip_suffix(quote)
        {
            return inner;
        }
    }
    literal
}

/// Whether `row`'s `field` equals the filter's `literal`.
fn row_matches(row: &Value, field: &str, literal: &str) -> bool {
    match field_of(row, field) {
        Value::String(value) => value == literal,
        Value::Bool(value) => value.to_string() == literal.to_ascii_lowercase(),
        Value::Number(value) => value.to_string() == literal,
        _ => false,
    }
}

/// A row's top-level field, or `null` when the row does not carry it.
fn field_of<'a>(row: &'a Value, field: &str) -> &'a Value {
    row.get(field).unwrap_or(&Value::Null)
}

/// Total order over scalar field values — type-homogeneous in practice, but
/// never a partial-comparison panic on a mixed column: nulls first, then
/// booleans, numbers, and strings.
fn compare_values(left: &Value, right: &Value) -> Ordering {
    let rank = |value: &Value| match value {
        Value::Null => 0u8,
        Value::Bool(_) => 1,
        Value::Number(_) => 2,
        Value::String(_) => 3,
        _ => 4,
    };
    rank(left)
        .cmp(&rank(right))
        .then_with(|| match (left, right) {
            (Value::Bool(left), Value::Bool(right)) => left.cmp(right),
            (Value::Number(left), Value::Number(right)) => left
                .as_f64()
                .partial_cmp(&right.as_f64())
                .unwrap_or(Ordering::Equal),
            (Value::String(left), Value::String(right)) => left.cmp(right),
            _ => Ordering::Equal,
        })
}

/// Reduce a row to the fields `$select` named, keeping the requested order.
///
/// An unprojected row is returned whole.
fn project(row: &Value, select: &[String], fields: &ListFields) -> Value {
    if select.is_empty() {
        return row.clone();
    }
    let mut projected = serde_json::Map::new();
    for field in select {
        if fields.is_field(field)
            && let Some(value) = row.get(field)
        {
            projected.insert(field.clone(), value.clone());
        }
    }
    Value::Object(projected)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn uri(query: &str) -> Uri {
        Uri::builder()
            .path_and_query(format!("/oagw/v1/upstreams{query}"))
            .build()
            .expect("uri builds")
    }

    /// The query string carrying `name=value`, URL-encoded.
    fn query_pair(name: &str, value: &str) -> String {
        format!(
            "?{}",
            form_urlencoded::Serializer::new(String::new())
                .append_pair(name, value)
                .finish()
        )
    }

    fn row(alias: &str, enabled: bool, priority: i64) -> Value {
        serde_json::json!({ "alias": alias, "enabled": enabled, "priority": priority })
    }

    #[test]
    fn defaults_apply_when_no_option_is_sent() {
        let query = ListQuery::parse(&uri(""), upstream_fields()).expect("parses");
        assert_eq!(query.top, DEFAULT_TOP);
        assert_eq!(query.skip, 0);
        let rows = (0..DEFAULT_TOP + 5)
            .map(|index| {
                row(
                    &format!("a{index}"),
                    true,
                    i64::try_from(index).unwrap_or(0),
                )
            })
            .collect();
        assert_eq!(query.apply(rows, upstream_fields()).len(), DEFAULT_TOP);
    }

    #[test]
    fn pages_are_cut_from_the_requested_offset() {
        let query = ListQuery::parse(&uri("?$top=2&$skip=1"), upstream_fields()).expect("parses");
        let rows = vec![
            row("a", true, 0),
            row("b", true, 1),
            row("c", true, 2),
            row("d", true, 3),
        ];
        let page = query.apply(rows, upstream_fields());
        let aliases: Vec<&str> = page
            .iter()
            .map(|entry| entry["alias"].as_str().expect("alias"))
            .collect();
        assert_eq!(aliases, vec!["b", "c"]);
    }

    #[test]
    fn a_page_larger_than_the_ceiling_is_refused() {
        let error = ListQuery::parse(&uri("?$top=101"), upstream_fields()).expect_err("refused");
        assert_eq!(error.kind(), ErrorKind::Validation);
        assert!(
            error.detail().contains("$top"),
            "the problem names the offending parameter: {}",
            error.detail()
        );
    }

    #[test]
    fn a_non_integer_page_size_is_refused() {
        for raw in ["many", "-1", "1.5"] {
            let error =
                ListQuery::parse(&uri(&format!("?$top={raw}")), upstream_fields()).expect_err(raw);
            assert_eq!(error.kind(), ErrorKind::Validation, "{raw}");
        }
    }

    #[test]
    fn a_negative_skip_is_refused() {
        let error = ListQuery::parse(&uri("?$skip=-3"), upstream_fields()).expect_err("refused");
        assert_eq!(error.kind(), ErrorKind::Validation);
    }

    #[test]
    fn ordering_follows_the_requested_field_and_direction() {
        let rows = vec![
            row("delta", true, 1),
            row("alpha", true, 3),
            row("charlie", false, 2),
        ];
        let asc = ListQuery::parse(&uri("?$orderby=alias"), upstream_fields())
            .expect("parses")
            .apply(rows.clone(), upstream_fields());
        let names: Vec<&str> = asc
            .iter()
            .map(|entry| entry["alias"].as_str().expect("alias"))
            .collect();
        assert_eq!(names, vec!["alpha", "charlie", "delta"]);

        let desc = ListQuery::parse(&uri("?$orderby=enabled%20desc"), upstream_fields())
            .expect("parses")
            .apply(rows, upstream_fields());
        let enabled: Vec<bool> = desc
            .iter()
            .map(|entry| entry["enabled"].as_bool().expect("enabled"))
            .collect();
        assert_eq!(enabled, vec![true, true, false]);
    }

    #[test]
    fn a_route_orders_by_its_numeric_priority() {
        let fields = route_fields();
        let rows = vec![
            serde_json::json!({ "alias": "delta", "priority": 1 }),
            serde_json::json!({ "alias": "alpha", "priority": 3 }),
            serde_json::json!({ "alias": "charlie", "priority": 2 }),
        ];
        let ordered = ListQuery::parse(&uri(&query_pair("$orderby", "priority desc")), fields)
            .expect("parses")
            .apply(rows, fields);
        let priorities: Vec<i64> = ordered
            .iter()
            .map(|entry| entry["priority"].as_i64().expect("priority"))
            .collect();
        assert_eq!(priorities, vec![3, 2, 1]);
    }

    #[test]
    fn an_unknown_or_ambiguous_order_clause_is_refused() {
        for raw in ["nonexistent", "alias desc extra", "server"] {
            let error = ListQuery::parse(&uri(&query_pair("$orderby", raw)), upstream_fields())
                .expect_err(raw);
            assert_eq!(error.kind(), ErrorKind::Validation, "{raw}");
        }
    }

    #[test]
    fn filters_compare_the_field_value() {
        let rows = vec![row("alpha", true, 1), row("beta", false, 2)];
        let filtered = ListQuery::parse(
            &uri(&query_pair("$filter", "alias eq 'beta'")),
            upstream_fields(),
        )
        .expect("parses")
        .apply(rows, upstream_fields());
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["alias"], serde_json::json!("beta"));
    }

    #[test]
    fn an_unparseable_filter_is_refused() {
        for raw in [
            "alias",
            "alias ne 'beta'",
            "eq 'beta'",
            "alias eq",
            "alias eqal x",
        ] {
            let error = ListQuery::parse(&uri(&query_pair("$filter", raw)), upstream_fields())
                .expect_err(raw);
            assert_eq!(error.kind(), ErrorKind::Validation, "{raw}");
        }
    }

    #[test]
    fn projection_keeps_only_the_requested_fields() {
        let query =
            ListQuery::parse(&uri("?$select=alias,enabled"), upstream_fields()).expect("parses");
        let projected = query.apply(vec![row("alpha", true, 7)], upstream_fields());
        assert_eq!(
            projected[0],
            serde_json::json!({ "alias": "alpha", "enabled": true }),
            "the unselected member is dropped"
        );
    }

    #[test]
    fn an_unknown_select_field_is_refused() {
        let error = ListQuery::parse(&uri("?$select=alias,secret"), upstream_fields())
            .expect_err("refused");
        assert_eq!(error.kind(), ErrorKind::Validation);
    }

    #[test]
    fn the_field_lists_come_from_the_serialized_models() {
        // The `match` member is renamed on the wire and `position` is never
        // serialized, so a route's field set is the wire shape, not the
        // struct's.
        assert!(route_fields().is_field("match"));
        assert!(!route_fields().is_field("position"));
        assert!(route_fields().is_scalar("priority"));
        assert!(!route_fields().is_scalar("match"));
        assert!(upstream_fields().is_scalar("alias"));
        assert!(plugin_fields().is_scalar("name"));
        assert!(plugin_fields().is_field("config_schema"));
        assert!(!plugin_fields().is_scalar("config_schema"));
    }
}
