//! OData list-query translation (`cpt-cf-oagw-algo-odata-query`).
//!
//! The list endpoints accept the OData subset the DESIGN documents — `$filter`,
//! `$select`, `$orderby`, `$top` and `$skip` — and the translation is domain
//! logic: the tenant scope is applied by the caller before any filtering,
//! ordering or paging (`inst-oq-01`), so this module only ever sees one
//! tenant's records.
//!
//! Only the comparison form the DESIGN documents is supported (`field eq
//! 'value'`); any other operator, unknown field or out-of-range page is `400`.

use std::cmp::Ordering;

use form_urlencoded::parse as parse_pairs;
use serde_json::Value;

use crate::domain::error::ManagementError;
use crate::domain::model::{Route, Timestamp, Upstream};

/// Recorded default of `$top`.
pub const DEFAULT_TOP: usize = 50;

/// Recorded maximum of `$top`.
pub const MAX_TOP: usize = 100;

/// Comparable, orderable form of one model field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    /// A string field (`alias`, `id`, `protocol`).
    Text(String),
    /// A boolean field (`enabled`).
    Flag(bool),
    /// An integer field (`priority`).
    Count(i64),
    /// A timestamp field (`created_at`).
    Stamp(Timestamp),
    /// A multi-valued field (`tags`): matches on membership.
    List(Vec<String>),
}

impl FieldValue {
    /// The rank that orders values of different kinds.
    const fn rank(&self) -> u8 {
        match self {
            Self::Text(_) => 0,
            Self::Flag(_) => 1,
            Self::Count(_) => 2,
            Self::Stamp(_) => 3,
            Self::List(_) => 4,
        }
    }

    /// Whether the field equals the value the filter expression carried.
    #[must_use]
    pub fn matches(&self, raw: &str) -> bool {
        match self {
            Self::Text(text) => text == raw,
            Self::Flag(flag) => raw.eq_ignore_ascii_case(&flag.to_string()),
            Self::Count(count) => raw.parse::<i64>().is_ok_and(|value| value == *count),
            Self::Stamp(stamp) => Timestamp::parse_rfc3339(raw).is_some_and(|value| {
                value.as_nanos() == stamp.as_nanos()
            }),
            Self::List(items) => items.iter().any(|item| item == raw),
        }
    }
}

impl Ord for FieldValue {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Text(left), Self::Text(right)) => left.cmp(right),
            (Self::Flag(left), Self::Flag(right)) => left.cmp(right),
            (Self::Count(left), Self::Count(right)) => left.cmp(right),
            (Self::Stamp(left), Self::Stamp(right)) => left.as_nanos().cmp(&right.as_nanos()),
            (Self::List(left), Self::List(right)) => left.cmp(right),
            (left, right) => left.rank().cmp(&right.rank()),
        }
    }
}

impl PartialOrd for FieldValue {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The model fields a list query may name.
pub trait QueryFields {
    /// The comparable value of one field, or `None` when the field is not part
    /// of the queryable model.
    fn field_value(&self, field: &str) -> Option<FieldValue>;

    /// The full stored representation, as the list contract returns it.
    fn as_json(&self) -> Value;
}

// @cpt-begin:cpt-cf-oagw-dod-odata-list:p1:inst-full
impl QueryFields for Upstream {
    fn field_value(&self, field: &str) -> Option<FieldValue> {
        match field {
            "id" => Some(FieldValue::Text(self.id.to_string())),
            "tenant_id" => Some(FieldValue::Text(self.tenant_id.to_string())),
            "enabled" => Some(FieldValue::Flag(self.enabled)),
            "alias" => Some(FieldValue::Text(self.alias.clone())),
            "tags" => Some(FieldValue::List(self.tags.clone())),
            "protocol" => Some(FieldValue::Text(self.protocol.as_str().to_string())),
            "created_at" => Some(FieldValue::Stamp(self.created_at)),
            _ => None,
        }
    }

    fn as_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

impl QueryFields for Route {
    fn field_value(&self, field: &str) -> Option<FieldValue> {
        match field {
            "id" => Some(FieldValue::Text(self.id.to_string())),
            "tenant_id" => Some(FieldValue::Text(self.tenant_id.to_string())),
            "upstream_id" => Some(FieldValue::Text(self.upstream_id.to_string())),
            "enabled" => Some(FieldValue::Flag(self.enabled)),
            "priority" => Some(FieldValue::Count(i64::from(self.priority))),
            "match_type" => Some(FieldValue::Text(self.match_type.as_str().to_string())),
            "tags" => Some(FieldValue::List(self.tags.clone())),
            "created_at" => Some(FieldValue::Stamp(self.created_at)),
            _ => None,
        }
    }

    fn as_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// One `$filter` comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    /// The compared field.
    pub field: String,
    /// The compared value, unquoted.
    pub value: String,
}

/// One `$orderby` term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderBy {
    /// The ordered field.
    pub field: String,
    /// Whether the direction is `desc`.
    pub descending: bool,
}

/// A translated list query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListQuery {
    /// The single comparison the filter carries, if any.
    pub filter: Option<Filter>,
    /// The single ordering term, if any.
    pub orderby: Option<OrderBy>,
    /// The projected fields, in request order; empty means the full record.
    pub select: Vec<String>,
    /// Page size, default [`DEFAULT_TOP`], capped at [`MAX_TOP`].
    pub top: usize,
    /// Page offset, non-negative.
    pub skip: usize,
}

impl Default for ListQuery {
    fn default() -> Self {
        Self {
            filter: None,
            orderby: None,
            select: Vec::new(),
            top: DEFAULT_TOP,
            skip: 0,
        }
    }
}

impl ListQuery {
    /// The fields the query names, for the projection and for validation.
    #[must_use]
    pub fn named_fields(&self) -> Vec<&str> {
        let mut fields = Vec::new();
        if let Some(filter) = self.filter.as_ref() {
            fields.push(filter.field.as_str());
        }
        if let Some(orderby) = self.orderby.as_ref() {
            fields.push(orderby.field.as_str());
        }
        fields.extend(self.select.iter().map(String::as_str));
        fields
    }
}

/// Translate the query string of a list request.
///
/// `filterable` is the comparable field set of the resource type and
/// `projectable` the member set of its stored representation: `$select` may
/// name any member, while `$filter` and `$orderby` only the comparable ones.
///
/// # Errors
///
/// Returns a mapped `400` for an unsupported query option, an unsupported
/// operator, an unknown field, an out-of-range `$top` and a negative or
/// non-numeric `$skip`.
pub fn parse(
    raw: &str,
    filterable: &[&str],
    projectable: &[&str],
) -> Result<ListQuery, ManagementError> {
    // @cpt-begin:cpt-cf-oagw-algo-odata-query:p1:inst-oq-01
    // The caller has already scoped the collection to the calling tenant, so
    // the translation only sees that tenant's records.
    let mut query = ListQuery::default();
    // @cpt-end:cpt-cf-oagw-algo-odata-query:p1:inst-oq-01

    for (name, value) in parse_pairs(raw.as_bytes()) {
        let name = name.into_owned();
        let value = value.into_owned();
        match name.as_str() {
            // @cpt-begin:cpt-cf-oagw-algo-odata-query:p1:inst-oq-02
            "$filter" => query.filter = Some(parse_filter(&value)?),
            // @cpt-end:cpt-cf-oagw-algo-odata-query:p1:inst-oq-02
            // @cpt-begin:cpt-cf-oagw-algo-odata-query:p1:inst-oq-04
            "$orderby" => query.orderby = Some(parse_orderby(&value)?),
            // @cpt-end:cpt-cf-oagw-algo-odata-query:p1:inst-oq-04
            // @cpt-begin:cpt-cf-oagw-algo-odata-query:p1:inst-oq-05
            "$select" => query.select = parse_select(&value)?,
            // @cpt-end:cpt-cf-oagw-algo-odata-query:p1:inst-oq-05
            // @cpt-begin:cpt-cf-oagw-algo-odata-query:p1:inst-oq-06
            "$top" => query.top = parse_top(&value)?,
            "$skip" => query.skip = parse_skip(&value)?,
            // @cpt-end:cpt-cf-oagw-algo-odata-query:p1:inst-oq-06
            other => {
                if other.starts_with('$') {
                    return Err(ManagementError::validation(format!(
                        "{other}: unsupported query option"
                    )));
                }
            }
        }
    }

    // @cpt-begin:cpt-cf-oagw-algo-odata-query:p1:inst-oq-03
    // Every named field must belong to the model of the resource type: the
    // filter and the ordering compare model fields, the projection names
    // members of the stored representation.
    if let Some(filter) = query.filter.as_ref()
        && !filterable.contains(&filter.field.as_str())
    {
        return Err(unknown_field(&filter.field));
    }
    if let Some(orderby) = query.orderby.as_ref()
        && !filterable.contains(&orderby.field.as_str())
    {
        return Err(unknown_field(&orderby.field));
    }
    for field in &query.select {
        if !projectable.contains(&field.as_str()) {
            return Err(unknown_field(field));
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-odata-query:p1:inst-oq-03

    Ok(query)
}

/// A `400` for a field the resource type does not carry.
fn unknown_field(field: &str) -> ManagementError {
    ManagementError::validation(format!("{field}: unknown field"))
}

/// Apply a translated query to one tenant's records.
///
/// Filter, then ordering, then offset and limit, so paging is stable for a
/// given tenant (`inst-oq-07`); the page is projected onto `$select` when the
/// parameter is present (`inst-oq-08`) and never carries credential material
/// (`inst-oq-09`).
#[must_use]
pub fn apply<T: QueryFields>(records: &[T], query: &ListQuery) -> Vec<Value> {
    // @cpt-begin:cpt-cf-oagw-algo-odata-query:p1:inst-oq-07
    let mut selected: Vec<&T> = records
        .iter()
        .filter(|record| matches_query(*record, query))
        .collect();
    if let Some(orderby) = query.orderby.as_ref() {
        selected.sort_by(|left, right| {
            let left = left.field_value(&orderby.field);
            let right = right.field_value(&orderby.field);
            let ordering = left.cmp(&right);
            if orderby.descending {
                ordering.reverse()
            } else {
                ordering
            }
        });
    }
    // @cpt-end:cpt-cf-oagw-algo-odata-query:p1:inst-oq-07

    selected
        .into_iter()
        .skip(query.skip)
        .take(query.top)
        .map(|record| project(record, query))
        .collect()
}

/// Whether one record passes the query's filter.
fn matches_query<T: QueryFields>(record: &T, query: &ListQuery) -> bool {
    match query.filter.as_ref() {
        // @cpt-begin:cpt-cf-oagw-algo-odata-query:p1:inst-oq-02
        Some(filter) => record
            .field_value(&filter.field)
            .is_some_and(|value| value.matches(&filter.value)),
        // @cpt-end:cpt-cf-oagw-algo-odata-query:p1:inst-oq-02
        None => true,
    }
}

/// The stored representation of one record, projected when `$select` is given.
fn project<T: QueryFields>(record: &T, query: &ListQuery) -> Value {
    let mut document = record.as_json();
    sanitize(&mut document);

    // @cpt-begin:cpt-cf-oagw-algo-odata-query:p1:inst-oq-08
    if query.select.is_empty() {
        return document;
    }
    let mut projected = serde_json::Map::new();
    if let Some(object) = document.as_object() {
        for field in &query.select {
            if let Some(value) = object.get(field) {
                projected.insert(field.clone(), value.clone());
            }
        }
    }
    Value::Object(projected)
    // @cpt-end:cpt-cf-oagw-algo-odata-query:p1:inst-oq-08
}

/// Drop any value that is not a `cred://` reference from the projected
/// configuration, so a stored secret can never reach a response body
/// (`cpt-cf-oagw-principle-cred-isolation`).
fn sanitize(document: &mut Value) {
    let Some(config) = document
        .get_mut("auth")
        .and_then(|auth| auth.get_mut("config"))
        .and_then(|config| config.as_object_mut())
    else {
        return;
    };
    config.retain(|_, value| {
        value
            .as_str()
            .is_some_and(|reference| reference.starts_with("cred://"))
    });
}

/// Parse `field eq 'value'`.
fn parse_filter(raw: &str) -> Result<Filter, ManagementError> {
    let (field, rest) = raw
        .split_once(char::is_whitespace)
        .ok_or_else(|| unsupported_filter(raw, "expected `field eq 'value'`"))?;
    let field = field.trim();
    let (operator, value) = rest
        .trim()
        .split_once(char::is_whitespace)
        .ok_or_else(|| unsupported_filter(raw, "expected `field eq 'value'`"))?;
    if operator != "eq" {
        return Err(unsupported_filter(raw, "unsupported operator"));
    }
    let value = value.trim();
    if field.is_empty() || value.is_empty() {
        return Err(unsupported_filter(raw, "expected `field eq 'value'`"));
    }
    // One comparison per filter: a quoted literal ends at its own closing quote
    // and an unquoted value may not carry the whitespace a second comparison
    // would need.
    let value = parse_literal(value, raw)?;
    Ok(Filter {
        field: field.to_string(),
        value,
    })
}

/// Parse `field [asc|desc]`.
fn parse_orderby(raw: &str) -> Result<OrderBy, ManagementError> {
    let mut parts = raw.split_whitespace();
    let Some(field) = parts.next() else {
        return Err(ManagementError::validation(
            "$orderby: expected a field name".to_string(),
        ));
    };
    let direction = parts.next();
    if parts.next().is_some() {
        return Err(ManagementError::validation(format!(
            "$orderby: `{raw}` orders on more than one field"
        )));
    }
    let descending = match direction {
        None => false,
        Some("asc") => false,
        Some("desc") => true,
        Some(other) => {
            return Err(ManagementError::validation(format!(
                "$orderby: `{other}` is not a supported direction"
            )));
        }
    };
    Ok(OrderBy {
        field: field.to_string(),
        descending,
    })
}

/// Parse the comma-separated field list of `$select`.
fn parse_select(raw: &str) -> Result<Vec<String>, ManagementError> {
    let fields: Vec<String> = raw
        .split(',')
        .map(|field| field.trim().to_string())
        .filter(|field| !field.is_empty())
        .collect();
    if fields.is_empty() {
        return Err(ManagementError::validation(
            "$select: expected a field list".to_string(),
        ));
    }
    Ok(fields)
}

/// Parse `$top` with its recorded default and cap.
fn parse_top(raw: &str) -> Result<usize, ManagementError> {
    let value: i64 = raw
        .parse()
        .map_err(|_| ManagementError::validation(format!("$top: `{raw}` is not a number")))?;
    if !(0..=MAX_TOP as i64).contains(&value) {
        return Err(ManagementError::validation(format!(
            "$top: `{raw}` is outside the range 0..={MAX_TOP}"
        )));
    }
    Ok(usize::try_from(value).unwrap_or(MAX_TOP))
}

/// Parse `$skip` as a non-negative offset.
fn parse_skip(raw: &str) -> Result<usize, ManagementError> {
    let value: i64 = raw
        .parse()
        .map_err(|_| ManagementError::validation(format!("$skip: `{raw}` is not a number")))?;
    if value < 0 {
        return Err(ManagementError::validation(
            "$skip: must be a non-negative offset".to_string(),
        ));
    }
    Ok(usize::try_from(value).unwrap_or(0))
}

/// The supported operator is the one the DESIGN documents.
fn unsupported_filter(raw: &str, reason: &str) -> ManagementError {
    ManagementError::validation(format!("$filter: `{raw}` — {reason}"))
}

/// The value of a comparison: the single-quoted OData literal form, or a bare
/// token without whitespace.
fn parse_literal(raw: &str, expression: &str) -> Result<String, ManagementError> {
    if let Some(inner) = raw.strip_prefix('\'') {
        let Some(end) = inner.find('\'') else {
            return Err(unsupported_filter(expression, "unterminated string literal"));
        };
        if end + 1 != inner.len() {
            return Err(unsupported_filter(
                expression,
                "expected a single comparison",
            ));
        }
        return Ok(inner[..end].to_string());
    }
    if raw.chars().any(char::is_whitespace) {
        return Err(unsupported_filter(
            expression,
            "expected a single comparison",
        ));
    }
    Ok(raw.to_string())
}
// @cpt-end:cpt-cf-oagw-dod-odata-list:p1:inst-full

// @cpt-begin:cpt-cf-oagw-algo-plugin-list-query:p1:inst-pqry-03
// @cpt-begin:cpt-cf-oagw-dod-plugin-model:p1:inst-full
impl QueryFields for crate::domain::model::Plugin {
    fn field_value(&self, field: &str) -> Option<FieldValue> {
        match field {
            "id" => Some(FieldValue::Text(self.id.clone())),
            "tenant_id" => Some(FieldValue::Text(self.tenant_id.to_string())),
            // The DESIGN documents the filter form `type eq 'guard'`, which
            // compares the plugin type token; the representation member is
            // `plugin_type` and both names compare the same value.
            "type" | "plugin_type" => {
                Some(FieldValue::Text(self.plugin_type.as_str().to_string()))
            }
            "name" => Some(FieldValue::Text(self.name.clone())),
            _ => None,
        }
    }

    fn as_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}
// @cpt-end:cpt-cf-oagw-dod-plugin-model:p1:inst-full
// @cpt-end:cpt-cf-oagw-algo-plugin-list-query:p1:inst-pqry-03

// @cpt-begin:cpt-cf-oagw-algo-plugin-list-query:p1:inst-pqry-02
/// Translate the query string of a plugin list request
/// (`cpt-cf-oagw-algo-plugin-list-query`).
///
/// The plugin list supports the four parameters the DESIGN declares for it —
/// `$filter`, `$select`, `$top` and `$skip` — and rejects `$orderby`, which the
/// DESIGN declares only for the upstream and route lists (`inst-pqry-07`).
///
/// # Errors
///
/// Returns the mapped `400` of an unsupported option, an unsupported operator,
/// an unknown field and an out-of-range page bound (`inst-pqry-04` to
/// `inst-pqry-06`).
pub fn parse_plugin_query(raw: &str) -> Result<ListQuery, ManagementError> {
    // @cpt-begin:cpt-cf-oagw-algo-plugin-list-query:p1:inst-pqry-04
    // @cpt-begin:cpt-cf-oagw-algo-plugin-list-query:p1:inst-pqry-05
    // @cpt-begin:cpt-cf-oagw-algo-plugin-list-query:p1:inst-pqry-06
    // The entry-2.2 translation rules of `cpt-cf-oagw-algo-odata-query` serve
    // this surface too: one `parse` applies the plugin field set, rejects an
    // unsupported operator or an unknown field, and checks the recorded page
    // bounds — a `$top` default of `50`, a cap of `100` and a non-negative
    // `$skip`.
    let query = parse(raw, PLUGIN_FILTER_FIELDS, PLUGIN_FIELDS)?;
    // @cpt-end:cpt-cf-oagw-algo-plugin-list-query:p1:inst-pqry-06
    // @cpt-end:cpt-cf-oagw-algo-plugin-list-query:p1:inst-pqry-05
    // @cpt-end:cpt-cf-oagw-algo-plugin-list-query:p1:inst-pqry-04

    // @cpt-begin:cpt-cf-oagw-algo-plugin-list-query:p1:inst-pqry-07
    // The DESIGN declares `$orderby` only for the upstream and route lists, so
    // the plugin list returns it as `400`.
    if query.orderby.is_some() {
        return Err(ManagementError::validation(
            "$orderby: unsupported query option for the plugin list".to_string(),
        ));
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-list-query:p1:inst-pqry-07

    // @cpt-begin:cpt-cf-oagw-algo-plugin-list-query:p1:inst-pqry-10
    Ok(query)
    // @cpt-end:cpt-cf-oagw-algo-plugin-list-query:p1:inst-pqry-10
}
// @cpt-end:cpt-cf-oagw-algo-plugin-list-query:p1:inst-pqry-02

/// The upstream fields `$filter` and `$orderby` may name.
pub const UPSTREAM_FILTER_FIELDS: &[&str] = &[
    "id",
    "tenant_id",
    "enabled",
    "alias",
    "tags",
    "protocol",
    "created_at",
];

/// The upstream members `$select` may project onto.
pub const UPSTREAM_FIELDS: &[&str] = &[
    "id",
    "tenant_id",
    "enabled",
    "alias",
    "tags",
    "server",
    "protocol",
    "auth",
    "headers",
    "rate_limit",
    "cors",
    "plugins",
    "created_at",
];

/// The route fields `$filter` and `$orderby` may name.
pub const ROUTE_FILTER_FIELDS: &[&str] = &[
    "id",
    "tenant_id",
    "upstream_id",
    "enabled",
    "priority",
    "match_type",
    "tags",
    "created_at",
];

/// The route members `$select` may project onto.
pub const ROUTE_FIELDS: &[&str] = &[
    "id",
    "tenant_id",
    "upstream_id",
    "enabled",
    "match",
    "match_type",
    "priority",
    "tags",
    "plugins",
    "rate_limit",
    "cors",
    "created_at",
];

// @cpt-begin:cpt-cf-oagw-algo-plugin-list-query:p1:inst-pqry-01
// The collection is scoped to the calling tenant before any filtering or paging:
// the service hands the translator only the rows of the caller's tenant.
/// The plugin fields `$filter` may name.
///
/// The DESIGN documents the filter form `type eq 'guard'`, which compares the
/// plugin type token, so the field is exposed both as `type` and under its
/// representation name `plugin_type` (`inst-pqry-03`).
pub const PLUGIN_FILTER_FIELDS: &[&str] = &[
    "id",
    "tenant_id",
    "type",
    "plugin_type",
    "name",
];

/// The plugin members `$select` may project onto.
///
/// The full stored representation the projection falls back to carries no
/// credential material: a definition holds configuration metadata and script
/// source only (`inst-pqry-09`).
pub const PLUGIN_FIELDS: &[&str] = &[
    "id",
    "tenant_id",
    "plugin_type",
    "name",
    "description",
    "config_schema",
    "phases",
    "source_code",
    "last_used_at",
    "gc_eligible_at",
];
// @cpt-begin:cpt-cf-oagw-algo-plugin-list-query:p1:inst-pqry-09
// No field of the projection carries credential material: a definition holds
// configuration metadata and script source only, and no `cred://` value is ever
// resolved onto this surface.
// @cpt-end:cpt-cf-oagw-algo-plugin-list-query:p1:inst-pqry-09
// @cpt-end:cpt-cf-oagw-algo-plugin-list-query:p1:inst-pqry-01

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;
    use crate::domain::model::{
        Endpoint, HttpMatch, MatchRule, MatchType, Scheme, ServerConfig, Timestamp,
    };
    use crate::domain::validation::validate_upstream;

    const TENANT: Uuid = uuid::uuid!("00000000-0000-0000-0000-0000000003aa");

    fn upstream(alias: &str, tags: &[&str]) -> Upstream {
        let mut record = upstream_with_rate(alias);
        record.tags = tags.iter().map(|tag| (*tag).to_string()).collect();
        record
    }

    fn upstream_with_rate(alias: &str) -> Upstream {
        let spec = serde_json::json!({
            "alias": alias,
            "server": { "endpoints": [ { "host": "api.vendor.com" } ] },
            "protocol": crate::domain::model::PROTOCOL_HTTP,
            "tags": [],
            "rate_limit": { "sustained": { "rate": 10 } }
        });
        let parsed: crate::domain::validation::UpstreamSpec =
            serde_json::from_value(spec).expect("bindable");
        validate_upstream(&parsed, TENANT, Timestamp::from_nanos(0)).expect("valid")
    }

    fn route(upstream_id: Uuid, path: &str, priority: i32) -> Route {
        Route {
            id: Uuid::new_v4(),
            tenant_id: TENANT,
            upstream_id,
            enabled: true,
            matches: MatchRule {
                http: Some(HttpMatch {
                    methods: vec!["GET".to_string()],
                    path: path.to_string(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: crate::domain::model::SuffixMode::Append,
                }),
                grpc: None,
            },
            match_type: MatchType::Http,
            priority,
            tags: Vec::new(),
            plugins: None,
            rate_limit: None,
            cors: None,
            created_at: Timestamp::from_nanos(0),
        }
    }

    fn parse_ok(raw: &str) -> ListQuery {
        parse(raw, UPSTREAM_FILTER_FIELDS, UPSTREAM_FIELDS).expect("translatable")
    }

    fn parse_err(raw: &str) -> ManagementError {
        parse(raw, UPSTREAM_FILTER_FIELDS, UPSTREAM_FIELDS).expect_err("rejected")
    }

    #[test]
    fn a_query_without_parameters_takes_the_recorded_defaults() {
        let query = parse_ok("");
        assert_eq!(query.top, DEFAULT_TOP);
        assert_eq!(query.skip, 0);
        assert_eq!(query.select, Vec::<String>::new());
        assert_eq!(query.filter, None);
        assert_eq!(query.orderby, None);
    }

    #[test]
    fn the_documented_comparison_form_translates() {
        let query = parse_ok("$filter=alias%20eq%20'api.openai.com'");
        assert_eq!(
            query.filter,
            Some(Filter {
                field: "alias".to_string(),
                value: "api.openai.com".to_string()
            })
        );
    }

    #[test]
    fn an_unsupported_operator_is_rejected() {
        for raw in [
            "$filter=alias ne 'api.openai.com'",
            "$filter=alias gt 'api.openai.com'",
            "$filter=alias eq 'a' and alias eq 'b'",
            "$filter=contains(alias,'api')",
            "$filter=alias eq 'a' or alias eq 'b'",
        ] {
            let error = parse_err(raw);
            assert_eq!(error.status(), 400, "{raw}");
            assert!(error.detail().contains("$filter"), "{}", error.detail());
        }
    }

    #[test]
    fn an_unknown_filter_or_select_field_is_rejected() {
        let error = parse_err("$filter=hostname eq 'api.openai.com'");
        assert_eq!(error.status(), 400);
        assert!(error.detail().contains("unknown field"), "{}", error.detail());

        let error = parse_err("$select=id,hostname");
        assert_eq!(error.status(), 400);
        assert!(error.detail().contains("unknown field"), "{}", error.detail());

        let error = parse_err("$orderby=hostname desc");
        assert_eq!(error.status(), 400);
    }

    #[test]
    fn orderby_takes_an_optional_direction() {
        let query = parse_ok("$orderby=created_at desc");
        assert_eq!(
            query.orderby,
            Some(OrderBy {
                field: "created_at".to_string(),
                descending: true
            })
        );
        let query = parse_ok("$orderby=created_at asc");
        assert_eq!(query.orderby.map(|term| term.descending), Some(false));
        let error = parse_err("$orderby=created_at sideways");
        assert_eq!(error.status(), 400);
    }

    #[test]
    fn top_defaults_to_50_and_is_capped_at_100() {
        assert_eq!(parse_ok("$top=100").top, 100);
        assert_eq!(parse_ok("$top=0").top, 0);
        let error = parse_err("$top=101");
        assert_eq!(error.status(), 400);
        let error = parse_err("$top=many");
        assert_eq!(error.status(), 400);
    }

    #[test]
    fn skip_must_be_a_non_negative_offset() {
        assert_eq!(parse_ok("$skip=25").skip, 25);
        assert_eq!(parse_ok("$skip=0").skip, 0);
        let error = parse_err("$skip=-1");
        assert_eq!(error.status(), 400);
        let error = parse_err("$skip=soon");
        assert_eq!(error.status(), 400);
    }

    #[test]
    fn an_unsupported_query_option_is_rejected() {
        let error = parse_err("$count=true");
        assert_eq!(error.status(), 400);
        assert!(
            error.detail().contains("unsupported query option"),
            "{}",
            error.detail()
        );
        // A non-OData parameter is not a query option and is ignored.
        assert_eq!(parse_ok("trace=1").top, DEFAULT_TOP);
    }

    #[test]
    fn filter_then_order_then_page_is_applied_in_that_order() {
        let records = vec![
            upstream("b.example.com", &["core"]),
            upstream("api.openai.com", &["core"]),
            upstream("api.vendor.com", &["edge"]),
            upstream("aaa.example.com", &["core"]),
        ];
        let query = parse_ok(
            "$filter=tags eq 'core'&$orderby=alias asc&$top=2&$skip=1",
        );
        let page = apply(&records, &query);
        let aliases: Vec<String> = page
            .iter()
            .filter_map(|row| row["alias"].as_str().map(str::to_owned))
            .collect();
        assert_eq!(
            aliases,
            vec!["api.openai.com".to_string(), "b.example.com".to_string()],
            "the match set ordered by alias is [aaa.example.com, api.openai.com, \
             b.example.com]; the page skips the first of it"
        );
    }

    #[test]
    fn created_at_orders_descending_across_windows() {
        let mut records = vec![
            upstream("one.example.com", &[]),
            upstream("two.example.com", &[]),
        ];
        records[0].created_at = Timestamp::parse_rfc3339("2026-01-02T03:04:05Z").expect("parsed");
        records[1].created_at = Timestamp::parse_rfc3339("2026-01-02T03:04:06Z").expect("parsed");
        let query = parse_ok("$orderby=created_at desc");
        let page = apply(&records, &query);
        assert_eq!(page[0]["alias"], serde_json::json!("two.example.com"));

        let query = parse_ok("$orderby=created_at asc");
        let page = apply(&records, &query);
        assert_eq!(page[0]["alias"], serde_json::json!("one.example.com"));
    }

    #[test]
    fn select_projects_the_page_onto_the_requested_fields() {
        let records = vec![upstream("api.vendor.com", &["core"])];
        let query = parse_ok("$select=id,alias,server");
        let page = apply(&records, &query);
        let object = page[0].as_object().expect("projected object");
        assert_eq!(object.len(), 3, "{}", page[0]);
        assert!(object.contains_key("id"));
        assert!(object.contains_key("alias"));
        assert!(object.contains_key("server"));
        assert!(!object.contains_key("rate_limit"));

        // An empty projection is not the full record.
        let query = parse_ok("$select=alias");
        let page = apply(&records, &query);
        assert_eq!(page[0].as_object().expect("object").len(), 1);
    }

    #[test]
    fn a_projected_response_never_carries_non_cred_configuration() {
        let mut record = upstream("api.vendor.com", &[]);
        record.auth = Some(crate::domain::model::AuthConfig {
            kind: "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.api_key.v1".to_string(),
            sharing: crate::domain::model::Sharing::Inherit,
            config: [
                ("key".to_string(), "cred://vendor/api-key".to_string()),
                ("raw".to_string(), "sk-raw-secret".to_string()),
            ]
            .into_iter()
            .collect(),
        });
        let query = parse_ok("$select=alias,auth");
        let page = apply(&record_set(record), &query);
        let rendered = page[0].to_string();
        assert!(rendered.contains("cred://vendor/api-key"), "{rendered}");
        assert!(!rendered.contains("sk-raw-secret"), "{rendered}");
    }

    fn record_set(record: Upstream) -> Vec<Upstream> {
        vec![record]
    }

    #[test]
    fn routes_filter_on_upstream_id_and_priority() {
        let upstream_id = Uuid::new_v4();
        let other = Uuid::new_v4();
        let records = vec![
            route(upstream_id, "/v1/a", 0),
            route(other, "/v1/b", 5),
        ];
        let query = parse(
            format!("$filter=upstream_id eq '{upstream_id}'").as_str(),
            ROUTE_FILTER_FIELDS,
            ROUTE_FIELDS,
        )
        .expect("translatable");
        let page = apply(&records, &query);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0]["upstream_id"], serde_json::json!(upstream_id));

        let query = parse("$orderby=priority desc", ROUTE_FILTER_FIELDS, ROUTE_FIELDS)
            .expect("translatable");
        let page = apply(&records, &query);
        assert_eq!(page[0]["priority"], serde_json::json!(5));
    }

    #[test]
    fn a_page_larger_than_the_match_set_returns_every_match() {
        let records = vec![upstream("api.vendor.com", &["core"])];
        let query = parse_ok("$top=100");
        assert_eq!(apply(&records, &query).len(), 1);
    }

    #[test]
    fn the_queryable_field_sets_come_from_the_model() {
        assert!(UPSTREAM_FILTER_FIELDS.contains(&"alias"));
        assert!(ROUTE_FILTER_FIELDS.contains(&"upstream_id"));
        assert!(!ROUTE_FILTER_FIELDS.contains(&"alias"));
        assert!(!UPSTREAM_FILTER_FIELDS.contains(&"upstream_id"));
        assert!(!UPSTREAM_FILTER_FIELDS.contains(&"server"), "no comparable object");
        // The projection may name the nested members the record carries.
        assert!(UPSTREAM_FIELDS.contains(&"server"));
        assert!(UPSTREAM_FIELDS.contains(&"auth"));
        assert!(ROUTE_FIELDS.contains(&"match"));
    }

    #[test]
    fn field_values_are_comparable_across_kinds() {
        let record = upstream("api.vendor.com", &["core"]);
        assert_eq!(
            record.field_value("alias"),
            Some(FieldValue::Text("api.vendor.com".to_string()))
        );
        assert!(record
            .field_value("enabled")
            .is_some_and(|value| value.matches("true")));
        assert!(record
            .field_value("tags")
            .is_some_and(|value| value.matches("core")));
        assert!(record
            .field_value("protocol")
            .is_some_and(|value| value.matches(crate::domain::model::PROTOCOL_HTTP)));
        assert_eq!(record.field_value("hostname"), None);

        let route = route(Uuid::new_v4(), "/v1", 7);
        assert_eq!(route.field_value("priority"), Some(FieldValue::Count(7)));
        assert!(route
            .field_value("priority")
            .is_some_and(|value| value.matches("7")));
        assert_eq!(
            route.field_value("match_type"),
            Some(FieldValue::Text("http".to_string()))
        );

        assert!(
            FieldValue::Stamp(Timestamp::from_nanos(2)) > FieldValue::Stamp(Timestamp::from_nanos(1))
        );
        assert!(FieldValue::Text("a".to_string()) < FieldValue::Flag(true));
    }

    #[test]
    fn created_at_matches_an_rfc3339_filter_value() {
        let mut record = upstream("api.vendor.com", &[]);
        record.created_at = Timestamp::parse_rfc3339("2026-01-02T03:04:05Z").expect("parsed");
        let query = parse_ok("$filter=created_at eq '2026-01-02T03:04:05Z'");
        assert_eq!(apply(&[record.clone()], &query).len(), 1);
        let query = parse_ok("$filter=created_at eq '2026-01-02T03:04:06Z'");
        assert!(apply(&[record], &query).is_empty());
    }

    #[test]
    fn the_route_helpers_of_the_model_stay_usable_from_here() {
        let _ = Endpoint {
            scheme: Scheme::Https,
            host: "api.vendor.com".to_string(),
            port: 443,
        };
        let _ = ServerConfig { endpoints: Vec::new() };
    }
}
