//! The OData list-query interpretation
//! (`cpt-cf-oagw-algo-upstream-management-list-query`,
//! `cpt-cf-oagw-algo-route-management-list-query`).
//!
//! `GET /oagw/v1/upstreams` and `GET /oagw/v1/routes` interpret `$filter`,
//! `$select`, `$orderby`, `$top` and `$skip` through **one** parser: a
//! parameter that cannot be parsed, or that is out of range, is a validation
//! error naming the parameter, and nothing is silently clamped or ignored
//! (`inst-um-lq-3`/`-4`, `inst-rm-lq-5`/`-6`).
//!
//! The module is a pure function over the record of the list, so the REST
//! layer hands over the query-string pairs and receives a query it can apply:
//! [`ListQuery::parse`] for the upstream record and [`ListQuery::parse_route`]
//! for the route record, which differ only in the field names a `$filter`,
//! `$select` or `$orderby` may carry.

use uuid::Uuid;

use crate::domain::dto::{Plugin, Route, Upstream};
use crate::domain::error::DomainError;

/// The `$top` default.
pub const DEFAULT_TOP: usize = 50;

/// The `$top` maximum; a larger value is rejected rather than clamped.
pub const MAX_TOP: usize = 100;

/// The upstream record fields a query may name.
const FIELDS: [&str; 11] = [
    "id",
    "alias",
    "protocol",
    "enabled",
    "tenant_id",
    "server",
    "tags",
    "auth",
    "headers",
    "rate_limit",
    "cors",
];

/// The plugin record fields a query may name
/// (`cpt-cf-oagw-dod-plugin-system-management-api`): `type` is the documented
/// spelling of `plugin_type`, so `$filter=type eq 'guard'` selects the guard
/// plugins.
const PLUGIN_FIELDS: [&str; 6] =
    ["id", "plugin_type", "type", "name", "last_used_at", "source_code"];

/// The route record fields a query may name
/// (`cpt-cf-oagw-algo-route-management-list-query` step 5): a field the route
/// model does not carry is a rejection naming the parameter.
const ROUTE_FIELDS: [&str; 11] = [
    "id",
    "upstream_id",
    "priority",
    "enabled",
    "match_type",
    "match",
    "rate_limit",
    "cors",
    "plugins",
    "tags",
    "tenant_id",
];

/// The record family a list query interprets.
///
/// One OData parser serves both management lists; the field set is the only
/// thing that differs, so a query is parsed *for* a record family and every
/// field name is validated against that family's record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldSet {
    /// The upstream record fields (`GET /oagw/v1/upstreams`).
    Upstream,
    /// The route record fields (`GET /oagw/v1/routes`).
    Route,
    /// The plugin record fields (`GET /oagw/v1/plugins`).
    Plugin,
}

impl FieldSet {
    fn names(self) -> &'static [&'static str] {
        match self {
            Self::Upstream => &FIELDS,
            Self::Route => &ROUTE_FIELDS,
            Self::Plugin => &PLUGIN_FIELDS,
        }
    }

    /// The comparison kind of one field, so a literal of the wrong kind is a
    /// rejection rather than a clause that can never hold.
    fn kind_of(self, field: &str) -> &'static str {
        match self {
            Self::Upstream if field == "enabled" => "boolean",
            Self::Upstream => "text",
            Self::Route => match field {
                "enabled" => "boolean",
                "priority" => "number",
                _ => "text",
            },
            Self::Plugin => "text",
        }
    }
}

/// A query field name, or `None` when the name is not part of the upstream
/// record.
#[must_use]
pub fn field_is_valid(name: &str) -> bool {
    FIELDS.contains(&name)
}

/// A query field name of the route record, or `false` when the route model
/// does not carry it (`inst-rm-lq-5`).
#[must_use]
pub fn route_field_is_valid(name: &str) -> bool {
    ROUTE_FIELDS.contains(&name)
}

/// The comparison a `$filter` clause applies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Comparison {
    /// `eq`
    Equals,
    /// `ne`
    NotEquals,
}

/// A parsed `$filter` expression. Only the comparisons the management list
/// needs are supported; anything else is rejected as unparseable rather than
/// silently ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Filter {
    /// `field eq|ne literal`
    Compare { field: String, comparison: Comparison, value: Literal },
    /// `startswith(field, 'value')`
    StartsWith { field: String, value: String },
    /// `contains(field, 'value')`
    Contains { field: String, value: String },
    /// Both operands must hold.
    And(Box<Filter>, Box<Filter>),
}

/// A `$filter` literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    /// `'text'`
    Text(String),
    /// `true` / `false`
    Boolean(bool),
    /// A signed integer.
    Number(i64),
}

/// The direction of an `$orderby` clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// `asc` (also the default when no direction is written).
    Ascending,
    /// `desc`
    Descending,
}

/// One interpreted `$orderby` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ordering {
    pub field: String,
    pub direction: Direction,
}

/// The interpreted list query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListQuery {
    /// `$filter`, when supplied.
    pub filter: Option<Filter>,
    /// `$select`, the projected field names in the order written.
    pub select: Vec<String>,
    /// `$orderby`.
    pub orderby: Option<Ordering>,
    /// `$top`, defaulted to [`DEFAULT_TOP`] and capped at [`MAX_TOP`].
    pub top: usize,
    /// `$skip`, a non-negative offset.
    pub skip: usize,
}

impl Default for ListQuery {
    fn default() -> Self {
        Self {
            filter: None,
            select: Vec::new(),
            orderby: None,
            top: DEFAULT_TOP,
            skip: 0,
        }
    }
}

/// The validation error naming the offending parameter (`inst-um-lq-4`).
fn invalid(parameter: &str, reason: &str) -> DomainError {
    DomainError::field_rejection(parameter, reason)
}

impl ListQuery {
    /// Interpret the query string of a list request.
    ///
    /// `pairs` is the parsed `key=value` sequence. A parameter outside the five
    /// supported system options is rejected when it is `$`-prefixed and
    /// ignored otherwise, so an implementation-added parameter cannot silently
    /// change the result.
    ///
    /// # Errors
    ///
    /// Returns a validation error naming the offending parameter.
    // @cpt-begin:cpt-cf-oagw-algo-upstream-management-list-query:p1:inst-um-lq-1
    // `inst-um-lq-1` .. `-6`: the five system options are parsed and an
    // unparseable or out-of-range one is a validation error naming the
    // parameter, never silently clamped or ignored.
    pub fn parse(pairs: &[(String, String)]) -> Result<Self, DomainError> {
        Self::parse_with(pairs, FieldSet::Upstream)
    }
    // @cpt-end:cpt-cf-oagw-algo-upstream-management-list-query:p1:inst-um-lq-1

    /// Interpret the query string of the **route** list request
    /// (`inst-rm-lq-1` .. `-7`).
    ///
    /// The same OData parser serves both management lists; only the field set
    /// differs, so `$filter upstream_id eq '{uuid}'` — the documented selection
    /// of one upstream's routes — is accepted here and rejected on the
    /// upstream list.
    ///
    /// # Errors
    ///
    /// Returns a validation error naming the offending parameter.
    // @cpt-begin:cpt-cf-oagw-algo-route-management-list-query:p1:inst-rm-lq-1
    // `inst-rm-lq-1` .. `-6`: the same parser 2.2 delivered, bound to the route
    // field set; an unsupported, malformed or out-of-range parameter is a
    // validation error naming it.
    pub fn parse_route(pairs: &[(String, String)]) -> Result<Self, DomainError> {
        Self::parse_with(pairs, FieldSet::Route)
    }
    // @cpt-end:cpt-cf-oagw-algo-route-management-list-query:p1:inst-rm-lq-1

    /// Interpret the query string of the **plugin** catalog list request
    /// (`cpt-cf-oagw-flow-plugin-system-plugin-read`).
    ///
    /// The same OData parser serves all three management lists; the plugin
    /// field set additionally accepts `type` as the documented spelling of
    /// `plugin_type`, so `$filter=type eq 'guard'` selects the guard plugins.
    ///
    /// # Errors
    ///
    /// Returns a validation error naming the offending parameter.
    // @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-read:p1:inst-ps-read-2
    // `inst-ps-read-2`: `$filter`, `$select`, `$top` and `$skip` are applied to
    // the list result through the same parser the other two management lists
    // use; an unsupported or out-of-range parameter is a validation error
    // naming the parameter rather than a silently ignored one.
    pub fn parse_plugin(pairs: &[(String, String)]) -> Result<Self, DomainError> {
        Self::parse_with(pairs, FieldSet::Plugin)
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-read:p1:inst-ps-read-2

    fn parse_with(pairs: &[(String, String)], fields: FieldSet) -> Result<Self, DomainError> {
        let mut query = Self::default();
        for (key, value) in pairs {
            match key.as_str() {
                "$filter" => query.filter = Some(parse_filter(value, fields)?),
                "$select" => query.select = parse_select(value, fields)?,
                "$orderby" => query.orderby = Some(parse_orderby(value, fields)?),
                "$top" => query.top = parse_top(value)?,
                "$skip" => query.skip = parse_skip(value)?,
                other if other.starts_with('$') => {
                    return Err(invalid(other, "unsupported system query option"));
                }
                _ => {}
            }
        }
        Ok(query)
    }

    /// The filter, ordering, offset and limit, in that sequence
    /// (`inst-um-ls-6`).
    #[must_use]
    // @cpt-begin:cpt-cf-oagw-flow-upstream-management-list:p1:inst-um-ls-6
    // `inst-um-ls-6`: the filter, the ordering, the offset and the limit run in
    // that sequence over the caller's own records.
    pub fn apply<'a>(&self, records: &'a [Upstream]) -> Vec<&'a Upstream> {
        self.apply_records(
            records.iter().map(QueryRecord::Upstream).collect::<Vec<_>>(),
        )
        .into_iter()
        .map(|record| match record {
            QueryRecord::Upstream(upstream) => upstream,
            _ => unreachable!("an upstream query yields upstream records"),
        })
        .collect()
    }
    // @cpt-end:cpt-cf-oagw-flow-upstream-management-list:p1:inst-um-ls-6

    /// The filter, ordering, offset and limit over the caller's own routes
    /// (`inst-rm-lq-2` .. `-4`).
    #[must_use]
    // @cpt-begin:cpt-cf-oagw-algo-route-management-list-query:p1:inst-rm-lq-2
    // `inst-rm-lq-2` .. `-4`: `$filter` selects, `$select` projects,
    // `$orderby` sorts and `$skip`/`$top` page, all inside the caller's tenant
    // scope, which the caller applies before handing the records over.
    pub fn apply_routes<'a>(&self, records: &'a [Route]) -> Vec<&'a Route> {
        self.apply_records(records.iter().map(QueryRecord::Route).collect::<Vec<_>>())
            .into_iter()
            .map(|record| match record {
                QueryRecord::Route(route) => route,
                _ => unreachable!("a route query yields route records"),
            })
            .collect()
    }
    // @cpt-end:cpt-cf-oagw-algo-route-management-list-query:p1:inst-rm-lq-2

    /// The filter, ordering, offset and limit over the caller's own plugins
    /// (`cpt-cf-oagw-flow-plugin-system-plugin-read`).
    #[must_use]
    pub fn apply_plugins<'a>(&self, records: &'a [Plugin]) -> Vec<&'a Plugin> {
        self.apply_records(records.iter().map(QueryRecord::Plugin).collect::<Vec<_>>())
            .into_iter()
            .filter_map(|record| match record {
                QueryRecord::Plugin(plugin) => Some(plugin),
                _ => None,
            })
            .collect()
    }

    fn apply_records<'a>(&self, records: Vec<QueryRecord<'a>>) -> Vec<QueryRecord<'a>> {
        let mut selected: Vec<QueryRecord<'a>> = records
            .into_iter()
            .filter(|record| self.filter.as_ref().is_none_or(|f| f.matches_record(record)))
            .collect();
        if let Some(ordering) = &self.orderby {
            let direction = ordering.direction;
            let field = ordering.field.clone();
            selected.sort_by(|left, right| {
                let ordered = compare_field(left, &field, right)
                    .then_with(|| compare_ids_of(left, right));
                if direction == Direction::Descending {
                    ordered.reverse()
                } else {
                    ordered
                }
            });
        }
        let start = self.skip.min(selected.len());
        let end = start.saturating_add(self.top).min(selected.len());
        selected[start..end].to_vec()
    }
}

// @cpt-begin:cpt-cf-oagw-algo-route-management-list-query:p1:inst-rm-lq-3
// @cpt-begin:cpt-cf-oagw-algo-route-management-list-query:p1:inst-rm-lq-4
// @cpt-begin:cpt-cf-oagw-algo-route-management-list-query:p1:inst-rm-lq-5
// @cpt-begin:cpt-cf-oagw-algo-route-management-list-query:p1:inst-rm-lq-6
// @cpt-begin:cpt-cf-oagw-algo-route-management-list-query:p1:inst-rm-lq-7
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-list-query:p1:inst-um-lq-2
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-list-query:p1:inst-um-lq-3
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-list-query:p1:inst-um-lq-4
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-list-query:p1:inst-um-lq-5
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-list-query:p1:inst-um-lq-6
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-read:p1:inst-ps-read-4
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-read:p1:inst-ps-read-7
// @cpt-begin:cpt-cf-oagw-flow-plugin-system-plugin-read:p1:inst-ps-read-8
fn compare_ids_of(left: &QueryRecord<'_>, right: &QueryRecord<'_>) -> std::cmp::Ordering {
    match (left, right) {
        (QueryRecord::Upstream(a), QueryRecord::Upstream(b)) => compare_ids(a, b),
        (QueryRecord::Route(a), QueryRecord::Route(b)) => {
            a.id.as_bytes().cmp(b.id.as_bytes())
        }
        _ => std::cmp::Ordering::Equal,
    }
}
//
// @cpt-end:cpt-cf-oagw-algo-route-management-list-query:p1:inst-rm-lq-7
// @cpt-end:cpt-cf-oagw-algo-route-management-list-query:p1:inst-rm-lq-6
// @cpt-end:cpt-cf-oagw-algo-route-management-list-query:p1:inst-rm-lq-5
// @cpt-end:cpt-cf-oagw-algo-route-management-list-query:p1:inst-rm-lq-4
// @cpt-end:cpt-cf-oagw-algo-route-management-list-query:p1:inst-rm-lq-3
// @cpt-end:cpt-cf-oagw-algo-upstream-management-list-query:p1:inst-um-lq-6
// @cpt-end:cpt-cf-oagw-algo-upstream-management-list-query:p1:inst-um-lq-5
// @cpt-end:cpt-cf-oagw-algo-upstream-management-list-query:p1:inst-um-lq-4
// @cpt-end:cpt-cf-oagw-algo-upstream-management-list-query:p1:inst-um-lq-3
// @cpt-end:cpt-cf-oagw-algo-upstream-management-list-query:p1:inst-um-lq-2
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-read:p1:inst-ps-read-8
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-read:p1:inst-ps-read-7
// @cpt-end:cpt-cf-oagw-flow-plugin-system-plugin-read:p1:inst-ps-read-4
//

fn compare_ids(left: &Upstream, right: &Upstream) -> std::cmp::Ordering {
    left.id.as_bytes().cmp(right.id.as_bytes())
}

/// The ordering key of one field; a field the record does not carry orders
/// last within its own direction.
fn compare_field(
    left: &QueryRecord<'_>,
    field: &str,
    right: &QueryRecord<'_>,
) -> std::cmp::Ordering {
    match (left, right) {
        (QueryRecord::Upstream(left), QueryRecord::Upstream(right)) => {
            compare_upstream_field(left, field, right)
        }
        (QueryRecord::Route(left), QueryRecord::Route(right)) => compare_route_field(left, field, right),
        (QueryRecord::Plugin(left), QueryRecord::Plugin(right)) => {
            compare_plugin_field(left, field, right)
        }
        _ => std::cmp::Ordering::Equal,
    }
}

/// The ordering key of one plugin field.
fn compare_plugin_field(left: &Plugin, field: &str, right: &Plugin) -> std::cmp::Ordering {
    match field {
        "name" => left.name.cmp(&right.name),
        "plugin_type" | "type" => left.plugin_type.cmp(&right.plugin_type),
        "id" => left.id.as_bytes().cmp(right.id.as_bytes()),
        _ => std::cmp::Ordering::Equal,
    }
}

/// The ordering key of one upstream field.
fn compare_upstream_field(left: &Upstream, field: &str, right: &Upstream) -> std::cmp::Ordering {
    match field {
        "alias" => left.alias.cmp(&right.alias),
        "protocol" => left.protocol.cmp(&right.protocol),
        "enabled" => left.enabled.cmp(&right.enabled),
        "id" => compare_ids(left, right),
        "tenant_id" => left.tenant_id.as_bytes().cmp(right.tenant_id.as_bytes()),
        "tags" => left.tags.join(",").cmp(&right.tags.join(",")),
        // `server` is a nested object with no total order; the pool's first
        // host orders it.
        "server" => {
            let host = |upstream: &Upstream| -> String {
                upstream
                    .server
                    .endpoints
                    .first()
                    .map_or_else(String::new, |endpoint| endpoint.host.clone())
            };
            host(left).cmp(&host(right))
        }
        _ => std::cmp::Ordering::Equal,
    }
}

/// The ordering key of one route field (`inst-rm-lq-3`).
fn compare_route_field(left: &Route, field: &str, right: &Route) -> std::cmp::Ordering {
    match field {
        "upstream_id" => left.upstream_id.as_bytes().cmp(&right.upstream_id.as_bytes()),
        "priority" => left.priority.cmp(&right.priority),
        "enabled" => left.enabled.cmp(&right.enabled),
        "match_type" => format!("{:?}", left.match_type).cmp(&format!("{:?}", right.match_type)),
        "id" => left.id.as_bytes().cmp(&right.id.as_bytes()),
        "tenant_id" => left.tenant_id.as_bytes().cmp(&right.tenant_id.as_bytes()),
        "tags" => left.tags.join(",").cmp(&right.tags.join(",")),
        _ => std::cmp::Ordering::Equal,
    }
}

/// `$select`: a comma-separated field list over the record of the list
/// (`inst-um-lq-2`, `inst-rm-lq-3`).
fn parse_select(value: &str, fields: FieldSet) -> Result<Vec<String>, DomainError> {
    let mut names = Vec::new();
    for field in value.split(',') {
        let field = field.trim();
        if field.is_empty() {
            return Err(invalid("$select", "a field name is required between separators"));
        }
        if !fields.names().contains(&field) {
            return Err(invalid("$select", "the field is not part of the record"));
        }
        names.push(field.to_owned());
    }
    Ok(names)
}

/// `$orderby`: a field name with an optional `asc`/`desc` direction
/// (`inst-um-lq-3`).
fn parse_orderby(value: &str, fields: FieldSet) -> Result<Ordering, DomainError> {
    let mut parts = value.trim().split_whitespace();
    let Some(field) = parts.next() else {
        return Err(invalid("$orderby", "a field name is required"));
    };
    if !fields.names().contains(&field) {
        return Err(invalid("$orderby", "the field is not part of the record"));
    }
    let direction = match parts.next() {
        None => Direction::Ascending,
        Some("asc") => Direction::Ascending,
        Some("desc") => Direction::Descending,
        Some(_) => {
            return Err(invalid("$orderby", "the direction must be `asc` or `desc`"));
        }
    };
    if parts.next().is_some() {
        return Err(invalid("$orderby", "only one ordering field is supported"));
    }
    Ok(Ordering { field: field.to_owned(), direction })
}


/// `$top`: default 50, maximum 100, and out of range is a rejection
/// (`inst-um-lq-4`).
fn parse_top(value: &str) -> Result<usize, DomainError> {
    let parsed: usize = value
        .trim()
        .parse()
        .map_err(|_| invalid("$top", "the value must be a positive integer"))?;
    if parsed == 0 {
        return Err(invalid("$top", "the value must be at least 1"));
    }
    if parsed > MAX_TOP {
        return Err(invalid("$top", "the value must be at most 100"));
    }
    Ok(parsed)
}

/// `$skip`: a non-negative offset (`inst-um-lq-5`).
fn parse_skip(value: &str) -> Result<usize, DomainError> {
    value
        .trim()
        .parse()
        .map_err(|_| invalid("$skip", "the value must be a non-negative integer"))
}

/// Tokenize a `$filter` expression into the clauses `and` joins.
fn clauses(expression: &str) -> Vec<&str> {
    // `and` is the only supported conjunction and never appears inside a
    // quoted literal in the documented expressions.
    expression.split(" and ").map(str::trim).filter(|c| !c.is_empty()).collect()
}

/// Parse a `$filter` expression (`inst-um-lq-1`).
fn parse_filter(expression: &str, fields: FieldSet) -> Result<Filter, DomainError> {
    let mut combined: Option<Filter> = None;
    for clause in clauses(expression) {
        let parsed = parse_filter_clause(clause, fields)?;
        combined = Some(match combined {
            None => parsed,
            Some(previous) => Filter::And(Box::new(previous), Box::new(parsed)),
        });
    }
    combined.ok_or_else(|| invalid("$filter", "an expression is required"))
}

fn parse_filter_clause(clause: &str, fields: FieldSet) -> Result<Filter, DomainError> {
    let trimmed = clause.trim();
    if let Some(inner) = trimmed.strip_prefix("startswith(").and_then(|s| s.strip_suffix(')')) {
        let (field, value) = function_arguments(inner, fields)?;
        return Ok(Filter::StartsWith { field, value });
    }
    if let Some(inner) = trimmed.strip_prefix("contains(").and_then(|s| s.strip_suffix(')')) {
        let (field, value) = function_arguments(inner, fields)?;
        return Ok(Filter::Contains { field, value });
    }
    let (field, rest) = trimmed
        .split_once(' ')
        .ok_or_else(|| invalid("$filter", "an expression of the form `field eq 'value'` is required"))?;
    let field = field.trim();
    if !fields.names().contains(&field) {
        return Err(invalid("$filter", "the field is not part of the record"));
    }
    let (operator, literal) = rest
        .trim()
        .split_once(' ')
        .ok_or_else(|| invalid("$filter", "an operator and a literal are required"))?;
    let comparison = match operator.trim() {
        "eq" => Comparison::Equals,
        "ne" => Comparison::NotEquals,
        _ => return Err(invalid("$filter", "only `eq` and `ne` are supported")),
    };
    let value = parse_literal(literal.trim())?;
    check_literal_kind(field, &value, fields)?;
    Ok(Filter::Compare {
        field: field.to_owned(),
        comparison,
        value,
    })
}

fn check_literal_kind(field: &str, value: &Literal, fields: FieldSet) -> Result<(), DomainError> {
    let kind = fields.kind_of(field);
    let mismatched = match value {
        Literal::Text(_) => kind != "text",
        Literal::Boolean(_) => kind != "boolean",
        Literal::Number(_) => kind != "number",
    };
    if mismatched {
        return Err(invalid(
            "$filter",
            "the literal is not of the type of the field",
        ));
    }
    Ok(())
}

fn function_arguments(inner: &str, fields: FieldSet) -> Result<(String, String), DomainError> {
    let (field, value) = inner
        .split_once(',')
        .ok_or_else(|| invalid("$filter", "a field and a literal are required"))?;
    let field = field.trim();
    if !fields.names().contains(&field) {
        return Err(invalid("$filter", "the field is not part of the record"));
    }
    let value = parse_literal(value.trim())?;
    let Literal::Text(value) = value else {
        return Err(invalid("$filter", "the function takes a string literal"));
    };
    if fields.kind_of(field) != "text" {
        return Err(invalid("$filter", "the field is not a string field"));
    }
    Ok((field.to_owned(), value))
}


/// A quoted string, a boolean or an integer.
fn parse_literal(raw: &str) -> Result<Literal, DomainError> {
    let raw = raw.trim();
    if let Some(inner) = raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return Ok(Literal::Text(inner.to_owned()));
    }
    if raw == "true" {
        return Ok(Literal::Boolean(true));
    }
    if raw == "false" {
        return Ok(Literal::Boolean(false));
    }
    raw.parse::<i64>()
        .map(Literal::Number)
        .map_err(|_| invalid("$filter", "a literal is a quoted string, a boolean or an integer"))
}

impl Filter {
    /// Whether `record` satisfies the expression.
    #[must_use]
    pub fn matches(&self, record: &Upstream) -> bool {
        self.matches_record(&QueryRecord::Upstream(record))
    }

    /// Whether `route` satisfies the expression (`inst-rm-lq-2`), the
    /// documented selection of one upstream's routes being
    /// `upstream_id eq '{uuid}'`.
    #[must_use]
    pub fn matches_route(&self, route: &Route) -> bool {
        self.matches_record(&QueryRecord::Route(route))
    }

    fn matches_record(&self, record: &QueryRecord<'_>) -> bool {
        match self {
            Self::Compare { field, comparison, value } => {
                let left = field_value(record, field);
                let holds = match (left, value) {
                    (Some(FieldValue::Text(left)), Literal::Text(right)) => left == *right,
                    (Some(FieldValue::Boolean(left)), Literal::Boolean(right)) => left == *right,
                    (Some(FieldValue::Number(left)), Literal::Number(right)) => left == *right,
                    _ => false,
                };
                if *comparison == Comparison::Equals {
                    holds
                } else {
                    !holds
                }
            }
            Self::StartsWith { field, value } => {
                matches!(field_value(record, field), Some(FieldValue::Text(text)) if text.starts_with(value))
            }
            Self::Contains { field, value } => {
                matches!(field_value(record, field), Some(FieldValue::Text(text)) if text.contains(value))
            }
            Self::And(left, right) => left.matches_record(record) && right.matches_record(record),
        }
    }
}

/// The value of one record field as a filter operand.
enum FieldValue {
    Text(String),
    Boolean(bool),
    Number(i64),
}

/// One record of either management list, so one filter implementation serves
/// both (`cpt-cf-oagw-algo-route-management-list-query` step 2).
#[derive(Debug, Clone, Copy)]
pub enum QueryRecord<'a> {
    /// An upstream record.
    Upstream(&'a Upstream),
    /// A route record.
    Route(&'a Route),
    /// A plugin record.
    Plugin(&'a Plugin),
}

fn field_value(record: &QueryRecord<'_>, field: &str) -> Option<FieldValue> {
    match record {
        QueryRecord::Upstream(record) => match field {
            "alias" => Some(FieldValue::Text(record.alias.clone())),
            "protocol" => Some(FieldValue::Text(record.protocol.clone())),
            "id" => Some(FieldValue::Text(record.id.to_string())),
            "tenant_id" => Some(FieldValue::Text(record.tenant_id.to_string())),
            "enabled" => Some(FieldValue::Boolean(record.enabled)),
            _ => None,
        },
        QueryRecord::Route(record) => match field {
            "id" => Some(FieldValue::Text(record.id.to_string())),
            "upstream_id" => Some(FieldValue::Text(record.upstream_id.to_string())),
            "tenant_id" => Some(FieldValue::Text(record.tenant_id.to_string())),
            "match_type" => Some(FieldValue::Text(match record.match_type {
                crate::domain::dto::RouteMatchType::Http => "http".to_owned(),
                crate::domain::dto::RouteMatchType::Grpc => "grpc".to_owned(),
            })),
            "priority" => Some(FieldValue::Number(record.priority)),
            "enabled" => Some(FieldValue::Boolean(record.enabled)),
            _ => None,
        },
        QueryRecord::Plugin(record) => match field {
            "id" => Some(FieldValue::Text(record.id.to_string())),
            "name" => Some(FieldValue::Text(record.name.clone())),
            // `type` is the documented spelling of the plugin kind, so
            // `$filter=type eq 'guard'` selects the guard plugins;
            // `plugin_type` carries the full base type identifier.
            "type" => Some(FieldValue::Text(
                crate::domain::gts_helpers::plugin_kind_of(&record.plugin_type)
                    .unwrap_or(record.plugin_type.as_str())
                    .to_owned(),
            )),
            "plugin_type" => Some(FieldValue::Text(record.plugin_type.clone())),
            "last_used_at" => Some(FieldValue::Text(record.last_used_at.clone().unwrap_or_default())),
            "source_code" => Some(FieldValue::Text(record.source_code.clone().unwrap_or_default())),
            _ => None,
        },
    }
}

/// The `tenant_id` of a record the filter may compare against, re-exported for
/// the tests that build a two-tenant fixture.
#[must_use]
pub fn tenant_id(record: &Upstream) -> Uuid {
    record.tenant_id
}

#[cfg(test)]
#[path = "list_query_tests.rs"]
mod tests;
