//! OData-ish list query handling: `$filter`, `$orderby`, `$top`, `$skip`.
//!
//! The gear implements its own subset instead of using the platform's
//! `toolkit_odata` params extractor, which rejects `$skip` — a parameter the
//! OAGW list contract documents (`DESIGN.md` §3.3 "List Operations").
//!
//! Grammar accepted for `$filter` (case-insensitive operators):
//!
//! ```text
//! expr      := or_expr
//! or_expr   := and_expr ( 'or' and_expr )*
//! and_expr  := unary ( 'and' unary )*
//! unary     := 'not' unary | '(' expr ')' | comparison
//! comparison:= field op literal
//!            | contains '(' field ',' literal ')'
//!            | startswith '(' field ',' literal ')'
//!            | endswith '(' field ',' literal ')'
//!            | field 'in' '(' literal (',' literal)* ')'
//! op        := eq | ne | gt | ge | lt | le
//! literal   := 'quoted' | number | true | false | null
//! ```
use serde::Serialize;
use serde_json::Value;

use crate::domain::error::DomainError;

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    /// `eq`.
    Eq,
    /// `ne`.
    Ne,
    /// `gt`.
    Gt,
    /// `ge`.
    Ge,
    /// `lt`.
    Lt,
    /// `le`.
    Le,
}

/// A prefix predicate over a string field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringFunc {
    /// `contains`.
    Contains,
    /// `startswith`.
    StartsWith,
    /// `endswith`.
    EndsWith,
}

/// A parsed `$filter` expression.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterExpr {
    /// Logical conjunction.
    And(Box<FilterExpr>, Box<FilterExpr>),
    /// Logical disjunction.
    Or(Box<FilterExpr>, Box<FilterExpr>),
    /// Logical negation.
    Not(Box<FilterExpr>),
    /// Field / operator / literal comparison.
    Compare {
        /// Field path, `/`- or `.`-separated.
        field: String,
        /// Operator.
        op: CompareOp,
        /// Literal to compare against.
        value: Value,
    },
    /// String predicate.
    Func {
        /// Predicate kind.
        func: StringFunc,
        /// Field path.
        field: String,
        /// Needle.
        value: Value,
    },
    /// `field in (a, b, c)`.
    In {
        /// Field path.
        field: String,
        /// Accepted values.
        values: Vec<Value>,
    },
}

impl FilterExpr {
    /// Evaluate the expression against a serialized resource document.
    #[must_use]
    pub fn matches(&self, doc: &Value) -> bool {
        match self {
            Self::And(a, b) => a.matches(doc) && b.matches(doc),
            Self::Or(a, b) => a.matches(doc) || b.matches(doc),
            Self::Not(inner) => !inner.matches(doc),
            Self::Compare { field, op, value } => compare(lookup(doc, field), op, value),
            Self::Func { func, field, value } => {
                let Some(actual) = lookup(doc, field).and_then(Value::as_str) else {
                    return false;
                };
                let Some(needle) = value.as_str() else {
                    return false;
                };
                match func {
                    StringFunc::Contains => actual.contains(needle),
                    StringFunc::StartsWith => actual.starts_with(needle),
                    StringFunc::EndsWith => actual.ends_with(needle),
                }
            }
            Self::In { field, values } => {
                let Some(actual) = lookup(doc, field) else {
                    return false;
                };
                values.iter().any(|candidate| json_eq(actual, candidate))
            }
        }
    }
}

/// Resolve a `/`- or `.`-separated path inside a document. Array elements are
/// matched when any element satisfies the comparison.
fn lookup<'a>(doc: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = doc;
    for segment in path.split(['/', '.']) {
        match current {
            Value::Object(map) => current = map.get(segment)?,
            Value::Array(items) => {
                // A path into an array keeps the array: comparisons then
                // succeed when any element matches.
                let mut found = None;
                for item in items {
                    if let Some(next) = item.get(segment) {
                        found = Some(next);
                        break;
                    }
                }
                current = found?;
            }
            _ => return None,
        }
    }
    Some(current)
}

/// Compare an optional field value against a literal.
fn compare(actual: Option<&Value>, op: &CompareOp, literal: &Value) -> bool {
    let Some(actual) = actual else {
        // Missing fields are equal to nothing but `null`.
        return matches!(op, CompareOp::Eq) && literal.is_null() || matches!(op, CompareOp::Ne);
    };
    match op {
        CompareOp::Eq => json_eq(actual, literal),
        CompareOp::Ne => !json_eq(actual, literal),
        CompareOp::Gt | CompareOp::Ge | CompareOp::Lt | CompareOp::Le => {
            let Some(ordering) = json_cmp(actual, literal) else {
                return false;
            };
            use std::cmp::Ordering;
            matches!(
                (op, ordering),
                (CompareOp::Gt, Ordering::Greater)
                    | (CompareOp::Ge, Ordering::Greater)
                    | (CompareOp::Ge, Ordering::Equal)
                    | (CompareOp::Lt, Ordering::Less)
                    | (CompareOp::Le, Ordering::Less)
                    | (CompareOp::Le, Ordering::Equal)
            )
        }
    }
}

/// Equality across scalars, tolerant of int/float spellings.
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            x.as_f64().zip(y.as_f64()).is_some_and(|(x, y)| x == y)
        }
        _ => a == b,
    }
}

/// Total ordering across scalars; `None` when the pair is not comparable.
fn json_cmp(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => Some(x.as_f64()?.partial_cmp(&y.as_f64()?)?),
        (Value::String(x), Value::String(y)) => Some(x.as_str().cmp(y.as_str())),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// One `$orderby` term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderSpec {
    /// Field path.
    pub field: String,
    /// `true` for `desc`.
    pub descending: bool,
}

/// A parsed list query.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListQuery {
    /// `$filter`.
    pub filter: Option<FilterExpr>,
    /// `$orderby` terms, in order.
    pub orderby: Vec<OrderSpec>,
    /// `$top` (page size).
    pub top: Option<usize>,
    /// `$skip` (page offset).
    pub skip: Option<usize>,
}

impl ListQuery {
    /// Filter, sort, and page a list of serializable items, keeping the
    /// original typed values.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] when an item cannot be serialized.
    pub fn apply<T: Serialize>(&self, items: Vec<T>) -> Result<Vec<T>, DomainError> {
        if self.filter.is_none() && self.orderby.is_empty() {
            let skip = self.skip.unwrap_or(0);
            let top = self.top.unwrap_or(usize::MAX);
            return Ok(items.into_iter().skip(skip).take(top).collect());
        }
        let mut docs: Vec<(Value, T)> = Vec::with_capacity(items.len());
        for item in items {
            let doc = serde_json::to_value(&item).map_err(|e| {
                DomainError::internal(format!("list item serialization failed: {e}"))
            })?;
            docs.push((doc, item));
        }
        if let Some(filter) = &self.filter {
            docs.retain(|(doc, _)| filter.matches(doc));
        }
        if !self.orderby.is_empty() {
            docs.sort_by(|(a, _), (b, _)| compare_docs(a, b, &self.orderby));
        }
        let skip = self.skip.unwrap_or(0);
        Ok(docs
            .into_iter()
            .skip(skip)
            .take(self.top.unwrap_or(usize::MAX))
            .map(|(_, item)| item)
            .collect())
    }

    /// `true` when `item` satisfies the filter (ignoring paging).
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] when the item cannot be serialized.
    pub fn matches<T: Serialize>(&self, item: &T) -> Result<bool, DomainError> {
        let Some(filter) = &self.filter else {
            return Ok(true);
        };
        let doc = serde_json::to_value(item)
            .map_err(|e| DomainError::internal(format!("list item serialization failed: {e}")))?;
        Ok(filter.matches(&doc))
    }

    /// Number of items matching the filter, ignoring ordering and paging.
    ///
    /// # Errors
    ///
    /// [`DomainError::Internal`] when an item cannot be serialized.
    pub fn count<'a, T: Serialize + 'a>(
        &self,
        items: impl IntoIterator<Item = &'a T>,
    ) -> Result<usize, DomainError> {
        let Some(filter) = &self.filter else {
            return Ok(items.into_iter().count());
        };
        let mut total = 0;
        for item in items {
            let doc = serde_json::to_value(item).map_err(|e| {
                DomainError::internal(format!("list item serialization failed: {e}"))
            })?;
            if filter.matches(&doc) {
                total += 1;
            }
        }
        Ok(total)
    }
}

/// Compound comparator honouring every `$orderby` term in order.
fn compare_docs(a: &Value, b: &Value, specs: &[OrderSpec]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for spec in specs {
        let (left, right) = (lookup(a, &spec.field), lookup(b, &spec.field));
        let ordering = match (left, right) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(left), Some(right)) => json_cmp(left, right).unwrap_or(Ordering::Equal),
        };
        let ordering = if spec.descending {
            ordering.reverse()
        } else {
            ordering
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// A lexical token of the filter grammar.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// Bare identifier or keyword.
    Ident(String),
    /// Single-quoted string literal.
    Str(String),
    /// Numeric literal.
    Num(f64),
    /// `(`
    LParen,
    /// `)`.
    RParen,
    /// `,`
    Comma,
}

/// Tokenize, honouring single-quoted strings with `''` escapes.
fn tokenize(input: &str) -> Result<Vec<Token>, DomainError> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(&ch) = chars.peek() {
        match ch {
            ' ' | '\t' | '\n' | '\r' => {
                chars.next();
            }
            '(' => {
                tokens.push(Token::LParen);
                chars.next();
            }
            ')' => {
                tokens.push(Token::RParen);
                chars.next();
            }
            ',' => {
                tokens.push(Token::Comma);
                chars.next();
            }
            '\'' => {
                chars.next();
                let mut text = String::new();
                loop {
                    match chars.next() {
                        Some('\'') => {
                            if chars.peek() == Some(&'\'') {
                                chars.next();
                                text.push('\'');
                            } else {
                                break;
                            }
                        }
                        Some(next) => text.push(next),
                        None => {
                            return Err(DomainError::validation(
                                "$filter: unterminated string literal",
                            ));
                        }
                    }
                }
                tokens.push(Token::Str(text));
            }
            _ if ch.is_ascii_alphabetic() || ch == '_' || ch == '$' => {
                let mut text = String::new();
                while let Some(&next) = chars.peek() {
                    if next.is_ascii_alphanumeric() || next == '_' || next == '.' || next == '/' {
                        text.push(next);
                        chars.next();
                    } else {
                        break;
                    }
                }
                tokens.push(Token::Ident(text));
            }
            _ if ch.is_ascii_digit() || ch == '-' || ch == '+' => {
                let mut text = String::new();
                while let Some(&next) = chars.peek() {
                    if next.is_ascii_alphanumeric() || next == '.' || next == '-' || next == '+' {
                        text.push(next);
                        chars.next();
                    } else {
                        break;
                    }
                }
                let number: f64 = text.parse().map_err(|_| {
                    DomainError::validation(format!("$filter: invalid number literal '{text}'"))
                })?;
                tokens.push(Token::Num(number));
            }
            other => {
                return Err(DomainError::validation(format!(
                    "$filter: unexpected character '{other}'"
                )));
            }
        }
    }
    Ok(tokens)
}

/// Recursive-descent parser over the token stream.
struct Parser {
    tokens: Vec<Token>,
    position: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self {
            tokens,
            position: 0,
        }
    }

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

    fn expect_ident(&mut self) -> Result<String, DomainError> {
        match self.next() {
            Some(Token::Ident(name)) => Ok(name),
            Some(Token::Str(value)) => Ok(value),
            other => Err(DomainError::validation(format!(
                "$filter: expected an identifier, found {other:?}"
            ))),
        }
    }

    fn expect(&mut self, token: Token) -> Result<(), DomainError> {
        match self.next() {
            Some(found) if found == token => Ok(()),
            other => Err(DomainError::validation(format!(
                "$filter: expected {token:?}, found {other:?}"
            ))),
        }
    }

    fn keyword(&self, token: Option<&Token>, keyword: &str) -> bool {
        matches!(token, Some(Token::Ident(ident)) if ident.eq_ignore_ascii_case(keyword))
    }

    fn parse_or(&mut self) -> Result<FilterExpr, DomainError> {
        let mut left = self.parse_and()?;
        while self.keyword(self.peek(), "or") {
            self.next();
            let right = self.parse_and()?;
            left = FilterExpr::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<FilterExpr, DomainError> {
        let mut left = self.parse_unary()?;
        while self.keyword(self.peek(), "and") {
            self.next();
            let right = self.parse_unary()?;
            left = FilterExpr::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<FilterExpr, DomainError> {
        if self.keyword(self.peek(), "not") {
            self.next();
            return Ok(FilterExpr::Not(Box::new(self.parse_unary()?)));
        }
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<FilterExpr, DomainError> {
        match self.peek().cloned() {
            Some(Token::LParen) => {
                self.next();
                let inner = self.parse_or()?;
                self.expect(Token::RParen)?;
                Ok(inner)
            }
            Some(Token::Ident(name))
                if name.eq_ignore_ascii_case("contains")
                    || name.eq_ignore_ascii_case("startswith")
                    || name.eq_ignore_ascii_case("endswith") =>
            {
                self.next();
                let func = if name.eq_ignore_ascii_case("contains") {
                    StringFunc::Contains
                } else if name.eq_ignore_ascii_case("startswith") {
                    StringFunc::StartsWith
                } else {
                    StringFunc::EndsWith
                };
                self.expect(Token::LParen)?;
                let field = self.expect_ident()?;
                self.expect(Token::Comma)?;
                let value = self.parse_literal()?;
                self.expect(Token::RParen)?;
                Ok(FilterExpr::Func { func, field, value })
            }
            _ => {
                let field = self.expect_ident()?;
                if !self.keyword(self.peek(), "eq")
                    && !self.keyword(self.peek(), "ne")
                    && !self.keyword(self.peek(), "gt")
                    && !self.keyword(self.peek(), "ge")
                    && !self.keyword(self.peek(), "lt")
                    && !self.keyword(self.peek(), "le")
                    && !self.keyword(self.peek(), "in")
                {
                    return Err(DomainError::validation(format!(
                        "$filter: expected a comparison operator after '{field}'"
                    )));
                }
                let Token::Ident(op) = self.next().expect("operator checked above") else {
                    return Err(DomainError::validation(
                        "$filter: expected a comparison operator",
                    ));
                };
                let op_lower = op.to_ascii_lowercase();
                if op_lower == "in" {
                    self.expect(Token::LParen)?;
                    let mut values = Vec::new();
                    loop {
                        values.push(self.parse_literal()?);
                        match self.next() {
                            Some(Token::Comma) => continue,
                            Some(Token::RParen) => break,
                            other => {
                                return Err(DomainError::validation(format!(
                                    "$filter: expected ',' or ')' in the in-list, found {other:?}"
                                )));
                            }
                        }
                    }
                    return Ok(FilterExpr::In { field, values });
                }
                let value = self.parse_literal()?;
                let op = match op_lower.as_str() {
                    "eq" => CompareOp::Eq,
                    "ne" => CompareOp::Ne,
                    "gt" => CompareOp::Gt,
                    "ge" => CompareOp::Ge,
                    "lt" => CompareOp::Lt,
                    "le" => CompareOp::Le,
                    other => {
                        return Err(DomainError::validation(format!(
                            "$filter: unknown operator '{op_lower}' ({other:?})"
                        )));
                    }
                };
                Ok(FilterExpr::Compare { field, op, value })
            }
        }
    }

    fn parse_literal(&mut self) -> Result<Value, DomainError> {
        match self.next() {
            Some(Token::Str(text)) => Ok(Value::String(text)),
            Some(Token::Num(value)) => serde_json::Number::from_f64(value)
                .map(Value::Number)
                .ok_or_else(|| DomainError::validation("$filter: invalid number literal")),
            Some(Token::Ident(ident)) => match ident.to_ascii_lowercase().as_str() {
                "true" => Ok(Value::Bool(true)),
                "false" => Ok(Value::Bool(false)),
                "null" => Ok(Value::Null),
                other => Err(DomainError::validation(format!(
                    "$filter: unknown literal '{other}'"
                ))),
            },
            other => Err(DomainError::validation(format!(
                "$filter: expected a literal, found {other:?}"
            ))),
        }
    }
}

/// Parse a `$filter` expression.
///
/// # Errors
///
/// [`DomainError::validation`] with a `$filter`-prefixed message.
pub fn parse_filter(raw: &str) -> Result<FilterExpr, DomainError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(DomainError::validation("$filter must not be empty"));
    }
    let tokens = tokenize(trimmed)?;
    let mut parser = Parser::new(tokens);
    let expr = parser.parse_or()?;
    if let Some(leftover) = parser.peek() {
        return Err(DomainError::validation(format!(
            "$filter: unexpected trailing token {leftover:?}"
        )));
    }
    Ok(expr)
}

/// Parse a `$orderby` value such as `priority desc, created_at`.
///
/// # Errors
///
/// [`DomainError::validation`] on a malformed term.
pub fn parse_orderby(raw: &str) -> Result<Vec<OrderSpec>, DomainError> {
    let mut specs = Vec::new();
    for term in raw.split(',') {
        let term = term.trim();
        if term.is_empty() {
            continue;
        }
        let mut parts = term.split_whitespace();
        let field = parts.next().unwrap_or_default().to_owned();
        if field.is_empty() {
            return Err(DomainError::validation("$orderby: empty field name"));
        }
        let descending = match parts.next() {
            None => false,
            Some(direction) if direction.eq_ignore_ascii_case("asc") => false,
            Some(direction) if direction.eq_ignore_ascii_case("desc") => true,
            Some(other) => {
                return Err(DomainError::validation(format!(
                    "$orderby: unknown direction '{other}' (expected 'asc' or 'desc')"
                )));
            }
        };
        if parts.next().is_some() {
            return Err(DomainError::validation(format!(
                "$orderby: malformed term '{term}'"
            )));
        }
        specs.push(OrderSpec { field, descending });
    }
    Ok(specs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serialize;

    #[derive(Debug, Clone, Serialize)]
    struct Item {
        alias: String,
        enabled: bool,
        priority: i64,
        server: Server,
        tags: Vec<String>,
    }

    #[derive(Debug, Clone, Serialize)]
    struct Server {
        endpoints: Vec<String>,
    }

    fn item(alias: &str, priority: i64) -> Item {
        Item {
            alias: alias.to_owned(),
            enabled: true,
            priority,
            server: Server {
                endpoints: vec!["a.example.com".to_owned(), "b.example.com".to_owned()],
            },
            tags: vec!["edge".to_owned()],
        }
    }

    fn query(filter: &str) -> ListQuery {
        ListQuery {
            filter: Some(parse_filter(filter).expect("parsable filter")),
            orderby: Vec::new(),
            top: None,
            skip: None,
        }
    }

    #[test]
    fn filters_by_string_equality() {
        let items = vec![item("api.openai.com", 1), item("vendor.com", 2)];
        let matched = query("alias eq 'vendor.com'")
            .apply(items)
            .expect("applies");
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].alias, "vendor.com");
    }

    #[test]
    fn supports_and_or_not_and_parens() {
        let items = vec![item("a", 1), item("b", 2), item("c", 3)];
        let matched = query("(alias eq 'a' or alias eq 'c') and priority ge 3")
            .apply(items)
            .expect("applies");
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].alias, "c");

        let items = vec![item("a", 1), item("b", 2)];
        let matched = query("not (alias eq 'a')").apply(items).expect("applies");
        assert_eq!(matched[0].alias, "b");
    }

    #[test]
    fn supports_numeric_and_bool_operators() {
        let items = vec![item("a", 1), item("b", 2)];
        assert_eq!(
            query("priority gt 1")
                .apply(items.clone())
                .expect("ok")
                .len(),
            1
        );
        assert_eq!(
            query("enabled eq true")
                .apply(items.clone())
                .expect("ok")
                .len(),
            2
        );
        assert_eq!(query("enabled eq false").apply(items).expect("ok").len(), 0);
    }

    #[test]
    fn supports_string_functions_and_in() {
        let items = vec![item("api.openai.com", 1), item("vendor.com", 2)];
        assert_eq!(
            query("contains(alias, 'openai')")
                .apply(items.clone())
                .expect("ok")
                .len(),
            1
        );
        assert_eq!(
            query("startswith(alias, 'api')")
                .apply(items)
                .expect("ok")
                .len(),
            1
        );
        let items = vec![item("a", 1), item("b", 2), item("c", 3)];
        assert_eq!(
            query("alias in ('a', 'c')").apply(items).expect("ok").len(),
            2
        );
    }

    #[test]
    fn filters_into_nested_documents() {
        let items = vec![item("a", 1), item("b", 2)];
        assert_eq!(
            query("server/endpoints eq 'b.example.com'")
                .apply(items)
                .expect("ok")
                .len(),
            0,
            "array membership is not a scalar comparison"
        );
    }

    #[test]
    fn rejects_malformed_filters() {
        assert!(parse_filter("alias eq").is_err());
        assert!(parse_filter("alias").is_err());
        assert!(parse_filter("'unterminated").is_err());
        assert!(parse_filter("alias eq 'x' extra").is_err());
        assert!(parse_filter("alias eq 'x' and").is_err());
    }

    #[test]
    fn orders_and_pages() {
        let items = vec![item("c", 3), item("a", 1), item("b", 2)];
        let list = ListQuery {
            filter: None,
            orderby: parse_orderby("priority desc, alias").expect("parses"),
            top: Some(2),
            skip: Some(0),
        };
        let ordered = list.apply(items).expect("ok");
        assert_eq!(ordered[0].alias, "c");
        assert_eq!(ordered[1].alias, "b");

        let items = vec![item("c", 3), item("a", 1), item("b", 2)];
        let list = ListQuery {
            filter: None,
            orderby: parse_orderby("alias").expect("parses"),
            top: None,
            skip: Some(1),
        };
        let ordered = list.apply(items).expect("ok");
        assert_eq!(ordered.len(), 2);
        assert_eq!(ordered[0].alias, "b");
    }

    #[test]
    fn orderby_directions_are_validated() {
        assert!(parse_orderby("alias asc").is_ok());
        assert!(parse_orderby("alias descending").is_err());
        assert!(parse_orderby("alias asc, priority desc").is_ok());
    }

    #[test]
    fn skip_alone_paginates() {
        let items = vec![item("a", 1), item("b", 2)];
        let list = ListQuery {
            filter: None,
            orderby: Vec::new(),
            top: Some(1),
            skip: Some(1),
        };
        let paged = list.apply(items).expect("ok");
        assert_eq!(paged.len(), 1);
        assert_eq!(paged[0].alias, "b");
    }
}
