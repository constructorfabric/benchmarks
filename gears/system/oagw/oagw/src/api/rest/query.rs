//! List-query support for the management API.
//!
//! `DESIGN.md` § *List Query Parameters* specifies `$filter`, `$select`,
//! `$orderby`, `$top` and `$skip` on every list endpoint. The implementation
//! works on the serialised JSON view of a resource, so the filterable and
//! selectable field names are exactly the ones the API returns — there is no
//! second, divergent list of "supported fields".

use serde_json::{Map, Value};

use crate::domain::error::{OagwError, OagwResult};

/// Default page size.
pub const DEFAULT_TOP: usize = 50;
/// Maximum page size.
pub const MAX_TOP: usize = 100;

/// A parsed list query.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// Conjunction of predicates from `$filter`.
    pub predicates: Vec<Predicate>,
    /// Field names from `$select`.
    pub select: Option<Vec<String>>,
    /// `(field, descending)` pairs from `$orderby`.
    pub order_by: Vec<(String, bool)>,
    /// Page size.
    pub top: usize,
    /// Offset.
    pub skip: usize,
}

/// One comparison from `$filter`.
#[derive(Debug, Clone, PartialEq)]
pub struct Predicate {
    /// Field path, dot-separated for nested members.
    pub field: String,
    /// Comparison operator.
    pub op: Operator,
    /// Right-hand value.
    pub value: Value,
}

/// Supported `$filter` operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    /// `eq`
    Eq,
    /// `ne`
    Ne,
    /// `gt`
    Gt,
    /// `ge`
    Ge,
    /// `lt`
    Lt,
    /// `le`
    Le,
    /// `contains(field,'value')`
    Contains,
    /// `startswith(field,'value')`
    StartsWith,
    /// `endswith(field,'value')`
    EndsWith,
}

fn invalid(field: &'static str, detail: impl Into<String>) -> OagwError {
    OagwError::field(field, detail)
}

/// Parse the raw query string of a list request.
///
/// # Errors
///
/// `400` when a parameter is malformed or out of range.
pub fn parse(raw: &str) -> OagwResult<ListQuery> {
    let mut query = ListQuery {
        top: DEFAULT_TOP,
        ..ListQuery::default()
    };
    for (key, value) in form_urlencoded::parse(raw.as_bytes()) {
        match key.as_ref() {
            "$filter" => query.predicates = parse_filter(&value)?,
            "$select" => {
                let fields: Vec<String> = value
                    .split(',')
                    .map(|field| field.trim().to_owned())
                    .filter(|field| !field.is_empty())
                    .collect();
                if fields.is_empty() {
                    return Err(invalid("$select", "$select must name at least one field"));
                }
                query.select = Some(fields);
            }
            "$orderby" => query.order_by = parse_order_by(&value)?,
            "$top" => {
                let top: usize = value
                    .trim()
                    .parse()
                    .map_err(|_| invalid("$top", "$top must be a non-negative integer"))?;
                if top > MAX_TOP {
                    return Err(invalid("$top", format!("$top must not exceed {MAX_TOP}")));
                }
                query.top = top;
            }
            "$skip" => {
                query.skip = value
                    .trim()
                    .parse()
                    .map_err(|_| invalid("$skip", "$skip must be a non-negative integer"))?;
            }
            other if other.starts_with('$') => {
                return Err(invalid(
                    "query",
                    format!(
                        "unsupported system query option {other:?}; supported: $filter, \
                         $select, $orderby, $top, $skip"
                    ),
                ));
            }
            _ => {}
        }
    }
    Ok(query)
}

/// Parse `$filter` into a conjunction of predicates.
fn parse_filter(raw: &str) -> OagwResult<Vec<Predicate>> {
    let mut predicates = Vec::new();
    for clause in split_conjunction(raw) {
        let clause = clause.trim();
        if clause.is_empty() {
            continue;
        }
        predicates.push(parse_clause(clause)?);
    }
    if predicates.is_empty() {
        return Err(invalid("$filter", "$filter must contain a predicate"));
    }
    Ok(predicates)
}

/// Split on ` and `, ignoring separators inside quoted literals.
fn split_conjunction(raw: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let chars: Vec<char> = raw.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];
        if ch == '\'' {
            in_quotes = !in_quotes;
            current.push(ch);
            index += 1;
            continue;
        }
        if !in_quotes {
            let rest: String = chars[index..].iter().collect();
            let lower = rest.to_ascii_lowercase();
            if lower.starts_with(" and ") {
                parts.push(current.clone());
                current.clear();
                index += 5;
                continue;
            }
        }
        current.push(ch);
        index += 1;
    }
    parts.push(current);
    parts
}

/// Parse one predicate: either `field op value` or `func(field,'value')`.
fn parse_clause(clause: &str) -> OagwResult<Predicate> {
    for (name, op) in [
        ("contains", Operator::Contains),
        ("startswith", Operator::StartsWith),
        ("endswith", Operator::EndsWith),
    ] {
        let prefix = format!("{name}(");
        if clause.to_ascii_lowercase().starts_with(&prefix) {
            let inner = clause[prefix.len()..].strip_suffix(')').ok_or_else(|| {
                invalid("$filter", format!("unbalanced parentheses in {clause:?}"))
            })?;
            let (field, value) = inner.split_once(',').ok_or_else(|| {
                invalid(
                    "$filter",
                    format!("{name}() takes a field and a value: {clause:?}"),
                )
            })?;
            return Ok(Predicate {
                field: field.trim().to_owned(),
                op,
                value: parse_literal(value.trim())?,
            });
        }
    }

    let mut tokens = clause.splitn(3, ' ');
    let field = tokens
        .next()
        .filter(|field| !field.is_empty())
        .ok_or_else(|| invalid("$filter", format!("missing field in {clause:?}")))?;
    let op_token = tokens
        .next()
        .ok_or_else(|| invalid("$filter", format!("missing operator in {clause:?}")))?;
    let value = tokens
        .next()
        .ok_or_else(|| invalid("$filter", format!("missing value in {clause:?}")))?;
    let op = match op_token.to_ascii_lowercase().as_str() {
        "eq" => Operator::Eq,
        "ne" => Operator::Ne,
        "gt" => Operator::Gt,
        "ge" => Operator::Ge,
        "lt" => Operator::Lt,
        "le" => Operator::Le,
        other => {
            return Err(invalid(
                "$filter",
                format!(
                    "unsupported operator {other:?}; supported: eq, ne, gt, ge, lt, le, \
                     contains(), startswith(), endswith()"
                ),
            ));
        }
    };
    Ok(Predicate {
        field: field.to_owned(),
        op,
        value: parse_literal(value.trim())?,
    })
}

/// Parse a literal: quoted string, number, boolean or `null`.
fn parse_literal(raw: &str) -> OagwResult<Value> {
    if let Some(inner) = raw.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')) {
        return Ok(Value::String(inner.replace("''", "'")));
    }
    match raw {
        "true" => return Ok(Value::Bool(true)),
        "false" => return Ok(Value::Bool(false)),
        "null" => return Ok(Value::Null),
        _ => {}
    }
    if let Ok(number) = raw.parse::<i64>() {
        return Ok(Value::Number(number.into()));
    }
    if let Ok(number) = raw.parse::<f64>()
        && let Some(number) = serde_json::Number::from_f64(number)
    {
        return Ok(Value::Number(number));
    }
    Err(invalid(
        "$filter",
        format!("value must be a quoted string, number, boolean or null: {raw:?}"),
    ))
}

/// Parse `$orderby` into `(field, descending)` pairs.
fn parse_order_by(raw: &str) -> OagwResult<Vec<(String, bool)>> {
    let mut keys = Vec::new();
    for clause in raw.split(',') {
        let mut tokens = clause.split_whitespace();
        let Some(field) = tokens.next() else { continue };
        let descending = match tokens.next().map(str::to_ascii_lowercase).as_deref() {
            None | Some("asc") => false,
            Some("desc") => true,
            Some(other) => {
                return Err(invalid(
                    "$orderby",
                    format!("sort direction must be `asc` or `desc`: {other:?}"),
                ));
            }
        };
        keys.push((field.to_owned(), descending));
    }
    if keys.is_empty() {
        return Err(invalid("$orderby", "$orderby must name at least one field"));
    }
    Ok(keys)
}

/// Read a dot-separated field path out of a JSON object.
#[must_use]
pub fn lookup<'a>(item: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = item;
    for segment in path.split('.') {
        current = current.get(segment)?;
    }
    Some(current)
}

/// Evaluate one predicate against an item.
#[must_use]
pub fn matches(item: &Value, predicate: &Predicate) -> bool {
    let Some(actual) = lookup(item, &predicate.field) else {
        // An absent field can only satisfy `ne`.
        return predicate.op == Operator::Ne;
    };
    match predicate.op {
        Operator::Eq => values_equal(actual, &predicate.value),
        Operator::Ne => !values_equal(actual, &predicate.value),
        Operator::Contains | Operator::StartsWith | Operator::EndsWith => {
            let (Some(actual), Some(expected)) = (actual.as_str(), predicate.value.as_str()) else {
                return false;
            };
            match predicate.op {
                Operator::Contains => actual.contains(expected),
                Operator::StartsWith => actual.starts_with(expected),
                _ => actual.ends_with(expected),
            }
        }
        Operator::Gt | Operator::Ge | Operator::Lt | Operator::Le => {
            match compare(actual, &predicate.value) {
                None => false,
                Some(ordering) => match predicate.op {
                    Operator::Gt => ordering.is_gt(),
                    Operator::Ge => ordering.is_ge(),
                    Operator::Lt => ordering.is_lt(),
                    _ => ordering.is_le(),
                },
            }
        }
    }
}

/// Equality that treats an array as "contains", so `tags eq 'llm'` works.
fn values_equal(actual: &Value, expected: &Value) -> bool {
    if let Value::Array(items) = actual {
        return items.iter().any(|item| item == expected);
    }
    actual == expected
}

/// Order two JSON scalars, if they are comparable.
fn compare(left: &Value, right: &Value) -> Option<std::cmp::Ordering> {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left.as_f64()?.partial_cmp(&right.as_f64()?),
        (Value::String(left), Value::String(right)) => Some(left.cmp(right)),
        (Value::Bool(left), Value::Bool(right)) => Some(left.cmp(right)),
        _ => None,
    }
}

/// Apply filter, order, paging and projection, in that order.
#[must_use]
pub fn apply(items: Vec<Value>, query: &ListQuery) -> Vec<Value> {
    let mut items: Vec<Value> = items
        .into_iter()
        .filter(|item| {
            query
                .predicates
                .iter()
                .all(|predicate| matches(item, predicate))
        })
        .collect();

    for (field, descending) in query.order_by.iter().rev() {
        items.sort_by(|left, right| {
            let ordering = match (lookup(left, field), lookup(right, field)) {
                (Some(left), Some(right)) => {
                    compare(left, right).unwrap_or(std::cmp::Ordering::Equal)
                }
                (None, Some(_)) => std::cmp::Ordering::Less,
                (Some(_), None) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            };
            if *descending {
                ordering.reverse()
            } else {
                ordering
            }
        });
    }

    let page: Vec<Value> = items.into_iter().skip(query.skip).take(query.top).collect();
    match &query.select {
        None => page,
        Some(fields) => page
            .into_iter()
            .map(|item| project(&item, fields))
            .collect(),
    }
}

/// Keep only the named top-level fields.
fn project(item: &Value, fields: &[String]) -> Value {
    let mut out = Map::new();
    for field in fields {
        if let Some(value) = item.get(field) {
            out.insert(field.clone(), value.clone());
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn items() -> Vec<Value> {
        vec![
            json!({"id": "1", "alias": "api.openai.com", "enabled": true, "priority": 10, "tags": ["llm", "openai"]}),
            json!({"id": "2", "alias": "api.stripe.com", "enabled": false, "priority": 5, "tags": ["payments"]}),
            json!({"id": "3", "alias": "vendor.com", "enabled": true, "priority": 20, "tags": []}),
        ]
    }

    #[test]
    fn defaults_apply_when_nothing_is_given() {
        let query = parse("").expect("empty query");
        assert_eq!(query.top, DEFAULT_TOP);
        assert_eq!(query.skip, 0);
        assert!(query.predicates.is_empty());
        assert!(query.select.is_none());
        assert_eq!(apply(items(), &query).len(), 3);
    }

    #[test]
    fn filter_eq_on_a_string() {
        let query = parse("%24filter=alias+eq+%27api.openai.com%27").expect("filter");
        let result = apply(items(), &query);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], "1");
    }

    #[test]
    fn filter_eq_on_a_boolean_and_a_number() {
        let query = parse("%24filter=enabled+eq+true").unwrap();
        assert_eq!(apply(items(), &query).len(), 2);
        let query = parse("%24filter=priority+eq+5").unwrap();
        assert_eq!(apply(items(), &query).len(), 1);
    }

    #[test]
    fn filter_ne_and_comparisons() {
        let query = parse("%24filter=priority+gt+5").unwrap();
        assert_eq!(apply(items(), &query).len(), 2);
        let query = parse("%24filter=priority+le+10").unwrap();
        assert_eq!(apply(items(), &query).len(), 2);
        let query = parse("%24filter=alias+ne+%27vendor.com%27").unwrap();
        assert_eq!(apply(items(), &query).len(), 2);
    }

    #[test]
    fn filter_functions_work_on_strings() {
        let query = parse("%24filter=contains%28alias%2C%27openai%27%29").unwrap();
        assert_eq!(apply(items(), &query).len(), 1);
        let query = parse("%24filter=startswith%28alias%2C%27api.%27%29").unwrap();
        assert_eq!(apply(items(), &query).len(), 2);
        let query = parse("%24filter=endswith%28alias%2C%27.com%27%29").unwrap();
        assert_eq!(apply(items(), &query).len(), 3);
    }

    #[test]
    fn filter_conjunction() {
        let query = parse("%24filter=enabled+eq+true+and+priority+gt+15").unwrap();
        let result = apply(items(), &query);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], "3");
    }

    #[test]
    fn filter_matches_inside_an_array() {
        let query = parse("%24filter=tags+eq+%27llm%27").unwrap();
        let result = apply(items(), &query);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], "1");
    }

    #[test]
    fn order_by_ascending_and_descending() {
        let query = parse("%24orderby=priority+desc").unwrap();
        let result = apply(items(), &query);
        assert_eq!(result[0]["id"], "3");
        let query = parse("%24orderby=priority").unwrap();
        let result = apply(items(), &query);
        assert_eq!(result[0]["id"], "2");
    }

    #[test]
    fn top_and_skip_page_the_result() {
        let query = parse("%24top=2").unwrap();
        assert_eq!(apply(items(), &query).len(), 2);
        let query = parse("%24skip=2").unwrap();
        let result = apply(items(), &query);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], "3");
    }

    #[test]
    fn select_projects_named_fields() {
        let query = parse("%24select=id%2Calias").unwrap();
        let result = apply(items(), &query);
        assert_eq!(result[0].as_object().unwrap().len(), 2);
        assert!(result[0].get("enabled").is_none());
    }

    #[test]
    fn malformed_parameters_are_rejected() {
        assert!(parse("%24top=abc").is_err());
        assert!(parse("%24top=1000").is_err());
        assert!(parse("%24skip=-1").is_err());
        assert!(parse("%24orderby=alias+sideways").is_err());
        assert!(parse("%24orderby=").is_err());
        assert!(parse("%24select=").is_err());
        assert!(parse("%24filter=alias+like+%27x%27").is_err());
        assert!(parse("%24filter=alias").is_err());
        assert!(parse("%24expand=routes").is_err());
    }

    #[test]
    fn unprefixed_parameters_are_ignored() {
        assert!(parse("page=2&limit=10").is_ok());
    }

    #[test]
    fn nested_fields_are_reachable() {
        let items = vec![json!({"match": {"http": {"path": "/v1/chat"}}})];
        let query = parse("%24filter=match.http.path+eq+%27%2Fv1%2Fchat%27").unwrap();
        assert_eq!(apply(items, &query).len(), 1);
    }

    #[test]
    fn quoted_and_escaped_literals() {
        assert_eq!(parse_literal("'it''s'").unwrap(), json!("it's"));
        assert_eq!(parse_literal("'a and b'").unwrap(), json!("a and b"));
        let query = parse_filter("alias eq 'a and b'").unwrap();
        assert_eq!(query.len(), 1, "` and ` inside quotes is not a separator");
        assert_eq!(query[0].value, json!("a and b"));
    }
}
