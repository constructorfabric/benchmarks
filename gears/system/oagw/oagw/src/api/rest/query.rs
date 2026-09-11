//! The `$filter` / `$select` / `$orderby` / `$top` / `$skip` surface the list
//! endpoints expose (`docs/DESIGN.md` §"List Query Parameters").
//!
//! Filtering is evaluated over the serialized JSON of each item, so it works
//! uniformly for upstreams, routes and plugins without a per-entity field
//! table. The supported grammar is the documented subset:
//!
//! ```text
//! expr    := term (('and' | 'or') term)*
//! term    := field ('eq' | 'ne') literal
//! literal := "'" text "'" | number | true | false | null
//! ```

use serde_json::Value;
use std::collections::HashMap;

use crate::domain::error::{DomainError, DomainResult};

/// Default page size when `$top` is absent.
pub const DEFAULT_TOP: usize = 50;
/// Hard ceiling on `$top`.
pub const MAX_TOP: usize = 100;

/// A parsed list query.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// Conjunction/disjunction of comparisons.
    pub filter: Option<Filter>,
    /// Fields to project.
    pub select: Option<Vec<String>>,
    /// Sort keys, applied in order.
    pub order_by: Vec<(String, bool)>,
    /// Page size.
    pub top: usize,
    /// Offset.
    pub skip: usize,
}

/// A filter expression.
#[derive(Debug, Clone)]
pub enum Filter {
    /// `field eq literal`.
    Eq(String, Value),
    /// `field ne literal`.
    Ne(String, Value),
    /// Every child must match.
    And(Vec<Filter>),
    /// At least one child must match.
    Or(Vec<Filter>),
}

impl Filter {
    /// Evaluate against one item.
    #[must_use]
    pub fn matches(&self, item: &Value) -> bool {
        match self {
            Self::Eq(field, expected) => field_value(item, field) == Some(expected.clone()),
            Self::Ne(field, expected) => field_value(item, field) != Some(expected.clone()),
            Self::And(children) => children.iter().all(|c| c.matches(item)),
            Self::Or(children) => children.iter().any(|c| c.matches(item)),
        }
    }
}

/// Resolve a possibly dotted field path within an item.
fn field_value(item: &Value, path: &str) -> Option<Value> {
    let mut current = item;
    for segment in path.split('/') {
        current = current.get(segment)?;
    }
    Some(current.clone())
}

/// Parse the raw query-string parameters.
///
/// # Errors
///
/// `400` when a parameter is malformed, `$top`/`$skip` are not integers, or a
/// non-OData parameter is supplied.
pub fn parse(params: &HashMap<String, String>) -> DomainResult<ListQuery> {
    for key in params.keys() {
        if !matches!(
            key.as_str(),
            "$filter" | "$select" | "$orderby" | "$top" | "$skip"
        ) {
            return Err(DomainError::validation(format!(
                "unsupported query parameter '{key}'; the list endpoints accept OData parameters \
                 only ($filter, $select, $orderby, $top, $skip)"
            )));
        }
    }

    let top = params
        .get("$top")
        .map(|raw| {
            raw.trim()
                .parse::<usize>()
                .map_err(|_| DomainError::validation("'$top' must be a non-negative integer"))
        })
        .transpose()?
        .unwrap_or(DEFAULT_TOP)
        .clamp(1, MAX_TOP);

    let skip = params
        .get("$skip")
        .map(|raw| {
            raw.trim()
                .parse::<usize>()
                .map_err(|_| DomainError::validation("'$skip' must be a non-negative integer"))
        })
        .transpose()?
        .unwrap_or(0);

    let select = params.get("$select").map(|raw| {
        raw.split(',')
            .map(|f| f.trim().to_owned())
            .filter(|f| !f.is_empty())
            .collect::<Vec<_>>()
    });

    let mut order_by = Vec::new();
    if let Some(raw) = params.get("$orderby") {
        for clause in raw.split(',') {
            let mut parts = clause.split_whitespace();
            let Some(field) = parts.next() else { continue };
            let descending = match parts.next() {
                None | Some("asc") => false,
                Some("desc") => true,
                Some(other) => {
                    return Err(DomainError::validation(format!(
                        "'$orderby' direction must be 'asc' or 'desc', got '{other}'"
                    )));
                }
            };
            order_by.push((field.to_owned(), descending));
        }
    }

    let filter = params.get("$filter").map(|raw| parse_filter(raw)).transpose()?;

    Ok(ListQuery {
        filter,
        select,
        order_by,
        top,
        skip,
    })
}

/// Parse the documented filter subset.
///
/// # Errors
///
/// `400` describing the first token that could not be understood.
pub fn parse_filter(raw: &str) -> DomainResult<Filter> {
    // `or` binds loosest, then `and`; no parentheses in the documented
    // subset, so a two-level split is exact rather than an approximation.
    let or_parts = split_on_keyword(raw, "or");
    if or_parts.len() > 1 {
        let children = or_parts
            .iter()
            .map(|part| parse_filter(part))
            .collect::<DomainResult<Vec<_>>>()?;
        return Ok(Filter::Or(children));
    }
    let and_parts = split_on_keyword(raw, "and");
    if and_parts.len() > 1 {
        let children = and_parts
            .iter()
            .map(|part| parse_filter(part))
            .collect::<DomainResult<Vec<_>>>()?;
        return Ok(Filter::And(children));
    }
    parse_comparison(raw)
}

/// Split on a bare keyword, ignoring occurrences inside quoted literals.
fn split_on_keyword(raw: &str, keyword: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut token = String::new();

    let flush_token = |current: &mut String, token: &mut String| {
        if !token.is_empty() {
            current.push_str(token);
            token.clear();
        }
    };

    for ch in raw.chars() {
        if ch == '\'' {
            in_quotes = !in_quotes;
            token.push(ch);
            continue;
        }
        if in_quotes || !ch.is_whitespace() {
            token.push(ch);
            continue;
        }
        if token.eq_ignore_ascii_case(keyword) {
            parts.push(current.trim().to_owned());
            current.clear();
            token.clear();
        } else {
            flush_token(&mut current, &mut token);
            current.push(' ');
        }
    }
    if token.eq_ignore_ascii_case(keyword) {
        parts.push(current.trim().to_owned());
        current.clear();
    } else {
        current.push_str(&token);
    }
    parts.push(current.trim().to_owned());
    parts.retain(|p| !p.is_empty());
    parts
}

fn parse_comparison(raw: &str) -> DomainResult<Filter> {
    let trimmed = raw.trim();
    let malformed = || {
        DomainError::validation(format!(
            "'$filter' expression must be `<field> eq|ne <literal>`, got '{trimmed}'"
        ))
    };
    let mut parts = trimmed.splitn(3, char::is_whitespace);
    let field = parts.next().filter(|f| !f.is_empty()).ok_or_else(malformed)?;
    let operator = parts.next().ok_or_else(malformed)?;
    let literal = parts.next().ok_or_else(malformed)?.trim();
    let value = parse_literal(literal)?;

    match operator.to_ascii_lowercase().as_str() {
        "eq" => Ok(Filter::Eq(field.to_owned(), value)),
        "ne" => Ok(Filter::Ne(field.to_owned(), value)),
        other => Err(DomainError::validation(format!(
            "'$filter' supports only the 'eq' and 'ne' operators, got '{other}'"
        ))),
    }
}

fn parse_literal(raw: &str) -> DomainResult<Value> {
    if let Some(inner) = raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return Ok(Value::String(inner.replace("''", "'")));
    }
    match raw {
        "true" => Ok(Value::Bool(true)),
        "false" => Ok(Value::Bool(false)),
        "null" => Ok(Value::Null),
        number => number
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| DomainError::validation(format!("'$filter' literal '{raw}' is not a quoted string, number, boolean or null"))),
    }
}

/// Apply the whole query to a serialized item list.
#[must_use]
pub fn apply(query: &ListQuery, items: Vec<Value>) -> Vec<Value> {
    let mut filtered: Vec<Value> = items
        .into_iter()
        .filter(|item| query.filter.as_ref().is_none_or(|f| f.matches(item)))
        .collect();

    for (field, descending) in query.order_by.iter().rev() {
        filtered.sort_by(|a, b| {
            let ordering = compare(field_value(a, field), field_value(b, field));
            if *descending { ordering.reverse() } else { ordering }
        });
    }

    filtered
        .into_iter()
        .skip(query.skip)
        .take(query.top)
        .map(|item| project_select(query.select.as_deref(), item))
        .collect()
}

fn compare(a: Option<Value>, b: Option<Value>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(a), Some(b)) => match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
            _ => a.to_string().cmp(&b.to_string()),
        },
    }
}

fn project_select(select: Option<&[String]>, item: Value) -> Value {
    let Some(fields) = select else { return item };
    let Value::Object(map) = item else { return item };
    let projected: serde_json::Map<String, Value> = map
        .into_iter()
        .filter(|(key, _)| fields.iter().any(|f| f == key))
        .collect();
    Value::Object(projected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn items() -> Vec<Value> {
        vec![
            json!({ "id": 1, "alias": "api.openai.com", "enabled": true }),
            json!({ "id": 2, "alias": "api.stripe.com", "enabled": false }),
            json!({ "id": 3, "alias": "api.anthropic.com", "enabled": true }),
        ]
    }

    #[test]
    fn defaults_are_applied() {
        let query = parse(&HashMap::new()).expect("parses");
        assert_eq!(query.top, DEFAULT_TOP);
        assert_eq!(query.skip, 0);
        assert!(query.filter.is_none());
    }

    #[test]
    fn top_is_clamped_to_the_documented_maximum() {
        let query = parse(&params(&[("$top", "5000")])).expect("parses");
        assert_eq!(query.top, MAX_TOP);
    }

    #[test]
    fn non_odata_parameters_are_rejected() {
        let err = parse(&params(&[("alias", "x")])).expect_err("rejected");
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn eq_filter_on_a_string_field() {
        let query = parse(&params(&[("$filter", "alias eq 'api.openai.com'")])).expect("parses");
        let out = apply(&query, items());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], json!(1));
    }

    #[test]
    fn ne_filter_and_boolean_literals() {
        let query = parse(&params(&[("$filter", "enabled ne true")])).expect("parses");
        let out = apply(&query, items());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], json!(2));
    }

    #[test]
    fn and_or_combinations() {
        let query = parse(&params(&[(
            "$filter",
            "enabled eq true and alias eq 'api.openai.com'",
        )]))
        .expect("parses");
        assert_eq!(apply(&query, items()).len(), 1);

        let query = parse(&params(&[(
            "$filter",
            "alias eq 'api.openai.com' or alias eq 'api.stripe.com'",
        )]))
        .expect("parses");
        assert_eq!(apply(&query, items()).len(), 2);
    }

    #[test]
    fn orderby_skip_and_top() {
        let query = parse(&params(&[
            ("$orderby", "alias desc"),
            ("$top", "2"),
            ("$skip", "1"),
        ]))
        .expect("parses");
        let out = apply(&query, items());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["alias"], json!("api.openai.com"));
    }

    #[test]
    fn select_projects_named_fields_only() {
        let query = parse(&params(&[("$select", "id,alias")])).expect("parses");
        let out = apply(&query, items());
        assert!(out[0].get("enabled").is_none());
        assert!(out[0].get("alias").is_some());
    }

    #[test]
    fn a_malformed_filter_is_a_400() {
        assert!(parse(&params(&[("$filter", "alias")])).is_err());
        assert!(parse(&params(&[("$filter", "alias like 'x'")])).is_err());
        assert!(parse(&params(&[("$orderby", "alias sideways")])).is_err());
        assert!(parse(&params(&[("$top", "many")])).is_err());
    }

    #[test]
    fn quoted_keywords_are_not_split() {
        let query =
            parse(&params(&[("$filter", "alias eq 'and or example.com'")])).expect("parses");
        let out = apply(
            &query,
            vec![json!({ "alias": "and or example.com" }), json!({ "alias": "x" })],
        );
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn dotted_paths_reach_into_nested_objects() {
        let query =
            parse(&params(&[("$filter", "match/http/path eq '/v1/models'")])).expect("parses");
        let out = apply(
            &query,
            vec![
                json!({ "id": 1, "match": { "http": { "path": "/v1/models" } } }),
                json!({ "id": 2, "match": { "http": { "path": "/v1/chat" } } }),
            ],
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], json!(1));
    }
}
