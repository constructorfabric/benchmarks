//! Domain-side list-query model and the proxy request context.
//!
//! `DESIGN` §3.3 mandates the `OData` system query options `$filter`,
//! `$select`, `$orderby`, `$top` and `$skip` on every collection. The platform
//! `OData` extractor (`toolkit::api::odata`) is deliberately not used here: it
//! rejects `$skip` — offset paging — and leaves `$top` unclamped, both of which
//! this gear's contract requires. The AST below is parsed and evaluated
//! in-domain so it stays free of `axum` and unit-testable.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::domain::error::DomainError;

/// Maximum accepted length of a `$filter` value.
pub const MAX_FILTER_LEN: usize = 8 * 1024;
/// Maximum accepted length of an `$orderby` value.
pub const MAX_ORDERBY_LEN: usize = 1024;
/// Maximum accepted length of a `$select` value.
pub const MAX_SELECT_LEN: usize = 2048;
/// Maximum number of comma-separated `$select` fields.
pub const MAX_SELECT_FIELDS: usize = 100;
/// Maximum number of `$orderby` keys.
pub const MAX_ORDER_FIELDS: usize = 10;

/// A comparison operator accepted in `$filter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    /// Field equals the literal.
    Eq,
    /// Field differs from the literal.
    Ne,
    /// Field is greater than the literal.
    Gt,
    /// Field is greater than or equal to the literal.
    Ge,
    /// Field is less than the literal.
    Lt,
    /// Field is less than or equal to the literal.
    Le,
    /// Field (a string) contains the literal.
    Contains,
    /// Field (a string) starts with the literal.
    StartsWith,
}

impl CompareOp {
    fn parse(token: &str) -> Option<Self> {
        Some(match token {
            "eq" => Self::Eq,
            "ne" => Self::Ne,
            "gt" => Self::Gt,
            "ge" => Self::Ge,
            "lt" => Self::Lt,
            "le" => Self::Le,
            "contains" => Self::Contains,
            "startswith" => Self::StartsWith,
            _ => return None,
        })
    }
}

impl fmt::Display for CompareOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Eq => "eq",
            Self::Ne => "ne",
            Self::Gt => "gt",
            Self::Ge => "ge",
            Self::Lt => "lt",
            Self::Le => "le",
            Self::Contains => "contains",
            Self::StartsWith => "startswith",
        })
    }
}

/// A literal accepted on the right-hand side of a comparison.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterValue {
    /// Quoted string literal.
    Text(String),
    /// Numeric literal.
    Number(f64),
    /// `true` / `false`.
    Bool(bool),
    /// `null`.
    Null,
}

/// A `$filter` expression.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterExpr {
    /// `field op literal`.
    Compare {
        /// Dotted/slash-separated field path.
        field: String,
        /// Operator.
        op: CompareOp,
        /// Right-hand literal.
        value: FilterValue,
    },
    /// Conjunction.
    And(Vec<FilterExpr>),
    /// Disjunction.
    Or(Vec<FilterExpr>),
    /// Negation.
    Not(Box<FilterExpr>),
}

impl FilterExpr {
    /// Evaluate the expression against a JSON document.
    ///
    /// A missing field never matches a comparison, so a filter cannot
    /// accidentally select rows that lack the attribute it constrains.
    #[must_use]
    pub fn matches(&self, doc: &serde_json::Value) -> bool {
        match self {
            Self::Compare { field, op, value } => {
                let actual = lookup(doc, field);
                compare(actual, *op, value)
            }
            Self::And(terms) => terms.iter().all(|term| term.matches(doc)),
            Self::Or(terms) => terms.iter().any(|term| term.matches(doc)),
            Self::Not(term) => !term.matches(doc),
        }
    }
}

/// Resolve a `a.b/c` field path against a JSON document.
fn lookup<'a>(doc: &'a serde_json::Value, field: &str) -> Option<&'a serde_json::Value> {
    let mut current = doc;
    for segment in field.split(['.', '/']) {
        current = current.get(segment)?;
    }
    Some(current)
}

fn compare(actual: Option<&serde_json::Value>, op: CompareOp, expected: &FilterValue) -> bool {
    use serde_json::Value as V;
    // OData semantics: an absent property compares as `null`, so
    // `$filter=alias eq null` selects the rows that lack the field.
    const NULL: V = V::Null;
    let actual = actual.unwrap_or(&NULL);
    let ordering = match (actual, expected) {
        (V::String(actual), FilterValue::Text(expected)) => {
            Some(actual.as_str().cmp(expected.as_str()))
        }
        (V::Number(actual), FilterValue::Number(expected)) => {
            actual.as_f64().and_then(|a| a.partial_cmp(expected))
        }
        (V::Bool(actual), FilterValue::Bool(expected)) => Some(actual.cmp(expected)),
        (V::Null, FilterValue::Null) => Some(std::cmp::Ordering::Equal),
        _ => None,
    };
    match op {
        CompareOp::Eq => ordering == Some(std::cmp::Ordering::Equal),
        CompareOp::Ne => ordering != Some(std::cmp::Ordering::Equal),
        CompareOp::Gt => ordering == Some(std::cmp::Ordering::Greater),
        CompareOp::Ge => ordering.is_some_and(|o| o != std::cmp::Ordering::Less),
        CompareOp::Lt => ordering == Some(std::cmp::Ordering::Less),
        CompareOp::Le => ordering.is_some_and(|o| o != std::cmp::Ordering::Greater),
        // Textual operators never match a non-string actual value.
        CompareOp::Contains => {
            let (V::String(actual), FilterValue::Text(expected)) = (actual, expected) else {
                return false;
            };
            actual.contains(expected.as_str())
        }
        CompareOp::StartsWith => {
            let (V::String(actual), FilterValue::Text(expected)) = (actual, expected) else {
                return false;
            };
            actual.starts_with(expected.as_str())
        }
    }
}

/// One `$orderby` key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrderKey {
    /// Field name.
    pub field: String,
    /// Sort direction; ascending when omitted on the wire.
    #[serde(default = "default_asc")]
    pub dir: SortDir,
}

/// Default sort direction (`asc`), for serde.
fn default_asc() -> SortDir {
    SortDir::Asc
}

/// Sort direction of an `$orderby` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortDir {
    /// Ascending.
    Asc,
    /// Descending.
    Desc,
}

/// A parsed, validated list request.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ListQuery {
    /// Parsed `$filter`, when supplied.
    pub filter: Option<FilterExpr>,
    /// Parsed `$orderby` keys, in wire order.
    pub order: Vec<OrderKey>,
    /// Lowercased `$select` field names, in wire order.
    pub select: Vec<String>,
    /// Page size after clamping; `None` means the default page size.
    pub top: Option<u64>,
    /// Offset paging position.
    pub skip: u64,
}

impl ListQuery {
    /// `true` when the query carries no restriction at all.
    #[must_use]
    pub fn is_unconstrained(&self) -> bool {
        self.filter.is_none()
            && self.order.is_empty()
            && self.select.is_empty()
            && self.top.is_none()
            && self.skip == 0
    }
}

/// Compare two JSON documents by the parsed `$orderby` keys.
///
/// Keys are applied in wire order; a missing key sorts before a present one.
fn order_compare(
    left: &serde_json::Value,
    right: &serde_json::Value,
    order: &[OrderKey],
) -> std::cmp::Ordering {
    for key in order {
        let ordering = match (lookup(left, &key.field), lookup(right, &key.field)) {
            (Some(a), Some(b)) => json_cmp(a, b),
            (Some(_), None) => std::cmp::Ordering::Greater,
            (None, Some(_)) => std::cmp::Ordering::Less,
            (None, None) => std::cmp::Ordering::Equal,
        };
        let ordering = match key.dir {
            SortDir::Asc => ordering,
            SortDir::Desc => ordering.reverse(),
        };
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    std::cmp::Ordering::Equal
}

/// Apply a parsed [`ListQuery`] to a repository result set.
///
/// The rows are filtered, ordered, offset and truncated in that order. The
/// projection callback renders each row as JSON so the filter and the sort
/// keys can address every serialized field, including nested ones; rows are
/// returned in their original type, so `$select` remains a transport concern.
#[must_use]
pub fn apply_list<T, F>(rows: Vec<T>, query: &ListQuery, mut project: F) -> Vec<T>
where
    F: FnMut(&T) -> serde_json::Value,
{
    let mut paired: Vec<(serde_json::Value, T)> = rows
        .into_iter()
        .map(|row| {
            let document = project(&row);
            (document, row)
        })
        .collect();
    if let Some(filter) = &query.filter {
        paired.retain(|(document, _)| filter.matches(document));
    }
    if !query.order.is_empty() {
        let order = query.order.as_slice();
        paired.sort_by(|(left, _), (right, _)| order_compare(left, right, order));
    }
    let mut selected: Vec<T> = paired.into_iter().map(|(_, row)| row).collect();
    let skip = usize::try_from(query.skip).unwrap_or(usize::MAX);
    if skip < selected.len() {
        selected.drain(..skip);
    } else {
        selected.clear();
    }
    if let Some(top) = query.top {
        let top = usize::try_from(top).unwrap_or(usize::MAX);
        selected.truncate(top);
    }
    selected
}

fn json_cmp(left: &serde_json::Value, right: &serde_json::Value) -> std::cmp::Ordering {
    use serde_json::Value as V;
    match (left, right) {
        (V::String(a), V::String(b)) => a.cmp(b),
        (V::Number(a), V::Number(b)) => match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal),
            _ => std::cmp::Ordering::Equal,
        },
        (V::Bool(a), V::Bool(b)) => a.cmp(b),
        _ => std::cmp::Ordering::Equal,
    }
}

// ---------------------------------------------------------------- filtering

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

/// Lex a `$filter` value.
fn tokenize(input: &str) -> Result<Vec<Token>, DomainError> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            ' ' | '\t' | '\n' | '\r' => {
                chars.next();
            }
            '(' => {
                tokens.push(Token::Open);
                chars.next();
            }
            ')' => {
                tokens.push(Token::Close);
                chars.next();
            }
            ',' => {
                tokens.push(Token::Comma);
                chars.next();
            }
            '\'' => {
                chars.next();
                let mut literal = String::new();
                loop {
                    match chars.next() {
                        Some('\'') => {
                            if chars.peek() == Some(&'\'') {
                                literal.push('\'');
                                chars.next();
                            } else {
                                break;
                            }
                        }
                        Some(next) => literal.push(next),
                        None => {
                            return Err(DomainError::validation(
                                "$filter has an unterminated string literal",
                            ));
                        }
                    }
                }
                tokens.push(Token::Text(literal));
            }
            c if c.is_ascii_digit()
                || (c == '-' && chars.peek().is_some_and(char::is_ascii_digit)) =>
            {
                let mut literal = String::new();
                literal.push(c);
                chars.next();
                while let Some(&next) = chars.peek() {
                    if next.is_ascii_digit() || next == '.' {
                        literal.push(next);
                        chars.next();
                    } else {
                        break;
                    }
                }
                let parsed = literal.parse::<f64>().map_err(|_| {
                    DomainError::validation(format!("$filter has a bad number: {literal}"))
                })?;
                tokens.push(Token::Number(parsed));
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let mut literal = String::new();
                while let Some(&next) = chars.peek() {
                    if next.is_ascii_alphanumeric() || matches!(next, '_' | '.' | '/') {
                        literal.push(next);
                        chars.next();
                    } else {
                        break;
                    }
                }
                match literal.as_str() {
                    "true" => tokens.push(Token::Bool(true)),
                    "false" => tokens.push(Token::Bool(false)),
                    "null" => tokens.push(Token::Null),
                    _ => tokens.push(Token::Ident(literal)),
                }
            }
            other => {
                return Err(DomainError::validation(format!(
                    "$filter contains an unexpected character '{other}'"
                )));
            }
        }
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    cursor: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.cursor)
    }

    fn bump(&mut self) -> Option<Token> {
        let token = self.tokens.get(self.cursor).cloned();
        if token.is_some() {
            self.cursor += 1;
        }
        token
    }

    fn expression(&mut self) -> Result<FilterExpr, DomainError> {
        let first = self.conjunction()?;
        if !matches!(self.peek(), Some(Token::Ident(word)) if word == "or") {
            return Ok(first);
        }
        let mut terms = vec![first];
        while matches!(self.peek(), Some(Token::Ident(word)) if word == "or") {
            self.bump();
            terms.push(self.conjunction()?);
        }
        Ok(FilterExpr::Or(terms))
    }

    fn conjunction(&mut self) -> Result<FilterExpr, DomainError> {
        let first = self.unit()?;
        if !matches!(self.peek(), Some(Token::Ident(word)) if word == "and") {
            return Ok(first);
        }
        let mut terms = vec![first];
        while matches!(self.peek(), Some(Token::Ident(word)) if word == "and") {
            self.bump();
            terms.push(self.unit()?);
        }
        Ok(FilterExpr::And(terms))
    }

    fn unit(&mut self) -> Result<FilterExpr, DomainError> {
        match self.peek() {
            Some(Token::Ident(word)) if word == "not" => {
                self.bump();
                Ok(FilterExpr::Not(Box::new(self.unit()?)))
            }
            Some(Token::Open) => {
                self.bump();
                let inner = self.expression()?;
                match self.bump() {
                    Some(Token::Close) => Ok(inner),
                    _ => Err(DomainError::validation("$filter has an unbalanced '('")),
                }
            }
            _ => self.comparison(),
        }
    }

    fn comparison(&mut self) -> Result<FilterExpr, DomainError> {
        // Function form: contains(field, 'value') / startswith(field, 'value').
        if let Some(Token::Ident(word)) = self.peek().cloned() {
            let op = CompareOp::parse(&word)
                .filter(|op| matches!(op, CompareOp::Contains | CompareOp::StartsWith));
            if let Some(op) = op {
                self.bump();
                self.expect_open()?;
                let field = self.expect_field()?;
                self.expect_comma()?;
                let value = self.expect_literal()?;
                self.expect_close()?;
                return Ok(FilterExpr::Compare { field, op, value });
            }
        }

        let field = self.expect_field()?;
        let Some(Token::Ident(word)) = self.peek().cloned() else {
            return Err(DomainError::validation(format!(
                "$filter is missing an operator after '{field}'"
            )));
        };
        let Some(op) = CompareOp::parse(&word) else {
            return Err(DomainError::validation(format!(
                "$filter has an unknown operator '{word}'"
            )));
        };
        self.bump();
        let value = self.expect_literal()?;
        Ok(FilterExpr::Compare { field, op, value })
    }

    fn expect_open(&mut self) -> Result<(), DomainError> {
        match self.bump() {
            Some(Token::Open) => Ok(()),
            _ => Err(DomainError::validation(
                "$filter expects '(' after contains",
            )),
        }
    }

    fn expect_close(&mut self) -> Result<(), DomainError> {
        match self.bump() {
            Some(Token::Close) => Ok(()),
            _ => Err(DomainError::validation(
                "$filter expects ')' to close the call",
            )),
        }
    }

    fn expect_comma(&mut self) -> Result<(), DomainError> {
        match self.bump() {
            Some(Token::Comma) => Ok(()),
            _ => Err(DomainError::validation(
                "$filter expects ',' between arguments",
            )),
        }
    }

    fn expect_field(&mut self) -> Result<String, DomainError> {
        match self.bump() {
            Some(Token::Ident(field)) => Ok(field),
            _ => Err(DomainError::validation(
                "$filter expects a field name before the operator",
            )),
        }
    }

    fn expect_literal(&mut self) -> Result<FilterValue, DomainError> {
        match self.bump() {
            Some(Token::Text(text)) => Ok(FilterValue::Text(text)),
            Some(Token::Number(number)) => Ok(FilterValue::Number(number)),
            Some(Token::Bool(value)) => Ok(FilterValue::Bool(value)),
            Some(Token::Null) => Ok(FilterValue::Null),
            Some(Token::Ident(word)) => Ok(FilterValue::Text(word)),
            _ => Err(DomainError::validation(
                "$filter expects a literal after the operator",
            )),
        }
    }
}

/// Parse a `$filter` value into an [`FilterExpr`].
///
/// The accepted subset is the one `DESIGN` §3.3 names: `eq ne gt ge lt le`,
/// the `contains(field,'literal')` and `startswith(field,'literal')` functions,
/// `and` / `or` / `not`, parentheses, and dotted or slash-separated field
/// paths. Anything else is a `400`, never a silent ignore.
///
/// # Errors
/// Returns [`DomainError::Validation`] for an empty, oversized or malformed
/// filter.
pub fn parse_filter(raw: &str) -> Result<FilterExpr, DomainError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(DomainError::validation("$filter cannot be empty"));
    }
    if raw.len() > MAX_FILTER_LEN {
        return Err(DomainError::validation("$filter is too long"));
    }
    let tokens = tokenize(raw)?;
    let mut parser = Parser { tokens, cursor: 0 };
    let expr = parser.expression()?;
    if parser.cursor != parser.tokens.len() {
        return Err(DomainError::validation(
            "$filter has trailing tokens after the expression",
        ));
    }
    Ok(expr)
}

/// Parse an `$orderby` value.
///
/// # Errors
/// Returns [`DomainError::Validation`] for an empty, oversized or malformed
/// clause list.
pub fn parse_order(raw: &str) -> Result<Vec<OrderKey>, DomainError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(DomainError::validation("$orderby cannot be empty"));
    }
    if raw.len() > MAX_ORDERBY_LEN {
        return Err(DomainError::validation("$orderby is too long"));
    }
    let mut keys = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let mut words = part.split_whitespace();
        let Some(field) = words.next() else {
            continue;
        };
        let dir = match words.next() {
            None | Some("asc") => SortDir::Asc,
            Some("desc") => SortDir::Desc,
            Some(other) => {
                return Err(DomainError::validation(format!(
                    "$orderby direction must be 'asc' or 'desc', got '{other}'"
                )));
            }
        };
        if words.next().is_some() {
            return Err(DomainError::validation(format!(
                "$orderby clause is malformed: {part}"
            )));
        }
        keys.push(OrderKey {
            field: field.to_owned(),
            dir,
        });
    }
    if keys.len() > MAX_ORDER_FIELDS {
        return Err(DomainError::validation(format!(
            "$orderby must contain at most {MAX_ORDER_FIELDS} keys"
        )));
    }
    Ok(keys)
}

/// Parse a `$select` value into lowercased field names.
///
/// # Errors
/// Returns [`DomainError::Validation`] for an empty, oversized or malformed
/// field list.
pub fn parse_select(raw: &str) -> Result<Vec<String>, DomainError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(DomainError::validation("$select cannot be empty"));
    }
    if raw.len() > MAX_SELECT_LEN {
        return Err(DomainError::validation("$select is too long"));
    }
    let mut fields = Vec::new();
    for part in raw.split(',') {
        let field = part.trim().to_ascii_lowercase();
        if field.is_empty() {
            return Err(DomainError::validation(
                "$select must not contain empty field names",
            ));
        }
        fields.push(field);
    }
    if fields.len() > MAX_SELECT_FIELDS {
        return Err(DomainError::validation(format!(
            "$select must contain at most {MAX_SELECT_FIELDS} fields"
        )));
    }
    Ok(fields)
}

/// The proxy request as the plugin contracts and the data plane see it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProxyContext {
    /// Routing key of the upstream the request is destined for.
    pub alias: String,
    /// Request method, uppercased.
    pub method: String,
    /// Request path, starting with `/`.
    pub path: String,
    /// Decoded query parameters, in wire order.
    pub query: Vec<(String, String)>,
    /// Request headers, lowercased names.
    pub headers: BTreeMap<String, String>,
    /// Correlation id propagated by the platform.
    pub trace_id: Option<String>,
    /// Tenant of the authenticated caller. Nil when the caller is anonymous.
    pub tenant: uuid::Uuid,
    /// Subject id of the authenticated caller. Nil when the caller is
    /// anonymous.
    pub subject: uuid::Uuid,
}

impl ProxyContext {
    /// The first value of a header, case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }
}

// ------------------------------------------------------------------ commands

/// A create or replace request for an upstream.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamCommand {
    /// Explicit alias; required for non-derivable endpoint pools.
    pub alias: Option<String>,
    /// Upstream protocol.
    pub protocol: crate::domain::model::Protocol,
    /// Disabled upstreams reject every request.
    pub enabled: bool,
    /// Endpoint pool.
    pub server: crate::domain::model::ServerConfig,
    /// Outbound auth configuration.
    pub auth: Option<crate::domain::model::AuthConfig>,
    /// Header transformation rules.
    pub headers: Option<crate::domain::model::HeadersConfig>,
    /// Rate-limit configuration.
    pub rate_limit: Option<crate::domain::model::RateLimitConfig>,
    /// CORS configuration.
    pub cors: Option<crate::domain::model::CorsConfig>,
    /// Upstream plugin chain.
    pub plugins: Option<crate::domain::model::PluginsConfig>,
    /// Flat categorization tags.
    pub tags: Vec<String>,
}

/// A create or replace request for a route.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteCommand {
    /// Owning upstream; immutable on replace.
    pub upstream_id: uuid::Uuid,
    /// Match rules; exactly one of `http`/`grpc`.
    pub r#match: crate::domain::model::MatchConfig,
    /// Higher priority wins when several routes match.
    pub priority: u32,
    /// Disabled routes never match.
    pub enabled: bool,
    /// Route-level rate-limit override.
    pub rate_limit: Option<crate::domain::model::RateLimitConfig>,
    /// Route-level CORS override.
    pub cors: Option<crate::domain::model::CorsConfig>,
    /// Route-level plugin chain.
    pub plugins: Option<crate::domain::model::PluginsConfig>,
    /// Flat categorization tags.
    pub tags: Vec<String>,
}

/// A create request for a custom plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct PluginCommand {
    /// Plugin kind.
    pub plugin_type: crate::domain::model::PluginType,
    /// Human-readable name.
    pub name: String,
    /// JSON Schema the plugin `config` must satisfy.
    pub config_schema: Option<serde_json::Value>,
    /// Starlark source.
    pub source_code: String,
    /// Phases the plugin declares.
    pub phases: Vec<crate::domain::model::PluginPhase>,
}

/// The identity a management request was authenticated as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestContext {
    /// Tenant the request is scoped to.
    pub tenant: uuid::Uuid,
    /// Authenticated principal.
    pub subject: String,
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "dto_tests.rs"]
mod tests;
