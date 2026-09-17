//! Wire helpers for the OAGW REST list endpoints.
//!
//! Domain records double as wire DTOs (see `domain/model.rs`), so this
//! layer only fills the OData gap: a small in-memory evaluator over the
//! parsed `$filter` AST from `toolkit_odata`, an `$orderby` sorter, a
//! keyset cursor for `$top`/`$skiptoken` pagination, and `$select`
//! projection via `toolkit::api::select::apply_select`.
//!
//! Scope: scalar fields only. `upstreams` filters on
//! `id|alias|enabled|protocol|created_at|updated_at`, `routes` on
//! `id|upstream_id|enabled|created_at|updated_at`, `plugins` on
//! `id|kind|name|created_at`.

use std::cmp::Ordering;

use bigdecimal::BigDecimal;
use uuid::Uuid;

use toolkit_odata::ast::{CompareOperator, Expr};
use toolkit_odata::filter::{FieldKind, FilterField, ODataValue};
use toolkit_odata::{ODataOrderBy, ODataQuery, Page, PageInfo, SortDir};

/// Default page size when `$top`/`limit` is omitted (DESIGN: default 50).
pub const DEFAULT_LIST_TOP: u64 = 50;
/// Maximum page size (DESIGN: max 100).
pub const MAX_LIST_TOP: u64 = 100;

// =====================================================================
//                        Filter-field catalogues
// =====================================================================

/// Filterable/orderable fields on upstreams.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UpstreamField {
    Id,
    Alias,
    Enabled,
    Protocol,
    CreatedAt,
    UpdatedAt,
}

impl FilterField for UpstreamField {
    const FIELDS: &'static [Self] = &[
        Self::Id,
        Self::Alias,
        Self::Enabled,
        Self::Protocol,
        Self::CreatedAt,
        Self::UpdatedAt,
    ];

    fn name(&self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Alias => "alias",
            Self::Enabled => "enabled",
            Self::Protocol => "protocol",
            Self::CreatedAt => "created_at",
            Self::UpdatedAt => "updated_at",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::Id => FieldKind::Uuid,
            Self::Alias | Self::Protocol => FieldKind::String,
            Self::Enabled => FieldKind::Bool,
            Self::CreatedAt | Self::UpdatedAt => FieldKind::I64,
        }
    }
}

/// Filterable/orderable fields on routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RouteField {
    Id,
    UpstreamId,
    Enabled,
    CreatedAt,
    UpdatedAt,
}

impl FilterField for RouteField {
    const FIELDS: &'static [Self] = &[
        Self::Id,
        Self::UpstreamId,
        Self::Enabled,
        Self::CreatedAt,
        Self::UpdatedAt,
    ];

    fn name(&self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::UpstreamId => "upstream_id",
            Self::Enabled => "enabled",
            Self::CreatedAt => "created_at",
            Self::UpdatedAt => "updated_at",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::Id | Self::UpstreamId => FieldKind::Uuid,
            Self::Enabled => FieldKind::Bool,
            Self::CreatedAt | Self::UpdatedAt => FieldKind::I64,
        }
    }
}

/// Filterable/orderable fields on plugins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginField {
    Id,
    Kind,
    Name,
    CreatedAt,
}

impl FilterField for PluginField {
    const FIELDS: &'static [Self] = &[Self::Id, Self::Kind, Self::Name, Self::CreatedAt];

    fn name(&self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::Kind => "kind",
            Self::Name => "name",
            Self::CreatedAt => "created_at",
        }
    }

    fn kind(&self) -> FieldKind {
        match self {
            Self::Id => FieldKind::Uuid,
            Self::Kind | Self::Name => FieldKind::String,
            Self::CreatedAt => FieldKind::I64,
        }
    }
}

// =====================================================================
//                     Scalar field extraction
// =====================================================================

/// Extract the OData value of a named field from a record, or `None`
/// when the record has no such field.
pub(crate) fn upstream_field_value(record: &crate::domain::model::UpstreamRecord, name: &str) -> Option<ODataValue> {
    let v = match name {
        "id" => ODataValue::Uuid(record.id),
        "alias" => ODataValue::String(record.alias.clone()),
        "enabled" => ODataValue::Bool(record.enabled),
        "protocol" => ODataValue::String(record.protocol.clone()),
        "created_at" => ODataValue::Number(BigDecimal::from(record.created_at)),
        "updated_at" => ODataValue::Number(BigDecimal::from(record.updated_at)),
        _ => return None,
    };
    Some(v)
}

pub(crate) fn route_field_value(record: &crate::domain::model::RouteRecord, name: &str) -> Option<ODataValue> {
    let v = match name {
        "id" => ODataValue::Uuid(record.id),
        "upstream_id" => ODataValue::Uuid(record.upstream_id),
        "enabled" => ODataValue::Bool(record.enabled),
        "created_at" => ODataValue::Number(BigDecimal::from(record.created_at)),
        "updated_at" => ODataValue::Number(BigDecimal::from(record.updated_at)),
        _ => return None,
    };
    Some(v)
}

pub(crate) fn plugin_field_value(record: &crate::domain::model::PluginRecord, name: &str) -> Option<ODataValue> {
    let v = match name {
        "id" => ODataValue::Uuid(record.id),
        "kind" => ODataValue::String(record.kind.gts_segment().to_owned()),
        "name" => ODataValue::String(record.name.clone()),
        "created_at" => ODataValue::Number(BigDecimal::from(record.created_at)),
        _ => return None,
    };
    Some(v)
}

// =====================================================================
//                          AST evaluation
// =====================================================================

/// Evaluate a parsed `$filter` expression against a row's field
/// extractor. Unknown fields and type-mismatched comparisons yield
/// `false` (fail-closed).
pub(crate) fn matches(expr: &Expr, field: &dyn Fn(&str) -> Option<ODataValue>) -> bool {
    match expr {
        Expr::And(a, b) => matches(a, field) && matches(b, field),
        Expr::Or(a, b) => matches(a, field) || matches(b, field),
        Expr::Not(inner) => !matches(inner, field),
        Expr::Compare(left, op, right) => compare_node(left, *op, right, field),
        Expr::In(boxed, values) => {
            let Some(field_name) = identifier_of(boxed) else {
                return false;
            };
            let Some(actual) = field(field_name) else {
                return false;
            };
            values
                .iter()
                .any(|v| literal_value(v).is_some_and(|lit| value_eq(&actual, &lit)))
        }
        Expr::Function(name, args) => function_call(name, args, field),
        Expr::Identifier(_) | Expr::Value(_) => false,
    }
}

fn compare_node(
    left: &Expr,
    op: CompareOperator,
    right: &Expr,
    field: &dyn Fn(&str) -> Option<ODataValue>,
) -> bool {
    let (Some(a), Some(b)) = (operand(left, field), operand(right, field)) else {
        return false;
    };
    match op {
        CompareOperator::Eq => value_eq(&a, &b),
        CompareOperator::Ne => !value_eq(&a, &b),
        CompareOperator::Gt => value_cmp(&a, &b) == Some(Ordering::Greater),
        CompareOperator::Ge => matches!(value_cmp(&a, &b), Some(Ordering::Greater | Ordering::Equal)),
        CompareOperator::Lt => value_cmp(&a, &b) == Some(Ordering::Less),
        CompareOperator::Le => matches!(value_cmp(&a, &b), Some(Ordering::Less | Ordering::Equal)),
    }
}

/// Resolve an operand (field identifier or literal) to a value.
fn operand(expr: &Expr, field: &dyn Fn(&str) -> Option<ODataValue>) -> Option<ODataValue> {
    match expr {
        Expr::Identifier(name) => field(name),
        Expr::Value(v) => literal_value(expr).or_else(|| Some(clone_value(v))),
        _ => None,
    }
}

fn literal_value(expr: &Expr) -> Option<ODataValue> {
    match expr {
        Expr::Value(v) => Some(clone_value(v)),
        _ => None,
    }
}

fn clone_value(v: &ODataValue) -> ODataValue {
    match v {
        ODataValue::Null => ODataValue::Null,
        ODataValue::Bool(b) => ODataValue::Bool(*b),
        ODataValue::Number(n) => ODataValue::Number(n.clone()),
        ODataValue::Uuid(u) => ODataValue::Uuid(*u),
        ODataValue::DateTime(d) => ODataValue::DateTime(*d),
        ODataValue::Date(d) => ODataValue::Date(*d),
        ODataValue::Time(t) => ODataValue::Time(*t),
        ODataValue::String(s) => ODataValue::String(s.clone()),
    }
}

fn identifier_of(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Identifier(name) => Some(name),
        _ => None,
    }
}

/// `contains` / `startswith` / `endswith` string predicates.
fn function_call(
    name: &str,
    args: &[Expr],
    field: &dyn Fn(&str) -> Option<ODataValue>,
) -> bool {
    if args.len() != 2 {
        return false;
    }
    let Some(field_name) = identifier_of(&args[0]) else {
        return false;
    };
    let Some(actual) = field(field_name) else {
        return false;
    };
    let Some(needle) = literal_value(&args[1]) else {
        return false;
    };
    let (ODataValue::String(haystack), ODataValue::String(needle)) = (&actual, &needle) else {
        return false;
    };
    match name.to_ascii_lowercase().as_str() {
        "contains" => haystack.contains(needle.as_str()),
        "startswith" => haystack.starts_with(needle.as_str()),
        "endswith" => haystack.ends_with(needle.as_str()),
        _ => false,
    }
}

/// Typed equality with light coercion so UUID/numeric literals parsed
/// as strings still compare correctly.
fn value_eq(a: &ODataValue, b: &ODataValue) -> bool {
    match (a, b) {
        (ODataValue::Null, ODataValue::Null) => true,
        (ODataValue::Bool(x), ODataValue::Bool(y)) => x == y,
        (ODataValue::Number(x), ODataValue::Number(y)) => x == y,
        (ODataValue::Uuid(x), ODataValue::Uuid(y)) => x == y,
        (ODataValue::String(x), ODataValue::String(y)) => x == y,
        (ODataValue::DateTime(x), ODataValue::DateTime(y)) => x == y,
        (ODataValue::Date(x), ODataValue::Date(y)) => x == y,
        (ODataValue::Time(x), ODataValue::Time(y)) => x == y,
        // Coerce string literals to their field's native shape.
        (ODataValue::Uuid(x), ODataValue::String(y)) => {
            Uuid::parse_str(y).is_ok_and(|y| y == *x)
        }
        (ODataValue::String(x), ODataValue::Uuid(y)) => {
            Uuid::parse_str(x).is_ok_and(|x| x == *y)
        }
        (ODataValue::Number(x), ODataValue::String(y)) => {
            y.parse::<BigDecimal>().is_ok_and(|y| y == *x)
        }
        (ODataValue::String(x), ODataValue::Number(y)) => {
            x.parse::<BigDecimal>().is_ok_and(|x| x == *y)
        }
        (ODataValue::Bool(x), ODataValue::String(y)) => {
            x == &y.eq_ignore_ascii_case("true")
        }
        (ODataValue::String(x), ODataValue::Bool(y)) => {
            x.eq_ignore_ascii_case("true") == *y
        }
        _ => false,
    }
}

/// Total order across compatible value shapes; `None` on incompatibility.
fn value_cmp(a: &ODataValue, b: &ODataValue) -> Option<Ordering> {
    match (a, b) {
        (ODataValue::Number(x), ODataValue::Number(y)) => x.partial_cmp(y),
        (ODataValue::String(x), ODataValue::String(y)) => Some(x.cmp(y)),
        (ODataValue::Uuid(x), ODataValue::Uuid(y)) => Some(x.cmp(y)),
        (ODataValue::Bool(x), ODataValue::Bool(y)) => Some(x.cmp(y)),
        (ODataValue::DateTime(x), ODataValue::DateTime(y)) => Some(x.cmp(y)),
        (ODataValue::Date(x), ODataValue::Date(y)) => Some(x.cmp(y)),
        (ODataValue::Time(x), ODataValue::Time(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

// =====================================================================
//                       List pipeline (shared)
// =====================================================================

/// Apply `$filter`, `$orderby`/default sort, `$skiptoken` cursor, `$top`
/// and `$select` to a tenant-scoped record list, producing an OData
/// page of projected JSON.
///
/// `project` renders an item as its final (pre-`$select`) JSON; fields
/// for filtering/ordering come from `field`.
pub(crate) fn page<T, F>(
    items: Vec<T>,
    query: &ODataQuery,
    field: &F,
    project: &dyn Fn(&T) -> serde_json::Value,
) -> Page<serde_json::Value>
where
    F: Fn(&T, &str) -> Option<ODataValue>,
{
    let limit = query.limit.unwrap_or(DEFAULT_LIST_TOP).clamp(1, MAX_LIST_TOP) as usize;

    let mut rows = items;
    if let Some(expr) = query.filter.as_ref() {
        rows.retain(|row| matches(expr, &|name| field(row, name)));
    }
    rows.sort_by(|a, b| order_cmp(a, b, &query.order, field));

    // Keyset cursor: the toolkit extractor decodes `$skiptoken` into
    // `k = [created_at, id]` (asc). Skip rows up to and including that
    // position, then hand out the next window.
    let start = query
        .cursor
        .as_ref()
        .and_then(|c| cursor_position(c, &rows, field))
        .unwrap_or(0);

    let page_items: Vec<serde_json::Value> = rows
        .iter()
        .skip(start)
        .take(limit)
        .map(|row| crate::api::rest::dto::select(project(row), query.select.as_deref()))
        .collect();

    let has_more = start + page_items.len() < rows.len();
    let next_cursor = if has_more {
        // Mint the cursor from the *last* row of the current page:
        // `cursor_position` skips rows strictly after it, so the next
        // request resumes exactly at `has_more`'s first unseen row.
        rows.get(start + page_items.len() - 1)
            .map(|next| make_cursor(next, field))
    } else {
        None
    };

    Page::new(
        page_items,
        PageInfo {
            next_cursor,
            prev_cursor: None,
            limit: limit as u64,
        },
    )
}

/// `$select` projection (no-op when no selection is supplied).
pub(crate) fn select(value: serde_json::Value, select: Option<&[String]>) -> serde_json::Value {
    if value.is_object() {
        if let Some(fields) = select {
            if !fields.is_empty() {
                let set: std::collections::HashSet<String> =
                    fields.iter().map(|f| f.to_lowercase()).collect();
                let Some(obj) = value.as_object() else {
                    return value;
                };
                let mut out = serde_json::Map::new();
                for (key, val) in obj {
                    if set.contains(&key.to_lowercase()) {
                        out.insert(key.clone(), val.clone());
                    }
                }
                return serde_json::Value::Object(out);
            }
        }
    }
    value
}

/// Sort comparator honoring `$orderby` keys (falling back to the stable
/// `(created_at ASC, id ASC)` base order so pagination stays
/// deterministic across `created_at` ties).
fn order_cmp<T>(
    a: &T,
    b: &T,
    order: &ODataOrderBy,
    field: &dyn Fn(&T, &str) -> Option<ODataValue>,
) -> Ordering {
    for key in &order.0 {
        let ord = match (field(a, &key.field), field(b, &key.field)) {
            (Some(x), Some(y)) => value_cmp(&x, &y).unwrap_or(Ordering::Equal),
            (Some(_), None) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (None, None) => Ordering::Equal,
        };
        if ord != Ordering::Equal {
            return if key.dir == SortDir::Desc {
                ord.reverse()
            } else {
                ord
            };
        }
    }
    // Stable base order.
    let by_time = compare_values(
        field(a, "created_at"),
        field(b, "created_at"),
        SortDir::Asc,
    );
    if by_time != Ordering::Equal {
        return by_time;
    }
    compare_values(field(a, "id"), field(b, "id"), SortDir::Asc)
}

fn compare_values(
    a: Option<ODataValue>,
    b: Option<ODataValue>,
    dir: SortDir,
) -> Ordering {
    let ord = match (a, b) {
        (Some(x), Some(y)) => value_cmp(&x, &y).unwrap_or(Ordering::Equal),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => Ordering::Equal,
    };
    match dir {
        SortDir::Asc => ord,
        SortDir::Desc => ord.reverse(),
    }
}

/// Locate the first row strictly after the cursor's `(created_at, id)`
/// keyset position.
fn cursor_position<T>(
    cursor: &toolkit_odata::CursorV1,
    rows: &[T],
    field: &dyn Fn(&T, &str) -> Option<ODataValue>,
) -> Option<usize> {
    if cursor.k.len() != 2 {
        return None;
    }
    let created: i64 = cursor.k[0].parse().ok()?;
    let id: Uuid = Uuid::parse_str(&cursor.k[1]).ok()?;
    rows.iter().position(|row| {
        let row_created = numeric_i64(field(row, "created_at")).unwrap_or(i64::MIN);
        let row_id = uuid_of(field(row, "id")).unwrap_or(Uuid::nil());
        row_created > created || (row_created == created && row_id > id)
    })
}

/// Mint the `$skiptoken` for the page starting just after `next` (the
/// last row of the current page; consumers skip rows strictly after it).
fn make_cursor<T>(next: &T, field: &dyn Fn(&T, &str) -> Option<ODataValue>) -> String {
    let created = numeric_i64(field(next, "created_at")).unwrap_or_default().to_string();
    let id = uuid_of(field(next, "id"))
        .unwrap_or(Uuid::nil())
        .to_string();
    toolkit_odata::CursorV1 {
        k: vec![created, id],
        o: SortDir::Asc,
        s: "created_at,id".to_owned(),
        f: None,
        d: "fwd".to_owned(),
    }
    .encode()
    .unwrap_or_default()
}

fn numeric_i64(value: Option<ODataValue>) -> Option<i64> {
    match value? {
        ODataValue::Number(n) => n.to_string().parse::<i64>().ok(),
        _ => None,
    }
}

fn uuid_of(value: Option<ODataValue>) -> Option<Uuid> {
    match value? {
        ODataValue::Uuid(u) => Some(u),
        ODataValue::String(s) => Uuid::parse_str(&s).ok(),
        _ => None,
    }
}
