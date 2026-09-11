//! The OData query options the list endpoints accept (DESIGN §3.3).
//!
//! `$filter`, `$select`, `$orderby`, `$top` and `$skip` are supported over
//! rows that are already `serde_json::Value` objects, so the parser stays
//! independent of which resource is being listed. Only the subset the
//! specification shows is implemented: equality / inequality / `contains`
//! comparisons joined by `and`.

use serde_json::Value;

/// Parsed list options.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListQuery {
    /// Field/value comparisons, all of which must hold.
    pub filters: Vec<(String, FilterOp, String)>,
    /// Fields to project, in the order given.
    pub select: Vec<String>,
    /// Sort keys, in priority order.
    pub orderby: Vec<(String, bool)>,
    /// Maximum number of rows (default 50, max 100).
    pub top: Option<usize>,
    /// Number of rows to skip.
    pub skip: Option<usize>,
}

/// The comparison a `$filter` clause applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    /// `eq`
    Equal,
    /// `ne`
    NotEqual,
    /// `contains(field, 'value')`
    Contains,
}

impl ListQuery {
    /// Parse the options out of a raw query string (the part after `?`).
    #[must_use]
    pub fn parse(query: Option<&str>) -> Self {
        let mut parsed = Self::default();
        let Some(query) = query else {
            return parsed;
        };
        for pair in query.split('&') {
            let Some((name, value)) = pair.split_once('=') else {
                continue;
            };
            let name = percent_decode(name);
            let value = percent_decode(value);
            match name.as_str() {
                "$filter" => parsed.filters = parse_filter(&value),
                "$select" => {
                    parsed.select = value
                        .split(',')
                        .map(str::trim)
                        .filter(|field| !field.is_empty())
                        .map(str::to_owned)
                        .collect();
                }
                "$orderby" => {
                    parsed.orderby = value.split(',').filter_map(parse_order_key).collect();
                }
                "$top" => parsed.top = value.trim().parse::<usize>().ok(),
                "$skip" => parsed.skip = value.trim().parse::<usize>().ok(),
                _ => {}
            }
        }
        parsed
    }

    /// Clamp `top` into the documented range.
    #[must_use]
    pub fn effective_top(&self) -> usize {
        self.top.unwrap_or(50).clamp(1, 100)
    }

    /// Apply the options to `rows`, returning projected rows.
    #[must_use]
    pub fn apply(&self, rows: Vec<Value>) -> Vec<Value> {
        let mut rows: Vec<Value> = rows
            .into_iter()
            .filter(|row| self.filters.iter().all(|filter| matches(row, filter)))
            .collect();
        if !self.orderby.is_empty() {
            rows = sort_rows(rows, &self.orderby);
        }
        if let Some(skip) = self.skip {
            rows = rows.into_iter().skip(skip).collect();
        }
        rows.into_iter()
            .take(self.effective_top())
            .map(|row| project(&row, &self.select))
            .collect()
    }
}

fn matches(row: &Value, (field, op, expected): &(String, FilterOp, String)) -> bool {
    let Some(actual) = row.get(field.as_str()) else {
        return false;
    };
    let actual_text = match actual {
        Value::String(text) => text.clone(),
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        other => other.to_string(),
    };
    match op {
        FilterOp::Equal => actual_text.eq_ignore_ascii_case(expected),
        FilterOp::NotEqual => !actual_text.eq_ignore_ascii_case(expected),
        FilterOp::Contains => actual_text
            .to_ascii_lowercase()
            .contains(&expected.to_ascii_lowercase()),
    }
}

/// Sort rows by a list of `(field, descending)` keys, applied last key first.
fn sort_rows(rows: Vec<Value>, keys: &[(String, bool)]) -> Vec<Value> {
    if keys.is_empty() || rows.len() < 2 {
        return rows;
    }
    // Each key is one stable descending pass; an ascending key reverses the
    // pass, so its equal-keyed rows come back in reverse arrival order.
    let mut sorted = rows;
    for (field, descending) in keys.iter().rev() {
        let field = field.as_str();
        sorted.sort_by_key(|row| std::cmp::Reverse(field_value(row, field)));
        if !descending {
            sorted.reverse();
        }
    }
    sorted
}

/// A comparable rendering of one field of a row; missing fields sort last.
fn field_value(row: &Value, field: &str) -> String {
    match row.get(field) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => format!("{number:030}"),
        Some(Value::Bool(flag)) => format!("{flag}"),
        Some(other) => other.to_string(),
        None => "\u{10ffff}".to_owned(),
    }
}

fn parse_order_key(raw: &str) -> Option<(String, bool)> {
    let mut parts = raw.split_whitespace();
    let field = parts.next()?;
    let descending =
        matches!(parts.next(), Some(direction) if direction.eq_ignore_ascii_case("desc"));
    Some((field.to_owned(), descending))
}

fn parse_filter(raw: &str) -> Vec<(String, FilterOp, String)> {
    raw.split(" and ").filter_map(parse_comparison).collect()
}

fn parse_comparison(raw: &str) -> Option<(String, FilterOp, String)> {
    let raw = raw.trim();
    if let Some(inner) = raw
        .strip_prefix("contains(")
        .and_then(|rest| rest.strip_suffix(')'))
    {
        let (field, value) = inner.split_once(',')?;
        return Some((
            field.trim().to_owned(),
            FilterOp::Contains,
            unquote(value.trim()),
        ));
    }
    for (token, op) in [(" eq ", FilterOp::Equal), (" ne ", FilterOp::NotEqual)] {
        if let Some((field, value)) = raw.split_once(token) {
            return Some((field.trim().to_owned(), op, unquote(value.trim())));
        }
    }
    None
}

fn unquote(raw: &str) -> String {
    raw.trim()
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .unwrap_or(raw)
        .to_owned()
}

/// Project a row onto `fields`, or return it unchanged when nothing is asked.
fn project(row: &Value, select: &[String]) -> Value {
    if select.is_empty() {
        return row.clone();
    }
    let mut projected = serde_json::Map::new();
    for field in select {
        if let Some(value) = row.get(field.as_str()) {
            projected.insert(field.clone(), value.clone());
        }
    }
    Value::Object(projected)
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 3 <= bytes.len() => {
                match u8::from_str_radix(&raw[index + 1..index + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rows() -> Vec<Value> {
        vec![
            json!({"id": "a", "alias": "api.openai.com", "enabled": true, "order": 2}),
            json!({"id": "b", "alias": "vendor.com", "enabled": false, "order": 1}),
            json!({"id": "c", "alias": "api.stripe.com", "enabled": true, "order": 1}),
        ]
    }

    #[test]
    fn filter_matches_on_equality() {
        let query = ListQuery::parse(Some("%24filter=alias%20eq%20%27vendor.com%27"));
        assert_eq!(query.filters.len(), 1);
        let result = query.apply(rows());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], "b");
    }

    #[test]
    fn filter_joins_comparisons_with_and() {
        let query = ListQuery::parse(Some(
            "$filter=enabled eq 'true' and alias eq 'api.openai.com'",
        ));
        let result = query.apply(rows());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], "a");
    }

    #[test]
    fn filter_supports_ne_and_contains() {
        let query = ListQuery::parse(Some("$filter=enabled ne 'true'"));
        assert_eq!(query.apply(rows())[0]["id"], "b");

        let query = ListQuery::parse(Some("$filter=contains(alias,'api.')"));
        let result = query.apply(rows());
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn select_projects_the_requested_fields() {
        let query = ListQuery::parse(Some("$select=id,alias"));
        let result = query.apply(rows());
        assert_eq!(result[0], json!({"id": "a", "alias": "api.openai.com"}));
    }

    #[test]
    fn orderby_sorts_ascending_and_descending() {
        let query = ListQuery::parse(Some("$orderby=order"));
        let result = query.apply(rows());
        let ids: Vec<&str> = result
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["c", "b", "a"]);

        let query = ListQuery::parse(Some("$orderby=order%20desc"));
        let result = query.apply(rows());
        let ids: Vec<&str> = result
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn top_and_skip_page_the_results() {
        let query = ListQuery::parse(Some("$top=1&$skip=1"));
        let result = query.apply(rows());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["id"], "b");
    }

    #[test]
    fn top_is_clamped_to_the_documented_maximum() {
        let query = ListQuery::parse(Some("$top=5000"));
        assert_eq!(query.effective_top(), 100);
        let query = ListQuery::parse(None);
        assert_eq!(query.effective_top(), 50);
    }

    #[test]
    fn unknown_query_parameters_are_ignored() {
        let query = ListQuery::parse(Some("$count=true&api-version=1"));
        assert!(query.filters.is_empty());
        assert_eq!(query.apply(rows()).len(), 3);
    }

    #[test]
    fn a_missing_field_sorts_last() {
        let mut sparse = rows();
        sparse[1].as_object_mut().unwrap().remove("order");
        let query = ListQuery::parse(Some("$orderby=order"));
        let result = query.apply(sparse);
        assert_eq!(result[2]["id"], "b");
    }
}
