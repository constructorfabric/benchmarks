//! `OData` system query options for the list endpoints: `$filter`,
//! `$select`, `$orderby`, `$top`, `$skip` (DESIGN "List Query Parameters").
//!
//! Filtering, ordering and projection run over the serialized DTOs, so one
//! implementation serves upstreams, routes and plugins and stays honest about
//! the field names a client sees on the wire.

use std::collections::HashMap;

use serde_json::Value;
use toolkit::{Page, PageInfo};

use crate::domain::error::OagwError;

/// Default page size when `$top` is omitted.
pub const DEFAULT_TOP: usize = 50;
/// Largest page size a client may request.
pub const MAX_TOP: usize = 100;

/// A parsed list query.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// Conjunction of comparisons.
    pub filter: Vec<Comparison>,
    /// Fields to project, in request order.
    pub select: Vec<String>,
    /// Sort keys, most significant first.
    pub order_by: Vec<OrderKey>,
    /// Page size.
    pub top: usize,
    /// Offset.
    pub skip: usize,
}

/// One `$filter` comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    /// Field name.
    pub field: String,
    /// Operator.
    pub op: CompareOp,
    /// Right-hand literal, with quotes stripped.
    pub literal: String,
}

/// Supported `$filter` operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    /// Equality.
    Eq,
    /// Inequality.
    Ne,
}

/// One `$orderby` key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderKey {
    /// Field name.
    pub field: String,
    /// `true` for `desc`.
    pub descending: bool,
}

impl ListQuery {
    /// Parse the query options out of a decoded query-parameter map.
    ///
    /// # Errors
    ///
    /// Returns `400` for a malformed `$filter`, `$orderby`, `$top` or
    /// `$skip`. Unknown, non-`$`-prefixed parameters are ignored so a client
    /// may pass its own bookkeeping through.
    pub fn parse(params: &HashMap<String, String>) -> Result<Self, OagwError> {
        let mut query = Self {
            top: DEFAULT_TOP,
            ..Self::default()
        };
        if let Some(raw) = params.get("$filter") {
            query.filter = parse_filter(raw)?;
        }
        if let Some(raw) = params.get("$select") {
            query.select = raw
                .split(',')
                .map(|field| field.trim().to_owned())
                .filter(|field| !field.is_empty())
                .collect();
            if query.select.is_empty() {
                return Err(OagwError::validation(
                    "$select must name at least one field",
                ));
            }
        }
        if let Some(raw) = params.get("$orderby") {
            query.order_by = parse_order_by(raw)?;
        }
        if let Some(raw) = params.get("$top") {
            let top: usize = raw
                .trim()
                .parse()
                .map_err(|_| OagwError::validation("$top must be a non-negative integer"))?;
            if top > MAX_TOP {
                return Err(OagwError::validation(format!(
                    "$top must not exceed {MAX_TOP}"
                )));
            }
            query.top = top;
        }
        if let Some(raw) = params.get("$skip") {
            query.skip = raw
                .trim()
                .parse()
                .map_err(|_| OagwError::validation("$skip must be a non-negative integer"))?;
        }
        Ok(query)
    }

    /// Apply the query to a list of serialized DTOs.
    #[must_use]
    pub fn apply(&self, items: Vec<Value>) -> Page<Value> {
        let mut filtered: Vec<Value> = items
            .into_iter()
            .filter(|item| self.filter.iter().all(|c| c.matches(item)))
            .collect();
        for key in self.order_by.iter().rev() {
            filtered.sort_by(|a, b| {
                let ordering = compare_field(a, b, &key.field);
                if key.descending {
                    ordering.reverse()
                } else {
                    ordering
                }
            });
        }
        let total = filtered.len();
        let page: Vec<Value> = filtered
            .into_iter()
            .skip(self.skip)
            .take(self.top)
            .map(|item| project(item, &self.select))
            .collect();
        let next_cursor = if self.skip + page.len() < total && !page.is_empty() {
            Some((self.skip + page.len()).to_string())
        } else {
            None
        };
        Page {
            items: page,
            page_info: PageInfo {
                next_cursor,
                prev_cursor: None,
                limit: self.top as u64,
            },
        }
    }
}

impl Comparison {
    /// Evaluate the comparison against one item.
    #[must_use]
    pub fn matches(&self, item: &Value) -> bool {
        let equal = match item.get(&self.field) {
            None => false,
            Some(Value::String(text)) => text == &self.literal,
            Some(Value::Bool(flag)) => self.literal.eq_ignore_ascii_case(&flag.to_string()),
            Some(Value::Number(number)) => number.to_string() == self.literal,
            Some(Value::Null) => self.literal.eq_ignore_ascii_case("null"),
            Some(Value::Array(values)) => values
                .iter()
                .any(|value| value.as_str() == Some(self.literal.as_str())),
            Some(other) => *other == Value::String(self.literal.clone()),
        };
        match self.op {
            CompareOp::Eq => equal,
            CompareOp::Ne => !equal,
        }
    }
}

fn parse_filter(raw: &str) -> Result<Vec<Comparison>, OagwError> {
    let mut out = Vec::new();
    for clause in split_conjunction(raw) {
        let tokens: Vec<&str> = clause.split_whitespace().collect();
        let [field, op, rest @ ..] = tokens.as_slice() else {
            return Err(OagwError::validation(format!(
                "$filter clause '{clause}' is not of the form '<field> eq <value>'"
            )));
        };
        let op = match op.to_ascii_lowercase().as_str() {
            "eq" => CompareOp::Eq,
            "ne" => CompareOp::Ne,
            other => {
                return Err(OagwError::validation(format!(
                    "$filter operator '{other}' is not supported; use 'eq' or 'ne'"
                )));
            }
        };
        if rest.is_empty() {
            return Err(OagwError::validation(format!(
                "$filter clause '{clause}' is missing a value"
            )));
        }
        let literal = rest.join(" ");
        let literal = literal
            .strip_prefix('\'')
            .and_then(|value| value.strip_suffix('\''))
            .unwrap_or(&literal)
            .to_owned();
        out.push(Comparison {
            field: (*field).to_owned(),
            op,
            literal,
        });
    }
    if out.is_empty() {
        return Err(OagwError::validation("$filter must not be empty"));
    }
    Ok(out)
}

/// Split on the `and` keyword, on whitespace boundaries only, so a value
/// containing the letters "and" is not torn apart.
fn split_conjunction(raw: &str) -> Vec<String> {
    let mut clauses = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    for token in raw.split_whitespace() {
        if token.eq_ignore_ascii_case("and") && current.len() >= 3 {
            clauses.push(current.join(" "));
            current.clear();
        } else {
            current.push(token);
        }
    }
    if !current.is_empty() {
        clauses.push(current.join(" "));
    }
    clauses
}

fn parse_order_by(raw: &str) -> Result<Vec<OrderKey>, OagwError> {
    let mut out = Vec::new();
    for clause in raw.split(',') {
        let tokens: Vec<&str> = clause.split_whitespace().collect();
        match tokens.as_slice() {
            [field] => out.push(OrderKey {
                field: (*field).to_owned(),
                descending: false,
            }),
            [field, direction] => {
                let descending = match direction.to_ascii_lowercase().as_str() {
                    "asc" => false,
                    "desc" => true,
                    other => {
                        return Err(OagwError::validation(format!(
                            "$orderby direction '{other}' must be 'asc' or 'desc'"
                        )));
                    }
                };
                out.push(OrderKey {
                    field: (*field).to_owned(),
                    descending,
                });
            }
            _ => {
                return Err(OagwError::validation(format!(
                    "$orderby clause '{clause}' is not of the form '<field> [asc|desc]'"
                )));
            }
        }
    }
    if out.is_empty() {
        return Err(OagwError::validation(
            "$orderby must name at least one field",
        ));
    }
    Ok(out)
}

fn compare_field(a: &Value, b: &Value, field: &str) -> std::cmp::Ordering {
    match (a.get(field), b.get(field)) {
        (Some(Value::String(left)), Some(Value::String(right))) => left.cmp(right),
        (Some(Value::Number(left)), Some(Value::Number(right))) => left
            .as_f64()
            .partial_cmp(&right.as_f64())
            .unwrap_or(std::cmp::Ordering::Equal),
        (Some(Value::Bool(left)), Some(Value::Bool(right))) => left.cmp(right),
        (Some(left), Some(right)) => left.to_string().cmp(&right.to_string()),
        (Some(_), None) => std::cmp::Ordering::Greater,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// Keep only the selected fields. An empty selection keeps everything.
#[must_use]
pub fn project(item: Value, select: &[String]) -> Value {
    if select.is_empty() {
        return item;
    }
    let Value::Object(map) = item else {
        return item;
    };
    let mut out = serde_json::Map::new();
    for field in select {
        if let Some(value) = map.get(field) {
            out.insert(field.clone(), value.clone());
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::{CompareOp, ListQuery, MAX_TOP, project};
    use serde_json::json;
    use std::collections::HashMap;

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn items() -> Vec<serde_json::Value> {
        vec![
            json!({ "id": "u1", "alias": "api.openai.com", "enabled": true, "priority": 3 }),
            json!({ "id": "u2", "alias": "api.anthropic.com", "enabled": false, "priority": 1 }),
            json!({ "id": "u3", "alias": "vendor.com", "enabled": true, "priority": 2 }),
        ]
    }

    #[test]
    fn defaults_when_nothing_is_requested() {
        let query = ListQuery::parse(&HashMap::new()).expect("parses");
        assert_eq!(query.top, 50);
        assert_eq!(query.skip, 0);
        assert!(query.filter.is_empty());
        let page = query.apply(items());
        assert_eq!(page.items.len(), 3);
        assert_eq!(page.page_info.limit, 50);
        assert!(page.page_info.next_cursor.is_none());
    }

    #[test]
    fn filter_on_a_quoted_string() {
        let query =
            ListQuery::parse(&params(&[("$filter", "alias eq 'api.openai.com'")])).expect("parses");
        assert_eq!(query.filter[0].op, CompareOp::Eq);
        assert_eq!(query.filter[0].literal, "api.openai.com");
        let page = query.apply(items());
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0]["id"], "u1");
    }

    #[test]
    fn filter_on_a_boolean_and_an_unquoted_value() {
        let page = ListQuery::parse(&params(&[("$filter", "enabled eq true")]))
            .expect("parses")
            .apply(items());
        assert_eq!(page.items.len(), 2);
    }

    #[test]
    fn filter_supports_ne_and_conjunction() {
        let page = ListQuery::parse(&params(&[(
            "$filter",
            "enabled eq true and alias ne 'vendor.com'",
        )]))
        .expect("parses")
        .apply(items());
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0]["id"], "u1");
    }

    #[test]
    fn a_malformed_filter_is_rejected() {
        for bad in ["alias", "alias like 'x'", "alias eq", ""] {
            assert_eq!(
                ListQuery::parse(&params(&[("$filter", bad)]))
                    .expect_err("rejected")
                    .status,
                400,
                "{bad}"
            );
        }
    }

    #[test]
    fn order_by_ascending_and_descending() {
        let ascending = ListQuery::parse(&params(&[("$orderby", "alias")]))
            .expect("parses")
            .apply(items());
        assert_eq!(ascending.items[0]["alias"], "api.anthropic.com");

        let descending = ListQuery::parse(&params(&[("$orderby", "priority desc")]))
            .expect("parses")
            .apply(items());
        assert_eq!(descending.items[0]["priority"], 3);
    }

    #[test]
    fn a_malformed_orderby_is_rejected() {
        assert_eq!(
            ListQuery::parse(&params(&[("$orderby", "alias sideways")]))
                .expect_err("rejected")
                .status,
            400
        );
    }

    #[test]
    fn top_and_skip_page_the_result() {
        let query = ListQuery::parse(&params(&[("$top", "2"), ("$skip", "1")])).expect("parses");
        let page = query.apply(items());
        assert_eq!(page.items.len(), 2);
        assert_eq!(page.items[0]["id"], "u2");
        assert!(
            page.page_info.next_cursor.is_none(),
            "the window reaches the end of the collection"
        );

        let first = ListQuery::parse(&params(&[("$top", "1")]))
            .expect("parses")
            .apply(items());
        assert_eq!(first.page_info.next_cursor.as_deref(), Some("1"));
    }

    #[test]
    fn top_is_capped() {
        assert_eq!(
            ListQuery::parse(&params(&[("$top", (MAX_TOP + 1).to_string().as_str())]))
                .expect_err("rejected")
                .status,
            400
        );
        assert_eq!(
            ListQuery::parse(&params(&[("$top", "-1")]))
                .expect_err("rejected")
                .status,
            400
        );
    }

    #[test]
    fn select_projects_the_named_fields() {
        let page = ListQuery::parse(&params(&[("$select", "id,alias")]))
            .expect("parses")
            .apply(items());
        let first = page.items[0].as_object().expect("object");
        assert_eq!(first.len(), 2);
        assert!(first.contains_key("id"));
        assert!(first.contains_key("alias"));
        assert!(!first.contains_key("enabled"));
    }

    #[test]
    fn an_empty_select_is_rejected() {
        assert_eq!(
            ListQuery::parse(&params(&[("$select", " , ")]))
                .expect_err("rejected")
                .status,
            400
        );
    }

    #[test]
    fn unknown_plain_parameters_are_ignored() {
        ListQuery::parse(&params(&[("page", "2")])).expect("ignored");
    }

    #[test]
    fn projection_of_an_empty_selection_is_the_identity() {
        let value = json!({ "a": 1 });
        assert_eq!(project(value.clone(), &[]), value);
    }
}
