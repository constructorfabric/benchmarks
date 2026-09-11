//! The OData subset the three list endpoints accept
//! (`cpt-cf-oagw-algo-list-query`).
//!
//! The subset is closed: five parameters, one `eq` comparison on the one field
//! DESIGN names for the resource, exactly one `$orderby` field with an
//! `asc`/`desc` direction, and a `$select` projection over the aggregate field
//! set of the resource. Anything else is a 400 validation error, so list
//! results stay deterministic and stay inside what
//! `cpt-cf-oagw-algo-inmemory-repository` can order and project over.

use std::cmp::Ordering;

use form_urlencoded::parse as form_parse;
use serde_json::Value;

use crate::domain::error::{DomainError, Violation, ViolationKind, Violations};

/// The five parameters the subset declares, and no other.
pub const LIST_PARAMETERS: &[&str] = &["$filter", "$select", "$orderby", "$top", "$skip"];

/// The `$top` a request that omits the parameter reads as.
pub const TOP_DEFAULT: usize = 50;
/// The ceiling `$top` is clamped to.
pub const TOP_MAX: usize = 100;
/// The `$skip` a request that omits the parameter reads as.
pub const SKIP_DEFAULT: usize = 0;

/// One parsed list query, ready to be applied to a page of serialized
/// resources.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ListQuery {
    /// The filter field and value, when the request carries `$filter`.
    pub filter: Option<(String, String)>,
    /// The `$orderby` field and whether it sorts ascending, defaulted to `asc`.
    pub order_by: Option<(String, bool)>,
    /// The `$select` field set, in the order the request names the fields.
    pub select: Vec<String>,
    /// The clamped page size.
    pub top: usize,
    /// The offset the page starts at.
    pub skip: usize,
}

/// The field universe a list query is resolved over: the one filter field the
/// resource answers and the aggregate field set the other parameters resolve
/// over (`cpt-cf-oagw-algo-list-query`, §1.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Universe {
    /// The `$filter` field the resource answers, e.g. `alias`.
    pub filter_field: &'static str,
    /// The aggregate field that field addresses, e.g. `plugin_type` for the
    /// plugin filter field `type`.
    pub filter_target: &'static str,
    /// The aggregate field set `$orderby` and `$select` resolve over.
    pub fields: &'static [&'static str],
}

impl Universe {
    /// The universe of a resource, per §1.5.
    #[must_use]
    pub fn of(resource: super::dto::Resource) -> Self {
        use super::dto::Resource;
        match resource {
            Resource::Upstream => Self {
                filter_field: "alias",
                filter_target: "alias",
                fields: resource.aggregate_fields(),
            },
            Resource::Route => Self {
                filter_field: "upstream_id",
                filter_target: "upstream_id",
                fields: resource.aggregate_fields(),
            },
            Resource::Plugin => Self {
                filter_field: "type",
                filter_target: "plugin_type",
                fields: resource.aggregate_fields(),
            },
        }
    }
}

/// Parses the query string of a list endpoint
/// (`cpt-cf-oagw-algo-list-query`, `inst-lq-01` to `inst-lq-07`).
///
/// The parsed query carries the defaults of §1.5 for every parameter the
/// request omits.
///
/// # Errors
/// Returns the collected query violations, to be rendered as 400 with GTS type
/// `gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1`.
pub fn parse(raw: &str, universe: &Universe) -> Result<ListQuery, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-list-query:p1:inst-lq-01
    // The five parameters are read from the query string, with the `$top` and
    // `$skip` defaults of §1.5 for the ones the request omits.
    let mut violations = Violations::new();
    let mut query = ListQuery {
        filter: None,
        order_by: None,
        select: Vec::new(),
        top: TOP_DEFAULT,
        skip: SKIP_DEFAULT,
    };
    let mut seen: Vec<&'static str> = Vec::new();
    for (name, value) in form_parse(raw.as_bytes()) {
        let name = name.into_owned();
        let value = value.into_owned();
        // @cpt-end:cpt-cf-oagw-algo-list-query:p1:inst-lq-01
        // @cpt-begin:cpt-cf-oagw-algo-list-query:p1:inst-lq-03
        // A parameter outside the closed five-parameter universe is rejected,
        // as is the same parameter twice, which has no defined reading.
        let Some(declared) = LIST_PARAMETERS.iter().find(|declared| **declared == name) else {
            violations.record(
                ViolationKind::UnknownField,
                name.clone(),
                format!("'{name}' is not a parameter of the OData subset"),
            );
            continue;
        };
        if seen.contains(declared) {
            violations.record(
                ViolationKind::UnknownField,
                name.clone(),
                format!("'{name}' is repeated in the query string"),
            );
            continue;
        }
        seen.push(declared);
        // @cpt-end:cpt-cf-oagw-algo-list-query:p1:inst-lq-03
        match *declared {
            "$filter" => match filter(&value, universe) {
                Ok(filter) => query.filter = Some(filter),
                Err(error) => record(&mut violations, error),
            },
            "$orderby" => match order_by(&value, universe) {
                Ok(order_by) => query.order_by = Some(order_by),
                Err(error) => record(&mut violations, error),
            },
            "$select" => match select(&value, universe) {
                Ok(select) => query.select = select,
                Err(error) => record(&mut violations, error),
            },
            // @cpt-begin:cpt-cf-oagw-algo-list-query:p1:inst-lq-06
            // `$top` and `$skip` are integers: `$top` is clamped to its
            // maximum, `$skip` refuses a negative offset.
            "$top" => match top(&value) {
                Ok(top) => query.top = top,
                Err(error) => record(&mut violations, error),
            },
            "$skip" => match skip(&value) {
                Ok(skip) => query.skip = skip,
                Err(error) => record(&mut violations, error),
            },
            // @cpt-end:cpt-cf-oagw-algo-list-query:p1:inst-lq-06
            other => violations.record(
                ViolationKind::UnknownField,
                other,
                format!("'{other}' is not a parameter of the OData subset"),
            ),
        }
    }

    // @cpt-begin:cpt-cf-oagw-algo-list-query:p1:inst-lq-07
    // The collected violations are returned together, as one 400 validation
    // error.
    violations.into_result(&[])?;
    // @cpt-end:cpt-cf-oagw-algo-list-query:p1:inst-lq-07
    Ok(query)
}

/// Applies a parsed query to the serialized resources of the calling tenant
/// (`inst-lq-08` to `inst-lq-11`).
#[must_use]
pub fn apply(
    query: &ListQuery,
    universe: &Universe,
    items: Vec<serde_json::Value>,
) -> Vec<serde_json::Value> {
    // @cpt-begin:cpt-cf-oagw-algo-list-query:p1:inst-lq-08
    // The resources arrive already read through the tenant-scoped repository,
    // so the page cannot contain another tenant's resource.
    let mut page = items;
    // @cpt-end:cpt-cf-oagw-algo-list-query:p1:inst-lq-08

    // @cpt-begin:cpt-cf-oagw-algo-list-query:p1:inst-lq-09
    // `$filter`, then `$orderby`, then `$skip`, then the clamped `$top`, in
    // that order.
    if let Some((field, value)) = &query.filter {
        page.retain(|item| matches_filter(item, universe, field, value));
    }
    if let Some((field, ascending)) = &query.order_by {
        page.sort_by(|left, right| {
            let ordering = compare(field_value(left, field), field_value(right, field));
            if *ascending {
                ordering
            } else {
                ordering.reverse()
            }
        });
    }
    page = page.into_iter().skip(query.skip).take(query.top).collect();
    // @cpt-end:cpt-cf-oagw-algo-list-query:p1:inst-lq-09

    // @cpt-begin:cpt-cf-oagw-algo-list-query:p1:inst-lq-10
    // The projection onto the `$select` field set, in the order the request
    // names the fields.
    if !query.select.is_empty() {
        page = page
            .into_iter()
            .map(|item| project(&item, &query.select))
            .collect();
    }
    // @cpt-end:cpt-cf-oagw-algo-list-query:p1:inst-lq-10

    // @cpt-begin:cpt-cf-oagw-algo-list-query:p1:inst-lq-11
    // The page is a bare JSON array with no envelope.
    page
    // @cpt-end:cpt-cf-oagw-algo-list-query:p1:inst-lq-11
}

/// Records the violations a sub-parser reported, so every offending parameter
/// is named in the one 400 response.
fn record(violations: &mut Violations, error: DomainError) {
    for violation in error.to_violations() {
        violations.push(violation);
    }
}

/// Parses the `$filter` value (`inst-lq-02`).
fn filter(expression: &str, universe: &Universe) -> Result<(String, String), DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-list-query:p1:inst-lq-02
    // Parse `$filter` and accept only the `eq` comparison on the one field the
    // DESIGN names for the resource — `alias` for upstreams, `upstream_id` for
    // routes, `type` for plugins — any other operator, field or unparseable
    // expression being a rejection.
    let rejection = |message: String| {
        Err(DomainError::from_violation(Violation::new(
            ViolationKind::UnknownField,
            "$filter",
            message,
        )))
    };
    let mut tokens = expression.trim().splitn(3, char::is_whitespace);
    let field = tokens.next().unwrap_or_default().trim();
    let operator = tokens.next().unwrap_or_default().trim();
    let value = tokens.next().unwrap_or_default().trim();
    if field != universe.filter_field {
        return rejection(format!(
            "the list supports only the '{named}' filter field, not '{field}'",
            named = universe.filter_field
        ));
    }
    if operator != "eq" {
        return rejection(format!(
            "the list supports only the eq comparison, not '{operator}'"
        ));
    }
    if value.is_empty() {
        return rejection("the comparison carries no value".to_owned());
    }
    // The value is one OData literal: either a bare token or a single-quoted
    // string. Anything more is a compound expression the subset does not
    // declare, so it is an unparseable expression here.
    let quoted = value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'');
    if (quoted && unquote(value).contains('\'')) || (!quoted && value.contains(char::is_whitespace))
    {
        return rejection(
            "the comparison carries more than the one term the subset declares".to_owned(),
        );
    }
    Ok((field.to_owned(), unquote(value).to_owned()))
    // @cpt-end:cpt-cf-oagw-algo-list-query:p1:inst-lq-02
}

/// Parses the `$orderby` value (`inst-lq-05`).
fn order_by(expression: &str, universe: &Universe) -> Result<(String, bool), DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-list-query:p1:inst-lq-05
    // Parse `$orderby` as exactly one field with an optional `asc` or `desc`
    // direction, defaulting to `asc`, and reject a second field, a missing
    // direction keyword in an `asc`/`desc` position, and a name outside the
    // governing field universe of §1.5.
    let rejection = |message: String| {
        Err(DomainError::from_violation(Violation::new(
            ViolationKind::UnknownField,
            "$orderby",
            message,
        )))
    };
    let tokens: Vec<&str> = expression.split_whitespace().collect();
    let (field, ascending) = match tokens.as_slice() {
        [field] => (*field, true),
        [field, "asc"] => (*field, true),
        [field, "desc"] => (*field, false),
        [field, direction] => {
            return rejection(format!(
                "'{direction}' is not a direction the subset declares for '{field}'"
            ));
        }
        _ => {
            return rejection(
                "the subset orders by exactly one field with an optional asc or desc direction"
                    .to_owned(),
            );
        }
    };
    if !universe.fields.contains(&field) {
        return rejection(format!(
            "'{field}' is not a field of the aggregate the list orders over"
        ));
    }
    Ok((field.to_owned(), ascending))
    // @cpt-end:cpt-cf-oagw-algo-list-query:p1:inst-lq-05
}

/// Parses the `$select` value (`inst-lq-04`).
fn select(expression: &str, universe: &Universe) -> Result<Vec<String>, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-list-query:p1:inst-lq-04
    // Parse `$select` as a comma-separated projection onto the resource's own
    // fields — the aggregate field set of §1.5 — and reject a name that is not
    // a field of that set.
    let rejection = |message: String| {
        Err(DomainError::from_violation(Violation::new(
            ViolationKind::UnknownField,
            "$select",
            message,
        )))
    };
    let split = expression.split(',').count();
    let fields: Vec<String> = expression
        .split(',')
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if fields.len() != split {
        return rejection("the projection names an empty field".to_owned());
    }
    for field in &fields {
        if !universe.fields.contains(&field.as_str()) {
            return rejection(format!(
                "'{field}' is not a field of the aggregate the list projects over"
            ));
        }
    }
    Ok(fields)
    // @cpt-end:cpt-cf-oagw-algo-list-query:p1:inst-lq-04
}

/// Parses the `$top` value, clamped to [`TOP_MAX`] (`inst-lq-06`).
fn top(value: &str) -> Result<usize, DomainError> {
    let parsed = value
        .trim()
        .parse::<i64>()
        .map_err(|_| malformed("$top", value))?;
    if parsed.is_negative() {
        return Err(malformed("$top", value));
    }
    Ok((parsed as usize).min(TOP_MAX))
}

/// Parses the `$skip` value (`inst-lq-06`).
fn skip(value: &str) -> Result<usize, DomainError> {
    let parsed = value
        .trim()
        .parse::<i64>()
        .map_err(|_| malformed("$skip", value))?;
    if parsed.is_negative() {
        return Err(DomainError::from_violation(Violation::new(
            ViolationKind::UnknownField,
            "$skip",
            format!("'{value}' is a negative offset"),
        )));
    }
    Ok(parsed as usize)
}

/// The violation a malformed numeric parameter is reported with.
fn malformed(parameter: &'static str, value: &str) -> DomainError {
    DomainError::from_violation(Violation::new(
        ViolationKind::UnknownField,
        parameter,
        format!("'{value}' is not an integer the parameter accepts"),
    ))
}

/// Removes the single quotes an OData string literal is wrapped in.
fn unquote(value: &str) -> &str {
    value
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .unwrap_or(value)
}

/// Whether the serialized resource satisfies the `eq` comparison.
fn matches_filter(item: &serde_json::Value, universe: &Universe, field: &str, value: &str) -> bool {
    if field != universe.filter_field {
        return false;
    }
    field_value(item, universe.filter_target).is_some_and(|found| render(found) == value)
}

/// The value of an aggregate field of a serialized resource.
fn field_value<'a>(item: &'a serde_json::Value, field: &str) -> Option<&'a serde_json::Value> {
    item.as_object().and_then(|object| object.get(field))
}

/// Renders a field value for the `eq` comparison.
fn render(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// The type rank two field values are compared across, so a page can be ordered
/// over any field of the aggregate field set.
fn rank(value: &serde_json::Value) -> u8 {
    match value {
        serde_json::Value::Null => 0,
        serde_json::Value::Bool(_) => 1,
        serde_json::Value::Number(_) => 2,
        serde_json::Value::String(_) => 3,
        serde_json::Value::Array(_) => 4,
        serde_json::Value::Object(_) => 5,
    }
}

/// Compares two field values, so the page can be ordered over any field of the
/// aggregate field set. A missing field sorts before a present one, and the
/// sort the caller applies is stable, so equal fields keep the page order.
fn compare(left: Option<&serde_json::Value>, right: Option<&serde_json::Value>) -> Ordering {
    match (left, right) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(left), Some(right)) => {
            rank(left)
                .cmp(&rank(right))
                .then_with(|| match (left, right) {
                    (serde_json::Value::Bool(left), serde_json::Value::Bool(right)) => {
                        left.cmp(right)
                    }
                    (serde_json::Value::Number(left), serde_json::Value::Number(right)) => {
                        compare_numbers(left, right)
                    }
                    (serde_json::Value::String(left), serde_json::Value::String(right)) => {
                        left.cmp(right)
                    }
                    (serde_json::Value::Array(left), serde_json::Value::Array(right)) => {
                        compare_slices(left, right)
                    }
                    (serde_json::Value::Object(left), serde_json::Value::Object(right)) => {
                        compare_maps(left, right)
                    }
                    _ => Ordering::Equal,
                })
        }
    }
}

/// Compares two JSON numbers, keeping the integral part of both exact.
fn compare_numbers(left: &serde_json::Number, right: &serde_json::Number) -> Ordering {
    if let (Some(left), Some(right)) = (left.as_i64(), right.as_i64()) {
        return left.cmp(&right);
    }
    if let (Some(left), Some(right)) = (left.as_u64(), right.as_u64()) {
        return left.cmp(&right);
    }
    left.as_f64()
        .partial_cmp(&right.as_f64())
        .unwrap_or(Ordering::Equal)
}

/// Compares two JSON arrays element by element.
fn compare_slices(left: &[Value], right: &[Value]) -> Ordering {
    for (left, right) in left.iter().zip(right.iter()) {
        let ordering = compare(Some(left), Some(right));
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

/// Compares two JSON maps by their sorted keys, then their values.
fn compare_maps(
    left: &serde_json::Map<String, Value>,
    right: &serde_json::Map<String, Value>,
) -> Ordering {
    let left_keys: Vec<&str> = left.keys().map(String::as_str).collect();
    let right_keys: Vec<&str> = right.keys().map(String::as_str).collect();
    left_keys.cmp(&right_keys).then_with(|| {
        for key in left_keys {
            let ordering = compare(left.get(key), right.get(key));
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        Ordering::Equal
    })
}

/// Projects a serialized resource onto the `$select` field set.
fn project(item: &Value, fields: &[String]) -> Value {
    let Some(object) = item.as_object() else {
        return item.clone();
    };
    let projected = fields
        .iter()
        .filter_map(|field| {
            object
                .get(field)
                .map(|value| (field.clone(), value.clone()))
        })
        .collect();
    Value::Object(projected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::control_plane::dto::Resource;
    use serde_json::json;

    /// A universe over a small aggregate field set, for the pipeline tests.
    fn universe() -> Universe {
        Universe {
            filter_field: "alias",
            filter_target: "alias",
            fields: &["id", "enabled", "alias", "priority"],
        }
    }

    fn page() -> Vec<Value> {
        vec![
            json!({"id": "3", "alias": "c.vendor.com", "enabled": true, "priority": 30}),
            json!({"id": "1", "alias": "a.vendor.com", "enabled": false, "priority": 10}),
            json!({"id": "2", "alias": "b.vendor.com", "enabled": true, "priority": 20}),
        ]
    }

    fn parsed(raw: &str) -> ListQuery {
        parse(raw, &universe()).expect("the query is inside the subset")
    }

    fn id(item: &Value) -> String {
        render(field_value(item, "id").expect("the field is carried"))
    }

    /// `inst-lq-01`: the defaults of §1.5 are applied for the omitted
    /// parameters.
    #[test]
    fn the_defaults_of_the_subset_are_applied() {
        let query = parsed("");
        assert_eq!(query.top, TOP_DEFAULT);
        assert_eq!(query.skip, SKIP_DEFAULT);
        assert_eq!(query.filter, None);
        assert_eq!(query.order_by, None);
        assert!(query.select.is_empty());
        assert_eq!(apply(&query, &universe(), page()), page());
    }

    /// `inst-lq-02` and `inst-lq-09`: the filter narrows the page.
    #[test]
    fn a_filter_is_parsed_and_applied() {
        let query = parsed("$filter=alias%20eq%20%27a.vendor.com%27");
        assert_eq!(
            query.filter,
            Some(("alias".to_owned(), "a.vendor.com".to_owned()))
        );
        let page = apply(&query, &universe(), page());
        assert_eq!(page.len(), 1);
        assert_eq!(id(&page[0]), "1");
    }

    /// `inst-lq-05` and `inst-lq-09`: one field with an optional direction.
    #[test]
    fn an_ordering_is_parsed_and_applied() {
        let query = parsed("$orderby=alias");
        assert_eq!(query.order_by, Some(("alias".to_owned(), true)));
        let sorted = apply(&query, &universe(), page());
        assert_eq!(ids(&sorted), ["1", "2", "3"]);

        let query = parsed("$orderby=priority%20desc");
        assert_eq!(query.order_by, Some(("priority".to_owned(), false)));
        let sorted = apply(&query, &universe(), page());
        assert_eq!(ids(&sorted), ["3", "2", "1"]);
    }

    /// `inst-lq-04` and `inst-lq-10`: the projection onto the named fields.
    #[test]
    fn a_projection_is_parsed_and_applied() {
        let query = parsed("$select=alias,priority");
        assert_eq!(query.select, ["alias", "priority"]);
        let page = apply(&query, &universe(), page());
        assert_eq!(page[0], json!({"alias": "c.vendor.com", "priority": 30}));
    }

    /// `inst-lq-09`: `$filter`, then `$orderby`, then `$skip`, then `$top`.
    #[test]
    fn the_page_is_sliced_in_the_declared_order() {
        let query = parsed("$orderby=alias&$skip=1&$top=1");
        assert_eq!(query.skip, 1);
        assert_eq!(query.top, 1);
        let page = apply(&query, &universe(), page());
        assert_eq!(page.len(), 1);
        assert_eq!(id(&page[0]), "2");
    }

    /// `inst-lq-06`: `$top` is clamped, never rejected for being large.
    #[test]
    fn a_large_top_is_clamped_to_the_maximum() {
        assert_eq!(parsed("$top=500").top, TOP_MAX);
        assert_eq!(parsed("$top=100").top, 100);
        assert_eq!(parsed("$top=1").top, 1);
        assert_eq!(parsed("$top=0").top, 0);
    }

    /// `inst-lq-07`: a malformed `$top` or `$skip` is a 400.
    #[test]
    fn a_malformed_top_or_skip_is_rejected() {
        for raw in ["$top=abc", "$top=", "$top=1.5", "$skip=abc", "$skip=1.5"] {
            let error = parse(raw, &universe()).expect_err(raw);
            assert!(
                error.message().contains("is not an integer"),
                "{raw}: {}",
                error.message()
            );
        }
    }

    /// `inst-lq-06`: a negative `$skip` is rejected.
    #[test]
    fn a_negative_skip_is_rejected() {
        let error = parse("$skip=-1", &universe()).expect_err("negative");
        assert!(error.message().contains("negative"));
    }

    /// `inst-lq-03`: any other operator, field or unparseable expression.
    #[test]
    fn an_unknown_filter_operator_or_field_is_rejected() {
        for raw in [
            "$filter=alias%20ne%20%27a%27",
            "$filter=priority%20eq%20%2710%27",
            "$filter=alias",
            "$filter=alias%20eq",
            "$filter=alias%20eq%20%27a%27%20and%20priority%20eq%20%2710%27",
        ] {
            let error = parse(raw, &universe()).expect_err(raw);
            assert_eq!(error.field(), "$filter", "{raw}");
        }
    }

    /// `inst-lq-05`: a field outside the universe and a bad direction.
    #[test]
    fn an_unknown_orderby_field_or_direction_is_rejected() {
        for raw in [
            "$orderby=created_at%20desc",
            "$orderby=not_a_field",
            "$orderby=alias%20sideways",
            "$orderby=alias%20desc%20extra",
        ] {
            let error = parse(raw, &universe()).expect_err(raw);
            assert_eq!(error.field(), "$orderby", "{raw}: {}", error.message());
        }
    }

    /// `inst-lq-04`: a name outside the aggregate field set.
    #[test]
    fn an_unknown_select_field_is_rejected() {
        let error = parse("$select=alias,created_at", &universe()).expect_err("unknown field");
        assert_eq!(error.field(), "$select");
        assert!(error.message().contains("created_at"));
    }

    /// `inst-lq-01` and `inst-lq-03`: the parameter universe is closed.
    #[test]
    fn a_parameter_outside_the_universe_is_rejected() {
        for raw in ["$count=1", "$search=x", "$expand=upstream"] {
            let error = parse(raw, &universe()).expect_err(raw);
            assert!(error.message().contains("is not a parameter"), "{raw}");
        }
        let error = parse("$top=1&$top=2", &universe()).expect_err("repeated");
        assert!(error.message().contains("repeated"));
    }

    /// `inst-lq-09`: the sort is stable, so equal fields keep the page order.
    #[test]
    fn a_stable_sort_keeps_equal_fields_in_page_order() {
        let items = vec![
            json!({"id": "1", "priority": 10}),
            json!({"id": "2", "priority": 10}),
        ];
        let query = parsed("$orderby=priority");
        let page = apply(&query, &universe(), items);
        assert_eq!(id(&page[0]), "1");
        assert_eq!(id(&page[1]), "2");
    }

    /// §1.5: the plugin filter field `type` addresses the `plugin_type` field.
    #[test]
    fn the_plugin_filter_field_addresses_the_plugin_type_field() {
        let universe = Universe::of(Resource::Plugin);
        assert_eq!(universe.filter_field, "type");
        assert_eq!(universe.filter_target, "plugin_type");
        let query = parse(
            "$filter=type%20eq%20%27gts.cf.core.oagw.guard_plugin.v1~%27",
            &universe,
        )
        .expect("the filter is inside the subset");
        let items = vec![
            json!({"plugin_type": "gts.cf.core.oagw.guard_plugin.v1~"}),
            json!({"plugin_type": "gts.cf.core.oagw.auth_plugin.v1~"}),
        ];
        let page = apply(&query, &universe, items);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0]["plugin_type"], "gts.cf.core.oagw.guard_plugin.v1~");
    }

    /// §1.5: the universes of the three resources.
    #[test]
    fn the_universe_of_each_resource_is_the_documented_one() {
        let upstream = Universe::of(Resource::Upstream);
        assert_eq!(upstream.filter_field, "alias");
        let route = Universe::of(Resource::Route);
        assert_eq!(route.filter_field, "upstream_id");
        assert!(route.fields.contains(&"priority"));
        assert!(route.fields.contains(&"enabled"));
    }

    /// The identifiers of a page, in page order.
    fn ids(page: &[Value]) -> Vec<String> {
        page.iter().map(&id).collect()
    }
}
