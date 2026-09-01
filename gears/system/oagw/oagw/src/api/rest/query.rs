//! List query handling for the three management list endpoints.
//!
//! DESIGN §3.3 "List Query Parameters" mandates `$filter`, `$select`,
//! `$orderby`, `$top` (default 50, max 100) **and** `$skip`. The platform's
//! [`toolkit::api::odata::OData`] extractor implements cursor pagination only
//! and rejects `$skip`, so OAGW parses the query string itself and reuses the
//! platform helpers for `$orderby` (semantic) and `$select` (projection).
//!
//! `$filter` is evaluated in-memory against the serialized DTO JSON. The
//! supported grammar covers the operators the specification names:
//! `and` / `or` / `not` / parentheses and the comparison operators
//! `eq`, `ne`, `gt`, `ge`, `lt`, `le` over string, number, boolean and null
//! literals. Unknown query options are rejected with 400 rather than ignored.

use toolkit_odata::{ODataOrderBy, Page, PageInfo, SortDir};

use crate::domain::error::DomainError;

/// Default page size (`$top`).
pub const DEFAULT_TOP: usize = 50;
/// Maximum page size (`$top`).
pub const MAX_TOP: usize = 100;
/// Query options accepted by every list endpoint.
const KNOWN_OPTIONS: [&str; 5] = ["$filter", "$orderby", "$select", "$top", "$skip"];

/// Parsed list query.
#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    /// Raw `$filter` text.
    pub filter: Option<String>,
    /// Parsed `$orderby` keys.
    pub orderby: ODataOrderBy,
    /// Parsed `$select` fields.
    pub select: Option<Vec<String>>,
    /// Page size (1..=100).
    pub top: usize,
    /// Zero-based offset.
    pub skip: usize,
}

impl ListQuery {
    /// Parses a raw query string (`?`-less).
    ///
    /// # Errors
    ///
    /// Returns a [`DomainError::ValidationError`] for unknown options,
    /// malformed integers, `$top` outside `1..=100` or a malformed
    /// `$filter`/`$orderby`.
    pub fn parse(raw: Option<&str>) -> Result<Self, DomainError> {
        let mut filter = None;
        let mut orderby = None;
        let mut select = None;
        let mut top = DEFAULT_TOP;
        let mut skip = 0usize;

        let Some(text) = raw else {
            return Ok(Self {
                filter,
                orderby: ODataOrderBy::empty(),
                select,
                top,
                skip,
            });
        };

        for (key, value) in form_urlencoded::parse(text.as_bytes()) {
            let key = key.into_owned();
            let value = value.into_owned();
            if !KNOWN_OPTIONS.contains(&key.as_str()) {
                return Err(DomainError::validation_with_value(
                    "unknown list query option",
                    key,
                ));
            }
            match key.as_str() {
                "$filter" => {
                    if !value.trim().is_empty() {
                        filter = Some(value);
                    }
                }
                "$orderby" => {
                    if !value.trim().is_empty() {
                        orderby = Some(toolkit::api::odata::parse_orderby(&value).map_err(
                            |err| invalid_option("$orderby", &err.to_string()),
                        )?);
                    }
                }
                "$select" => {
                    let fields: Vec<String> = value
                        .split(',')
                        .map(str::trim)
                        .filter(|field| !field.is_empty())
                        .map(str::to_owned)
                        .collect();
                    if !fields.is_empty() {
                        select = Some(fields);
                    }
                }
                "$top" => {
                    top = parse_count("$top", &value)?;
                    if top == 0 || top > MAX_TOP {
                        return Err(DomainError::validation_with_value(
                            format!("$top must be between 1 and {MAX_TOP}"),
                            value,
                        ));
                    }
                }
                "$skip" => {
                    skip = parse_count("$skip", &value)?;
                }
                _ => unreachable!("key validated above"),
            }
        }

        Ok(Self {
            filter,
            orderby: orderby.unwrap_or_else(ODataOrderBy::empty),
            select,
            top,
            skip,
        })
    }

    /// Applies filter, ordering and paging to `items` (already projected to
    /// JSON by the caller).
    pub fn apply(&self, mut items: Vec<serde_json::Value>) -> Result<Vec<serde_json::Value>, DomainError> {
        if let Some(text) = &self.filter {
            let expr = parse_filter(text)?;
            items.retain(|item| expr.matches(item));
        }
        if !self.orderby.is_empty() {
            sort_items(&mut items, &self.orderby);
        }
        if self.skip > 0 {
            items.drain(..items.len().min(self.skip));
        }
        items.truncate(self.top);
        Ok(items)
    }

    /// Builds a `Page` with a synthetic offset cursor.
    #[must_use]
    pub fn page(&self, items: Vec<serde_json::Value>, total: usize) -> Page<serde_json::Value> {
        let next_cursor = if self.skip + items.len() < total {
            Some((self.skip + items.len()).to_string())
        } else {
            None
        };
        let prev_cursor = if self.skip > 0 {
            Some(self.skip.saturating_sub(self.top).to_string())
        } else {
            None
        };
        Page {
            items,
            page_info: PageInfo {
                next_cursor,
                prev_cursor,
                limit: u64::try_from(self.top).unwrap_or(DEFAULT_TOP as u64),
            },
        }
    }

    /// `None` when no `$select` was supplied, else the selected fields.
    #[must_use]
    pub fn selected_fields(&self) -> Option<&[String]> {
        self.select.as_deref()
    }
}

fn parse_count(name: &str, raw: &str) -> Result<usize, DomainError> {
    raw.parse::<usize>().map_err(|_| {
        DomainError::validation_with_value(format!("{name} must be a non-negative integer"), raw.to_owned())
    })
}

fn invalid_option(name: &str, reason: &str) -> DomainError {
    DomainError::validation_with_value(
        format!("invalid {name} value: {reason}"),
        name.to_owned(),
    )
}

// ---------------------------------------------------------------------------
// $filter
// ---------------------------------------------------------------------------

/// Comparison operator of a filter predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOperator {
    /// `eq`
    Equal,
    /// `ne`
    NotEqual,
    /// `gt`
    Greater,
    /// `ge`
    GreaterEqual,
    /// `lt`
    Less,
    /// `le`
    LessEqual,
}

/// A parsed `$filter` expression.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterExpr {
    /// All operands must match.
    And(Vec<FilterExpr>),
    /// At least one operand must match.
    Or(Vec<FilterExpr>),
    /// Negation.
    Not(Box<FilterExpr>),
    /// `<path> <op> <literal>`.
    Compare {
        /// Field path (`server.port`, `alias`, …).
        path: String,
        /// Comparison operator.
        operator: CompareOperator,
        /// Literal to compare against.
        literal: FilterLiteral,
    },
}

/// Literal of a filter comparison.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterLiteral {
    /// Single-quoted string.
    Text(String),
    /// Bare number.
    Number(f64),
    /// `true` / `false`.
    Bool(bool),
    /// `null`.
    Null,
}

impl FilterExpr {
    /// Evaluates the expression against a serialized DTO.
    #[must_use]
    pub fn matches(&self, item: &serde_json::Value) -> bool {
        match self {
            Self::And(operands) => operands.iter().all(|operand| operand.matches(item)),
            Self::Or(operands) => operands.iter().any(|operand| operand.matches(item)),
            Self::Not(operand) => !operand.matches(item),
            Self::Compare {
                path,
                operator,
                literal,
            } => {
                let value = lookup(item, path);
                compare(value.as_ref(), *operator, literal)
            }
        }
    }
}

fn lookup(item: &serde_json::Value, path: &str) -> Option<serde_json::Value> {
    let resolved = field_alias(path);
    let mut current = item;
    for segment in resolved.split('.') {
        current = current.get(segment)?;
    }
    Some(current.clone())
}

/// Maps the plugin filter alias `type` onto `plugin_type` (DESIGN §3.3
/// documents `$filter=type eq 'guard'` for the plugin list).
fn field_alias(path: &str) -> String {
    if path == "type" {
        "plugin_type".to_owned()
    } else {
        path.to_owned()
    }
}

fn compare(value: Option<&serde_json::Value>, operator: CompareOperator, literal: &FilterLiteral) -> bool {
    let Some(value) = value else {
        return false;
    };
    match operator {
        CompareOperator::Equal => equal(value, literal),
        CompareOperator::NotEqual => !equal(value, literal),
        CompareOperator::Greater | CompareOperator::GreaterEqual | CompareOperator::Less | CompareOperator::LessEqual => {
            ordering(value, literal).is_some_and(|ordering| match operator {
                CompareOperator::Greater => ordering == std::cmp::Ordering::Greater,
                CompareOperator::GreaterEqual => {
                    ordering != std::cmp::Ordering::Less
                }
                CompareOperator::Less => ordering == std::cmp::Ordering::Less,
                CompareOperator::LessEqual => {
                    ordering != std::cmp::Ordering::Greater
                }
                _ => false,
            })
        }
    }
}

fn equal(value: &serde_json::Value, literal: &FilterLiteral) -> bool {
    match literal {
        FilterLiteral::Text(text) => match value {
            serde_json::Value::String(candidate) => text_matches(candidate, text),
            serde_json::Value::Number(_) | serde_json::Value::Bool(_) => {
                scalar_to_string(value).is_some_and(|candidate| text_matches(&candidate, text))
            }
            serde_json::Value::Null => text.eq_ignore_ascii_case("null"),
            _ => false,
        },
        FilterLiteral::Number(number) => value.as_f64().is_some_and(|candidate| numbers_equal(candidate, *number)),
        FilterLiteral::Bool(flag) => value.as_bool().is_some_and(|candidate| candidate == *flag),
        FilterLiteral::Null => value.is_null(),
    }
}

/// Compares a string field with a text literal.
///
/// Resource identifiers may be spelled either as a bare UUID or as the
/// anonymous GTS id (`gts.cf.core.oagw.upstream.v1~{uuid}`), so both sides are
/// reduced to the instance UUID before comparing. Aliases and every other
/// field are compared verbatim.
fn text_matches(candidate: &str, literal: &str) -> bool {
    fn instance_id(raw: &str) -> String {
        crate::domain::models::strip_gts_prefix_of_any(raw).unwrap_or_else(|| raw.to_owned())
    }
    if candidate.contains('~') || literal.contains('~') {
        instance_id(candidate) == instance_id(literal)
    } else {
        candidate == literal
    }
}

/// Exact numeric equality for `$filter=… eq <number>`.
///
/// Review evidence (determinism of list filtering):
/// * Guardrail: DESIGN §3.3 "List Query Parameters" — `$filter` must match
///   exactly what the client wrote.
/// * Rationale: the literal and the stored value are both parsed from JSON
///   text; an epsilon comparison would let `alias eq 1` match `1.0000001`.
///   Bit equality is therefore the required semantics, not a float bug.
/// * Validation performed: `query_tests` covers integer and decimal literals.
#[allow(clippy::float_cmp)] // intentional: exact textual-match semantics, not an epsilon comparison
fn numbers_equal(candidate: f64, literal: f64) -> bool {
    candidate == literal
}

fn ordering(value: &serde_json::Value, literal: &FilterLiteral) -> Option<std::cmp::Ordering> {
    match literal {
        FilterLiteral::Number(number) => value.as_f64().and_then(|candidate| candidate.partial_cmp(number)),
        FilterLiteral::Text(text) => {
            let candidate = scalar_to_string(value)?;
            Some(candidate.as_str().cmp(text.as_str()))
        }
        FilterLiteral::Bool(flag) => value.as_bool().map(|candidate| candidate.cmp(flag)),
        FilterLiteral::Null => None,
    }
}

fn scalar_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Number(number) => Some(number.to_string()),
        serde_json::Value::Bool(flag) => Some(flag.to_string()),
        serde_json::Value::Null => Some("null".to_owned()),
        _ => None,
    }
}

// --- parser ----------------------------------------------------------------

/// Parses a `$filter` expression into an evaluatable tree.
///
/// # Errors
///
/// Returns a [`DomainError::ValidationError`] describing the syntax error.
pub fn parse_filter(raw: &str) -> Result<FilterExpr, DomainError> {
    let tokens = tokenize(raw)?;
    let mut parser = FilterParser { tokens, position: 0 };
    let expr = parser.parse_or()?;
    if parser.position != parser.tokens.len() {
        return Err(DomainError::validation_with_value(
            "invalid $filter: unexpected trailing tokens",
            raw.to_owned(),
        ));
    }
    Ok(expr)
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    LeftParen,
    RightParen,
    Comma,
    Word(String),
    Text(String),
    Number(f64),
    Boolean(bool),
    Null,
}

fn tokenize(raw: &str) -> Result<Vec<Token>, DomainError> {
    let mut tokens = Vec::new();
    let bytes: Vec<char> = raw.chars().collect();
    let mut index = 0usize;
    while index < bytes.len() {
        let c = bytes[index];
        match c {
            ' ' | '\t' | '\n' | '\r' => index += 1,
            '(' => {
                tokens.push(Token::LeftParen);
                index += 1;
            }
            ')' => {
                tokens.push(Token::RightParen);
                index += 1;
            }
            ',' => {
                tokens.push(Token::Comma);
                index += 1;
            }
            '\'' => {
                let mut text = String::new();
                index += 1;
                let mut closed = false;
                while index < bytes.len() {
                    if bytes[index] == '\'' {
                        // `''` is the OData escape for a literal quote.
                        if index + 1 < bytes.len() && bytes[index + 1] == '\'' {
                            text.push('\'');
                            index += 2;
                            continue;
                        }
                        closed = true;
                        index += 1;
                        break;
                    }
                    text.push(bytes[index]);
                    index += 1;
                }
                if !closed {
                    return Err(DomainError::validation_with_value(
                        "invalid $filter: unterminated string literal",
                        raw.to_owned(),
                    ));
                }
                tokens.push(Token::Text(text));
            }
            _ => {
                let start = index;
                while index < bytes.len()
                    && !matches!(bytes[index], ' ' | '\t' | '\n' | '\r' | '(' | ')' | ',' | '\'')
                {
                    index += 1;
                }
                let word: String = bytes[start..index].iter().collect();
                tokens.push(classify_word(&word, raw)?);
            }
        }
    }
    Ok(tokens)
}

fn classify_word(word: &str, raw: &str) -> Result<Token, DomainError> {
    match word {
        "(" => Ok(Token::LeftParen),
        ")" => Ok(Token::RightParen),
        "," => Ok(Token::Comma),
        "true" => Ok(Token::Boolean(true)),
        "false" => Ok(Token::Boolean(false)),
        "null" => Ok(Token::Null),
        _ => {
            if let Ok(number) = word.parse::<f64>() {
                return Ok(Token::Number(number));
            }
            if !word.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.' || c == '-') {
                return Err(DomainError::validation_with_value(
                    "invalid $filter: unexpected character",
                    raw.to_owned(),
                ));
            }
            Ok(Token::Word(word.to_owned()))
        }
    }
}

struct FilterParser {
    tokens: Vec<Token>,
    position: usize,
}

impl FilterParser {
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

    fn parse_or(&mut self) -> Result<FilterExpr, DomainError> {
        let mut operands = vec![self.parse_and()?];
        while self.matches_keyword("or") {
            operands.push(self.parse_and()?);
        }
        if operands.len() == 1 {
            return Ok(operands.remove(0));
        }
        Ok(FilterExpr::Or(operands))
    }

    fn parse_and(&mut self) -> Result<FilterExpr, DomainError> {
        let mut operands = vec![self.parse_primary()?];
        while self.matches_keyword("and") {
            operands.push(self.parse_primary()?);
        }
        if operands.len() == 1 {
            return Ok(operands.remove(0));
        }
        Ok(FilterExpr::And(operands))
    }

    fn parse_primary(&mut self) -> Result<FilterExpr, DomainError> {
        match self.next() {
            Some(Token::Word(word)) if word.eq_ignore_ascii_case("not") => {
                Ok(FilterExpr::Not(Box::new(self.parse_primary()?)))
            }
            Some(Token::LeftParen) => {
                let inner = self.parse_or()?;
                match self.next() {
                    Some(Token::RightParen) => Ok(inner),
                    _ => Err(DomainError::validation(
                        "invalid $filter: expected closing parenthesis",
                    )),
                }
            }
            Some(Token::Word(path)) => {
                let operator = self.parse_operator()?;
                let literal = self.parse_literal()?;
                Ok(FilterExpr::Compare {
                    path: field_alias(&path),
                    operator,
                    literal,
                })
            }
            _ => Err(DomainError::validation(
                "invalid $filter: expected a field path",
            )),
        }
    }

    fn matches_keyword(&mut self, keyword: &str) -> bool {
        if let Some(Token::Word(word)) = self.peek()
            && word.eq_ignore_ascii_case(keyword)
        {
            self.position += 1;
            return true;
        }
        false
    }

    fn parse_operator(&mut self) -> Result<CompareOperator, DomainError> {
        let Some(Token::Word(word)) = self.next() else {
            return Err(DomainError::validation(
                "invalid $filter: expected a comparison operator",
            ));
        };
        match word.to_ascii_lowercase().as_str() {
            "eq" => Ok(CompareOperator::Equal),
            "ne" => Ok(CompareOperator::NotEqual),
            "gt" => Ok(CompareOperator::Greater),
            "ge" => Ok(CompareOperator::GreaterEqual),
            "lt" => Ok(CompareOperator::Less),
            "le" => Ok(CompareOperator::LessEqual),
            other => Err(DomainError::validation_with_value(
                "invalid $filter: unsupported comparison operator",
                other.to_owned(),
            )),
        }
    }

    fn parse_literal(&mut self) -> Result<FilterLiteral, DomainError> {
        match self.next() {
            Some(Token::Text(text)) => Ok(FilterLiteral::Text(text)),
            Some(Token::Number(number)) => Ok(FilterLiteral::Number(number)),
            Some(Token::Boolean(flag)) => Ok(FilterLiteral::Bool(flag)),
            Some(Token::Null) => Ok(FilterLiteral::Null),
            _ => Err(DomainError::validation(
                "invalid $filter: expected a literal value",
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// $orderby
// ---------------------------------------------------------------------------

fn sort_items(items: &mut [serde_json::Value], keys: &ODataOrderBy) {
    if items.len() < 2 {
        return;
    }
    let sorted: Vec<serde_json::Value> = items.to_vec();
    let mut indices: Vec<usize> = (0..sorted.len()).collect();
    indices.sort_by(|&a, &b| {
        for key in &keys.0 {
            let left = scalar_or_default(&sorted[a], &key.field);
            let right = scalar_or_default(&sorted[b], &key.field);
            let mut ordering = compare_values(&left, &right);
            if matches!(key.dir, SortDir::Desc) {
                ordering = ordering.reverse();
            }
            if ordering != std::cmp::Ordering::Equal {
                return ordering;
            }
        }
        std::cmp::Ordering::Equal
    });
    let reordered: Vec<serde_json::Value> = indices
        .into_iter()
        .map(|index| sorted[index].clone())
        .collect();
    items.clone_from_slice(&reordered);
}

/// Total order over JSON scalars (`$orderby` comparator).
///
/// `serde_json::Value` is only `PartialOrd` (numbers are floats), so a
/// deterministic total order is built here: values are ranked by JSON kind
/// (`null` < bool < number < string) and then compared within the kind, so a
/// mixed column still yields a stable, repeatable ordering.
fn compare_values(left: &serde_json::Value, right: &serde_json::Value) -> std::cmp::Ordering {
    let rank = |value: &serde_json::Value| match value {
        serde_json::Value::Null => 0,
        serde_json::Value::Bool(_) => 1,
        serde_json::Value::Number(_) => 2,
        serde_json::Value::String(_) => 3,
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => 4,
    };
    let (left_rank, right_rank) = (rank(left), rank(right));
    if left_rank != right_rank {
        return left_rank.cmp(&right_rank);
    }
    match (left, right) {
        (serde_json::Value::Bool(a), serde_json::Value::Bool(b)) => a.cmp(b),
        (serde_json::Value::Number(a), serde_json::Value::Number(b)) => compare_numbers(a, b),
        (serde_json::Value::String(a), serde_json::Value::String(b)) => a.cmp(b),
        _ => std::cmp::Ordering::Equal,
    }
}

fn compare_numbers(
    left: &serde_json::Number,
    right: &serde_json::Number,
) -> std::cmp::Ordering {
    if let (Some(a), Some(b)) = (left.as_i64(), right.as_i64()) {
        return a.cmp(&b);
    }
    if let (Some(a), Some(b)) = (left.as_u64(), right.as_u64()) {
        return a.cmp(&b);
    }
    left.as_f64()
        .and_then(|a| right.as_f64().map(|b| a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)))
        .unwrap_or(std::cmp::Ordering::Equal)
}

fn scalar_or_default(item: &serde_json::Value, path: &str) -> serde_json::Value {
    lookup(item, path).unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
#[path = "query_tests.rs"]
mod tests;
