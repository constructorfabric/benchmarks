//! Minimal OData-style list binding for the OAGW management API (DESIGN
//! §3.3 "List Query Parameters").
//!
//! The toolkit ships a cursor-based `OData` extractor that deliberately
//! rejects `$skip`; OAGW's contract requires offset pagination, so the list
//! endpoints bind `$filter` / `$select` / `$orderby` / `$top` / `$skip`
//! themselves through this module.

use std::collections::HashMap;

use serde_json::Value;

/// Default page size when `$top` is absent.
pub const DEFAULT_TOP: usize = 50;
/// Maximum page size; larger `$top` values are clamped.
pub const MAX_TOP: usize = 100;

/// Parsed list-query parameters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListParams {
    /// Raw `$filter` expression.
    pub filter: Option<String>,
    /// Raw comma-separated `$select` field list.
    pub select: Option<String>,
    /// Raw comma-separated `$orderby` clauses.
    pub orderby: Option<String>,
    /// Page size (default [`DEFAULT_TOP`], clamped to [`MAX_TOP`]).
    pub top: usize,
    /// Number of leading results to skip.
    pub skip: usize,
}

impl ListParams {
    /// Parse the raw query map. `$top` / `$skip` must be non-negative
    /// integers when present.
    ///
    /// # Errors
    ///
    /// Returns a human-readable reason for malformed `$top` / `$skip`.
    pub fn parse(raw: &HashMap<String, String>) -> Result<Self, String> {
        let get = |key: &str| raw.get(key).map(String::as_str).filter(|s| !s.is_empty());
        let top = match get("$top") {
            Some(s) => s
                .parse::<usize>()
                .map_err(|_| format!("$top must be a non-negative integer, got {s:?}"))?
                .min(MAX_TOP),
            None => DEFAULT_TOP,
        };
        let skip = match get("$skip") {
            Some(s) => s
                .parse::<usize>()
                .map_err(|_| format!("$skip must be a non-negative integer, got {s:?}"))?,
            None => 0,
        };
        Ok(Self {
            filter: get("$filter").map(str::to_owned),
            select: get("$select").map(str::to_owned),
            orderby: get("$orderby").map(str::to_owned),
            top,
            skip,
        })
    }
}

/// A field of `T` that can be filtered and sorted on.
///
/// `get` renders the field as its wire string; `case_insensitive` controls
/// both `eq`/`ne` matching and ordering.
#[derive(Clone, Copy)]
pub struct FieldAccessor<T> {
    /// Wire field name, as it appears in `$filter` / `$orderby`.
    pub name: &'static str,
    /// Extract the field's string value.
    pub get: fn(&T) -> String,
    /// Whether comparisons fold case.
    pub case_insensitive: bool,
}

impl<T> FieldAccessor<T> {
    /// The field's comparison value for `item`.
    #[must_use]
    pub fn value_of(&self, item: &T) -> String {
        let v = (self.get)(item);
        if self.case_insensitive {
            v.to_ascii_lowercase()
        } else {
            v
        }
    }
}

/// A parsed `$filter` expression: `field op 'value'` (or a bare field name,
/// which acts as a presence filter).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FilterExpr {
    /// `field eq 'value'` / `field ne 'value'`.
    Compare {
        /// Accessor index into the field allowlist.
        index: usize,
        /// `eq` or `ne`.
        op: FilterOp,
        /// Case-folded expected value.
        value: String,
    },
    /// Bare field name — matches when the field is non-empty.
    Present { index: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FilterOp {
    Eq,
    Ne,
}

/// Parse a single-field accessor lookup, resolving against the allowlist.
fn resolve_accessor<T>(fields: &[FieldAccessor<T>], name: &str) -> Result<usize, String> {
    fields
        .iter()
        .position(|f| f.name == name)
        .ok_or_else(|| format!("unknown list field {name:?}"))
}

/// Quote-aware tokenizer for `$filter` expressions.
///
/// A token may be bare (`alias`) or single-quoted (`'hello world'`); quoted
/// values may contain whitespace and doubled single quotes (`''`) as an
/// escape for a literal quote. Unterminated quotes and stray quote characters
/// inside bare tokens are rejected rather than half-parsed.
fn lex_filter(expr: &str) -> Result<Vec<String>, String> {
    let chars: Vec<char> = expr.chars().collect();
    let mut tokens: Vec<String> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        if chars[i] == '\'' {
            let mut value = String::new();
            i += 1;
            let mut closed = false;
            while i < chars.len() {
                if chars[i] == '\'' {
                    if i + 1 < chars.len() && chars[i + 1] == '\'' {
                        value.push('\'');
                        i += 2;
                    } else {
                        closed = true;
                        i += 1;
                        break;
                    }
                } else {
                    value.push(chars[i]);
                    i += 1;
                }
            }
            if !closed {
                return Err(format!("unterminated quoted value in $filter {expr:?}"));
            }
            tokens.push(value);
        } else {
            let mut token = String::new();
            while i < chars.len() && !chars[i].is_whitespace() && chars[i] != '\'' {
                token.push(chars[i]);
                i += 1;
            }
            if token.is_empty() {
                return Err(format!("malformed $filter expression {expr:?}"));
            }
            tokens.push(token);
        }
    }
    Ok(tokens)
}

/// Parse the `$filter` expression against the field allowlist.
///
/// # Errors
///
/// Returns a human-readable reason for unknown fields or a malformed filter.
pub(crate) fn parse_filter<T>(
    expr: &str,
    fields: &[FieldAccessor<T>],
) -> Result<FilterExpr, String> {
    let tokens = lex_filter(expr)?;
    match tokens.as_slice() {
        [field] => {
            let index = resolve_accessor(fields, field)?;
            Ok(FilterExpr::Present { index })
        }
        [field, op, value] => {
            let index = resolve_accessor(fields, field)?;
            let op = match op.as_str() {
                "eq" => FilterOp::Eq,
                "ne" => FilterOp::Ne,
                other => {
                    return Err(format!(
                        "unsupported filter operator {other:?} (use eq or ne)"
                    ));
                }
            };
            let value = if fields[index].case_insensitive {
                value.to_ascii_lowercase()
            } else {
                value.clone()
            };
            Ok(FilterExpr::Compare { index, op, value })
        }
        _ => Err(format!("malformed $filter expression {expr:?}")),
    }
}

/// An ordered list of sort keys: `field` + optional `asc`/`desc` suffix.
struct SortKey {
    index: usize,
    descending: bool,
}

/// Parse the comma-separated `$orderby` expression.
///
/// # Errors
///
/// Returns a human-readable reason for unknown fields or malformed clauses.
fn parse_orderby<T>(expr: &str, fields: &[FieldAccessor<T>]) -> Result<Vec<SortKey>, String> {
    expr.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|clause| {
            let mut parts = clause.split_whitespace();
            let name = parts
                .next()
                .ok_or_else(|| format!("malformed $orderby clause {clause:?}"))?;
            let direction = parts.next().unwrap_or("asc");
            if parts.next().is_some() {
                return Err(format!("malformed $orderby clause {clause:?}"));
            }
            let descending = match direction {
                "asc" => false,
                "desc" => true,
                other => {
                    return Err(format!(
                        "unsupported sort direction {other:?} (use asc or desc)"
                    ));
                }
            };
            let index = resolve_accessor(fields, name)?;
            Ok(SortKey { index, descending })
        })
        .collect()
}

/// Apply `$filter` and `$orderby` to the items, then slice by `$skip`/`$top`.
///
/// Filtering and ordering happen **before** pagination so the page is stable
/// across calls. Unknown filter/order fields are rejected rather than
/// silently ignored.
///
/// # Errors
///
/// Returns a human-readable reason for unknown fields or malformed clauses.
pub fn apply_filter_order<T: Clone>(
    items: Vec<T>,
    params: &ListParams,
    fields: &[FieldAccessor<T>],
) -> Result<Vec<T>, String> {
    let mut out = items;
    if let Some(filter) = &params.filter {
        let expr = parse_filter(filter, fields)?;
        out.retain(|item| match expr {
            FilterExpr::Present { index } => !fields[index].value_of(item).is_empty(),
            FilterExpr::Compare {
                index,
                op,
                ref value,
            } => {
                let actual = fields[index].value_of(item);
                match op {
                    FilterOp::Eq => actual == *value,
                    FilterOp::Ne => actual != *value,
                }
            }
        });
    }
    if let Some(orderby) = &params.orderby {
        let keys = parse_orderby(orderby, fields)?;
        if !keys.is_empty() {
            out.sort_by(|a, b| {
                for key in &keys {
                    let av = fields[key.index].value_of(a);
                    let bv = fields[key.index].value_of(b);
                    let ordering = av.cmp(&bv);
                    let ordering = if key.descending {
                        ordering.reverse()
                    } else {
                        ordering
                    };
                    if ordering != std::cmp::Ordering::Equal {
                        return ordering;
                    }
                }
                std::cmp::Ordering::Equal
            });
        }
    }
    let out = out.into_iter().skip(params.skip).take(params.top).collect();
    Ok(out)
}

/// Keep only the comma-separated `$select` fields of a serialized entity.
///
/// A missing/blank `$select` returns the entity unchanged.
#[must_use]
pub fn project(value: &Value, select: Option<&str>) -> Value {
    let Some(select) = select else {
        return value.clone();
    };
    let fields: Vec<String> = select
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    if fields.is_empty() {
        return value.clone();
    }
    let Value::Object(map) = value else {
        return value.clone();
    };
    let mut out = serde_json::Map::new();
    for name in fields {
        if let Some(v) = map.get(&name) {
            out.insert(name, v.clone());
        }
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct Row {
        id: String,
        alias: String,
        enabled: bool,
    }

    fn fields() -> Vec<FieldAccessor<Row>> {
        vec![
            FieldAccessor {
                name: "id",
                get: |r| r.id.clone(),
                case_insensitive: false,
            },
            FieldAccessor {
                name: "alias",
                get: |r| r.alias.clone(),
                case_insensitive: true,
            },
            FieldAccessor {
                name: "enabled",
                get: |r| r.enabled.to_string(),
                case_insensitive: false,
            },
        ]
    }

    fn rows() -> Vec<Row> {
        vec![
            Row {
                id: "3".to_owned(),
                alias: "Bravo".to_owned(),
                enabled: true,
            },
            Row {
                id: "1".to_owned(),
                alias: "alpha".to_owned(),
                enabled: false,
            },
            Row {
                id: "2".to_owned(),
                alias: "Bravo".to_owned(),
                enabled: true,
            },
        ]
    }

    fn params(raw: &[(&str, &str)]) -> ListParams {
        let map: HashMap<String, String> = raw
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        ListParams::parse(&map).expect("valid params")
    }

    #[test]
    fn default_pagination() {
        let p = params(&[]);
        assert_eq!((p.top, p.skip), (DEFAULT_TOP, 0));
        assert_eq!(p.filter, None);
    }

    #[test]
    fn top_is_clamped_and_skip_parsed() {
        let p = params(&[("$top", "1000"), ("$skip", "7")]);
        assert_eq!((p.top, p.skip), (MAX_TOP, 7));
    }

    #[test]
    fn malformed_numeric_params_rejected() {
        let map = HashMap::from([("$top".to_owned(), "abc".to_owned())]);
        assert!(ListParams::parse(&map).is_err());
    }

    #[test]
    fn filter_eq_is_case_insensitive_when_flagged() {
        let p = params(&[("$filter", "alias eq 'bravo'")]);
        let out = apply_filter_order(rows(), &p, &fields()).expect("valid");
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|r| r.alias == "Bravo"));
    }

    #[test]
    fn filter_ne_and_unknown_field() {
        let p = params(&[("$filter", "enabled ne 'true'")]);
        let out = apply_filter_order(rows(), &p, &fields()).expect("valid");
        assert_eq!(out.len(), 1);
        assert!(!out[0].enabled);

        let bad = params(&[("$filter", "nope eq 'x'")]);
        assert!(apply_filter_order(rows(), &bad, &fields()).is_err());
    }

    #[test]
    fn orderby_desc_then_asc() {
        let p = params(&[("$orderby", "alias desc, id asc")]);
        let out = apply_filter_order(rows(), &p, &fields()).expect("valid");
        let aliases: Vec<&str> = out.iter().map(|r| r.alias.as_str()).collect();
        assert_eq!(aliases, vec!["Bravo", "Bravo", "alpha"]);
        assert_eq!(out[0].id, "2"); // id asc breaks the Bravo tie
        assert_eq!(out[1].id, "3");
    }

    #[test]
    fn presence_filter() {
        let p = params(&[("$filter", "id")]);
        let out = apply_filter_order(rows(), &p, &fields()).expect("valid");
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn skip_and_top_slice_after_sorting() {
        let p = params(&[("$orderby", "id"), ("$top", "1"), ("$skip", "1")]);
        let out = apply_filter_order(rows(), &p, &fields()).expect("valid");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "2");
    }

    #[test]
    fn project_selects_only_requested_fields() {
        let value = serde_json::json!({"id": "x", "alias": "a", "server": {"endpoints": []}});
        assert_eq!(
            project(&value, Some("id,alias")),
            serde_json::json!({"id": "x", "alias": "a"})
        );
        assert_eq!(project(&value, Some("missing")), serde_json::json!({}));
        assert_eq!(project(&value, None), value);
    }

    #[test]
    fn quoted_values_may_contain_spaces_and_escaped_quotes() {
        // The lexer keeps a quoted value as one token even with spaces.
        let p = params(&[("$filter", "alias eq 'two words'")]);
        let expr = parse_filter(&p.filter.expect("filter"), &fields()).expect("parse");
        assert_eq!(
            expr,
            FilterExpr::Compare {
                index: 1,
                op: FilterOp::Eq,
                value: "two words".to_owned(),
            }
        );

        // Doubled quotes escape a literal quote (OData convention).
        let p = params(&[("$filter", "alias eq 'it''s'")]);
        let expr = parse_filter(&p.filter.expect("filter"), &fields()).expect("parse");
        assert_eq!(
            expr,
            FilterExpr::Compare {
                index: 1,
                op: FilterOp::Eq,
                value: "it's".to_owned(),
            }
        );
    }

    #[test]
    fn unterminated_quote_is_rejected() {
        let map = HashMap::from([("$filter".to_owned(), "alias eq 'broken".to_owned())]);
        let p = ListParams::parse(&map).expect("params");
        assert!(parse_filter(&p.filter.expect("filter"), &fields()).is_err());
    }
}
