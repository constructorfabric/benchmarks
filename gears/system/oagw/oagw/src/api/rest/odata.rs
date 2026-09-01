//! List query options (DESIGN §3.4 "List Query Parameters").
//!
//! The management lists accept the OData subset DESIGN documents — `$filter`,
//! `$orderby`, `$select`, `$top` and `$skip` — evaluated against the in-memory
//! store. `$skip` is bound here rather than through the shared platform
//! extractor because that binding rejects offset paging and DESIGN requires it.

use std::collections::HashSet;

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use serde_json::Value;

use crate::api::error::OagwError;
use crate::domain::error::DomainError;

/// Largest page size (DESIGN §3.4: "`$top` … max: 100").
pub const MAX_TOP: u64 = 100;
/// Page size applied when `$top` is absent.
pub const DEFAULT_TOP: u64 = 50;

/// Parsed system query options of a list endpoint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListParams {
    /// `$filter` expression, verbatim.
    pub filter: Option<String>,
    /// `$orderby` expression, verbatim.
    pub orderby: Option<String>,
    /// `$select` projection fields.
    pub select: Vec<String>,
    /// `$top` page size.
    pub top: Option<u64>,
    /// `$skip` offset.
    pub skip: Option<u64>,
}

/// Extractor wrapper binding [`ListParams`] off the query string.
#[derive(Debug, Clone, Default)]
pub struct ListQuery(pub ListParams);

impl<S: Send + Sync> FromRequestParts<S> for ListQuery {
    type Rejection = OagwError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        ListParams::parse(parts.uri.query().unwrap_or_default()).map(Self)
    }
}

impl ListParams {
    /// Parses the raw query string, rejecting unknown `$`-prefixed options and
    /// out-of-range pagination.
    ///
    /// # Errors
    ///
    /// Returns a validation error for an unknown system option, an empty or
    /// oversized `$select`, or a `$top` above [`MAX_TOP`].
    pub fn parse(raw: &str) -> Result<Self, OagwError> {
        let mut query = Self::default();
        for (key, value) in form_urlencoded::parse(raw.as_bytes()) {
            match key.as_ref() {
                "$filter" => query.filter = Some(value.into_owned()),
                "$orderby" => query.orderby = Some(value.into_owned()),
                "$select" => query.select = parse_select(&value)?,
                "$top" => query.top = Some(parse_count(&value, "$top")?),
                "$skip" => query.skip = Some(parse_count(&value, "$skip")?),
                "$skiptoken" | "$cursor" | "$count" => {
                    return Err(validation(format!("{key} is not supported")));
                }
                other if other.starts_with('$') => {
                    return Err(validation(format!("unknown query option {other:?}")));
                }
                _ => {}
            }
        }
        if query.top.is_some_and(|top| top > MAX_TOP) {
            return Err(validation(format!("$top must not exceed {MAX_TOP}")));
        }
        Ok(query)
    }

    /// Field names requested by `$select`, when any.
    #[must_use]
    pub fn selected(&self) -> Option<&[String]> {
        (!self.select.is_empty()).then_some(self.select.as_slice())
    }

    /// Applies filter, order, offset and page size to already-serialized rows.
    ///
    /// Rows must carry every field the query addresses — including fields the
    /// wire schema omits, such as `created_at`.
    ///
    /// # Errors
    ///
    /// Returns a validation error when `$orderby` or `$filter` cannot be parsed.
    pub fn apply(&self, mut rows: Vec<Value>) -> Result<Vec<Value>, OagwError> {
        let filter = self
            .filter
            .as_deref()
            .map(str::trim)
            .filter(|filter| !filter.is_empty())
            .map(Filter::parse)
            .transpose()?;
        if let Some(filter) = &filter {
            rows.retain(|row| filter.matches(row));
        }
        if let Some(orderby) = self.orderby.as_deref() {
            sort_rows(&mut rows, orderby)?;
        }
        let skip = self.skip.unwrap_or(0).min(rows.len() as u64) as usize;
        rows.drain(..skip);
        let top = self.top.unwrap_or(DEFAULT_TOP);
        rows.truncate(top as usize);
        Ok(rows)
    }
}

/// Projects a serialized entity onto the `$select` field set.
///
/// Nested paths select with dot notation; an empty projection list returns the
/// row untouched.
#[must_use]
pub fn project(row: &Value, fields: &[String]) -> Value {
    let Some(object) = row.as_object() else {
        return row.clone();
    };
    let selected: HashSet<String> = fields
        .iter()
        .map(|field| field.trim().to_lowercase())
        .collect();
    let mut projected = serde_json::Map::new();
    for (key, value) in object {
        let lower = key.to_lowercase();
        if selected.contains(&lower) {
            projected.insert(key.clone(), value.clone());
            continue;
        }
        let prefix = format!("{lower}.");
        let nested: Vec<String> = selected
            .iter()
            .filter_map(|field| field.strip_prefix(&prefix).map(ToOwned::to_owned))
            .collect();
        if !nested.is_empty() {
            projected.insert(key.clone(), project(value, &nested));
        }
    }
    Value::Object(projected)
}

fn validation(message: impl Into<String>) -> OagwError {
    OagwError::from(DomainError::validation(message))
}

fn parse_count(raw: &str, name: &str) -> Result<u64, OagwError> {
    raw.trim().parse::<u64>().map_err(|_| {
        validation(format!(
            "{name} must be a non-negative integer, got {raw:?}"
        ))
    })
}

fn parse_select(raw: &str) -> Result<Vec<String>, OagwError> {
    let fields: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if fields.is_empty() {
        return Err(validation("$select cannot be empty"));
    }
    Ok(fields)
}

/// Sorts rows by the comma-separated `$orderby` expression.
///
/// Each key is `field` or `field asc`/`field desc`; an empty expression and
/// unknown directions are rejected.
fn sort_rows(rows: &mut [Value], orderby: &str) -> Result<(), OagwError> {
    let mut keys = Vec::new();
    for term in orderby.split(',') {
        let term = term.trim();
        if term.is_empty() {
            continue;
        }
        let mut parts = term.split_whitespace();
        let field = parts.next().unwrap_or_default().to_lowercase();
        if field.is_empty() {
            return Err(validation("$orderby field name is empty"));
        }
        let descending = match parts.next() {
            None => false,
            Some(direction) if direction.eq_ignore_ascii_case("asc") => false,
            Some(direction) if direction.eq_ignore_ascii_case("desc") => true,
            Some(direction) => {
                return Err(validation(format!(
                    "unknown $orderby direction {direction:?}"
                )));
            }
        };
        if parts.next().is_some() {
            return Err(validation(format!("malformed $orderby term {term:?}")));
        }
        keys.push((field, descending));
    }
    if keys.is_empty() {
        return Ok(());
    }
    rows.sort_by(|left, right| {
        for (field, descending) in &keys {
            let ordering = compare(lookup(left, field), lookup(right, field));
            let ordering = if *descending {
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
    Ok(())
}

fn lookup<'a>(row: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = row;
    for segment in path.split('.') {
        current = current.as_object()?.get(segment)?;
    }
    Some(current)
}

fn compare(left: Option<&Value>, right: Option<&Value>) -> std::cmp::Ordering {
    match (left, right) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(left), Some(right)) => match (left.as_f64(), right.as_f64()) {
            (Some(left), Some(right)) => left
                .partial_cmp(&right)
                .unwrap_or(std::cmp::Ordering::Equal),
            _ => string_of(left).cmp(&string_of(right)),
        },
    }
}

fn string_of(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), ToOwned::to_owned)
}

/// Comparison operator of a `$filter` predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Operator {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

impl Operator {
    fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "eq" => Self::Eq,
            "ne" => Self::Ne,
            "gt" => Self::Gt,
            "ge" => Self::Ge,
            "lt" => Self::Lt,
            "le" => Self::Le,
            _ => return None,
        })
    }

    fn holds(self, left: &Value, right: &Value) -> bool {
        let equal = values_equal(left, right);
        match self {
            Self::Eq => equal,
            Self::Ne => !equal,
            Self::Gt => !equal && strictly_greater(left, right),
            Self::Ge => equal || strictly_greater(left, right),
            Self::Lt => !equal && strictly_greater(right, left),
            Self::Le => equal || strictly_greater(right, left),
        }
    }
}

fn strictly_greater(left: &Value, right: &Value) -> bool {
    compare(Some(left), Some(right)) == std::cmp::Ordering::Greater
}

fn values_equal(left: &Value, right: &Value) -> bool {
    match (left.as_f64(), right.as_f64()) {
        (Some(left), Some(right)) => left == right,
        _ => left == right,
    }
}

/// Compiled `$filter` predicate.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    /// Field compared against a literal.
    Compare {
        /// Property path.
        path: String,
        /// Operator.
        operator: Operator,
        /// Literal operand.
        operand: Value,
    },
    /// `contains(field, literal)`.
    Contains {
        /// Property path.
        path: String,
        /// Substring operand.
        operand: String,
    },
    /// `startswith(field, literal)`.
    StartsWith {
        /// Property path.
        path: String,
        /// Prefix operand.
        operand: String,
    },
    /// `endswith(field, literal)`.
    EndsWith {
        /// Property path.
        path: String,
        /// Suffix operand.
        operand: String,
    },
    /// Conjunction.
    And(Box<Self>, Box<Self>),
    /// Disjunction.
    Or(Box<Self>, Box<Self>),
    /// Negation.
    Not(Box<Self>),
}

impl Filter {
    /// Parses a `$filter` expression (OData 4.01 subset).
    ///
    /// # Errors
    ///
    /// Returns a validation error naming the offending token.
    pub fn parse(raw: &str) -> Result<Self, OagwError> {
        let mut parser = Parser {
            tokens: tokenize(raw)?,
            position: 0,
        };
        let filter = parser.parse_or()?;
        if parser.peek().is_some() {
            return Err(validation(format!(
                "unexpected token {:?} in $filter",
                parser.peek()
            )));
        }
        Ok(filter)
    }

    /// Evaluates the predicate against a serialized row.
    #[must_use]
    pub fn matches(&self, row: &Value) -> bool {
        match self {
            Self::Compare {
                path,
                operator,
                operand,
            } => lookup(row, path).is_some_and(|value| operator.holds(value, operand)),
            Self::Contains { path, operand } => lookup(row, path)
                .and_then(Value::as_str)
                .is_some_and(|value| value.contains(operand.as_str())),
            Self::StartsWith { path, operand } => lookup(row, path)
                .and_then(Value::as_str)
                .is_some_and(|value| value.starts_with(operand.as_str())),
            Self::EndsWith { path, operand } => lookup(row, path)
                .and_then(Value::as_str)
                .is_some_and(|value| value.ends_with(operand.as_str())),
            Self::And(left, right) => left.matches(row) && right.matches(row),
            Self::Or(left, right) => left.matches(row) || right.matches(row),
            Self::Not(inner) => !inner.matches(row),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Text(String),
    Number(f64),
    Bool(bool),
    Null,
    Open,
    Close,
    Comma,
}

fn tokenize(raw: &str) -> Result<Vec<Token>, OagwError> {
    let mut tokens = Vec::new();
    let mut characters = raw.chars().peekable();
    while let Some(&character) = characters.peek() {
        match character {
            ' ' | '\t' => {
                characters.next();
            }
            '(' => {
                tokens.push(Token::Open);
                characters.next();
            }
            ')' => {
                tokens.push(Token::Close);
                characters.next();
            }
            ',' => {
                tokens.push(Token::Comma);
                characters.next();
            }
            '\'' => {
                characters.next();
                let mut text = String::new();
                loop {
                    match characters.next() {
                        Some('\'') => break,
                        Some(other) => text.push(other),
                        None => return Err(validation("unterminated string literal in $filter")),
                    }
                }
                tokens.push(Token::Text(text));
            }
            _ => {
                let mut word = String::new();
                while let Some(&next) = characters.peek() {
                    if next.is_whitespace() || "(), '".contains(next) {
                        break;
                    }
                    word.push(next);
                    characters.next();
                }
                tokens.push(match word.as_str() {
                    "true" => Token::Bool(true),
                    "false" => Token::Bool(false),
                    "null" => Token::Null,
                    _ if word.parse::<f64>().is_ok_and(|_| {
                        word.chars()
                            .next()
                            .is_some_and(|first| first.is_ascii_digit() || first == '-')
                    }) =>
                    {
                        Token::Number(word.parse::<f64>().unwrap_or_default())
                    }
                    _ => Token::Ident(word),
                });
            }
        }
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.position)
    }

    fn next(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.position).cloned();
        if token.is_some() {
            self.position += 1;
        }
        token
    }

    fn parse_or(&mut self) -> Result<Filter, OagwError> {
        let mut left = self.parse_and()?;
        while self.peek() == Some(&Token::Ident("or".to_owned())) {
            self.next();
            let right = self.parse_and()?;
            left = Filter::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Filter, OagwError> {
        let mut left = self.parse_unary()?;
        while self.peek() == Some(&Token::Ident("and".to_owned())) {
            self.next();
            let right = self.parse_unary()?;
            left = Filter::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Filter, OagwError> {
        if self.peek() == Some(&Token::Ident("not".to_owned())) {
            self.next();
            return Ok(Filter::Not(Box::new(self.parse_unary()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Filter, OagwError> {
        match self.next() {
            Some(Token::Open) => {
                let inner = self.parse_or()?;
                if self.next() != Some(Token::Close) {
                    return Err(validation("expected ')' in $filter"));
                }
                Ok(inner)
            }
            Some(Token::Ident(name))
                if matches!(name.as_str(), "contains" | "startswith" | "endswith") =>
            {
                self.parse_function(&name)
            }
            Some(Token::Ident(path)) => {
                let Some(Token::Ident(operator)) = self.next() else {
                    return Err(validation(format!("expected an operator after {path:?}")));
                };
                let operator = Operator::parse(&operator).ok_or_else(|| {
                    validation(format!("unknown comparison operator {operator:?}"))
                })?;
                let operand = self.parse_literal()?;
                Ok(Filter::Compare {
                    path,
                    operator,
                    operand,
                })
            }
            other => Err(validation(format!("unexpected token {other:?} in $filter"))),
        }
    }

    fn parse_function(&mut self, name: &str) -> Result<Filter, OagwError> {
        if self.next() != Some(Token::Open) {
            return Err(validation(format!("{name} expects '('")));
        }
        let Some(Token::Ident(path)) = self.next() else {
            return Err(validation(format!("{name} expects a field name")));
        };
        if self.next() != Some(Token::Comma) {
            return Err(validation(format!("{name} expects a second argument")));
        }
        let literal = self.parse_literal()?;
        let Some(operand) = literal.as_str().map(ToOwned::to_owned) else {
            return Err(validation(format!("{name} expects a string literal")));
        };
        if self.next() != Some(Token::Close) {
            return Err(validation(format!("{name} expects ')'")));
        }
        Ok(match name {
            "contains" => Filter::Contains { path, operand },
            "startswith" => Filter::StartsWith { path, operand },
            _ => Filter::EndsWith { path, operand },
        })
    }

    fn parse_literal(&mut self) -> Result<Value, OagwError> {
        match self.next() {
            Some(Token::Text(text)) => Ok(Value::String(text)),
            Some(Token::Number(number)) => Ok(serde_json::Number::from_f64(number)
                .map(Value::Number)
                .unwrap_or(Value::Null)),
            Some(Token::Bool(value)) => Ok(Value::Bool(value)),
            Some(Token::Null) => Ok(Value::Null),
            other => Err(validation(format!("expected a literal, got {other:?}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(alias: &str, enabled: bool, created_at: u64) -> Value {
        json!({"alias": alias, "enabled": enabled, "created_at": created_at})
    }

    #[test]
    fn parses_the_documented_options() {
        let query = ListParams::parse(
            "$filter=alias eq 'api.openai.com'&$orderby=created_at desc&$select=alias,enabled&$top=10&$skip=5",
        )
        .expect("parses");
        assert_eq!(query.filter.as_deref(), Some("alias eq 'api.openai.com'"));
        assert_eq!(query.orderby.as_deref(), Some("created_at desc"));
        assert_eq!(query.select, vec!["alias", "enabled"]);
        assert_eq!(query.top, Some(10));
        assert_eq!(query.skip, Some(5));
    }

    #[test]
    fn rejects_unknown_options_and_out_of_range_top() {
        assert!(ListParams::parse("$filtre=alias eq 'x'").is_err());
        assert!(ListParams::parse("$skiptoken=abc").is_err());
        assert!(ListParams::parse("$top=101").is_err());
        assert!(ListParams::parse("$top=x").is_err());
        assert!(ListParams::parse("$select=").is_err());
    }

    #[test]
    fn applies_filter_order_offset_and_page() {
        let query =
            ListParams::parse("$filter=enabled eq true&$orderby=created_at desc&$skip=1&$top=1")
                .expect("parses");
        let rows = vec![
            row("a.example", true, 1),
            row("b.example", false, 2),
            row("c.example", true, 3),
            row("d.example", true, 4),
        ];
        let filtered = query.apply(rows).expect("applies");
        assert_eq!(
            filtered,
            vec![json!({"alias": "c.example", "enabled": true, "created_at": 3})]
        );
    }

    #[test]
    fn defaults_to_the_documented_page_size() {
        let rows: Vec<Value> = (0..60).map(|index| row("a", true, index)).collect();
        assert_eq!(
            ListParams::default().apply(rows).expect("applies").len(),
            50
        );
    }

    #[test]
    fn filter_supports_operators_functions_and_logic() {
        assert!(
            Filter::parse("alias eq 'a.example'")
                .expect("parses")
                .matches(&row("a.example", true, 1))
        );
        assert!(
            !Filter::parse("alias ne 'a.example'")
                .expect("parses")
                .matches(&row("a.example", true, 1))
        );
        assert!(
            Filter::parse("created_at ge 2 and enabled eq true")
                .expect("parses")
                .matches(&row("a", true, 3))
        );
        assert!(
            Filter::parse("not (created_at lt 2)")
                .expect("parses")
                .matches(&row("a", true, 3))
        );
        assert!(
            Filter::parse("contains(alias, 'example')")
                .expect("parses")
                .matches(&row("a.example", true, 3))
        );
        assert!(
            Filter::parse("startswith(alias, 'a.')")
                .expect("parses")
                .matches(&row("a.example", true, 3))
        );
        assert!(
            Filter::parse("endswith(alias, 'example')")
                .expect("parses")
                .matches(&row("a.example", false, 3))
        );
        assert!(
            Filter::parse("enabled eq false or created_at eq 1")
                .expect("parses")
                .matches(&row("a", false, 1))
        );
        assert!(
            Filter::parse("server.auth.plugin_type eq null")
                .expect("parses")
                .matches(&json!({"server": {"auth": {"plugin_type": null}}}))
        );
        assert!(Filter::parse("alias eq 'unterminated").is_err());
        assert!(Filter::parse("alias like 'x'").is_err());
    }

    #[test]
    fn select_projects_nested_paths() {
        let row = json!({"alias": "a", "server": {"enabled": true, "endpoints": [{"host": "h"}]}});
        assert_eq!(
            project(&row, &["alias".to_owned(), "server.enabled".to_owned()]),
            json!({"alias": "a", "server": {"enabled": true}})
        );
    }
}
