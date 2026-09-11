//! The OData subset the management list endpoints accept.
//!
//! `$filter`, `$select`, `$orderby`, `$top` and `$skip` are applied over the
//! JSON projection of each resource, so a new field becomes filterable without
//! a second schema to keep in step.

use serde_json::Value;

use crate::domain::error::{OagwError, OagwResult};

/// Parsed list query parameters.
#[derive(Debug, Clone, Default)]
pub struct ListParams {
    pub filter: Vec<FilterTerm>,
    pub select: Vec<String>,
    pub orderby: Vec<OrderTerm>,
    pub top: Option<usize>,
    pub skip: usize,
}

/// One `field op value` conjunct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterTerm {
    pub field: String,
    pub op: FilterOp,
    pub value: String,
}

/// Comparison operators of the supported subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    Eq,
    Ne,
    Contains,
    StartsWith,
    EndsWith,
}

/// One `field [asc|desc]` sort key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderTerm {
    pub field: String,
    pub descending: bool,
}

impl ListParams {
    /// Parse the query string of a list request.
    ///
    /// # Errors
    ///
    /// `400` when a parameter is malformed or `$top`/`$skip` are not numbers.
    pub fn from_query(query: &str) -> OagwResult<Self> {
        let mut params = Self::default();
        for (key, value) in form_urlencoded::parse(query.as_bytes()) {
            match key.as_ref() {
                "$filter" => params.filter = parse_filter(&value)?,
                "$select" => {
                    params.select = value
                        .split(',')
                        .map(|field| field.trim().to_owned())
                        .filter(|field| !field.is_empty())
                        .collect();
                }
                "$orderby" => params.orderby = parse_orderby(&value)?,
                "$top" => {
                    params.top = Some(value.trim().parse::<usize>().map_err(|_| {
                        OagwError::validation("$top must be a non-negative integer")
                    })?);
                }
                "$skip" => {
                    params.skip = value.trim().parse::<usize>().map_err(|_| {
                        OagwError::validation("$skip must be a non-negative integer")
                    })?;
                }
                _ => {}
            }
        }
        Ok(params)
    }

    /// Filter, sort, paginate and project `items`, returning
    /// `(page, total_before_pagination)`.
    #[must_use]
    pub fn apply(&self, mut items: Vec<Value>, default_top: usize) -> (Vec<Value>, usize) {
        items.retain(|item| self.filter.iter().all(|term| term.matches(item)));
        let total = items.len();

        for term in self.orderby.iter().rev() {
            items.sort_by(|a, b| {
                let ordering = compare(field_of(a, &term.field), field_of(b, &term.field));
                if term.descending {
                    ordering.reverse()
                } else {
                    ordering
                }
            });
        }

        let page: Vec<Value> = items
            .into_iter()
            .skip(self.skip)
            .take(self.top.unwrap_or(default_top))
            .map(|item| self.project(item))
            .collect();
        (page, total)
    }

    fn project(&self, item: Value) -> Value {
        if self.select.is_empty() {
            return item;
        }
        let Value::Object(object) = item else {
            return item;
        };
        let mut projected = serde_json::Map::new();
        for field in &self.select {
            if let Some(value) = object.get(field) {
                projected.insert(field.clone(), value.clone());
            }
        }
        Value::Object(projected)
    }
}

impl FilterTerm {
    /// Whether `item` satisfies this term.
    #[must_use]
    pub fn matches(&self, item: &Value) -> bool {
        let Some(actual) = field_of(item, &self.field) else {
            // An absent field only satisfies an inequality.
            return self.op == FilterOp::Ne;
        };
        let actual = scalar_to_string(actual);
        match self.op {
            FilterOp::Eq => actual == self.value,
            FilterOp::Ne => actual != self.value,
            FilterOp::Contains => actual.contains(&self.value),
            FilterOp::StartsWith => actual.starts_with(&self.value),
            FilterOp::EndsWith => actual.ends_with(&self.value),
        }
    }
}

fn field_of<'a>(item: &'a Value, field: &str) -> Option<&'a Value> {
    let mut current = item;
    for segment in field.split('/') {
        current = current.get(segment)?;
    }
    Some(current)
}

fn scalar_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn compare(a: Option<&Value>, b: Option<&Value>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(a), Some(b)) => match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal),
            _ => scalar_to_string(a).cmp(&scalar_to_string(b)),
        },
    }
}

/// Parse `field eq 'value'` conjuncts joined by `and`.
fn parse_filter(raw: &str) -> OagwResult<Vec<FilterTerm>> {
    let mut terms = Vec::new();
    for clause in split_conjuncts(raw) {
        let clause = clause.trim();
        if clause.is_empty() {
            continue;
        }
        terms.push(parse_clause(clause)?);
    }
    Ok(terms)
}

/// Split on ` and `, ignoring separators inside quoted literals.
fn split_conjuncts(raw: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut window: Vec<char> = Vec::new();

    for ch in raw.chars() {
        if ch == '\'' {
            in_quotes = !in_quotes;
        }
        current.push(ch);
        if !in_quotes {
            window.push(ch.to_ascii_lowercase());
            if window.len() > 5 {
                window.remove(0);
            }
            if window.iter().collect::<String>() == " and " {
                let keep = current.len() - 5;
                current.truncate(keep);
                parts.push(std::mem::take(&mut current));
                window.clear();
            }
        }
    }
    parts.push(current);
    parts
}

fn parse_clause(clause: &str) -> OagwResult<FilterTerm> {
    // Function form first: `contains(field,'value')`.
    for (name, op) in [
        ("contains", FilterOp::Contains),
        ("startswith", FilterOp::StartsWith),
        ("endswith", FilterOp::EndsWith),
    ] {
        let prefix = format!("{name}(");
        if clause.to_ascii_lowercase().starts_with(&prefix)
            && let Some(inner) = clause[prefix.len()..].strip_suffix(')')
        {
            let (field, value) = inner.split_once(',').ok_or_else(|| {
                OagwError::validation(format!("$filter: malformed '{name}' expression"))
            })?;
            return Ok(FilterTerm {
                field: field.trim().to_owned(),
                op,
                value: unquote(value.trim()),
            });
        }
    }

    let mut tokens = clause.splitn(3, char::is_whitespace);
    let field = tokens
        .next()
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .ok_or_else(|| OagwError::validation("$filter: missing field name"))?;
    let op_token = tokens
        .next()
        .map(str::trim)
        .ok_or_else(|| OagwError::validation("$filter: missing operator"))?;
    let value = tokens
        .next()
        .map(str::trim)
        .ok_or_else(|| OagwError::validation("$filter: missing value"))?;

    let op = match op_token.to_ascii_lowercase().as_str() {
        "eq" => FilterOp::Eq,
        "ne" => FilterOp::Ne,
        other => {
            return Err(OagwError::validation(format!(
                "$filter: unsupported operator '{other}' (supported: eq, ne, contains, \
                 startswith, endswith)"
            )));
        }
    };

    Ok(FilterTerm {
        field: field.to_owned(),
        op,
        value: unquote(value),
    })
}

fn parse_orderby(raw: &str) -> OagwResult<Vec<OrderTerm>> {
    let mut terms = Vec::new();
    for clause in raw.split(',') {
        let clause = clause.trim();
        if clause.is_empty() {
            continue;
        }
        let mut tokens = clause.split_whitespace();
        let field = tokens
            .next()
            .ok_or_else(|| OagwError::validation("$orderby: missing field name"))?;
        let descending = match tokens.next().map(str::to_ascii_lowercase).as_deref() {
            None | Some("asc") => false,
            Some("desc") => true,
            Some(other) => {
                return Err(OagwError::validation(format!(
                    "$orderby: unsupported direction '{other}'"
                )));
            }
        };
        terms.push(OrderTerm {
            field: field.to_owned(),
            descending,
        });
    }
    Ok(terms)
}

fn unquote(value: &str) -> String {
    value
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .map_or_else(|| value.to_owned(), |inner| inner.replace("''", "'"))
}

#[cfg(test)]
#[path = "query_tests.rs"]
mod tests;
