//! A deliberately small subset of OData v4 for the list endpoints:
//! `$filter` (`eq` / `ne` clauses joined by `and`), `$select`, `$orderby`,
//! `$top` and `$skip`.

use serde_json::Value;

use crate::domain::dto::ListQuery;
use crate::domain::error::{OagwError, OagwResult};

/// Default and maximum page sizes from `DESIGN.md` §3.3 "List Query Parameters".
pub const DEFAULT_TOP: usize = 50;
pub const MAX_TOP: usize = 100;

/// Parse the OData members of a query string.
///
/// # Errors
/// Returns `400 ValidationError` for a malformed `$top` / `$skip`.
pub fn parse_list_query(raw_query: Option<&str>) -> OagwResult<ListQuery> {
    let mut out = ListQuery::default();
    let Some(raw) = raw_query else {
        return Ok(out);
    };
    for (key, value) in form_urlencoded::parse(raw.as_bytes()) {
        match key.as_ref() {
            "$filter" => out.filter = Some(value.into_owned()),
            "$select" => {
                out.select = Some(
                    value
                        .split(',')
                        .map(|s| s.trim().to_owned())
                        .filter(|s| !s.is_empty())
                        .collect(),
                );
            }
            "$orderby" => {
                let mut specs = Vec::new();
                for clause in value.split(',') {
                    let mut parts = clause.split_whitespace();
                    let Some(field) = parts.next() else { continue };
                    let desc = parts
                        .next()
                        .is_some_and(|d| d.eq_ignore_ascii_case("desc"));
                    specs.push((field.to_owned(), desc));
                }
                out.orderby = Some(specs);
            }
            "$top" => {
                let n: usize = value.parse().map_err(|_| {
                    OagwError::validation(format!("$top must be a non-negative integer: {value}"))
                })?;
                out.top = Some(n.min(MAX_TOP));
            }
            "$skip" => {
                let n: usize = value.parse().map_err(|_| {
                    OagwError::validation(format!("$skip must be a non-negative integer: {value}"))
                })?;
                out.skip = Some(n);
            }
            _ => {}
        }
    }
    Ok(out)
}

/// A single `field <op> literal` comparison.
struct Clause<'a> {
    field: &'a str,
    negated: bool,
    literal: String,
}

fn parse_filter(filter: &str) -> OagwResult<Vec<Clause<'_>>> {
    let mut clauses = Vec::new();
    for part in split_on_and(filter) {
        let tokens: Vec<&str> = part.trim().splitn(3, char::is_whitespace).collect();
        if tokens.len() != 3 {
            return Err(OagwError::validation(format!(
                "unsupported $filter clause: '{part}' (expected `field eq 'value'`)"
            )));
        }
        let negated = match tokens[1].to_ascii_lowercase().as_str() {
            "eq" => false,
            "ne" => true,
            other => {
                return Err(OagwError::validation(format!(
                    "unsupported $filter operator '{other}' (only eq and ne are supported)"
                )));
            }
        };
        let literal = tokens[2]
            .trim()
            .trim_matches('\'')
            .trim_matches('"')
            .to_owned();
        clauses.push(Clause {
            field: tokens[0].trim(),
            negated,
            literal,
        });
    }
    Ok(clauses)
}

/// Split on the ` and ` keyword, ignoring occurrences inside quotes.
fn split_on_and(filter: &str) -> Vec<&str> {
    let lower = filter.to_ascii_lowercase();
    let bytes = filter.as_bytes();
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut in_quote = false;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            in_quote = !in_quote;
            i += 1;
            continue;
        }
        if !in_quote && lower[i..].starts_with(" and ") {
            parts.push(&filter[start..i]);
            i += 5;
            start = i;
            continue;
        }
        i += 1;
    }
    parts.push(&filter[start..]);
    parts
}

/// Apply `$filter`, `$orderby`, `$skip`, `$top` and `$select` to a set of
/// JSON projections, in that order.
///
/// # Errors
/// Returns `400 ValidationError` for an unsupported `$filter` expression.
pub fn apply(query: &ListQuery, mut rows: Vec<Value>) -> OagwResult<Vec<Value>> {
    if let Some(filter) = query.filter.as_deref() {
        let clauses = parse_filter(filter)?;
        rows.retain(|row| {
            clauses.iter().all(|c| {
                let actual = row.get(c.field).map(scalar_to_string);
                let matches = actual.as_deref() == Some(c.literal.as_str());
                matches != c.negated
            })
        });
    }

    if let Some(order) = query.orderby.as_ref() {
        rows.sort_by(|a, b| {
            for (field, desc) in order {
                let av = a.get(field).map(scalar_to_string).unwrap_or_default();
                let bv = b.get(field).map(scalar_to_string).unwrap_or_default();
                let ord = av.cmp(&bv);
                let ord = if *desc { ord.reverse() } else { ord };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
    }

    let skip = query.skip.unwrap_or(0);
    if skip > 0 {
        rows = rows.into_iter().skip(skip).collect();
    }
    let top = query.top.unwrap_or(DEFAULT_TOP);
    rows.truncate(top);

    if let Some(select) = query.select.as_ref() {
        rows = rows
            .into_iter()
            .map(|row| {
                let mut projected = serde_json::Map::new();
                if let Some(obj) = row.as_object() {
                    for field in select {
                        if let Some(v) = obj.get(field) {
                            projected.insert(field.clone(), v.clone());
                        }
                    }
                }
                Value::Object(projected)
            })
            .collect();
    }
    Ok(rows)
}

fn scalar_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rows() -> Vec<Value> {
        vec![
            json!({"id": "1", "alias": "api.openai.com", "enabled": true}),
            json!({"id": "2", "alias": "vendor.com", "enabled": false}),
            json!({"id": "3", "alias": "my-service", "enabled": true}),
        ]
    }

    #[test]
    fn filter_by_equality() {
        let q = parse_list_query(Some("$filter=alias%20eq%20%27vendor.com%27")).expect("parse");
        let out = apply(&q, rows()).expect("apply");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], "2");
    }

    #[test]
    fn filter_by_inequality_and_boolean() {
        let q = parse_list_query(Some("$filter=enabled eq true")).expect("parse");
        let out = apply(&q, rows()).expect("apply");
        assert_eq!(out.len(), 2);

        let q = parse_list_query(Some("$filter=alias ne 'vendor.com'")).expect("parse");
        let out = apply(&q, rows()).expect("apply");
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn conjunction_is_supported() {
        let q = parse_list_query(Some("$filter=enabled eq true and alias eq 'my-service'"))
            .expect("parse");
        let out = apply(&q, rows()).expect("apply");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], "3");
    }

    #[test]
    fn select_projects_only_requested_fields() {
        let q = parse_list_query(Some("$select=id,alias")).expect("parse");
        let out = apply(&q, rows()).expect("apply");
        assert_eq!(out[0].as_object().expect("object").len(), 2);
        assert!(out[0].get("enabled").is_none());
    }

    #[test]
    fn orderby_and_paging() {
        let q = parse_list_query(Some("$orderby=alias desc&$top=2")).expect("parse");
        let out = apply(&q, rows()).expect("apply");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["alias"], "vendor.com");

        let q = parse_list_query(Some("$orderby=alias&$skip=1&$top=1")).expect("parse");
        let out = apply(&q, rows()).expect("apply");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["alias"], "my-service");
    }

    #[test]
    fn top_is_clamped_to_the_documented_maximum() {
        let q = parse_list_query(Some("$top=1000")).expect("parse");
        assert_eq!(q.top, Some(MAX_TOP));
    }

    #[test]
    fn malformed_paging_is_a_validation_error() {
        assert!(parse_list_query(Some("$top=abc")).is_err());
        assert!(parse_list_query(Some("$skip=-1")).is_err());
    }

    #[test]
    fn unsupported_filter_operators_are_rejected() {
        let q = parse_list_query(Some("$filter=alias gt 'a'")).expect("parse");
        assert!(apply(&q, rows()).is_err());
    }
}
