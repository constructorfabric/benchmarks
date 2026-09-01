//! OData-lite list query support for the OAGW management endpoints.
//!
//! Implements a pragmatic subset of the DESIGN's list query parameters:
//! `$filter`, `$select`, `$orderby`, `$top`, `$skip`.
//! - `$top`: default 50, max 100 (the DESIGN caps at 100).
//! - `$filter`: `field eq 'value'` / `field ne 'value'` and boolean literals,
//!   combined with `and`.
//! - `$orderby`: single `field [asc|desc]`.
//! - `$select`: comma-separated field projection.
//!
//! # DESIGN-led deviation
//!
//! Full `OData` (function calls, nested exprs, cursor pagination from
//! `toolkit-odata`) is not a crate dependency; the subset above covers the
//! documented examples while returning RFC-style 400 validation problems for
//! unsupported syntax.

use serde_json::Value;

/// Default page size.
pub const DEFAULT_TOP: usize = 50;
/// Maximum page size (DESIGN: max 100).
pub const MAX_TOP: usize = 100;

/// A parsed `$filter` expression (minimal subset).
#[derive(Debug, Clone, PartialEq)]
pub enum FilterExpr {
    /// `field eq 'value'`.
    Eq(String, Value),
    /// `field ne 'value'`.
    Ne(String, Value),
    /// `expr and expr`.
    And(Box<FilterExpr>, Box<FilterExpr>),
}

/// Parsed list options.
#[derive(Debug, Clone, Default)]
pub struct ListOptions {
    /// Page size (clamped to `MAX_TOP`).
    pub top: usize,
    /// Offset.
    pub skip: usize,
    /// Optional filter.
    pub filter: Option<FilterExpr>,
    /// Optional `(field, ascending)` ordering.
    pub order_by: Option<(String, bool)>,
    /// Optional field projection.
    pub select: Option<Vec<String>>,
}

/// Error type for malformed `OData` query parameters.
#[derive(Debug, thiserror::Error, PartialEq)]
#[error("{message}")]
pub struct ODataError {
    pub message: String,
}

/// Parse OData-lite query parameters from axum's query mapping.
///
/// Accepts a map of raw query keys → first value (only `$`-prefixed keys are
/// consumed).
///
/// # Errors
///
/// Returns an [`ODataError`] when a `$`-prefixed parameter is malformed.
#[allow(clippy::implicit_hasher)] // mirrors axum's `Query` map shape.
pub fn parse_params(
    params: &std::collections::HashMap<String, String>,
) -> Result<ListOptions, ODataError> {
    let mut opts = ListOptions {
        top: DEFAULT_TOP,
        skip: 0,
        filter: None,
        order_by: None,
        select: None,
    };

    if let Some(v) = params.get("$top") {
        opts.top = parse_bounded_usize(v, "$top")?;
    }
    if let Some(v) = params.get("$skip") {
        opts.skip = parse_usize(v, "$skip")?;
    }
    if let Some(v) = params.get("$filter")
        && !v.trim().is_empty()
    {
        opts.filter = Some(parse_filter(v).map_err(|m| ODataError { message: m })?);
    }
    if let Some(v) = params.get("$orderby")
        && !v.trim().is_empty()
    {
        opts.order_by = Some(parse_orderby(v).map_err(|m| ODataError { message: m })?);
    }
    if let Some(v) = params.get("$select") {
        let fields: Vec<String> = v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect();
        if !fields.is_empty() {
            opts.select = Some(fields);
        }
    }
    Ok(opts)
}

fn parse_usize(v: &str, name: &str) -> Result<usize, ODataError> {
    v.trim().parse::<usize>().map_err(|_| ODataError {
        message: format!("{name} must be a non-negative integer"),
    })
}

fn parse_bounded_usize(v: &str, name: &str) -> Result<usize, ODataError> {
    let n = parse_usize(v, name)?;
    Ok(n.min(MAX_TOP))
}

/// Parse `field eq 'value'` / `field ne 'value'` / `true` / `false`, with
/// optional `and` composition.
fn parse_filter(input: &str) -> Result<FilterExpr, String> {
    let mut rest = input.trim();
    let mut parts = Vec::new();
    // Split on top-level " and " (no parens in the subset).
    while let Some(pos) = rest.find(" and ") {
        parts.push(rest[..pos].trim().to_owned());
        rest = rest[pos + 5..].trim();
    }
    parts.push(rest.to_owned());

    let mut exprs = Vec::new();
    for part in parts {
        exprs.push(parse_single_filter(&part)?);
    }
    let mut iter = exprs.into_iter();
    let Some(first) = iter.next() else {
        return Err("$filter must not be empty".to_owned());
    };
    Ok(iter.fold(first, |acc, e| FilterExpr::And(Box::new(acc), Box::new(e))))
}

fn parse_single_filter(part: &str) -> Result<FilterExpr, String> {
    let part = part.trim();
    if part == "true" {
        return Ok(FilterExpr::Eq("enabled".to_owned(), Value::Bool(true)));
    }
    if part == "false" {
        return Ok(FilterExpr::Eq("enabled".to_owned(), Value::Bool(false)));
    }
    for op in ["ne", "eq"] {
        if let Some(pos) = part.find(&format!(" {op} ")) {
            let field = part[..pos].trim().to_owned();
            let literal = part[pos + op.len() + 1..].trim();
            let value = parse_literal(literal);
            return if op == "eq" {
                Ok(FilterExpr::Eq(field, value))
            } else {
                Ok(FilterExpr::Ne(field, value))
            };
        }
    }
    Err(format!(
        "unsupported $filter expression '{part}' (supported: field eq 'value', field ne 'value', and-conjunctions)"
    ))
}

fn parse_literal(lit: &str) -> Value {
    let lit = lit.trim();
    if (lit.starts_with('\'') && lit.ends_with('\'') && lit.len() >= 2)
        || (lit.starts_with('\"') && lit.ends_with('\"') && lit.len() >= 2)
    {
        let inner = &lit[1..lit.len() - 1];
        // Strip OData double-'' escaping.
        return Value::String(inner.replace("''", "'"));
    }
    if let Ok(b) = lit.parse::<bool>() {
        return Value::Bool(b);
    }
    if let Ok(n) = lit.parse::<i64>() {
        return Value::Number(n.into());
    }
    if let Ok(u) = lit.parse::<u64>() {
        return Value::Number(u.into());
    }
    // Bare identifier: treat as string (e.g. uuid filter without quotes).
    Value::String(lit.to_owned())
}

/// Parse `field` or `field desc`/`field asc`.
fn parse_orderby(input: &str) -> Result<(String, bool), String> {
    let mut parts = input.split_whitespace();
    let field = parts
        .next()
        .ok_or_else(|| "$orderby must not be empty".to_owned())?
        .to_owned();
    let asc = match parts.next() {
        None => true,
        Some(dir) if dir.eq_ignore_ascii_case("asc") => true,
        Some(dir) if dir.eq_ignore_ascii_case("desc") => false,
        Some(other) => {
            return Err(format!(
                "unsupported $orderby direction '{other}' (expected asc|desc)"
            ));
        }
    };
    if parts.next().is_some() {
        return Err("multiple $orderby fields are not supported".to_owned());
    }
    Ok((field, asc))
}

/// Resolve a (possibly dotted) field path inside a JSON value, e.g.
/// `match.http.path` → `item["match"]["http"]["path"]`. Unresolvable paths
/// return `None`.
#[must_use]
pub fn resolve_field<'a>(item: &'a Value, field: &str) -> Option<&'a Value> {
    let mut cur = item;
    for seg in field.split('.') {
        cur = cur.get(seg)?;
    }
    Some(cur)
}

/// Evaluate a filter against an item's JSON value.
#[must_use]
pub fn matches_filter(item: &Value, filter: &FilterExpr) -> bool {
    match filter {
        FilterExpr::Eq(field, want) => value_eq(resolve_field(item, field), want),
        FilterExpr::Ne(field, want) => !value_eq(resolve_field(item, field), want),
        FilterExpr::And(a, b) => matches_filter(item, a) && matches_filter(item, b),
    }
}

fn value_eq(got: Option<&Value>, want: &Value) -> bool {
    match (got, want) {
        (Some(g), want) => match (g, want) {
            // Strings compared case-insensitively (aliases resolve case-insensitively).
            (Value::String(a), Value::String(b)) => a.eq_ignore_ascii_case(b),
            (Value::String(a), Value::Number(n)) => a == &n.to_string(),
            (Value::Number(a), Value::String(b)) => &a.to_string() == b,
            _ => g == want,
        },
        (None, Value::Null) => true,
        _ => false,
    }
}

/// Sort `items` by the requested `(field, ascending)` ordering (string compare).
fn sort_items(items: &mut [Value], order: &(String, bool)) {
    let (field, asc) = order;
    // Stable sort with string comparison of the field's serialized value.
    items.sort_by(|a, b| {
        let av = resolve_field(a, field)
            .map(value_to_ord)
            .unwrap_or_default();
        let bv = resolve_field(b, field)
            .map(value_to_ord)
            .unwrap_or_default();
        let ord = av.cmp(&bv);
        if *asc { ord } else { ord.reverse() }
    });
}

/// Ordering key: string, else serialized value.
fn value_to_ord(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Project an item to only the `$select`ed fields (dotted paths supported).
fn select_fields(value: &Value, fields: &[String]) -> Value {
    if !value.is_object() {
        return value.clone();
    }
    let mut out = serde_json::Map::new();
    for f in fields {
        if let Some(v) = resolve_field(value, f) {
            out.insert(f.clone(), v.clone());
        }
    }
    Value::Object(out)
}

/// Apply all list options to a set of items serialized as JSON values.
///
/// Returns the paginated, projected page of items.
#[must_use]
pub fn apply<T: serde::Serialize>(items: &[T], opts: &ListOptions) -> Vec<serde_json::Value> {
    let mut values: Vec<Value> = items
        .iter()
        .map(|i| serde_json::to_value(i).unwrap_or(Value::Null))
        .collect();

    if let Some(filter) = &opts.filter {
        values.retain(|v| matches_filter(v, filter));
    }
    if let Some(order) = &opts.order_by {
        sort_items(&mut values, order);
    }
    if let Some(fields) = &opts.select {
        for v in &mut values {
            *v = select_fields(v, fields);
        }
    }
    values.into_iter().skip(opts.skip).take(opts.top).collect()
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn parses_defaults() {
        let params: HashMap<String, String> = HashMap::new();
        let opts = parse_params(&params).unwrap();
        assert_eq!(opts.top, DEFAULT_TOP);
        assert_eq!(opts.skip, 0);
        assert!(opts.filter.is_none());
        assert!(opts.order_by.is_none());
        assert!(opts.select.is_none());
    }

    #[test]
    fn parses_top_skip_caps_at_100() {
        let mut params = HashMap::new();
        params.insert("$top".into(), "500".into());
        params.insert("$skip".into(), "12".into());
        let opts = parse_params(&params).unwrap();
        assert_eq!(opts.top, MAX_TOP);
        assert_eq!(opts.skip, 12);
    }

    #[test]
    fn invalid_top_is_error() {
        let mut params = HashMap::new();
        params.insert("$top".into(), "abc".into());
        assert!(parse_params(&params).is_err());
    }

    #[test]
    fn parses_filter_eq_and_ne() {
        let mut params = HashMap::new();
        params.insert("$filter".into(), "alias eq 'api.openai.com'".into());
        let opts = parse_params(&params).unwrap();
        let f = opts.filter.unwrap();
        assert_eq!(
            f,
            FilterExpr::Eq("alias".into(), Value::String("api.openai.com".into()))
        );
        let item = json!({ "alias": "api.openai.com" });
        assert!(matches_filter(&item, &f));
    }

    #[test]
    fn parses_and_filter() {
        let f = parse_filter("alias eq 'x' and enabled eq true").unwrap();
        let item = json!({ "alias": "x", "enabled": false });
        assert!(!matches_filter(&item, &f));
        let item2 = json!({ "alias": "x", "enabled": true });
        assert!(matches_filter(&item2, &f));
    }

    #[test]
    fn filter_resolves_dotted_paths() {
        let f = FilterExpr::Eq("match.http.path".into(), Value::String("/v1/chat".into()));
        let item = json!({ "match": { "http": { "path": "/v1/chat" } } });
        assert!(matches_filter(&item, &f));
        let miss = json!({ "match": { "http": { "path": "/other" } } });
        assert!(!matches_filter(&miss, &f));
    }

    #[test]
    fn filter_is_case_insensitive_for_strings() {
        let f = FilterExpr::Eq("alias".into(), Value::String("API.X".into()));
        let item = json!({ "alias": "api.x" });
        assert!(matches_filter(&item, &f));
    }

    #[test]
    fn parses_order_by() {
        let mut params = HashMap::new();
        params.insert("$orderby".into(), "alias desc".into());
        let opts = parse_params(&params).unwrap();
        assert_eq!(opts.order_by, Some(("alias".to_owned(), false)));
    }

    #[test]
    fn apply_filters_sorts_and_paginates() {
        let items = vec![
            json!({ "alias": "b.example.com", "enabled": true }),
            json!({ "alias": "a.example.com", "enabled": true }),
            json!({ "alias": "c.example.com", "enabled": false }),
        ];
        let mut params = HashMap::new();
        params.insert("$filter".into(), "enabled eq true".into());
        params.insert("$orderby".into(), "alias".into());
        params.insert("$top".into(), "1".into());
        params.insert("$skip".into(), "1".into());
        let opts = parse_params(&params).unwrap();
        let page = apply(&items, &opts);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0]["alias"], "b.example.com");
    }

    #[test]
    fn apply_select_projects_fields() {
        let items = vec![json!({ "id": "1", "alias": "x", "tags": [] })];
        let mut params = HashMap::new();
        params.insert("$select".into(), "id,alias".into());
        let opts = parse_params(&params).unwrap();
        let page = apply(&items, &opts);
        assert!(page[0].get("id").is_some());
        assert!(page[0].get("alias").is_some());
        assert!(page[0].get("tags").is_none());
    }
}
