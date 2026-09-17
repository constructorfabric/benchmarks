//! REST DTOs for the OAGW management API (DESIGN §3.3 "API Contracts").
//!
//! The request bodies ([`UpstreamRequest`], [`RouteRequest`],
//! [`PluginRequest`]) mirror `docs/schemas/*.schema.json`: `POST`/`PUT` share
//! the same body shape (`PUT` is a full replacement), `id` and `tenant_id` are
//! never accepted from the wire, and unknown members are rejected
//! (`additionalProperties: false`). Handlers convert them into the domain
//! specs ([`UpstreamSpec`], [`RouteSpec`], [`PluginSpec`]), which stay free of
//! transport traits.
//!
//! List endpoints return a **bare array** (no envelope), supporting the OData
//! query parameters `$filter`, `$select`, `$orderby`, `$top`, `$skip`. The
//! fields each resource exposes to `$filter`/`$orderby` are declared by the
//! [`ListFields`] implementations below: a clause naming anything else is a 400,
//! never a silently satisfied filter.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use crate::domain::types::{
    AuthConfig, CorsConfig, Endpoint, HeadersConfig, Plugin, PluginSpec, PluginsConfig, Protocol,
    RateLimitConfig, Route, RouteMatch, RouteSpec, ServerConfig, Upstream, UpstreamSpec,
};

/// Wire body of `POST /oagw/v1/upstreams` and `PUT /oagw/v1/upstreams/{id}`.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct UpstreamRequest {
    /// Whether this upstream is enabled; defaults to `true`.
    #[serde(default = "default_request_enabled")]
    pub enabled: bool,
    /// Routing identifier; auto-derived for hostname endpoints when omitted.
    #[serde(default)]
    pub alias: Option<String>,
    /// Flat tags for categorization and discovery.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Server endpoints; required.
    pub server: ServerConfig,
    /// Upstream protocol; required.
    pub protocol: Protocol,
    /// Authentication plugin binding.
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    /// Rate limiting configuration.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

/// `enabled` defaults to `true` (schema `upstream.v1.schema.json`).
fn default_request_enabled() -> bool {
    true
}

impl From<UpstreamRequest> for UpstreamSpec {
    fn from(request: UpstreamRequest) -> Self {
        Self {
            enabled: request.enabled,
            alias: request.alias,
            tags: request.tags,
            server: request.server,
            protocol: request.protocol,
            auth: request.auth,
            headers: request.headers,
            plugins: request.plugins,
            rate_limit: request.rate_limit,
            cors: request.cors,
        }
    }
}

impl From<UpstreamSpec> for UpstreamRequest {
    fn from(spec: UpstreamSpec) -> Self {
        Self {
            enabled: spec.enabled,
            alias: spec.alias,
            tags: spec.tags,
            server: spec.server,
            protocol: spec.protocol,
            auth: spec.auth,
            headers: spec.headers,
            plugins: spec.plugins,
            rate_limit: spec.rate_limit,
            cors: spec.cors,
        }
    }
}

/// Wire body of `POST /oagw/v1/upstreams`.
pub type CreateUpstreamRequest = UpstreamRequest;

/// Wire body of `PUT /oagw/v1/upstreams/{id}` (full replacement).
pub type ReplaceUpstreamRequest = UpstreamRequest;

/// Response of the upstream CRUD endpoints.
///
/// Shape matches `upstream.v1.schema.json`: `id` is server-generated, `alias`
/// is always present (derived when not supplied), and omitted optional sections
/// are absent rather than `null`.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct UpstreamDto {
    /// System-generated unique identifier.
    pub id: Uuid,
    /// Whether this upstream is enabled.
    pub enabled: bool,
    /// Human-readable routing identifier.
    pub alias: String,
    /// Flat tags for categorization and discovery.
    pub tags: Vec<String>,
    /// Server endpoints.
    pub server: ServerConfig,
    /// Upstream protocol.
    pub protocol: Protocol,
    /// Authentication plugin binding.
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    pub headers: Option<HeadersConfig>,
    /// Plugin chain.
    pub plugins: Option<PluginsConfig>,
    /// Rate limiting configuration.
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    pub cors: Option<CorsConfig>,
}

impl From<&Upstream> for UpstreamDto {
    fn from(upstream: &Upstream) -> Self {
        Self {
            id: upstream.id,
            enabled: upstream.spec.enabled,
            alias: upstream.alias.clone(),
            tags: upstream.spec.tags.clone(),
            server: ServerConfig {
                endpoints: upstream.spec.server.endpoints.clone(),
            },
            protocol: upstream.spec.protocol,
            auth: upstream.spec.auth.clone(),
            headers: upstream.spec.headers.clone(),
            plugins: upstream.spec.plugins.clone(),
            rate_limit: upstream.spec.rate_limit,
            cors: upstream.spec.cors.clone(),
        }
    }
}

impl From<Upstream> for UpstreamDto {
    fn from(upstream: Upstream) -> Self {
        Self::from(&upstream)
    }
}

impl UpstreamDto {
    /// The endpoints of this upstream.
    #[must_use]
    pub fn endpoints(&self) -> &[Endpoint] {
        &self.server.endpoints
    }

    /// The GTS instance identifier of this upstream.
    #[must_use]
    pub fn gts_id(&self) -> String {
        format!("{}{}", crate::domain::types::UPSTREAM_TYPE_ID, self.id)
    }
}

/// OData list query parameters shared by all list endpoints.
///
/// Unrecognized query parameters are ignored by the `Query` extractor; a
/// `$filter` clause the gear cannot evaluate is **rejected** with 400 (never
/// silently satisfied), so a list request never returns a superset the caller
/// did not ask for.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListQuery {
    /// Maximum number of items (default 50, max 100).
    #[serde(rename = "$top", default)]
    pub top: Option<usize>,
    /// Offset into the result set.
    #[serde(rename = "$skip", default)]
    pub skip: Option<usize>,
    /// Sort order, e.g. `alias desc`.
    #[serde(rename = "$orderby", default)]
    pub orderby: Option<String>,
    /// Comma-separated fields to project, e.g. `id,alias,server`.
    #[serde(rename = "$select", default)]
    pub select: Option<String>,
    /// OData filter expression; strictly parsed (see [`matches_filter`]).
    #[serde(rename = "$filter", default)]
    pub filter: Option<String>,
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// Wire body of `POST /oagw/v1/routes` and `PUT /oagw/v1/routes/{id}`
/// (`docs/schemas/route.v1.schema.json`).
///
/// The schema still requires `upstream_id` on a replacement, where the field is
/// immutable: the service accepts it and rejects a value that differs from the
/// stored one (DESIGN §3.3 "PUT (Replace)").
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct RouteRequest {
    /// Upstream the route belongs to; immutable after creation.
    pub upstream_id: Uuid,
    /// Protocol-scoped match rules; required.
    #[serde(rename = "match")]
    pub match_rules: RouteMatch,
    /// Whether this route participates in matching; defaults to `true`.
    #[serde(default = "default_request_enabled")]
    pub enabled: bool,
    /// Flat tags for categorization and discovery.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    /// Rate limiting configuration.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
}

impl From<RouteRequest> for RouteSpec {
    fn from(request: RouteRequest) -> Self {
        Self {
            upstream_id: request.upstream_id,
            match_rules: request.match_rules,
            enabled: request.enabled,
            tags: request.tags,
            plugins: request.plugins,
            rate_limit: request.rate_limit,
        }
    }
}

impl From<RouteSpec> for RouteRequest {
    fn from(spec: RouteSpec) -> Self {
        Self {
            upstream_id: spec.upstream_id,
            match_rules: spec.match_rules,
            enabled: spec.enabled,
            tags: spec.tags,
            plugins: spec.plugins,
            rate_limit: spec.rate_limit,
        }
    }
}

/// Wire body of `POST /oagw/v1/routes`.
pub type CreateRouteRequest = RouteRequest;

/// Wire body of `PUT /oagw/v1/routes/{id}` (full replacement).
pub type ReplaceRouteRequest = RouteRequest;

/// Response of the route CRUD endpoints.
///
/// Shape matches `route.v1.schema.json` plus the `created_at`/`updated_at`
/// metadata the list endpoints sort by; omitted optional sections are absent
/// rather than `null`.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct RouteDto {
    /// System-generated unique identifier.
    pub id: Uuid,
    /// Upstream the route belongs to.
    pub upstream_id: Uuid,
    /// Protocol-scoped match rules.
    #[serde(rename = "match")]
    pub match_rules: RouteMatch,
    /// Whether this route participates in matching.
    pub enabled: bool,
    /// Flat tags for categorization and discovery.
    pub tags: Vec<String>,
    /// Plugin chain.
    pub plugins: Option<PluginsConfig>,
    /// Rate limiting configuration.
    pub rate_limit: Option<RateLimitConfig>,
    /// Creation instant, Unix seconds.
    pub created_at: u64,
    /// Last modification instant, Unix seconds.
    pub updated_at: u64,
}

impl From<&Route> for RouteDto {
    fn from(route: &Route) -> Self {
        Self {
            id: route.id,
            upstream_id: route.upstream_id,
            match_rules: route.spec.match_rules.clone(),
            enabled: route.spec.enabled,
            tags: route.spec.tags.clone(),
            plugins: route.spec.plugins.clone(),
            rate_limit: route.spec.rate_limit,
            created_at: route.created_at,
            updated_at: route.updated_at,
        }
    }
}

impl From<Route> for RouteDto {
    fn from(route: Route) -> Self {
        Self::from(&route)
    }
}

impl RouteDto {
    /// The GTS instance identifier of this route
    /// (`gts.cf.core.oagw.route.v1~{uuid}`).
    #[must_use]
    pub fn gts_id(&self) -> String {
        format!("{}{}", crate::domain::types::ROUTE_TYPE_ID, self.id)
    }
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// Wire body of `POST /oagw/v1/plugins` (ADR-0002 Appendix A).
///
/// There is no replacement body: plugins are immutable (DESIGN §3.3), so `PUT`
/// is not registered at all.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct PluginRequest {
    /// Tenant-unique plugin name.
    pub name: String,
    /// Plugin schema type (`auth_plugin` / `guard_plugin` / `transform_plugin`).
    pub plugin_type: String,
    /// JSON schema of the plugin configuration.
    #[serde(default)]
    pub config_schema: Option<Value>,
    /// Starlark source code.
    pub source_code: String,
}

impl From<PluginRequest> for PluginSpec {
    fn from(request: PluginRequest) -> Self {
        Self {
            name: request.name,
            plugin_type: request.plugin_type,
            config_schema: request.config_schema,
            source_code: request.source_code,
        }
    }
}

/// Wire body of `POST /oagw/v1/plugins`.
pub type CreatePluginRequest = PluginRequest;

/// Response of the plugin CRUD endpoints.
///
/// `last_used_at` and `gc_eligible_at` are `null` until the plugin engine runs
/// the plugin (DESIGN §3.1); the Starlark source is also served verbatim by
/// `GET /plugins/{id}/source`.
#[derive(Debug, Clone, PartialEq)]
#[toolkit_macros::api_dto(response)]
pub struct PluginDto {
    /// System-generated unique identifier.
    pub id: Uuid,
    /// Plugin schema type.
    pub plugin_type: String,
    /// Tenant-unique plugin name.
    pub name: String,
    /// JSON schema of the plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<Value>,
    /// Starlark source code.
    pub source_code: String,
    /// Last usage instant, Unix seconds.
    pub last_used_at: Option<u64>,
    /// Instant from which the plugin may be garbage collected, Unix seconds.
    pub gc_eligible_at: Option<u64>,
}

impl From<&Plugin> for PluginDto {
    fn from(plugin: &Plugin) -> Self {
        Self {
            id: plugin.id,
            plugin_type: plugin.plugin_type.clone(),
            name: plugin.name.clone(),
            config_schema: plugin.config_schema.clone(),
            source_code: plugin.source_code.clone(),
            last_used_at: plugin.last_used_at,
            gc_eligible_at: plugin.gc_eligible_at,
        }
    }
}

impl From<Plugin> for PluginDto {
    fn from(plugin: Plugin) -> Self {
        Self::from(&plugin)
    }
}

impl PluginDto {
    /// The GTS instance identifier of this plugin
    /// (`gts.cf.core.oagw.{type}_plugin.v1~{uuid}`).
    #[must_use]
    pub fn gts_id(&self) -> String {
        format!(
            "{}{}",
            crate::domain::types::plugin_type_id(&self.plugin_type),
            self.id
        )
    }
}

/// Page bounds applied to a list response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    /// Offset into the ordered result set.
    pub skip: usize,
    /// Maximum number of items returned.
    pub top: usize,
}

/// Default page size for list endpoints (DESIGN §3.3).
pub const DEFAULT_PAGE_TOP: usize = 50;

/// Maximum page size for list endpoints (DESIGN §3.3).
pub const MAX_PAGE_TOP: usize = 100;

impl ListQuery {
    /// Resolve `$top`/`$skip` into page bounds.
    ///
    /// `$top=0` and `$top > 100` are rejected with 400: an operator asking for
    /// a page larger than the documented maximum is a client bug worth
    /// surfacing rather than silently clamping.
    ///
    /// # Errors
    /// [`OagwErrorKind::ValidationError`] when `$top` is out of range.
    pub fn page(&self) -> Result<Page, crate::error::OagwError> {
        let top = self.top.unwrap_or(DEFAULT_PAGE_TOP);
        if top == 0 || top > MAX_PAGE_TOP {
            return Err(crate::error::OagwError::validation(format!(
                "`$top` must be between 1 and {MAX_PAGE_TOP}"
            )));
        }

        Ok(Page {
            skip: self.skip.unwrap_or(0),
            top,
        })
    }

    /// Split `$orderby` into `(field, descending)`.
    ///
    /// Supports a single key with an optional `asc`/`desc` direction, matching
    /// the documented usage (`created_at desc`).
    ///
    /// # Errors
    /// [`OagwErrorKind::ValidationError`] when the expression is malformed or
    /// names more than one key.
    pub fn orderby(&self) -> Result<Option<(&str, bool)>, crate::error::OagwError> {
        let raw = self
            .orderby
            .as_deref()
            .map(str::trim)
            .filter(|raw| !raw.is_empty());
        let Some(raw) = raw else {
            return Ok(None);
        };

        let tokens: Vec<&str> = raw.split_whitespace().collect();
        if tokens.len() > 2 {
            return Err(crate::error::OagwError::validation(
                "`$orderby` supports a single field with an optional direction",
            ));
        }

        let field = tokens[0];
        let descending = match tokens.get(1).copied() {
            None | Some("asc") | Some("ASC") => false,
            Some("desc") | Some("DESC") => true,
            Some(other) => {
                return Err(crate::error::OagwError::validation(format!(
                    "`$orderby` direction must be `asc` or `desc`, got '{other}'"
                )));
            }
        };

        Ok(Some((field, descending)))
    }

    /// The `$select` field list, when present.
    ///
    /// Empty and whitespace-only selections yield `None` (no projection).
    #[must_use]
    pub fn selected_fields(&self) -> Option<Vec<String>> {
        let raw = self.select.as_deref()?;
        let fields: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|field| !field.is_empty())
            .map(ToOwned::to_owned)
            .collect();

        if fields.is_empty() {
            None
        } else {
            Some(fields)
        }
    }
}

/// Serialize a page of upstreams, applying `$select` projection when present.
///
/// Items are shared handles ([`Arc`]); the DTO — the only thing actually
/// serialized — is built per item of the returned page.
#[must_use]
pub fn upstreams_to_json(items: &[Arc<Upstream>], query: &ListQuery) -> Vec<Value> {
    let selected = query.selected_fields();
    items
        .iter()
        .map(|upstream| UpstreamDto::from(upstream.as_ref()))
        .map(|dto| toolkit::api::select::apply_select(dto, selected.as_deref()))
        .collect()
}

/// Apply `$orderby`, `$filter`, `$skip` and `$top` to a list of upstreams.
///
/// Ordering is applied first (so `$skip`/`$top` page a stable sequence), then
/// filtering, then paging. `$orderby` keys outside the resource's orderable
/// fields are ignored rather than rejected.
///
/// Records are copied as [`Arc`] handles only: the caller clones the
/// configuration of the page it actually serializes.
///
/// # Errors
/// [`crate::error::OagwErrorKind::ValidationError`] when `$top` is out of
/// range, `$orderby` is malformed, or a `$filter` clause cannot be evaluated.
pub fn page_upstreams(
    items: &[Arc<Upstream>],
    query: &ListQuery,
) -> Result<Vec<Arc<Upstream>>, crate::error::OagwError> {
    page_items(items, query)
}

/// Apply `$orderby`, `$filter`, `$skip` and `$top` to a list of routes.
///
/// # Errors
/// [`crate::error::OagwErrorKind::ValidationError`] when `$top` is out of
/// range, `$orderby` is malformed, or a `$filter` clause cannot be evaluated.
pub fn page_routes(
    items: &[Arc<Route>],
    query: &ListQuery,
) -> Result<Vec<Arc<Route>>, crate::error::OagwError> {
    page_items(items, query)
}

/// Apply `$orderby`, `$filter`, `$skip` and `$top` to a list of plugins.
///
/// # Errors
/// [`crate::error::OagwErrorKind::ValidationError`] when `$top` is out of
/// range, `$orderby` is malformed, or a `$filter` clause cannot be evaluated.
pub fn page_plugins(
    items: &[Arc<Plugin>],
    query: &ListQuery,
) -> Result<Vec<Arc<Plugin>>, crate::error::OagwError> {
    page_items(items, query)
}

/// [`page_upstreams`]/[`page_routes`]/[`page_plugins`], generic over the list
/// vocabulary the resource declares.
fn page_items<T: ListFields>(
    items: &[Arc<T>],
    query: &ListQuery,
) -> Result<Vec<Arc<T>>, crate::error::OagwError> {
    let mut ordered: Vec<Arc<T>> = items.to_vec();

    if let Some((field, descending)) = query.orderby()?
        && T::orderable_fields().contains(&field)
    {
        ordered.sort_by(|left, right| left.compare(right, field));
        if descending {
            ordered.reverse();
        }
    }

    let filtered: Vec<Arc<T>> = match query.filter.as_deref().map(str::trim) {
        Some(filter) if !filter.is_empty() => {
            let clauses = parse_filter::<T>(filter)?;
            ordered
                .into_iter()
                .filter(|item| clauses.iter().all(|clause| clause.matches(item.as_ref())))
                .collect()
        }
        _ => ordered,
    };

    let page = query.page()?;
    Ok(filtered
        .into_iter()
        .skip(page.skip)
        .take(page.top)
        .collect())
}

/// A `$filter`-comparable field of one resource: its name and literal kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterField {
    /// Field name as it appears in the expression.
    pub name: &'static str,
    /// Whether the field compares as a boolean literal rather than a
    /// single-quoted string.
    pub boolean: bool,
}

/// The list vocabulary of one resource: the fields `$filter` can evaluate and
/// the fields `$orderby` can sort by.
///
/// Implemented by the domain records each list endpoint pages
/// ([`Upstream`], [`Route`], [`Plugin`]). A clause naming a field outside
/// [`Self::filter_fields`] is rejected with 400, so a list request can never be
/// silently satisfied by a filter the gear did not understand.
pub trait ListFields: Sized {
    /// The fields `$filter` accepts, in declaration order.
    fn filter_fields() -> &'static [FilterField];
    /// The fields `$orderby` accepts.
    fn orderable_fields() -> &'static [&'static str];
    /// The text value of a text-valued `$filter` field.
    fn text_value(&self, field: &str) -> String;
    /// The boolean value of a boolean-valued `$filter` field.
    fn boolean_value(&self, field: &str) -> bool;
    /// Compare two records by an `$orderby` field.
    fn compare(&self, other: &Self, field: &str) -> std::cmp::Ordering;
}

/// Comparison operator of a `$filter` clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterOperator {
    /// Equality.
    Equal,
    /// Inequality.
    NotEqual,
}

/// The literal of a `$filter` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FilterValue {
    /// A single-quoted string literal.
    Text(String),
    /// An unquoted boolean literal.
    Bool(bool),
}

/// One parsed and validated `$filter` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FilterClause {
    /// `contains(field,'value')`.
    Contains {
        /// Field searched for the literal.
        field: FilterField,
        /// Searched substring.
        value: String,
    },
    /// `field eq|ne value`.
    Compare {
        /// Field compared.
        field: FilterField,
        /// Comparison operator.
        operator: FilterOperator,
        /// Compared literal.
        value: FilterValue,
    },
}

impl FilterClause {
    /// Evaluate this clause against one record.
    fn matches<T: ListFields>(&self, item: &T) -> bool {
        match self {
            Self::Contains { field, value } => item.text_value(field.name).contains(value),
            Self::Compare {
                field,
                operator,
                value,
            } => {
                let equal = match (field.boolean, value) {
                    (true, FilterValue::Bool(literal)) => {
                        item.boolean_value(field.name) == *literal
                    }
                    // A boolean field is never compared as text, and a text
                    // field never as a boolean literal: the mismatch keeps the
                    // clause false instead of guessing.
                    (true, FilterValue::Text(_)) => false,
                    (false, FilterValue::Text(literal)) => item.text_value(field.name) == *literal,
                    (false, FilterValue::Bool(_)) => false,
                };

                match operator {
                    FilterOperator::Equal => equal,
                    FilterOperator::NotEqual => !equal,
                }
            }
        }
    }
}

/// Parse a `$filter` expression into clauses combined with `and`.
///
/// Strict by design: a clause the parser cannot evaluate (unknown field,
/// unsupported operator or function, wrong arity, malformed literal) is a 400
/// that names the offending expression. Lenient parsing would silently return a
/// superset of what the caller asked for.
///
/// # Errors
/// [`crate::error::OagwErrorKind::ValidationError`] describing the unsupported
/// expression.
fn parse_filter<T: ListFields>(filter: &str) -> Result<Vec<FilterClause>, crate::error::OagwError> {
    split_and_clauses(filter)
        .iter()
        .map(|clause| parse_filter_clause::<T>(clause, filter))
        .collect()
}

/// Split a `$filter` expression on its top-level `and` keywords.
///
/// `and` inside a single-quoted literal is literal text, not a conjunction.
fn split_and_clauses(filter: &str) -> Vec<String> {
    let bytes = filter.as_bytes();
    let mut clauses = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'\'' => {
                quoted = !quoted;
                index += 1;
            }
            b'a' | b'A' if !quoted => {
                let word = bytes.get(index..index + 3);
                let starts_a_word = index == 0 || !bytes[index - 1].is_ascii_alphanumeric();
                let ends_a_word = !bytes
                    .get(index + 3)
                    .is_some_and(|byte| byte.is_ascii_alphanumeric());
                let is_conjunction = word.is_some_and(|word| word.eq_ignore_ascii_case(b"and"));

                if is_conjunction && starts_a_word && ends_a_word {
                    clauses.push(filter[start..index].trim().to_owned());
                    index += 3;
                    start = index;
                } else {
                    index += 1;
                }
            }
            _ => index += 1,
        }
    }

    clauses.push(filter[start..].trim().to_owned());
    clauses
}

/// Parse one `$filter` clause, reporting failures against the whole expression.
///
/// # Errors
/// [`crate::error::OagwErrorKind::ValidationError`] naming `clause`.
fn parse_filter_clause<T: ListFields>(
    clause: &str,
    expression: &str,
) -> Result<FilterClause, crate::error::OagwError> {
    parse_clause::<T>(clause).map_err(|reason| {
        crate::error::OagwError::validation(format!(
            "the `$filter` clause '{clause}' is not supported ({reason}); the expression was \
             '{expression}'"
        ))
        .with_extension(
            "unsupported_filter_clause",
            serde_json::Value::String(clause.to_owned()),
        )
    })
}

/// Parse a single `$filter` clause.
///
/// # Errors
/// A human-readable reason when the clause is not an expression the gear can
/// evaluate.
fn parse_clause<T: ListFields>(clause: &str) -> Result<FilterClause, String> {
    let trimmed = clause.trim();
    if trimmed.is_empty() {
        return Err("empty expression".to_owned());
    }

    // Function call form: `contains(field,'value')`.
    if let Some(open) = trimmed.find('(') {
        let name = trimmed[..open].trim();
        let arguments = trimmed[open + 1..]
            .strip_suffix(')')
            .ok_or("unbalanced parentheses")?;
        if !name.eq_ignore_ascii_case("contains") {
            return Err(format!("the function '{name}' is not supported"));
        }

        let (field, value) = arguments
            .split_once(',')
            .ok_or("contains() takes exactly two arguments")?;
        let field = text_filter_field::<T>(field.trim())?;
        let value = string_literal(value.trim())?;
        if value.is_empty() {
            return Err("contains() takes a non-empty literal".to_owned());
        }

        return Ok(FilterClause::Contains { field, value });
    }

    // Comparison form: `field operator value`.
    let Some(after_field) = trimmed.find(char::is_whitespace) else {
        return Err("expected `field operator value`".to_owned());
    };
    let field = filter_field::<T>(trimmed[..after_field].trim())?;
    let rest = trimmed[after_field..].trim_start();
    let Some(after_operator) = rest.find(char::is_whitespace) else {
        return Err("expected a literal after the operator".to_owned());
    };
    let operator = match rest[..after_operator].trim() {
        "eq" => FilterOperator::Equal,
        "ne" => FilterOperator::NotEqual,
        other => return Err(format!("the operator '{other}' is not supported")),
    };

    let value = rest[after_operator..].trim();
    let literal = match field.boolean {
        true => FilterValue::Bool(boolean_literal(value)?),
        false => FilterValue::Text(string_literal(value)?),
    };

    Ok(FilterClause::Compare {
        field,
        operator,
        value: literal,
    })
}

/// Resolve the field of a `contains()` clause: the text-valued fields.
fn text_filter_field<T: ListFields>(name: &str) -> Result<FilterField, String> {
    let field = filter_field::<T>(name)?;
    if field.boolean {
        return Err(format!("the field '{name}' is not supported by contains()"));
    }

    Ok(field)
}

/// Resolve the field of a `$filter` clause against the resource's vocabulary.
fn filter_field<T: ListFields>(name: &str) -> Result<FilterField, String> {
    T::filter_fields()
        .iter()
        .copied()
        .find(|field| field.name == name)
        .ok_or_else(|| format!("the field '{name}' is not supported"))
}

/// Parse a single-quoted OData string literal.
///
/// Quoted escapes (`''`) are not supported: a nested quote is a malformed
/// literal, which keeps the clause grammar unambiguous.
fn string_literal(value: &str) -> Result<String, String> {
    let literal = value
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
        .ok_or_else(|| format!("'{value}' must be a single-quoted string literal"))?;
    if literal.contains('\'') {
        return Err(format!("'{value}' is not a single-quoted string literal"));
    }

    Ok(literal.to_owned())
}

/// Parse an OData boolean literal (`true`/`false`, quoted or bare).
fn boolean_literal(value: &str) -> Result<bool, String> {
    let unquoted = value.trim_matches('\'');
    match unquoted {
        "true" | "TRUE" | "True" => Ok(true),
        "false" | "FALSE" | "False" => Ok(false),
        other => Err(format!("'{other}' must be a boolean literal")),
    }
}

// ---------------------------------------------------------------------------
// List vocabulary of each resource
// ---------------------------------------------------------------------------

/// `$filter`/`$orderby` vocabulary of an upstream (DESIGN §3.3).
const UPSTREAM_FILTER_FIELDS: [FilterField; 4] = [
    FilterField {
        name: "alias",
        boolean: false,
    },
    // `name` is the documented synonym of `alias`.
    FilterField {
        name: "name",
        boolean: false,
    },
    FilterField {
        name: "protocol",
        boolean: false,
    },
    FilterField {
        name: "enabled",
        boolean: true,
    },
];

/// Fields accepted by `$orderby` for an upstream.
const UPSTREAM_ORDERABLE_FIELDS: [&str; 5] = ["alias", "id", "created_at", "updated_at", "enabled"];

impl ListFields for Upstream {
    fn filter_fields() -> &'static [FilterField] {
        &UPSTREAM_FILTER_FIELDS
    }

    fn orderable_fields() -> &'static [&'static str] {
        &UPSTREAM_ORDERABLE_FIELDS
    }

    fn text_value(&self, field: &str) -> String {
        match field {
            "alias" | "name" => self.alias.clone(),
            "protocol" => self.spec.protocol.gts_id().to_owned(),
            _ => String::new(),
        }
    }

    fn boolean_value(&self, field: &str) -> bool {
        match field {
            "enabled" => self.spec.enabled,
            _ => false,
        }
    }

    fn compare(&self, other: &Self, field: &str) -> std::cmp::Ordering {
        match field {
            "alias" => self.alias.cmp(&other.alias),
            "id" => self.id.cmp(&other.id),
            "created_at" => self.created_at.cmp(&other.created_at),
            "updated_at" => self.updated_at.cmp(&other.updated_at),
            "enabled" => self.spec.enabled.cmp(&other.spec.enabled),
            _ => std::cmp::Ordering::Equal,
        }
    }
}

/// `$filter`/`$orderby` vocabulary of a route (DESIGN §3.3).
const ROUTE_FILTER_FIELDS: [FilterField; 2] = [
    FilterField {
        name: "upstream_id",
        boolean: false,
    },
    FilterField {
        name: "enabled",
        boolean: true,
    },
];

/// Fields accepted by `$orderby` for a route.
const ROUTE_ORDERABLE_FIELDS: [&str; 4] = ["id", "created_at", "updated_at", "enabled"];

impl ListFields for Route {
    fn filter_fields() -> &'static [FilterField] {
        &ROUTE_FILTER_FIELDS
    }

    fn orderable_fields() -> &'static [&'static str] {
        &ROUTE_ORDERABLE_FIELDS
    }

    fn text_value(&self, field: &str) -> String {
        match field {
            "upstream_id" => self.upstream_id.to_string(),
            _ => String::new(),
        }
    }

    fn boolean_value(&self, field: &str) -> bool {
        match field {
            "enabled" => self.spec.enabled,
            _ => false,
        }
    }

    fn compare(&self, other: &Self, field: &str) -> std::cmp::Ordering {
        match field {
            "id" => self.id.cmp(&other.id),
            "created_at" => self.created_at.cmp(&other.created_at),
            "updated_at" => self.updated_at.cmp(&other.updated_at),
            "enabled" => self.spec.enabled.cmp(&other.spec.enabled),
            _ => std::cmp::Ordering::Equal,
        }
    }
}

/// `$filter`/`$orderby` vocabulary of a plugin (DESIGN §3.3).
const PLUGIN_FILTER_FIELDS: [FilterField; 4] = [
    FilterField {
        name: "name",
        boolean: false,
    },
    FilterField {
        name: "plugin_type",
        boolean: false,
    },
    // `type` is the documented spelling in the plugin filter examples.
    FilterField {
        name: "type",
        boolean: false,
    },
    FilterField {
        name: "last_used_at",
        boolean: false,
    },
];

/// Fields accepted by `$orderby` for a plugin.
const PLUGIN_ORDERABLE_FIELDS: [&str; 4] = ["id", "name", "plugin_type", "last_used_at"];

impl ListFields for Plugin {
    fn filter_fields() -> &'static [FilterField] {
        &PLUGIN_FILTER_FIELDS
    }

    fn orderable_fields() -> &'static [&'static str] {
        &PLUGIN_ORDERABLE_FIELDS
    }

    fn text_value(&self, field: &str) -> String {
        match field {
            "name" => self.name.clone(),
            "plugin_type" | "type" => self.plugin_type.clone(),
            // A `null` timestamp has no text value; the empty string never
            // matches a literal, which is the answer a null carries.
            _ => String::new(),
        }
    }

    fn boolean_value(&self, _field: &str) -> bool {
        false
    }

    fn compare(&self, other: &Self, field: &str) -> std::cmp::Ordering {
        match field {
            "id" => self.id.cmp(&other.id),
            "name" => self.name.cmp(&other.name),
            "plugin_type" => self.plugin_type.cmp(&other.plugin_type),
            "last_used_at" => self.last_used_at.cmp(&other.last_used_at),
            _ => std::cmp::Ordering::Equal,
        }
    }
}

/// Serialize a page of routes, applying `$select` projection when present.
#[must_use]
pub fn routes_to_json(items: &[Arc<Route>], query: &ListQuery) -> Vec<Value> {
    let selected = query.selected_fields();
    items
        .iter()
        .map(|route| RouteDto::from(route.as_ref()))
        .map(|dto| toolkit::api::select::apply_select(dto, selected.as_deref()))
        .collect()
}

/// Serialize a page of plugins, applying `$select` projection when present.
#[must_use]
pub fn plugins_to_json(items: &[Arc<Plugin>], query: &ListQuery) -> Vec<Value> {
    let selected = query.selected_fields();
    items
        .iter()
        .map(|plugin| PluginDto::from(plugin.as_ref()))
        .map(|dto| toolkit::api::select::apply_select(dto, selected.as_deref()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::types::Scheme;

    fn upstream(alias: &str, host: &str, port: u16, created_at: u64) -> Upstream {
        Upstream {
            id: Uuid::new_v4(),
            tenant_id: Uuid::new_v4(),
            alias: alias.to_owned(),
            created_at,
            updated_at: created_at,
            spec: UpstreamSpec {
                server: ServerConfig {
                    endpoints: vec![Endpoint {
                        scheme: Scheme::Https,
                        host: host.to_owned(),
                        port,
                    }],
                },
                ..UpstreamSpec::default()
            },
        }
    }

    /// Parse `filter` and evaluate it against one upstream.
    fn matches_filter(upstream: &Upstream, filter: &str) -> bool {
        parse_filter::<Upstream>(filter)
            .unwrap_or_else(|err| panic!("'{filter}' must parse: {err}"))
            .iter()
            .all(|clause| clause.matches(upstream))
    }

    #[test]
    fn dto_matches_the_wire_schema_shape() {
        let upstream = upstream("api.openai.com", "api.openai.com", 443, 7);
        let json = serde_json::to_value(UpstreamDto::from(&upstream)).expect("serializes");

        let keys: Vec<&str> = json
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        for expected in [
            "id",
            "enabled",
            "alias",
            "tags",
            "server",
            "protocol",
            "auth",
            "headers",
            "plugins",
            "rate_limit",
            "cors",
        ] {
            assert!(keys.contains(&expected), "missing key {expected}: {keys:?}");
        }
        assert_eq!(json["alias"], "api.openai.com");
        assert_eq!(json["enabled"], true);
        assert_eq!(json["server"]["endpoints"][0]["host"], "api.openai.com");
        assert_eq!(json["server"]["endpoints"][0]["port"], 443);
        assert_eq!(
            json["protocol"],
            "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
        );
    }

    #[test]
    fn dto_gts_id_uses_the_upstream_base_type() {
        let upstream = upstream("api.openai.com", "api.openai.com", 443, 7);
        let dto = UpstreamDto::from(&upstream);

        assert!(dto.gts_id().starts_with("gts.cf.core.oagw.upstream.v1~"));
        assert_eq!(dto.endpoints().len(), 1);
    }

    #[test]
    fn page_defaults_to_50_and_caps_at_100() {
        let query = ListQuery::default();
        assert_eq!(
            query.page().expect("default page"),
            Page { skip: 0, top: 50 }
        );

        let query = ListQuery {
            top: Some(100),
            skip: Some(10),
            ..ListQuery::default()
        };
        assert_eq!(query.page().expect("max page"), Page { skip: 10, top: 100 });
    }

    #[test]
    fn page_rejects_zero_and_oversized_top() {
        for top in [0_usize, 101, 1000] {
            let query = ListQuery {
                top: Some(top),
                ..ListQuery::default()
            };
            let err = query.page().expect_err("out of range $top");
            assert_eq!(err.status().as_u16(), 400, "top: {top}");
        }
    }

    #[test]
    fn orderby_parses_field_and_direction() {
        let query = ListQuery {
            orderby: Some("created_at desc".to_owned()),
            ..ListQuery::default()
        };
        assert_eq!(query.orderby().expect("parses"), Some(("created_at", true)));

        let query = ListQuery {
            orderby: Some("alias".to_owned()),
            ..ListQuery::default()
        };
        assert_eq!(query.orderby().expect("parses"), Some(("alias", false)));

        let query = ListQuery {
            orderby: Some("  ".to_owned()),
            ..ListQuery::default()
        };
        assert_eq!(query.orderby().expect("blank is no ordering"), None);
    }

    #[test]
    fn orderby_rejects_malformed_expressions() {
        for raw in ["created_at sideways", "alias desc protocol", ""] {
            let query = ListQuery {
                orderby: Some(raw.to_owned()),
                ..ListQuery::default()
            };
            let parsed = query.orderby();
            assert!(
                parsed.is_err() || parsed.expect("parsed").is_none(),
                "unexpected ordering for '{raw}'"
            );
        }

        let query = ListQuery {
            orderby: Some("alias desc protocol".to_owned()),
            ..ListQuery::default()
        };
        let err = query.orderby().expect_err("multiple keys");
        assert_eq!(err.status().as_u16(), 400);
    }

    #[test]
    fn selected_fields_trims_and_ignores_empty_entries() {
        let query = ListQuery {
            select: Some(" id, alias , , server ".to_owned()),
            ..ListQuery::default()
        };
        assert_eq!(
            query.selected_fields(),
            Some(vec![
                "id".to_owned(),
                "alias".to_owned(),
                "server".to_owned()
            ])
        );

        let query = ListQuery {
            select: Some(" , ".to_owned()),
            ..ListQuery::default()
        };
        assert_eq!(query.selected_fields(), None);
    }

    #[test]
    fn projection_applies_select_to_every_item() {
        let items = vec![
            Arc::new(upstream("a.example.com", "a.example.com", 443, 1)),
            Arc::new(upstream("b.example.com", "b.example.com", 443, 2)),
        ];
        let query = ListQuery {
            select: Some("id,alias".to_owned()),
            ..ListQuery::default()
        };

        let projected = upstreams_to_json(&items, &query);
        assert_eq!(projected.len(), 2);
        for (item, upstream) in projected.iter().zip(&items) {
            let mut keys: Vec<String> = item
                .as_object()
                .expect("object")
                .keys()
                .map(Clone::clone)
                .collect();
            keys.sort();

            assert_eq!(keys, vec!["alias".to_owned(), "id".to_owned()]);
            assert_eq!(
                item.get("id").and_then(Value::as_str),
                Some(upstream.id.to_string().as_str())
            );
            assert_eq!(
                item.get("alias").and_then(Value::as_str),
                Some(upstream.alias.as_str())
            );
        }
    }

    #[test]
    fn projection_without_select_returns_full_items() {
        let items = vec![Arc::new(upstream("a.example.com", "a.example.com", 443, 1))];
        let json = upstreams_to_json(&items, &ListQuery::default());

        assert!(json[0].as_object().expect("object").contains_key("server"));
    }

    #[test]
    fn filter_supports_the_documented_simple_forms() {
        let alias = upstream("api.openai.com", "api.openai.com", 443, 1);
        let mut disabled = upstream("disabled.example.com", "disabled.example.com", 443, 2);
        disabled.spec.enabled = false;

        for expression in [
            "alias eq 'api.openai.com'",
            "name eq 'api.openai.com'",
            "alias ne 'disabled.example.com'",
            "contains(alias,'openai')",
            "contains(name,'openai')",
            "enabled eq true and contains(alias,'openai')",
        ] {
            assert!(
                matches_filter(&alias, expression),
                "'{expression}' matches the alias"
            );
            assert!(
                !matches_filter(&disabled, expression),
                "'{expression}' does not match the other upstream"
            );
        }

        assert!(matches_filter(&disabled, "enabled eq false"));
        assert!(matches_filter(&alias, "enabled ne false"));
        assert!(!matches_filter(
            &alias,
            "enabled eq true and enabled eq false"
        ));
    }

    #[test]
    fn filter_literals_may_contain_the_conjunction_keyword() {
        let quoted = upstream("a and b", "a and b", 443, 1);

        assert!(matches_filter(&quoted, "alias eq 'a and b'"));
        assert!(!matches_filter(&quoted, "alias eq 'b'"));
    }

    #[test]
    fn filter_rejects_unsupported_expressions_instead_of_ignoring_them() {
        for expression in [
            "alias gt 'api.openai.com'",
            "alias like 'api'",
            "startswith(alias,'api')",
            "tenant_id eq '00000000-0000-0000-0000-000000000001'",
            "eq 'api.openai.com'",
            "alias eq",
            "alias eq api.openai.com",
            "enabled eq 'maybe'",
            "contains(alias)",
            "contains(alias,'a','b')",
            "contains(alias,unquoted)",
            "contains(alias,'')",
            "alias eq 'a' and",
        ] {
            let query = ListQuery {
                filter: Some(expression.to_owned()),
                ..ListQuery::default()
            };
            let err = page_upstreams(&[Arc::new(upstream("a", "a", 443, 1))], &query)
                .expect_err(expression);

            assert_eq!(err.status().as_u16(), 400, "'{expression}' is a 400");
            assert_eq!(err.kind(), crate::error::OagwErrorKind::ValidationError);
            assert!(
                err.detail().contains(expression),
                "'{expression}' is named in the detail: {}",
                err.detail()
            );
        }
    }

    #[test]
    fn filter_rejections_name_the_unsupported_clause() {
        let query = ListQuery {
            filter: Some("alias eq 'a' and id gt 3".to_owned()),
            ..ListQuery::default()
        };

        let err = page_upstreams(&[Arc::new(upstream("a", "a", 443, 1))], &query)
            .expect_err("unsupported operator");
        assert!(err.detail().contains("id gt 3"), "{}", err.detail());
    }

    #[test]
    fn paging_never_deep_copies_the_records_it_pages() {
        let items: Vec<Arc<Upstream>> = (0..5)
            .map(|index| Arc::new(upstream(&format!("h{index}.example.com"), "", 443, index)))
            .collect();
        let query = ListQuery {
            top: Some(1),
            skip: Some(4),
            ..ListQuery::default()
        };

        let page = page_upstreams(&items, &query).expect("page");
        assert_eq!(page.len(), 1);
        assert!(Arc::ptr_eq(&page[0], &items[4]), "handles are shared");
    }
}
