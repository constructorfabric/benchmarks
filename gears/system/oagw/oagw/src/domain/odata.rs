//! Local OData query engine for the OAGW list endpoints (DESIGN "List Query
//! Parameters").
//!
//! The platform extractor (`toolkit::api::odata::OData`) does not accept
//! `$skip`, which DESIGN requires on every list endpoint, so this gear ships a
//! small engine of its own covering exactly the documented subset:
//!
//! | Parameter | Behaviour |
//! |---|---|
//! | `$filter` | `field eq value` / `field ne value`, combined with `and`/`or`, parentheses allowed |
//! | `$select` | comma-separated field names, projected out of every item |
//! | `$orderby` | comma-separated `field [asc\|desc]`, multi-key and stable |
//! | `$top` | page size; defaults to [`DEFAULT_PAGE_SIZE`], clamped to [`MAX_PAGE_SIZE`] |
//! | `$skip` | number of items to skip; defaults to `0` |
//!
//! Only top-level fields listed in the resource's [`FieldCatalog`] are
//! accepted: an unknown field, an unknown operator or a malformed literal is a
//! `400` [`OagwError::Validation`]. `$top` above [`MAX_PAGE_SIZE`] is clamped
//! instead of rejected so a paging client never hard-fails on a page size it
//! cannot discover. Query parameters that do not start with `$` are ignored
//! (the platform may add its own); an unknown `$`-prefixed parameter is
//! rejected so a typo never silently returns an unfiltered list.

use std::cmp::Ordering;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::domain::error::OagwError;

/// Page size used when `$top` is absent (DESIGN: "default: 50").
pub const DEFAULT_PAGE_SIZE: usize = 50;

/// Largest accepted page size (DESIGN: "max: 100"); larger `$top` values are
/// clamped to this.
pub const MAX_PAGE_SIZE: usize = 100;

/// Largest accepted `$filter` expression, in bytes.
///
/// The platform extractor caps its own `$filter` at the same budget
/// (`toolkit::api::odata::MAX_FILTER_LEN`), so both surfaces agree on how much
/// filter text a caller may send.
pub const MAX_FILTER_LEN: usize = 8 * 1024;

/// Deepest accepted parenthesis nesting of a `$filter` expression.
///
/// `parse_primary` recurses once per `(`; a hostile `$filter` of `(((((…`
/// would otherwise recurse unbounded and overflow the stack inside a request
/// handler, so nesting beyond this budget is a `400`.
pub const MAX_FILTER_DEPTH: usize = 32;

/// Sort direction of one `$orderby` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    /// Ascending (the default).
    Asc,
    /// Descending.
    Desc,
}

/// Comparison operator of a `$filter` clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    /// `eq`
    Equal,
    /// `ne`
    NotEqual,
}

/// Literal of a `$filter` clause.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterValue {
    /// `'single quoted'` text (`''` escapes a quote).
    Text(String),
    /// Bare number, optionally signed with an optional fraction/exponent.
    Number(f64),
    /// `true` / `false`.
    Flag(bool),
    /// `null`, which also matches an absent field.
    Null,
}

/// Parsed `$filter` expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    /// One `field op value` comparison.
    Compare {
        /// Canonical field name (an alias has already been resolved).
        field: String,
        /// Comparison operator.
        op: CompareOp,
        /// Literal to compare against.
        value: FilterValue,
    },
    /// Conjunction; every clause must hold.
    All(Vec<Filter>),
    /// Disjunction; at least one clause must hold.
    Any(Vec<Filter>),
}

/// Fields a resource exposes to the query engine.
#[derive(Debug, Clone, Copy)]
pub struct FieldCatalog {
    /// Fields accepted by `$filter`.
    pub filterable: &'static [&'static str],
    /// Fields accepted by `$orderby`.
    pub sortable: &'static [&'static str],
    /// Fields accepted by `$select`.
    pub selectable: &'static [&'static str],
    /// Wire-name aliases, e.g. `("type", "plugin_type")`: the alias is accepted
    /// everywhere and replaced by its target before use.
    pub aliases: &'static [(&'static str, &'static str)],
}

/// A parsed, validated list query.
#[derive(Debug, Clone, PartialEq)]
pub struct ListQuery {
    /// `$filter`, `None` when absent.
    pub filter: Option<Filter>,
    /// `$select` as canonical field names, in request order.
    pub select: Vec<&'static str>,
    /// `$orderby` as `(canonical field name, direction)` pairs, in request order.
    pub orderby: Vec<(&'static str, SortDirection)>,
    /// Page size after clamping.
    pub top: usize,
    /// Number of items to skip.
    pub skip: usize,
}

impl ListQuery {
    /// Parses a raw query string (`x-www-form-urlencoded`, as sent on the wire).
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when a `$`-prefixed parameter is
    /// unknown, a field is not in the catalog, a filter expression is
    /// malformed, a sort direction is unknown, or `$top`/`$skip` are not
    /// non-negative integers.
    pub fn parse(raw: Option<&str>, catalog: &FieldCatalog) -> Result<Self, OagwError> {
        let mut query = Self {
            filter: None,
            select: Vec::new(),
            orderby: Vec::new(),
            top: DEFAULT_PAGE_SIZE,
            skip: 0,
        };
        let Some(raw) = raw else {
            return Ok(query);
        };

        for (key, value) in form_urlencoded::parse(raw.as_bytes()) {
            let key = key.as_ref();
            let value = value.as_ref();
            match key {
                "$filter" => {
                    if value.len() > MAX_FILTER_LEN {
                        return Err(OagwError::validation(format!(
                            "field `$filter`: must be at most {MAX_FILTER_LEN} bytes, got {}",
                            value.len()
                        )));
                    }
                    query.filter = Some(parse_filter(value, catalog)?);
                }
                "$select" => query.select = parse_select(value, catalog)?,
                "$orderby" => query.orderby = parse_orderby(value, catalog)?,
                "$top" => query.top = parse_size(value, "$top")?,
                "$skip" => query.skip = parse_size(value, "$skip")?,
                _ if key.starts_with('$') => {
                    return Err(OagwError::validation(format!(
                        "field `{key}`: unsupported OData query parameter"
                    )));
                }
                _ => {}
            }
        }
        query.top = query.top.min(MAX_PAGE_SIZE);
        Ok(query)
    }

    /// Applies the query to `items`, returning the canonical page envelope.
    ///
    /// Items are round-tripped through their JSON representation so `$filter`
    /// and `$orderby` use the wire field names and `$select` can drop fields.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when an item cannot be serialised or
    /// re-read, which cannot happen for the gear's own DTOs.
    pub fn apply<T>(&self, items: Vec<T>) -> Result<toolkit::Page<T>, OagwError>
    where
        T: Serialize + DeserializeOwned,
    {
        let mut rows = Vec::with_capacity(items.len());
        for item in &items {
            rows.push(serde_json::to_value(item).map_err(|error| {
                OagwError::validation(format!("list item is not serialisable: {error}"))
            })?);
        }
        let paged = self.apply_to_rows(rows);
        let mut projected = Vec::with_capacity(paged.len());
        for row in paged {
            projected.push(T::deserialize(row).map_err(|error| {
                OagwError::validation(format!("list item is not readable: {error}"))
            })?);
        }
        Ok(self.page(projected))
    }

    /// Wraps already-paged items in the canonical page envelope.
    ///
    /// Offset paging needs no cursors, so both stay `null` and `limit` reports
    /// the effective page size.
    #[must_use]
    pub fn page<T>(&self, items: Vec<T>) -> toolkit::Page<T> {
        toolkit::Page {
            items,
            page_info: toolkit::PageInfo {
                next_cursor: None,
                prev_cursor: None,
                limit: u64::try_from(self.top).unwrap_or(0),
            },
        }
    }

    /// Serialises `items`, applies the query and returns the projected page.
    ///
    /// List handlers use this instead of [`Self::apply`]: `$select` removes
    /// fields from the item objects, so the wire items cannot be read back
    /// into a struct with required fields. The OpenAPI document still declares
    /// the full item schema.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when an item cannot be serialised,
    /// which would breach the wire contract rather than a caller mistake.
    pub fn apply_values<T: Serialize>(
        &self,
        items: Vec<T>,
    ) -> Result<toolkit::Page<Value>, OagwError> {
        let rows = serialise_rows(items)?;
        Ok(self.page(self.apply_to_rows(rows)))
    }

    /// Filters, sorts, skips, tops and projects `rows`.
    #[must_use]
    pub fn apply_to_rows(&self, mut rows: Vec<Value>) -> Vec<Value> {
        if let Some(filter) = &self.filter {
            rows.retain(|row| filter.matches(row));
        }
        if !self.orderby.is_empty() {
            sort_rows(&mut rows, &self.orderby);
        }
        if self.skip > 0 {
            let skip = self.skip.min(rows.len());
            rows.drain(0..skip);
        }
        if rows.len() > self.top {
            rows.truncate(self.top);
        }
        if !self.select.is_empty() {
            rows = rows
                .into_iter()
                .map(|row| project(&row, &self.select))
                .collect();
        }
        rows
    }
}

impl Filter {
    /// Evaluates the filter against one row.
    #[must_use]
    pub fn matches(&self, row: &Value) -> bool {
        match self {
            Self::Compare { field, op, value } => {
                let equal = match row.get(field) {
                    Some(Value::String(actual)) => match value {
                        FilterValue::Text(expected) => actual == expected,
                        _ => false,
                    },
                    Some(Value::Number(actual)) => match value {
                        FilterValue::Number(expected) => actual
                            .as_f64()
                            .is_some_and(|actual| numbers_equal(actual, *expected)),
                        _ => false,
                    },
                    Some(Value::Bool(actual)) => match value {
                        FilterValue::Flag(expected) => actual == expected,
                        _ => false,
                    },
                    Some(Value::Null) | None => matches!(value, FilterValue::Null),
                    Some(Value::Array(_)) | Some(Value::Object(_)) => false,
                };
                match op {
                    CompareOp::Equal => equal,
                    CompareOp::NotEqual => !equal,
                }
            }
            Self::All(clauses) => clauses.iter().all(|clause| clause.matches(row)),
            Self::Any(clauses) => clauses.iter().any(|clause| clause.matches(row)),
        }
    }
}

/// Tolerant numeric equality (the crate denies `float_cmp`-style exact
/// comparisons): two numbers are equal when they differ by at most one ULP
/// scaled by magnitude.
#[must_use]
fn numbers_equal(left: f64, right: f64) -> bool {
    (left - right).abs() <= f64::EPSILON * left.abs().max(right.abs()).max(1.0)
}

/// Sorts rows by the `$orderby` keys, stable and multi-key.
fn sort_rows(rows: &mut Vec<Value>, orderby: &[(&'static str, SortDirection)]) {
    let mut indexed: Vec<(Vec<SortKey>, Value)> = rows
        .iter()
        .map(|row| {
            let keys = orderby
                .iter()
                .map(|(field, _)| sort_key(row, field))
                .collect();
            (keys, row.clone())
        })
        .collect();
    indexed.sort_by(|(left, _), (right, _)| {
        let mut first = Ordering::Equal;
        for (index, (_, direction)) in orderby.iter().enumerate() {
            let mut ordering = left[index].compare(&right[index]);
            if *direction == SortDirection::Desc {
                ordering = ordering.reverse();
            }
            if ordering != Ordering::Equal {
                first = ordering;
                break;
            }
        }
        first
    });
    *rows = indexed.into_iter().map(|(_, row)| row).collect();
}

/// One extracted sort key of a row.
#[derive(Debug, Clone)]
enum SortKey {
    /// String field.
    Text(String),
    /// Numeric field.
    Number(f64),
    /// Boolean field.
    Flag(bool),
    /// The field is absent (sorts last).
    Absent,
}

impl SortKey {
    /// Compares two keys; an absent key always sorts last.
    #[must_use]
    fn compare(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Absent, Self::Absent) => Ordering::Equal,
            (Self::Absent, _) => Ordering::Greater,
            (_, Self::Absent) => Ordering::Less,
            (Self::Text(left), Self::Text(right)) => left.cmp(right),
            (Self::Number(left), Self::Number(right)) => left.total_cmp(right),
            (Self::Flag(left), Self::Flag(right)) => left.cmp(right),
            _ => Ordering::Equal,
        }
    }
}

/// Extracts the sort key of `row` for `field`.
#[must_use]
fn sort_key(row: &Value, field: &str) -> SortKey {
    match row.get(field) {
        Some(Value::String(text)) => SortKey::Text(text.clone()),
        Some(Value::Number(number)) => number.as_f64().map_or(SortKey::Absent, SortKey::Number),
        Some(Value::Bool(flag)) => SortKey::Flag(*flag),
        _ => SortKey::Absent,
    }
}

/// Keeps only the selected keys of `row`.
#[must_use]
fn project(row: &Value, select: &[&'static str]) -> Value {
    match row {
        Value::Object(fields) => {
            let mut projected = serde_json::Map::new();
            for field in select {
                if let Some(value) = fields.get(*field) {
                    projected.insert((*field).to_owned(), value.clone());
                }
            }
            Value::Object(projected)
        }
        _ => row.clone(),
    }
}

/// Catalog of the upstream list endpoint.
///
/// Top-level wire fields only: nested sections (`server.endpoints[]`,
/// `plugins.items[]`) are not addressable by the query engine.
pub const UPSTREAM_FIELDS: FieldCatalog = FieldCatalog {
    filterable: &[
        "id",
        "alias",
        "enabled",
        "protocol",
        "created_at",
        "updated_at",
    ],
    sortable: &[
        "id",
        "alias",
        "enabled",
        "protocol",
        "created_at",
        "updated_at",
    ],
    selectable: &[
        "id",
        "alias",
        "enabled",
        "tags",
        "server",
        "protocol",
        "auth",
        "headers",
        "plugins",
        "rate_limit",
        "cors",
        "created_at",
        "updated_at",
    ],
    aliases: &[],
};

/// Catalog of the route list endpoint.
pub const ROUTE_FIELDS: FieldCatalog = FieldCatalog {
    filterable: &[
        "id",
        "upstream_id",
        "enabled",
        "priority",
        "created_at",
        "updated_at",
    ],
    sortable: &[
        "id",
        "upstream_id",
        "enabled",
        "priority",
        "created_at",
        "updated_at",
    ],
    selectable: &[
        "id",
        "upstream_id",
        "match",
        "headers",
        "plugins",
        "rate_limit",
        "cors",
        "enabled",
        "priority",
        "tags",
        "created_at",
        "updated_at",
    ],
    aliases: &[],
};

/// Catalog of the plugin list endpoint. `type` is the DESIGN spelling of
/// `plugin_type` and is accepted as an alias.
pub const PLUGIN_FIELDS: FieldCatalog = FieldCatalog {
    filterable: &["id", "plugin_type", "enabled", "created_at", "updated_at"],
    sortable: &["id", "plugin_type", "enabled", "created_at", "updated_at"],
    selectable: &[
        "id",
        "plugin_type",
        "config",
        "enabled",
        "tags",
        "created_at",
        "updated_at",
    ],
    aliases: &[("type", "plugin_type")],
};

impl FieldCatalog {
    /// Resolves `field` to its canonical catalog name, `None` when unknown.
    #[must_use]
    pub fn canonical(&self, field: &str) -> Option<&'static str> {
        for (alias, target) in self.aliases {
            if *alias == field {
                return Some(target);
            }
        }
        self.filterable
            .iter()
            .chain(self.sortable)
            .chain(self.selectable)
            .find(|candidate| **candidate == field)
            .copied()
    }

    /// Comma-separated list of the fields accepted for `purpose`, for error
    /// messages.
    #[must_use]
    fn names(&self, purpose: CatalogPurpose) -> String {
        let fields: &[&str] = match purpose {
            CatalogPurpose::Filter => self.filterable,
            CatalogPurpose::Sort => self.sortable,
            CatalogPurpose::Select => self.selectable,
        };
        fields.join(", ")
    }
}

/// Which catalog list an error message talks about.
#[derive(Debug, Clone, Copy)]
enum CatalogPurpose {
    /// `$filter`
    Filter,
    /// `$orderby`
    Sort,
    /// `$select`
    Select,
}

/// Parses `$select` into canonical field names.
fn parse_select(raw: &str, catalog: &FieldCatalog) -> Result<Vec<&'static str>, OagwError> {
    let mut selected: Vec<&'static str> = Vec::new();
    for token in raw.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let Some(field) = catalog.canonical(token) else {
            return Err(unknown_field(token, catalog, CatalogPurpose::Select));
        };
        if !selected.contains(&field) {
            selected.push(field);
        }
    }
    Ok(selected)
}

/// Parses `$orderby` into `(field, direction)` pairs.
fn parse_orderby(
    raw: &str,
    catalog: &FieldCatalog,
) -> Result<Vec<(&'static str, SortDirection)>, OagwError> {
    let mut keys: Vec<(&'static str, SortDirection)> = Vec::new();
    for token in raw.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let (name, direction) = match token.split_once(char::is_whitespace) {
            Some((name, rest)) => {
                let rest = rest.trim();
                let direction = match rest {
                    "asc" => SortDirection::Asc,
                    "desc" => SortDirection::Desc,
                    _ => {
                        return Err(OagwError::validation(format!(
                            "field `$orderby`: direction `{rest}` is not `asc` or `desc`"
                        )));
                    }
                };
                (name, direction)
            }
            None => (token, SortDirection::Asc),
        };
        let Some(field) = catalog.canonical(name) else {
            return Err(unknown_field(name, catalog, CatalogPurpose::Sort));
        };
        if !catalog.sortable.contains(&field) {
            return Err(unknown_field(name, catalog, CatalogPurpose::Sort));
        }
        if !keys.iter().any(|(existing, _)| *existing == field) {
            keys.push((field, direction));
        }
    }
    Ok(keys)
}

/// Parses `$top` / `$skip`.
fn parse_size(raw: &str, parameter: &str) -> Result<usize, OagwError> {
    raw.parse::<usize>().map_err(|_| {
        OagwError::validation(format!(
            "field `{parameter}`: must be a non-negative integer, got `{raw}`"
        ))
    })
}

/// Reads a single-quoted OData string literal, where `''` escapes a quote, and
/// returns the text plus the number of bytes consumed (both quotes included).
fn parse_string_literal(input: &str) -> Result<(String, usize), OagwError> {
    let bytes = input.as_bytes();
    let mut text = String::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\'' {
            if bytes.get(cursor + 1) == Some(&b'\'') {
                text.push('\'');
                cursor += 2;
            } else {
                return Ok((text, cursor + 2));
            }
        } else {
            let Some(character) = input[cursor..].chars().next() else {
                break;
            };
            text.push(character);
            cursor += character.len_utf8();
        }
    }
    Err(OagwError::validation(
        "field `$filter`: string literal is not closed".to_owned(),
    ))
}

/// Builds the "unknown field" error for `purpose`.
fn unknown_field(field: &str, catalog: &FieldCatalog, purpose: CatalogPurpose) -> OagwError {
    let clause = match purpose {
        CatalogPurpose::Filter => "`$filter`",
        CatalogPurpose::Sort => "`$orderby`",
        CatalogPurpose::Select => "`$select`",
    };
    OagwError::validation(format!(
        "field {clause}: `{field}` is not a queryable field; allowed: {}",
        catalog.names(purpose)
    ))
}

/// Parses a `$filter` expression.
fn parse_filter(raw: &str, catalog: &FieldCatalog) -> Result<Filter, OagwError> {
    let mut parser = FilterParser {
        input: raw.trim(),
        position: 0,
        depth: 0,
    };
    if parser.input.is_empty() {
        return Err(OagwError::validation(
            "field `$filter`: must not be empty".to_owned(),
        ));
    }
    let filter = parser.parse_or(catalog)?;
    parser.skip_whitespace();
    if parser.position != parser.input.len() {
        return Err(OagwError::validation(format!(
            "field `$filter`: unexpected trailing input `{}`",
            &parser.input[parser.position..]
        )));
    }
    Ok(filter)
}

/// Serialises typed list items into the JSON rows the engine works on.
///
/// # Errors
///
/// Returns [`OagwError::Validation`] when an item cannot be serialised, which
/// would breach the wire contract rather than a caller mistake.
fn serialise_rows<T: Serialize>(items: Vec<T>) -> Result<Vec<Value>, OagwError> {
    let mut rows = Vec::with_capacity(items.len());
    for item in items {
        rows.push(serde_json::to_value(&item).map_err(|error| {
            OagwError::validation(format!("list item is not serialisable: {error}"))
        })?);
    }
    Ok(rows)
}

/// Recursive-descent parser for the `$filter` subset.
struct FilterParser<'a> {
    /// Remaining input.
    input: &'a str,
    /// Byte offset into `input`.
    position: usize,
    /// Current parenthesis nesting depth, bounded by [`MAX_FILTER_DEPTH`].
    depth: usize,
}

impl<'a> FilterParser<'a> {
    /// `or` level of the grammar.
    fn parse_or(&mut self, catalog: &FieldCatalog) -> Result<Filter, OagwError> {
        let mut left = self.parse_and(catalog)?;
        while self.eat_keyword("or") {
            let right = self.parse_and(catalog)?;
            left = Filter::Any(vec![left, right]);
        }
        Ok(left)
    }

    /// `and` level of the grammar.
    fn parse_and(&mut self, catalog: &FieldCatalog) -> Result<Filter, OagwError> {
        let mut left = self.parse_primary(catalog)?;
        while self.eat_keyword("and") {
            let right = self.parse_primary(catalog)?;
            left = Filter::All(vec![left, right]);
        }
        Ok(left)
    }

    /// One parenthesised group or one comparison.
    fn parse_primary(&mut self, catalog: &FieldCatalog) -> Result<Filter, OagwError> {
        self.skip_whitespace();
        if self.peek() == Some('(') {
            self.depth += 1;
            if self.depth > MAX_FILTER_DEPTH {
                return Err(OagwError::validation(format!(
                    "field `$filter`: nested groups must not be deeper than {MAX_FILTER_DEPTH}"
                )));
            }
            self.position += 1;
            let group = self.parse_or(catalog)?;
            self.skip_whitespace();
            if self.peek() != Some(')') {
                return Err(OagwError::validation(
                    "field `$filter`: expected `)` to close a group".to_owned(),
                ));
            }
            self.position += 1;
            self.depth -= 1;
            return Ok(group);
        }

        let field = self.parse_identifier()?;
        self.skip_whitespace();
        let op = if self.eat_keyword("eq") {
            CompareOp::Equal
        } else if self.eat_keyword("ne") {
            CompareOp::NotEqual
        } else {
            return Err(OagwError::validation(
                "field `$filter`: only `eq` and `ne` comparisons are supported".to_owned(),
            ));
        };
        self.skip_whitespace();
        let value = self.parse_value()?;

        let canonical = catalog
            .canonical(&field)
            .ok_or_else(|| unknown_field(&field, catalog, CatalogPurpose::Filter))?;
        if !catalog.filterable.contains(&canonical) {
            return Err(unknown_field(&field, catalog, CatalogPurpose::Filter));
        }
        Ok(Filter::Compare {
            field: canonical.to_owned(),
            op,
            value,
        })
    }

    /// Reads a bare identifier (`[A-Za-z_][A-Za-z0-9_.]*`).
    fn parse_identifier(&mut self) -> Result<String, OagwError> {
        let rest = &self.input[self.position..];
        let end = rest
            .char_indices()
            .find(|(_, character)| {
                !character.is_ascii_alphanumeric() && *character != '_' && *character != '.'
            })
            .map_or(rest.len(), |(index, _)| index);
        if end == 0 {
            return Err(OagwError::validation(
                "field `$filter`: expected a field name".to_owned(),
            ));
        }
        let identifier = rest[..end].to_owned();
        self.position += end;
        Ok(identifier)
    }

    /// Reads one literal: `'text'`, a number, `true`, `false` or `null`.
    fn parse_value(&mut self) -> Result<FilterValue, OagwError> {
        self.skip_whitespace();
        let rest = &self.input[self.position..];
        if let Some(text) = rest.strip_prefix('\'') {
            let (literal, consumed) = parse_string_literal(text)?;
            self.position += consumed;
            return Ok(FilterValue::Text(literal));
        }
        for (literal, value) in [
            ("true", FilterValue::Flag(true)),
            ("false", FilterValue::Flag(false)),
            ("null", FilterValue::Null),
        ] {
            if rest.starts_with(literal) {
                self.position += literal.len();
                return Ok(value);
            }
        }
        let end = rest
            .char_indices()
            .find(|(_, character)| {
                !character.is_ascii_digit() && *character != '-' && *character != '.'
            })
            .map_or(rest.len(), |(index, _)| index);
        if end == 0 {
            return Err(OagwError::validation(
                "field `$filter`: string values must be single-quoted".to_owned(),
            ));
        }
        let number = &rest[..end];
        self.position += end;
        number.parse::<f64>().map(FilterValue::Number).map_err(|_| {
            OagwError::validation(format!(
                "field `$filter`: `{number}` is neither a number, a boolean nor a quoted string"
            ))
        })
    }

    /// Advances past whitespace.
    fn skip_whitespace(&mut self) {
        let rest = &self.input[self.position..];
        let trimmed = rest.trim_start();
        self.position += rest.len() - trimmed.len();
    }

    /// Consumes `keyword` when it starts here and is not a field-name prefix.
    fn eat_keyword(&mut self, keyword: &str) -> bool {
        self.skip_whitespace();
        let rest = &self.input[self.position..];
        let Some(after) = rest.strip_prefix(keyword) else {
            return false;
        };
        if after
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return false;
        }
        self.position += keyword.len();
        self.skip_whitespace();
        true
    }

    /// Next byte without consuming it.
    #[must_use]
    fn peek(&self) -> Option<char> {
        self.input[self.position..].chars().next()
    }
}

#[cfg(test)]
#[path = "odata_tests.rs"]
mod tests;
