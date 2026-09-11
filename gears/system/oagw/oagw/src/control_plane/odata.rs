//! OData list parameter parsing and bounding — `cpt-cf-oagw-algo-odata-list`.
//!
//! The five parameters are parsed from the raw query string, bounded, and
//! validated against the surface the resource kind exposes; a malformed or
//! unexposed expression is never interpreted as an absent one. The module is
//! pure: it takes the parsed parameters and the rows one tenant scan read, and
//! returns the page and the projection it was built with. No `axum`/`http`
//! type appears here.
//!
//! Every detail names the offending parameter and never echoes a value.

use std::cmp::Ordering;

use uuid::Uuid;

use crate::control_plane::validation::ResourceKind;
use crate::domain::error::{DomainError, ErrorKind};
use crate::gts;
use crate::store::{PluginRow, RouteRow, UpstreamRow};

/// `$top` the table declares as the default.
pub const DEFAULT_TOP: u64 = 50;
/// `$top` the table declares as the hard ceiling.
pub const MAX_TOP: u64 = 100;

/// The five parameter names the list surface declares.
const FILTER_KEY: &str = "$filter";
const SELECT_KEY: &str = "$select";
const ORDERBY_KEY: &str = "$orderby";
const TOP_KEY: &str = "$top";
const SKIP_KEY: &str = "$skip";

/// Upstream fields a `$filter` may name; the last row is the tag field, which
/// matches a parent holding the tag in its tag table.
const UPSTREAM_FILTER: [&str; 4] = ["id", "alias", "enabled", "tag"];
/// Upstream fields an `$orderby` may name; every one is single-valued per
/// parent row.
const UPSTREAM_ORDER: [&str; 3] = ["id", "alias", "enabled"];
/// Upstream properties a `$select` may name.
const UPSTREAM_SELECT: [&str; 11] = [
    "id",
    "alias",
    "protocol",
    "enabled",
    "server",
    "auth",
    "headers",
    "rate_limit",
    "cors",
    "plugins",
    "tags",
];
/// Route fields a `$filter` may name.
const ROUTE_FILTER: [&str; 7] = [
    "id",
    "upstream_id",
    "path",
    "method",
    "priority",
    "enabled",
    "tag",
];
/// Route fields an `$orderby` may name.
const ROUTE_ORDER: [&str; 4] = ["id", "upstream_id", "priority", "enabled"];
/// Route properties a `$select` may name.
const ROUTE_SELECT: [&str; 9] = [
    "id",
    "upstream_id",
    "priority",
    "enabled",
    "match",
    "rate_limit",
    "cors",
    "plugins",
    "tags",
];
/// Plugin fields a `$filter` may name; `type` names the family literal.
const PLUGIN_FILTER: [&str; 4] = ["id", "type", "plugin_type", "name"];
/// Plugin fields an `$orderby` may name: DESIGN's plugin table declares none,
/// so the surface admits no ordering at all.
const PLUGIN_ORDER: [&str; 0] = [];
/// Plugin properties a `$select` may name.
const PLUGIN_SELECT: [&str; 9] = [
    "id",
    "plugin_type",
    "name",
    "description",
    "config_schema",
    "phases",
    "source_code",
    "last_used_at",
    "gc_eligible_at",
];

/// The catalogue one list call reads, as the three tables expose different
/// parameter surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    /// The upstream catalogue.
    Upstream,
    /// The route catalogue.
    Route,
    /// The custom plugin catalogue.
    Plugin,
}

impl From<ResourceKind> for ListKind {
    fn from(kind: ResourceKind) -> Self {
        match kind {
            ResourceKind::Upstream => Self::Upstream,
            ResourceKind::Route => Self::Route,
        }
    }
}

/// The five parsed list parameters, with their defaults applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListQuery {
    /// Bounded page size: the parameter's value, capped at the ceiling.
    pub top: u64,
    /// Non-negative offset into the tenant-scoped result set.
    pub skip: u64,
    /// The parsed filter, or `None` when the parameter was absent.
    pub filter: Option<Filter>,
    /// The parsed ordering, or `None` when the parameter was absent.
    pub orderby: Option<OrderBy>,
    /// The projected properties; empty means the full representation.
    pub select: Vec<String>,
}

/// One parsed `$filter`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    /// The comparisons, joined by `and`.
    pub terms: Vec<Term>,
}

/// One `field eq 'value'` comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Term {
    /// The field the comparison names.
    pub field: Field,
    /// The value the comparison compares against, unquoted.
    pub value: String,
}

/// The field a filter comparison or an ordering names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// The parent row's identifier.
    Id,
    /// The upstream's normalized alias.
    Alias,
    /// Whether the row is enabled.
    Enabled,
    /// The route's owning upstream.
    UpstreamId,
    /// The route's match path.
    Path,
    /// The route's declared method.
    Method,
    /// The route's match-uniqueness ordering.
    Priority,
    /// A tag the parent holds.
    Tag,
    /// The plugin's family literal.
    PluginType,
    /// The plugin's human-readable name.
    Name,
}

/// One parsed `$orderby`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    /// The field the ordering names.
    pub field: Field,
    /// Whether the ordering is descending.
    pub descending: bool,
}

/// One assembled page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    /// The page's parent rows, in the order the parameters produced.
    pub items: Vec<T>,
    /// The projection the page was built with; empty means the full
    /// representation.
    pub projection: Vec<String>,
    /// The bounded page size the parameters produced, ceiling included.
    pub top: u64,
}

/// The raw parameter values the query string carried.
#[derive(Debug, Default)]
struct Raw {
    filter: Option<String>,
    select: Option<String>,
    orderby: Option<String>,
    top: Option<String>,
    skip: Option<String>,
}

/// The accumulated parameter defects of one query string.
#[derive(Debug, Default)]
struct Defects(Vec<String>);

impl Defects {
    /// Rejects a parameter the closed list surface does not declare.
    fn unknown(&mut self, parameter: &str) {
        self.0
            .push(format!("unknown list parameter '{parameter}'"));
    }

    /// Rejects a paging parameter whose value is not a non-negative integer.
    fn paging(&mut self, parameter: &str) {
        self.0
            .push(format!("{parameter} is not a non-negative integer"));
    }

    /// Rejects a filter that cannot be parsed.
    fn filter(&mut self) {
        self.0
            .push(String::from("$filter is not a well-formed filter expression"));
    }

    /// Rejects a filter that names a field the kind does not expose.
    fn filter_field(&mut self) {
        self.0
            .push(String::from("$filter names a field the resource kind does not expose"));
    }

    /// Rejects an ordering that cannot be parsed.
    fn orderby(&mut self) {
        self.0
            .push(String::from("$orderby is not a well-formed ordering expression"));
    }

    /// Rejects an ordering that names an unorderable field.
    fn orderby_field(&mut self) {
        self.0
            .push(String::from("$orderby names a field the resource kind does not order by"));
    }

    /// Rejects a projection that names an unexposed property.
    fn select(&mut self) {
        self.0
            .push(String::from("$select names a property the resource kind does not expose"));
    }

    /// Whether every parameter was admitted.
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The single validation error naming every offending parameter.
    fn into_error(self) -> DomainError {
        DomainError::gateway(ErrorKind::ValidationError, self.0.join(", "))
    }
}

/// Parses and bounds the five list parameters of one query string.
///
/// # Errors
///
/// Returns one gateway validation error naming every offending parameter: an
/// unknown parameter name, a `$top` or `$skip` that is not a non-negative
/// integer, a `$filter` or `$orderby` that cannot be parsed or names a field
/// the resource kind does not expose, and a `$select` naming a property
/// outside the selectable set.
#[allow(clippy::result_large_err)]
pub fn parse(kind: ListKind, query: &str) -> Result<ListQuery, DomainError> {
    let mut raw = Raw::default();
    let mut defects = Defects::default();

    // @cpt-begin:cpt-cf-oagw-algo-odata-list:p1:inst-odata-parse
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        let (key, value) = (key.into_owned(), value.into_owned());
        match key.as_str() {
            FILTER_KEY => raw.filter = Some(value),
            SELECT_KEY => raw.select = Some(value),
            ORDERBY_KEY => raw.orderby = Some(value),
            TOP_KEY => raw.top = Some(value),
            SKIP_KEY => raw.skip = Some(value),
            _ => defects.unknown(&key),
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-odata-list:p1:inst-odata-parse

    // @cpt-begin:cpt-cf-oagw-algo-odata-list:p1:inst-odata-paging
    let top = paging(TOP_KEY, raw.top.as_deref(), &mut defects);
    let skip = paging(SKIP_KEY, raw.skip.as_deref(), &mut defects);
    // @cpt-end:cpt-cf-oagw-algo-odata-list:p1:inst-odata-paging

    // @cpt-begin:cpt-cf-oagw-algo-odata-list:p1:inst-odata-top-if
    let top = top.map(|top| {
        // @cpt-begin:cpt-cf-oagw-algo-odata-list:p1:inst-odata-top-cap
        top.min(MAX_TOP)
        // @cpt-end:cpt-cf-oagw-algo-odata-list:p1:inst-odata-top-cap
    });
    // @cpt-end:cpt-cf-oagw-algo-odata-list:p1:inst-odata-top-if

    // @cpt-begin:cpt-cf-oagw-algo-odata-list:p1:inst-odata-expressions
    let filter = match raw.filter {
        None => None,
        Some(text) => match parse_filter(kind, &text) {
            Ok(filter) => Some(filter),
            Err(malformed) => {
                if malformed {
                    defects.filter();
                } else {
                    defects.filter_field();
                }
                None
            }
        },
    };
    let orderby = match raw.orderby {
        None => None,
        Some(text) => match parse_orderby(kind, &text) {
            Ok(orderby) => Some(orderby),
            Err(malformed) => {
                if malformed {
                    defects.orderby();
                } else {
                    defects.orderby_field();
                }
                None
            }
        },
    };
    let mut select = Vec::new();
    if let Some(text) = raw.select {
        match parse_select(kind, &text) {
            Ok(projected) => select = projected,
            Err(()) => defects.select(),
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-odata-list:p1:inst-odata-expressions

    // @cpt-begin:cpt-cf-oagw-algo-odata-list:p1:inst-odata-fail-if
    if !defects.is_empty() {
        // @cpt-begin:cpt-cf-oagw-algo-odata-list:p1:inst-odata-fail-return
        return Err(defects.into_error());
        // @cpt-end:cpt-cf-oagw-algo-odata-list:p1:inst-odata-fail-return
    }
    // @cpt-end:cpt-cf-oagw-algo-odata-list:p1:inst-odata-fail-if

    Ok(ListQuery {
        top: top.unwrap_or(DEFAULT_TOP),
        skip: skip.unwrap_or(0),
        filter,
        orderby,
        select,
    })
}

/// Applies the parsed parameters to one tenant scan and assembles the page.
///
/// The scan is the one query set the page costs: it carries every parent row
/// of the calling tenant with its dependent rows, so the page is built without
/// one query per parent. The tenant equality was applied by the scan before
/// any parameter of this module ran.
#[must_use]
pub fn apply_upstream(query: &ListQuery, scan: Vec<UpstreamRow>) -> Page<UpstreamRow> {
    // @cpt-begin:cpt-cf-oagw-algo-odata-list:p1:inst-odata-apply
    let filtered = match &query.filter {
        None => scan,
        Some(filter) => scan
            .into_iter()
            .filter(|row| filter.terms.iter().all(|term| upstream_matches(row, term)))
            .collect(),
    };
    let ordered = order(filtered, query.orderby.as_ref(), upstream_order);
    let bounded: Vec<UpstreamRow> = bounded(ordered, query.skip, query.top)
        .into_iter()
        .collect();
    // @cpt-end:cpt-cf-oagw-algo-odata-list:p1:inst-odata-apply

    // @cpt-begin:cpt-cf-oagw-algo-odata-list:p1:inst-odata-assemble
    // Every parent row of the page already carries its tag rows: the scan
    // materialized them in the same pass, so the page costs no further query.
    // @cpt-end:cpt-cf-oagw-algo-odata-list:p1:inst-odata-assemble

    // @cpt-begin:cpt-cf-oagw-algo-odata-list:p1:inst-odata-return
    Page {
        items: bounded,
        projection: query.select.clone(),
        top: query.top,
    }
    // @cpt-end:cpt-cf-oagw-algo-odata-list:p1:inst-odata-return
}

/// Applies the parsed parameters to one tenant route scan and assembles the
/// page.
///
/// The scan is the one query set the page costs: it carries every route row of
/// the calling tenant with its match, method, and tag rows.
#[must_use]
pub fn apply_route(query: &ListQuery, scan: Vec<RouteRow>) -> Page<RouteRow> {
    // @cpt-begin:cpt-cf-oagw-algo-odata-list:p1:inst-odata-apply
    let filtered = match &query.filter {
        None => scan,
        Some(filter) => scan
            .into_iter()
            .filter(|row| filter.terms.iter().all(|term| route_matches(row, term)))
            .collect(),
    };
    let ordered = order(filtered, query.orderby.as_ref(), route_order);
    let bounded: Vec<RouteRow> = bounded(ordered, query.skip, query.top).into_iter().collect();
    // @cpt-end:cpt-cf-oagw-algo-odata-list:p1:inst-odata-apply

    // @cpt-begin:cpt-cf-oagw-algo-odata-list:p1:inst-odata-assemble
    // Every route row of the page already carries its match, method, and tag
    // rows: the scan materialized them in the same pass, so the page costs no
    // further query.
    // @cpt-end:cpt-cf-oagw-algo-odata-list:p1:inst-odata-assemble

    // @cpt-begin:cpt-cf-oagw-algo-odata-list:p1:inst-odata-return
    Page {
        items: bounded,
        projection: query.select.clone(),
        top: query.top,
    }
    // @cpt-end:cpt-cf-oagw-algo-odata-list:p1:inst-odata-return
}

/// Applies the parsed parameters to one tenant plugin scan and assembles the
/// page.
///
/// The scan is the one query set the page costs: it carries every plugin row
/// of the calling tenant. DESIGN's plugin table declares no `$orderby`, so the
/// parsed surface answers none and the rows keep their catalogue order.
#[must_use]
pub fn apply_plugin(query: &ListQuery, scan: Vec<PluginRow>) -> Page<PluginRow> {
    // @cpt-begin:cpt-cf-oagw-algo-odata-list:p1:inst-odata-apply
    let filtered = match &query.filter {
        None => scan,
        Some(filter) => scan
            .into_iter()
            .filter(|row| filter.terms.iter().all(|term| plugin_matches(row, term)))
            .collect(),
    };
    let ordered = order(filtered, query.orderby.as_ref(), plugin_order);
    let bounded: Vec<PluginRow> = bounded(ordered, query.skip, query.top)
        .into_iter()
        .collect();
    // @cpt-end:cpt-cf-oagw-algo-odata-list:p1:inst-odata-apply

    // @cpt-begin:cpt-cf-oagw-algo-odata-list:p1:inst-odata-assemble
    // A plugin row is a single catalogue row with no dependents, so the page
    // costs no further query.
    // @cpt-end:cpt-cf-oagw-algo-odata-list:p1:inst-odata-assemble

    // @cpt-begin:cpt-cf-oagw-algo-odata-list:p1:inst-odata-return
    Page {
        items: bounded,
        projection: query.select.clone(),
        top: query.top,
    }
    // @cpt-end:cpt-cf-oagw-algo-odata-list:p1:inst-odata-return
}

/// The plugin ordering comparison of one field: no field is orderable, so the
/// comparison answers equal and the catalogue order stands.
fn plugin_order(_left: &PluginRow, _right: &PluginRow, _field: Field) -> Ordering {
    Ordering::Equal
}

/// Parses one paging value, or records the defect and answers `None`.
fn paging(key: &str, value: Option<&str>, defects: &mut Defects) -> Option<u64> {
    let value = value?;
    match value.parse::<u64>() {
        Ok(parsed) => Some(parsed),
        Err(_) => {
            defects.paging(key);
            None
        }
    }
}

/// Parses one `$filter` into its comparisons.
///
/// Answers `Err(true)` for a malformed expression and `Err(false)` for a
/// well-formed one naming a field the kind does not expose.
fn parse_filter(kind: ListKind, text: &str) -> Result<Filter, bool> {
    let mut terms = Vec::new();
    let mut at = 0;
    loop {
        let field_start = skip_spaces(text, at);
        let field_end = identifier_end(text, field_start);
        if field_end == field_start {
            return Err(true);
        }
        let field = &text[field_start..field_end];

        let keyword_start = skip_spaces(text, field_end);
        if !text[keyword_start..].starts_with("eq") {
            return Err(true);
        }
        let quote_start = skip_spaces(text, keyword_start + "eq".len());
        let Some(opening) = text[quote_start..].find('\'') else {
            return Err(true);
        };
        if opening != 0 {
            return Err(true);
        }
        let value_start = quote_start + 1;
        let Some(offset) = text[value_start..].find('\'') else {
            return Err(true);
        };
        let closing = value_start + offset;
        let value = String::from(&text[value_start..closing]);

        let admitted = filter_field(kind, field).ok_or(false)?;
        terms.push(Term {
            field: admitted,
            value,
        });

        let tail = skip_spaces(text, closing + 1);
        if tail >= text.len() {
            break;
        }
        if !text[tail..].starts_with("and") {
            return Err(true);
        }
        at = tail + "and".len();
        if skip_spaces(text, at) == at {
            return Err(true);
        }
    }
    Ok(Filter { terms })
}

/// Resolves one filter field name against the surface the kind exposes.
fn filter_field(kind: ListKind, field: &str) -> Option<Field> {
    let exposed: &[&str] = match kind {
        ListKind::Upstream => &UPSTREAM_FILTER,
        ListKind::Route => &ROUTE_FILTER,
        ListKind::Plugin => &PLUGIN_FILTER,
    };
    if !exposed.contains(&field) {
        return None;
    }
    Some(match field {
        "id" => Field::Id,
        "alias" => Field::Alias,
        "enabled" => Field::Enabled,
        "upstream_id" => Field::UpstreamId,
        "path" => Field::Path,
        "method" => Field::Method,
        "priority" => Field::Priority,
        "plugin_type" | "type" => Field::PluginType,
        "name" => Field::Name,
        _ => Field::Tag,
    })
}

/// Parses one `$orderby` into its field and direction.
///
/// Answers `Err(true)` for a malformed expression and `Err(false)` for a
/// well-formed one naming a field the kind does not order by.
fn parse_orderby(kind: ListKind, text: &str) -> Result<OrderBy, bool> {
    let trimmed = text.trim();
    let Some((field, direction)) = trimmed.split_once(' ') else {
        return order_field(kind, trimmed)
            .map(|field| OrderBy {
                field,
                descending: false,
            })
            .ok_or(false);
    };
    let admitted = order_field(kind, field).ok_or(false)?;
    match direction.trim() {
        "asc" => Ok(OrderBy {
            field: admitted,
            descending: false,
        }),
        "desc" => Ok(OrderBy {
            field: admitted,
            descending: true,
        }),
        _ => Err(true),
    }
}

/// Resolves one orderable field name against the surface the kind exposes.
fn order_field(kind: ListKind, field: &str) -> Option<Field> {
    let orderable: &[&str] = match kind {
        ListKind::Upstream => &UPSTREAM_ORDER,
        ListKind::Route => &ROUTE_ORDER,
        ListKind::Plugin => &PLUGIN_ORDER,
    };
    if !orderable.contains(&field) {
        return None;
    }
    Some(match field {
        "id" => Field::Id,
        "alias" => Field::Alias,
        "enabled" => Field::Enabled,
        "upstream_id" => Field::UpstreamId,
        _ => Field::Priority,
    })
}

/// Parses one `$select` into its projected properties.
fn parse_select(kind: ListKind, text: &str) -> Result<Vec<String>, ()> {
    let selectable: &[&str] = match kind {
        ListKind::Upstream => &UPSTREAM_SELECT,
        ListKind::Route => &ROUTE_SELECT,
        ListKind::Plugin => &PLUGIN_SELECT,
    };
    let mut projected = Vec::new();
    for name in text.split(',') {
        let name = name.trim();
        if !selectable.contains(&name) {
            return Err(());
        }
        if !projected.iter().any(|held| held == name) {
            projected.push(String::from(name));
        }
    }
    Ok(projected)
}

/// Whether one upstream row satisfies one comparison.
fn upstream_matches(row: &UpstreamRow, term: &Term) -> bool {
    match term.field {
        Field::Id => uuid_value(&term.value) == Some(row.upstream.id),
        // The alias compares case-insensitively, as normalization stored it.
        Field::Alias => row
            .upstream
            .alias
            .as_deref()
            .is_some_and(|alias| alias.eq_ignore_ascii_case(&term.value)),
        Field::Enabled => bool_value(&term.value).is_some_and(|value| value == row.upstream.enabled),
        Field::Tag => row.tags.contains(&term.value),
        Field::Path | Field::Method | Field::Priority | Field::UpstreamId => false,
        Field::PluginType | Field::Name => false,
    }
}

/// Whether one route row satisfies one comparison.
fn route_matches(row: &RouteRow, term: &Term) -> bool {
    match term.field {
        Field::Id => uuid_value(&term.value) == Some(row.route.id),
        Field::UpstreamId => uuid_value(&term.value) == Some(row.route.upstream_id),
        Field::Enabled => {
            bool_value(&term.value).is_some_and(|value| value == route_enabled(row))
        }
        Field::Priority => {
            term.value == row.route.priority.unwrap_or_default().to_string()
        }
        Field::Path => route_paths(row).contains(&term.value.as_str()),
        Field::Method => route_methods(row)
            .iter()
            .any(|method| method.eq_ignore_ascii_case(&term.value)),
        Field::Tag => row.tags.contains(&term.value),
        // A route declares no alias, so the field is not exposed to a filter.
        Field::Alias => false,
        Field::PluginType | Field::Name => false,
    }
}

/// Whether one plugin row satisfies one comparison.
fn plugin_matches(row: &PluginRow, term: &Term) -> bool {
    match term.field {
        Field::Id => uuid_value(&term.value) == Some(row.plugin.id),
        Field::PluginType => row.plugin.plugin_type == term.value,
        Field::Name => row.plugin.name == term.value,
        Field::Alias
        | Field::Enabled
        | Field::UpstreamId
        | Field::Path
        | Field::Method
        | Field::Priority
        | Field::Tag => false,
    }
}

/// The route's resolved enabled value, as the stored row always carries one.
fn route_enabled(row: &RouteRow) -> bool {
    row.route.enabled.unwrap_or(true)
}

/// The paths the route's match rows declare.
fn route_paths(row: &RouteRow) -> Vec<&str> {
    match &row.route.match_config.http {
        Some(http) => vec![http.path.as_str()],
        None => Vec::new(),
    }
}

/// The methods the route's match and method rows declare.
fn route_methods(row: &RouteRow) -> Vec<String> {
    match &row.route.match_config.http {
        Some(http) => http.methods.clone(),
        None => match &row.route.match_config.grpc {
            Some(grpc) => vec![grpc.method.clone()],
            None => Vec::new(),
        },
    }
}

/// Orders the rows, then offsets and bounds the page.
fn order<T>(
    rows: Vec<T>,
    orderby: Option<&OrderBy>,
    compare: impl Fn(&T, &T, Field) -> Ordering,
) -> Vec<T> {
    let Some(orderby) = orderby else {
        return rows;
    };
    let field = orderby.field;
    let mut rows = rows;
    if orderby.descending {
        rows.sort_by(|left, right| compare(right, left, field));
    } else {
        rows.sort_by(|left, right| compare(left, right, field));
    }
    rows
}

/// Applies the offset and the bound to an ordered result set.
fn bounded<T>(rows: Vec<T>, skip: u64, top: u64) -> Vec<T> {
    let offset = usize::try_from(skip).unwrap_or(0);
    let bound = usize::try_from(top).unwrap_or(0);
    rows.into_iter().skip(offset).take(bound).collect()
}

/// The upstream ordering comparison of one field.
fn upstream_order(left: &UpstreamRow, right: &UpstreamRow, field: Field) -> Ordering {
    match field {
        Field::Id => left.upstream.id.cmp(&right.upstream.id),
        Field::Alias => left.upstream.alias.cmp(&right.upstream.alias),
        Field::Enabled => left.upstream.enabled.cmp(&right.upstream.enabled),
        Field::Path | Field::Method | Field::Priority | Field::UpstreamId | Field::Tag => {
            Ordering::Equal
        }
        Field::PluginType | Field::Name => Ordering::Equal,
    }
}

/// The route ordering comparison of one field.
fn route_order(left: &RouteRow, right: &RouteRow, field: Field) -> Ordering {
    match field {
        Field::Id => left.route.id.cmp(&right.route.id),
        Field::UpstreamId => left.route.upstream_id.cmp(&right.route.upstream_id),
        Field::Priority => left.route.priority.cmp(&right.route.priority),
        Field::Enabled => left.route.enabled.cmp(&right.route.enabled),
        Field::Alias | Field::Path | Field::Method | Field::Tag => Ordering::Equal,
        Field::PluginType | Field::Name => Ordering::Equal,
    }
}

/// The `Uuid` a comparison value names, accepting the anonymous GTS instance
/// identifier of either resource kind or a bare `Uuid`.
fn uuid_value(value: &str) -> Option<Uuid> {
    for prefix in [gts::UPSTREAM_TYPE, gts::ROUTE_TYPE] {
        if let Some(id) = gts::parse_gts_instance(prefix, value) {
            return Some(id);
        }
    }
    Uuid::parse_str(value).ok()
}

/// The `bool` a comparison value names.
fn bool_value(value: &str) -> Option<bool> {
    match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// The first character boundary at or after `at` that is not a space.
fn skip_spaces(text: &str, at: usize) -> usize {
    text[at..]
        .bytes()
        .take_while(|byte| *byte == b' ' || *byte == b'\t')
        .count()
        + at
}

/// The end of the identifier that starts at `at`.
fn identifier_end(text: &str, at: usize) -> usize {
    at + text[at..]
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_' || *byte == b'.')
        .count()
}
