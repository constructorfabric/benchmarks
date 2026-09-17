//! Wire documents of the OAGW management REST API.
//!
//! Every document the API exchanges is declared here with
//! `#[toolkit_macros::api_dto(…)]`, exactly as the `types-registry` gear does:
//! `request` marks a body the handlers deserialize, `response` marks a body
//! they serialize, and both register a named `OpenAPI` component.
//!
//! The nested configuration members (`server`, `plugins`, `rate_limit`, …) are
//! the domain types themselves, so a submission is validated once, by the
//! domain layer, and no field can drift between the wire document and the
//! model.
//!
//! ## Wire identifiers
//!
//! Resources are addressed by anonymous GTS identifiers
//! (`gts.cf.core.oagw.upstream.v1~{uuid}`, DESIGN.md §3.3). Path and body
//! members accept the bare UUID form as well, so a client that only kept the
//! generated id is still answered.
//!
//! ## List parameters
//!
//! [`ListQuery`] binds the `OData` system query options every list endpoint
//! accepts (`$filter`, `$select`, `$orderby`, `$top`, `$skip`) and resolves
//! them into a [`ResolvedList`]. It is its own extractor so a malformed
//! parameter is reported as an RFC 9457 problem instead of axum's plain-text
//! rejection.

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, FromRequestParts, Query, Request};
use axum::http::request::Parts;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use std::cmp::Ordering;
use toolkit::api::select::apply_select;
use uuid::Uuid;

use super::{PluginKind, StoredPlugin};
use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, MatchRule, MatchSpec, PluginsConfig, Protocol,
    ROUTE_BASE_TYPE, RateLimitConfig, Route, RouteSpec, ServerConfig, UPSTREAM_BASE_TYPE, Upstream,
    UpstreamSpec,
};
use crate::error::GatewayError;

/// Page size used when `$top` is absent (DESIGN.md §3.3 "List Query
/// Parameters").
pub const DEFAULT_TOP: usize = 50;

/// Largest page size the list endpoints honour; larger values are capped at it.
pub const MAX_TOP: usize = 100;

/// Fields the upstream list endpoint accepts in `$filter`, `$orderby` and
/// `$select`.
pub const UPSTREAM_LIST_FIELDS: &[&str] = &[
    "id",
    "tenant_id",
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
];

/// Fields the route list endpoint accepts in `$filter`, `$orderby` and
/// `$select`.
pub const ROUTE_LIST_FIELDS: &[&str] = &[
    "id",
    "tenant_id",
    "upstream_id",
    "match",
    "enabled",
    "priority",
    "tags",
    "plugins",
    "rate_limit",
    "cors",
];

/// Fields the plugin list endpoint accepts in `$filter`, `$orderby` and
/// `$select`.
pub const PLUGIN_LIST_FIELDS: &[&str] = &[
    "id",
    "tenant_id",
    "name",
    "description",
    "plugin_type",
    "phases",
    "config_schema",
    "source_code",
];

// ---------------------------------------------------------------------------
// Wire identifiers
// ---------------------------------------------------------------------------

/// The anonymous GTS identifier of a stored upstream.
#[must_use]
pub fn gts_upstream_id(id: Uuid) -> String {
    format!("{UPSTREAM_BASE_TYPE}~{id}")
}

/// The anonymous GTS identifier of a stored route.
#[must_use]
pub fn gts_route_id(id: Uuid) -> String {
    format!("{ROUTE_BASE_TYPE}~{id}")
}

/// The anonymous GTS identifier of a stored plugin (ADR 0002, "Plugin Types").
#[must_use]
pub fn gts_plugin_id(kind: PluginKind, id: Uuid) -> String {
    format!("{}~{id}", kind.gts_base_type())
}

/// Parses a resource identifier from a path segment or a body member.
///
/// Both the anonymous GTS identifier (`gts.cf.core.oagw.route.v1~{uuid}`) and
/// the bare UUID are accepted.
///
/// # Errors
///
/// Returns a 400 [`GatewayError`] when `raw` is neither form.
pub fn parse_resource_id(raw: &str, label: &str) -> Result<Uuid, GatewayError> {
    let trimmed = raw.trim();
    if let Ok(id) = Uuid::parse_str(trimmed) {
        return Ok(id);
    }
    if let Some((_, instance)) = trimmed.split_once('~')
        && let Ok(id) = Uuid::parse_str(instance)
    {
        return Ok(id);
    }

    Err(GatewayError::validation(
        format!("`{raw}` is not a valid {label} identifier: expected a UUID or a GTS identifier"),
        "id",
    ))
}

// ---------------------------------------------------------------------------
// Upstreams
// ---------------------------------------------------------------------------

/// Body of `POST /oagw/v1/upstreams` and `PUT /oagw/v1/upstreams/{id}`.
///
/// This is the upstream schema without `id` and `tenant_id`: both members are
/// owned by the gateway, so a submission that sets them is rejected instead of
/// being silently overwritten (DESIGN.md §3.3 "Immutable fields"). Because
/// `PUT` is a full replacement, every optional member that is absent is
/// cleared back to its default.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct UpstreamRequest {
    /// Server-generated identifier. Setting it is rejected with a `400`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Owning tenant. Always taken from the authenticated caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<Uuid>,

    /// Routing alias. Derived from the endpoints when omitted, and required
    /// when the pool cannot be derived (IP literals, no common registrable
    /// domain). Immutable: an update that would change it is a `400`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,

    /// Whether the upstream accepts traffic. Defaults to `true`.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Tenant-local tags. Cleared when omitted.
    #[serde(default)]
    pub tags: Vec<String>,

    /// Endpoint pool. Required.
    #[schema(value_type = Object)]
    pub server: ServerConfig,

    /// Upstream protocol. Required.
    #[schema(value_type = String)]
    pub protocol: Protocol,

    /// Authentication plugin binding. Cleared when omitted.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub auth: Option<AuthConfig>,

    /// Header transformation rules. Cleared when omitted.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub headers: Option<HeadersConfig>,

    /// Plugin chain. Defaults to an empty private chain.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub plugins: PluginsConfig,

    /// Rate limiting. Cleared when omitted.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub rate_limit: Option<RateLimitConfig>,

    /// CORS configuration. Cleared when omitted.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub cors: Option<CorsConfig>,
}

impl UpstreamRequest {
    /// The stored alias a replacement keeps when the submission omits it.
    ///
    /// The alias is the routing key, not an ordinary optional field, so a
    /// replacement that does not mention it keeps the stored value; any
    /// attempt to change it is still rejected by the alias rules.
    #[must_use]
    pub fn resolved_alias(&self, existing: &str) -> Option<String> {
        self.alias.clone().or_else(|| Some(existing.to_owned()))
    }

    /// The document as the domain layer validates it.
    #[must_use]
    pub fn to_spec(&self, alias: Option<String>) -> UpstreamSpec {
        UpstreamSpec {
            alias,
            enabled: self.enabled,
            tags: self.tags.clone(),
            server: self.server.clone(),
            protocol: self.protocol,
            auth: self.auth.clone(),
            headers: self.headers.clone(),
            plugins: self.plugins.clone(),
            rate_limit: self.rate_limit,
            cors: self.cors.clone(),
        }
    }
}

/// A stored upstream as the API returns it.
#[toolkit_macros::api_dto(response)]
pub struct UpstreamDto {
    /// Anonymous GTS identifier `gts.cf.core.oagw.upstream.v1~{uuid}`.
    pub id: String,

    /// Owning tenant.
    pub tenant_id: Uuid,

    /// Routing alias.
    pub alias: String,

    /// Whether the upstream accepts traffic.
    pub enabled: bool,

    /// Tenant-local tags.
    pub tags: Vec<String>,

    /// Endpoint pool.
    #[schema(value_type = Object)]
    pub server: ServerConfig,

    /// Upstream protocol.
    #[schema(value_type = String)]
    pub protocol: Protocol,

    /// Authentication plugin binding.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub auth: Option<AuthConfig>,

    /// Header transformation rules.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub headers: Option<HeadersConfig>,

    /// Plugin chain.
    #[schema(value_type = Object)]
    pub plugins: PluginsConfig,

    /// Rate limiting.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub rate_limit: Option<RateLimitConfig>,

    /// CORS configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub cors: Option<CorsConfig>,
}

impl From<&Upstream> for UpstreamDto {
    fn from(upstream: &Upstream) -> Self {
        let config = &upstream.config;
        Self {
            id: gts_upstream_id(upstream.id),
            tenant_id: upstream.tenant_id,
            alias: config.alias.clone(),
            enabled: config.enabled,
            tags: config.tags.clone(),
            server: config.server.clone(),
            protocol: config.protocol,
            auth: config.auth.clone(),
            headers: config.headers.clone(),
            plugins: config.plugins.clone(),
            rate_limit: config.rate_limit,
            cors: config.cors.clone(),
        }
    }
}

impl From<Upstream> for UpstreamDto {
    fn from(upstream: Upstream) -> Self {
        Self::from(&upstream)
    }
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// Body of `POST /oagw/v1/routes` and `PUT /oagw/v1/routes/{id}`.
///
/// The route schema declares no `additionalProperties: false` at its root, so
/// unknown members are tolerated here exactly as they are by the domain model.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
pub struct RouteRequest {
    /// Server-generated identifier. Setting it is rejected with a `400`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Owning tenant. Always taken from the authenticated caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<Uuid>,

    /// Upstream this route belongs to. Required on `POST`; immutable on `PUT`,
    /// where a different value is rejected instead of overwritten.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_id: Option<String>,

    /// Match rules. Required: exactly one of `http` / `grpc`.
    #[serde(rename = "match", default)]
    #[schema(value_type = Object)]
    pub match_spec: MatchSpec,

    /// Whether the route accepts traffic. Defaults to `true`.
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// Match priority. Defaults to `0`.
    #[serde(default)]
    pub priority: u32,

    /// Tenant-local tags. Cleared when omitted.
    #[serde(default)]
    pub tags: Vec<String>,

    /// Plugin chain. Defaults to an empty private chain.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub plugins: PluginsConfig,

    /// Route-level rate limit. Cleared when omitted.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub rate_limit: Option<RateLimitConfig>,

    /// Route-level CORS configuration. Cleared when omitted.
    #[serde(default)]
    #[schema(value_type = Object)]
    pub cors: Option<CorsConfig>,
}

impl RouteRequest {
    /// The document as the domain layer validates it.
    #[must_use]
    pub fn to_spec(&self, upstream_id: Uuid) -> RouteSpec {
        RouteSpec {
            upstream_id,
            match_spec: self.match_spec.clone(),
            enabled: self.enabled,
            priority: self.priority,
            tags: self.tags.clone(),
            plugins: self.plugins.clone(),
            rate_limit: self.rate_limit,
            cors: self.cors.clone(),
        }
    }
}

/// A stored route as the API returns it.
#[toolkit_macros::api_dto(response)]
pub struct RouteDto {
    /// Anonymous GTS identifier `gts.cf.core.oagw.route.v1~{uuid}`.
    pub id: String,

    /// Owning tenant.
    pub tenant_id: Uuid,

    /// Owning upstream, as its anonymous GTS identifier.
    pub upstream_id: String,

    /// Resolved match rule (exactly one of `http` / `grpc`).
    #[serde(rename = "match")]
    #[schema(value_type = Object)]
    pub match_rule: MatchRule,

    /// Whether the route accepts traffic.
    pub enabled: bool,

    /// Match priority.
    pub priority: u32,

    /// Tenant-local tags.
    pub tags: Vec<String>,

    /// Plugin chain.
    #[schema(value_type = Object)]
    pub plugins: PluginsConfig,

    /// Route-level rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub rate_limit: Option<RateLimitConfig>,

    /// Route-level CORS configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub cors: Option<CorsConfig>,
}

impl From<&Route> for RouteDto {
    fn from(route: &Route) -> Self {
        let config = &route.config;
        Self {
            id: gts_route_id(route.id),
            tenant_id: route.tenant_id,
            upstream_id: gts_upstream_id(config.upstream_id),
            match_rule: config.match_rule.clone(),
            enabled: config.enabled,
            priority: config.priority,
            tags: config.tags.clone(),
            plugins: config.plugins.clone(),
            rate_limit: config.rate_limit,
            cors: config.cors.clone(),
        }
    }
}

impl From<Route> for RouteDto {
    fn from(route: Route) -> Self {
        Self::from(&route)
    }
}

// ---------------------------------------------------------------------------
// Plugins
// ---------------------------------------------------------------------------

/// Body of `POST /oagw/v1/plugins` (ADR 0002, Appendix A "Definition").
///
/// Plugins are immutable, so this is also the only body the resource accepts:
/// an update is performed by creating a new plugin and re-binding references.
#[derive(Debug, Clone)]
#[toolkit_macros::api_dto(request)]
#[serde(deny_unknown_fields)]
pub struct PluginRequest {
    /// Server-generated identifier. Setting it is rejected with a `400`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Owning tenant. Always taken from the authenticated caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<Uuid>,

    /// Human-readable plugin name. Required.
    pub name: String,

    /// Optional description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Plugin family: `auth`, `guard` or `transform` (ADR 0002).
    #[serde(rename = "plugin_type")]
    #[schema(value_type = String)]
    pub plugin_type: PluginKind,

    /// Lifecycle phases the plugin implements. Defaults to the phases of the
    /// plugin family.
    #[serde(default)]
    pub phases: Vec<String>,

    /// JSON schema of the plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub config_schema: Option<Value>,

    /// Starlark source of the plugin. Required.
    pub source_code: String,
}

/// A stored plugin as the API returns it.
#[toolkit_macros::api_dto(response)]
pub struct PluginDto {
    /// Anonymous GTS identifier `gts.cf.core.oagw.<kind>_plugin.v1~{uuid}`.
    pub id: String,

    /// Owning tenant.
    pub tenant_id: Uuid,

    /// Human-readable plugin name.
    pub name: String,

    /// Optional description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Plugin family: `auth`, `guard` or `transform`.
    #[serde(rename = "plugin_type")]
    #[schema(value_type = String)]
    pub plugin_type: String,

    /// Lifecycle phases the plugin implements.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<String>,

    /// JSON schema of the plugin configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub config_schema: Option<Value>,

    /// Starlark source of the plugin.
    pub source_code: String,
}

impl From<StoredPluginView<'_>> for PluginDto {
    fn from(plugin: StoredPluginView<'_>) -> Self {
        Self {
            id: plugin.record.gts_id(),
            tenant_id: plugin.record.tenant_id,
            name: plugin.record.name.clone(),
            description: plugin.record.description.clone(),
            plugin_type: plugin.record.kind.as_str().to_owned(),
            phases: plugin.record.phases.clone(),
            config_schema: plugin.record.config_schema.clone(),
            source_code: plugin.record.source_code.clone(),
        }
    }
}

/// Borrowed plugin record, so a DTO can be built without cloning the store's
/// entry first.
#[derive(Debug, Clone, Copy)]
pub struct StoredPluginView<'a> {
    /// The stored record.
    pub record: &'a StoredPlugin,
}

impl<'a> From<&'a StoredPlugin> for StoredPluginView<'a> {
    fn from(record: &'a StoredPlugin) -> Self {
        Self { record }
    }
}

/// Body of `GET /oagw/v1/plugins/{id}/source`: the Starlark source of a
/// plugin, served as JSON so the response stays machine-readable.
#[toolkit_macros::api_dto(response)]
pub struct PluginSourceDto {
    /// Anonymous GTS identifier of the plugin.
    pub plugin_id: String,

    /// Plugin name.
    pub name: String,

    /// Plugin family: `auth`, `guard` or `transform`.
    #[serde(rename = "plugin_type")]
    pub plugin_type: String,

    /// Starlark source of the plugin.
    pub source_code: String,
}

impl<'a> From<StoredPluginView<'a>> for PluginSourceDto {
    fn from(plugin: StoredPluginView<'a>) -> Self {
        Self {
            plugin_id: plugin.record.gts_id(),
            name: plugin.record.name.clone(),
            plugin_type: plugin.record.kind.as_str().to_owned(),
            source_code: plugin.record.source_code.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// List parameters
// ---------------------------------------------------------------------------

/// The `OData` system query options every list endpoint accepts (DESIGN.md §3.3
/// "List Query Parameters").
///
/// All members are optional: `$top` defaults to [`DEFAULT_TOP`] and is capped
/// at [`MAX_TOP`], `$skip` defaults to `0`, and an absent `$filter` accepts
/// every resource of the calling tenant.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListQuery {
    /// `$filter` — equality (or inequality) clauses joined by `and`.
    #[serde(rename = "$filter", default)]
    pub filter: Option<String>,

    /// `$select` — comma-separated projection of the returned fields.
    #[serde(rename = "$select", default)]
    pub select: Option<String>,

    /// `$orderby` — comma-separated sort keys, each optionally `asc`/`desc`.
    #[serde(rename = "$orderby", default)]
    pub orderby: Option<String>,

    /// `$top` — page size, default 50, capped at 100.
    #[serde(rename = "$top", default)]
    pub top: Option<String>,

    /// `$skip` — page offset.
    #[serde(rename = "$skip", default)]
    pub skip: Option<String>,
}

impl ListQuery {
    /// Resolves the parameters against the fields `allowed` exposes.
    ///
    /// # Errors
    ///
    /// Returns a 400 [`GatewayError`] when a parameter cannot be parsed or
    /// names a field the resource does not expose.
    pub fn resolve(&self, allowed: &[&str]) -> Result<ResolvedList, GatewayError> {
        let filter = match self.filter.as_deref() {
            Some(raw) => parse_filter(raw, allowed)?,
            None => Vec::new(),
        };
        let select = match self.select.as_deref() {
            Some(raw) => Some(parse_select_fields(raw, allowed)?),
            None => None,
        };
        let order = match self.orderby.as_deref() {
            Some(raw) => parse_orderby(raw, allowed)?,
            None => Vec::new(),
        };
        let top = match self.top.as_deref() {
            Some(raw) => parse_page_size(raw, "$top")?,
            None => DEFAULT_TOP,
        };
        let skip = match self.skip.as_deref() {
            Some(raw) => parse_page_size(raw, "$skip")?,
            None => 0,
        };

        Ok(ResolvedList {
            filter,
            select,
            order,
            top: top.min(MAX_TOP),
            skip,
        })
    }
}

impl<S> FromRequestParts<S> for ListQuery
where
    S: Send + Sync,
{
    type Rejection = GatewayError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        Query::<Self>::from_request_parts(parts, state)
            .await
            .map(|Query(parameters)| parameters)
            .map_err(|error| {
                GatewayError::validation(
                    format!("failed to parse the list query parameters: {error}"),
                    "$query",
                )
            })
    }
}

/// Comparison operator of a `$filter` clause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterOp {
    /// `field eq value`.
    Eq,
    /// `field ne value`.
    Ne,
}

/// The right-hand side of a `$filter` clause.
///
/// Equality is canonical: `'true'` and `true` compare equal, and so do a UUID
/// and its string form, because clients quote values inconsistently.
#[derive(Debug, Clone, Eq)]
pub enum FilterValue {
    /// A quoted string.
    Text(String),
    /// A boolean literal.
    Flag(bool),
    /// A bare number.
    Number(u64),
    /// A UUID (a resource or upstream identifier).
    Id(Uuid),
}

impl FilterValue {
    /// Builds a text value.
    #[must_use]
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }

    /// Builds a boolean value.
    #[must_use]
    pub const fn flag(value: bool) -> Self {
        Self::Flag(value)
    }

    /// Builds a numeric value.
    #[must_use]
    pub const fn number(value: u64) -> Self {
        Self::Number(value)
    }

    /// Builds an identifier value.
    #[must_use]
    pub const fn id(value: Uuid) -> Self {
        Self::Id(value)
    }

    /// Canonical spelling used for equality.
    fn canonical(&self) -> String {
        match self {
            Self::Text(value) => value.clone(),
            Self::Flag(value) => value.to_string(),
            Self::Number(value) => value.to_string(),
            Self::Id(value) => value.to_string(),
        }
    }

    /// Typed ordering used by `$orderby`: text first, then flags, numbers and
    /// identifiers, so a column of mixed values still has a stable order.
    fn compare(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Text(left), Self::Text(right)) => left.cmp(right),
            (Self::Flag(left), Self::Flag(right)) => left.cmp(right),
            (Self::Number(left), Self::Number(right)) => left.cmp(right),
            (Self::Id(left), Self::Id(right)) => left.as_u128().cmp(&right.as_u128()),
            _ => self.rank().cmp(&other.rank()),
        }
    }

    /// Rank of this value kind in the mixed-column ordering.
    fn rank(&self) -> u8 {
        match self {
            Self::Text(_) => 0,
            Self::Flag(_) => 1,
            Self::Number(_) => 2,
            Self::Id(_) => 3,
        }
    }
}

impl PartialEq for FilterValue {
    fn eq(&self, other: &Self) -> bool {
        self.canonical() == other.canonical()
    }
}

/// One `$filter` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterClause {
    /// Field the clause applies to.
    pub field: String,

    /// Comparison operator.
    pub operator: FilterOp,

    /// Value the field is compared against.
    pub value: FilterValue,
}

/// One `$orderby` key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderClause {
    /// Field the list is sorted by.
    pub field: String,

    /// Whether the sort is descending.
    pub descending: bool,
}

/// The list parameters of one request, resolved and validated.
#[derive(Debug, Clone, Default)]
pub struct ResolvedList {
    /// `$filter` clauses, all of which must hold.
    pub filter: Vec<FilterClause>,

    /// `$select` projection, `None` when the whole document is returned.
    pub select: Option<Vec<String>>,

    /// `$orderby` keys, in order of precedence.
    pub order: Vec<OrderClause>,

    /// Page size (`$top`, default 50, capped at 100).
    pub top: usize,

    /// Page offset (`$skip`).
    pub skip: usize,
}

impl ResolvedList {
    /// Whether an item passes `$filter`.
    ///
    /// `lookup` maps a field name to every value the item carries for it (a
    /// multi-valued field such as `tags` yields one value per tag); an empty
    /// list means the item does not expose the field, so it can never be
    /// selected by `eq`.
    #[must_use]
    pub fn matches<F>(&self, lookup: F) -> bool
    where
        F: Fn(&str) -> Vec<FilterValue>,
    {
        self.filter.iter().all(|clause| {
            let contained = lookup(&clause.field).contains(&clause.value);

            match clause.operator {
                FilterOp::Eq => contained,
                FilterOp::Ne => !contained,
            }
        })
    }

    /// Sorts the items by `$orderby`.
    ///
    /// `lookup` maps an item and a field name to the values to compare; the
    /// first one is used, and items that do not expose the field keep their
    /// relative order.
    pub fn sort_items<T, F>(&self, items: &mut [T], lookup: F)
    where
        F: Fn(&T, &str) -> Vec<FilterValue> + Copy,
    {
        if self.order.is_empty() {
            return;
        }

        items.sort_by(|left, right| {
            for clause in &self.order {
                let left_values = lookup(left, &clause.field);
                let right_values = lookup(right, &clause.field);
                let (Some(left_value), Some(right_value)) =
                    (left_values.first(), right_values.first())
                else {
                    continue;
                };

                let ordering = left_value.compare(right_value);
                let ordering = if clause.descending {
                    ordering.reverse()
                } else {
                    ordering
                };
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }

            Ordering::Equal
        });
    }

    /// Applies `$skip` and `$top`, returning the page and the number of items
    /// that matched.
    #[must_use]
    pub fn page<T>(&self, items: Vec<T>) -> (Vec<T>, usize) {
        let total = items.len();
        let page: Vec<T> = items.into_iter().skip(self.skip).take(self.top).collect();

        (page, total)
    }
}

/// Serializes a list page, applying `$select` to every item.
///
/// The `OpenAPI` document declares the typed response, so the projection is
/// applied to the serialized items with `toolkit::api::select::apply_select`.
#[must_use]
pub fn list_page<T>(items: Vec<T>, total: usize, key: &str, select: Option<&[String]>) -> Value
where
    T: serde::Serialize,
{
    let count = items.len();
    let rendered: Vec<Value> = items
        .into_iter()
        .map(|item| apply_select(item, select))
        .collect();

    let mut page = Map::with_capacity(3);
    page.insert(key.to_owned(), Value::Array(rendered));
    page.insert("count".to_owned(), Value::from(count));
    page.insert("total".to_owned(), Value::from(total));

    Value::Object(page)
}

/// JSON body extractor that renders every rejection as an RFC 9457 problem.
///
/// axum's own `Json` rejection is a plain-text `400`, which would break the
/// `application/problem+json` contract the gear answers with, so the body is
/// bound through [`GatewayError`] instead.
#[derive(Debug)]
pub struct JsonDocument<T>(pub T);

impl<S, T> FromRequest<S> for JsonDocument<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = GatewayError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        let Json(document) = Json::<T>::from_request(request, state)
            .await
            .map_err(|error| body_problem(&error))?;

        Ok(Self(document))
    }
}

/// A `400` problem for a request body that could not be bound.
fn body_problem(error: &JsonRejection) -> GatewayError {
    GatewayError::validation(
        format!("failed to parse the submitted document: {error}"),
        "body",
    )
}

/// A `400` problem for a query option that could not be honoured.
fn invalid_query(name: &str, detail: impl Into<String>) -> GatewayError {
    GatewayError::validation(detail, name)
}

/// Parses `$filter` into clauses joined by `and`.
fn parse_filter(raw: &str, allowed: &[&str]) -> Result<Vec<FilterClause>, GatewayError> {
    let mut clauses = Vec::new();
    for term in raw.split(" and ") {
        let term = term.trim();
        if term.is_empty() {
            return Err(invalid_query(
                "$filter",
                "`$filter` contains an empty expression",
            ));
        }

        clauses.push(parse_clause(term, allowed)?);
    }

    Ok(clauses)
}

/// Splits one filter expression into its field, operator and value.
fn split_clause(term: &str) -> Option<(&str, FilterOp, &str)> {
    for (token, operator) in [(" eq ", FilterOp::Eq), (" ne ", FilterOp::Ne)] {
        if let Some((field, value)) = term.split_once(token) {
            return Some((field, operator, value));
        }
    }

    None
}

/// Parses one `$filter` expression.
fn parse_clause(term: &str, allowed: &[&str]) -> Result<FilterClause, GatewayError> {
    let Some((field, operator, value)) = split_clause(term) else {
        return Err(invalid_query(
            "$filter",
            format!(
                "`{term}` is not a supported filter expression; expected `field eq value` or \
                 `field ne value`"
            ),
        ));
    };

    let field = field.trim().to_ascii_lowercase();
    if !allowed.contains(&field.as_str()) {
        return Err(invalid_query(
            "$filter",
            format!("`{field}` is not a filterable field; expected one of {allowed:?}"),
        ));
    }

    Ok(FilterClause {
        field,
        operator,
        value: parse_value(value)?,
    })
}

/// Parses the right-hand side of a `$filter` expression.
fn parse_value(raw: &str) -> Result<FilterValue, GatewayError> {
    let trimmed = raw.trim();
    if trimmed.len() >= 2 && trimmed.starts_with('\'') && trimmed.ends_with('\'') {
        return Ok(FilterValue::text(&trimmed[1..trimmed.len() - 1]));
    }
    if trimmed.eq_ignore_ascii_case("true") {
        return Ok(FilterValue::flag(true));
    }
    if trimmed.eq_ignore_ascii_case("false") {
        return Ok(FilterValue::flag(false));
    }
    if let Ok(number) = trimmed.parse::<u64>() {
        return Ok(FilterValue::number(number));
    }
    if let Ok(instance) = parse_resource_id(trimmed, "value") {
        return Ok(FilterValue::id(instance));
    }

    Err(invalid_query(
        "$filter",
        format!(
            "`{trimmed}` is not a supported filter value; use a quoted string, `true`, `false`, \
             a number or a UUID"
        ),
    ))
}

/// Parses `$select` into a de-duplicated, lower-cased field list.
fn parse_select_fields(raw: &str, allowed: &[&str]) -> Result<Vec<String>, GatewayError> {
    let mut fields: Vec<String> = Vec::new();
    for name in raw.split(',') {
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() {
            return Err(invalid_query(
                "$select",
                "`$select` contains an empty field name",
            ));
        }
        if !is_selectable(&name, allowed) {
            return Err(invalid_query(
                "$select",
                format!("`{name}` is not a selectable field; expected one of {allowed:?}"),
            ));
        }
        if !fields.contains(&name) {
            fields.push(name);
        }
    }

    Ok(fields)
}

/// Whether `name` addresses a field, or a member nested under one.
fn is_selectable(name: &str, allowed: &[&str]) -> bool {
    allowed.contains(&name)
        || allowed
            .iter()
            .any(|field| name.starts_with(&format!("{field}.")))
}

/// Parses `$orderby` into its sort keys.
fn parse_orderby(raw: &str, allowed: &[&str]) -> Result<Vec<OrderClause>, GatewayError> {
    let mut clauses = Vec::new();
    for term in raw.split(',') {
        let term = term.trim();
        if term.is_empty() {
            return Err(invalid_query(
                "$orderby",
                "`$orderby` contains an empty field name",
            ));
        }

        let (field, direction) = match term.split_once(' ') {
            Some((field, direction)) => (field.trim(), direction.trim()),
            None => (term, ""),
        };
        let field = field.to_ascii_lowercase();
        if !allowed.contains(&field.as_str()) {
            return Err(invalid_query(
                "$orderby",
                format!("`{field}` is not a sortable field; expected one of {allowed:?}"),
            ));
        }

        let descending = match direction.to_ascii_lowercase().as_str() {
            "" | "asc" => false,
            "desc" => true,
            _ => {
                return Err(invalid_query(
                    "$orderby",
                    format!("`{direction}` is not a supported sort direction; use `asc` or `desc`"),
                ));
            }
        };
        clauses.push(OrderClause { field, descending });
    }

    Ok(clauses)
}

/// Parses a page size or offset.
fn parse_page_size(raw: &str, name: &str) -> Result<usize, GatewayError> {
    raw.trim().parse::<usize>().map_err(|_| {
        invalid_query(
            name,
            format!("`{name}` must be a non-negative integer, not `{raw}`"),
        )
    })
}

/// The `enabled` default of the upstream and route schemas.
fn default_enabled() -> bool {
    true
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::domain::model::PROTOCOL_HTTP;
    use crate::domain::{validate_route, validate_upstream};
    use serde_json::json;

    const TENANT: Uuid = Uuid::from_u128(0x0D70);

    fn request<T: DeserializeOwned>(document: Value) -> T {
        serde_json::from_value(document).unwrap()
    }

    fn upstream(alias: &str) -> Upstream {
        let body: UpstreamRequest = request(json!({
            "alias": alias,
            "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com", "port": 443 }] },
            "protocol": PROTOCOL_HTTP,
            "tags": ["llm"],
        }));

        Upstream::new(
            Uuid::new_v4(),
            TENANT,
            validate_upstream(&body.to_spec(None)).unwrap(),
        )
    }

    fn route(upstream_id: Uuid, path: &str, priority: u32) -> Route {
        let body: RouteRequest = request(json!({
            "match": { "http": { "methods": ["POST"], "path": path } },
            "priority": priority,
            "tags": ["chat"],
        }));

        Route::new(
            Uuid::new_v4(),
            TENANT,
            validate_route(&body.to_spec(upstream_id)).unwrap(),
        )
    }

    #[test]
    fn test_wire_identifiers_use_the_gts_base_types() {
        let id = Uuid::from_u128(0x1D70);

        assert_eq!(
            gts_upstream_id(id),
            format!("gts.cf.core.oagw.upstream.v1~{id}")
        );
        assert_eq!(gts_route_id(id), format!("gts.cf.core.oagw.route.v1~{id}"));
        for kind in [PluginKind::Auth, PluginKind::Guard, PluginKind::Transform] {
            assert_eq!(
                gts_plugin_id(kind, id),
                format!("{}~{id}", kind.gts_base_type())
            );
        }
    }

    #[test]
    fn test_parse_resource_id_accepts_both_spellings() {
        let id = Uuid::new_v4();

        assert_eq!(parse_resource_id(&id.to_string(), "upstream").unwrap(), id);
        assert_eq!(
            parse_resource_id(&gts_upstream_id(id), "upstream").unwrap(),
            id
        );
        assert_eq!(
            parse_resource_id(&format!("  {} ", gts_route_id(id)), "route").unwrap(),
            id
        );

        let error = parse_resource_id("not-an-identifier", "upstream").unwrap_err();
        assert_eq!(error.status(), 400);
        assert_eq!(error.extensions().extra["field"], "id");
        assert!(parse_resource_id("gts.cf.core.oagw.upstream.v1~nope", "upstream").is_err());
    }

    #[test]
    fn test_list_query_defaults_to_fifty_items_without_projection() {
        let resolved = ListQuery::default().resolve(UPSTREAM_LIST_FIELDS).unwrap();

        assert!(resolved.filter.is_empty());
        assert!(resolved.order.is_empty());
        assert_eq!(resolved.select, None);
        assert_eq!(resolved.top, DEFAULT_TOP);
        assert_eq!(resolved.skip, 0);
    }

    #[test]
    fn test_list_query_caps_the_page_size() {
        let query: ListQuery = request(json!({ "$top": "500", "$skip": "7" }));

        let resolved = query.resolve(ROUTE_LIST_FIELDS).unwrap();

        assert_eq!(resolved.top, MAX_TOP);
        assert_eq!(resolved.skip, 7);
    }

    #[test]
    fn test_list_query_parses_filter_select_and_order() {
        let query: ListQuery = request(json!({
            "$filter": "enabled eq true and tags ne 'missing'",
            "$select": "id,priority",
            "$orderby": "priority desc, id asc",
        }));

        let resolved = query.resolve(ROUTE_LIST_FIELDS).unwrap();

        assert_eq!(resolved.filter.len(), 2);
        assert_eq!(resolved.filter[0].field, "enabled");
        assert_eq!(resolved.filter[0].operator, FilterOp::Eq);
        assert_eq!(resolved.filter[0].value, FilterValue::flag(true));
        assert_eq!(resolved.filter[1].operator, FilterOp::Ne);
        assert_eq!(
            resolved.select,
            Some(vec!["id".to_owned(), "priority".to_owned()])
        );
        assert_eq!(
            resolved.order,
            vec![
                OrderClause {
                    field: "priority".to_owned(),
                    descending: true
                },
                OrderClause {
                    field: "id".to_owned(),
                    descending: false
                },
            ]
        );
    }

    #[test]
    fn test_list_query_rejects_values_it_cannot_honour() {
        for (query, field) in [
            (json!({ "$filter": "secret eq 'x'" }), "$filter"),
            (json!({ "$filter": "alias like 'x'" }), "$filter"),
            (json!({ "$filter": "alias eq 'unterminated" }), "$filter"),
            (json!({ "$filter": "enabled eq maybe" }), "$filter"),
            (json!({ "$filter": "and" }), "$filter"),
            (json!({ "$select": "secret" }), "$select"),
            (json!({ "$orderby": "alias sideways" }), "$orderby"),
            (json!({ "$orderby": "secret" }), "$orderby"),
            (json!({ "$top": "lots" }), "$top"),
            (json!({ "$skip": "-1" }), "$skip"),
        ] {
            let query: ListQuery = request(query);
            let error = query.resolve(UPSTREAM_LIST_FIELDS).unwrap_err();

            assert_eq!(error.status(), 400, "{error}");
            assert_eq!(error.extensions().extra["field"], field, "{error}");
        }
    }

    #[test]
    fn test_filter_values_compare_across_their_spellings() {
        let id = Uuid::new_v4();
        let wire = gts_upstream_id(id);

        assert_eq!(FilterValue::text("openai"), FilterValue::text("openai"));
        assert_eq!(FilterValue::id(id), FilterValue::text(id.to_string()));
        assert_ne!(FilterValue::id(id), FilterValue::text(wire));
        assert_eq!(FilterValue::flag(true), FilterValue::text("true"));
        assert_eq!(FilterValue::number(2), FilterValue::text("2"));
    }

    #[test]
    fn test_matches_applies_every_clause() {
        let resolved = ResolvedList {
            filter: vec![
                FilterClause {
                    field: "tags".to_owned(),
                    operator: FilterOp::Eq,
                    value: FilterValue::text("llm"),
                },
                FilterClause {
                    field: "enabled".to_owned(),
                    operator: FilterOp::Ne,
                    value: FilterValue::flag(false),
                },
            ],
            ..ResolvedList::default()
        };

        let lookup = |field: &str| match field {
            "tags" => vec![FilterValue::text("llm")],
            "enabled" => vec![FilterValue::flag(true)],
            _ => Vec::new(),
        };

        assert!(resolved.matches(lookup));
        assert!(!resolved.matches(|_| Vec::new()));
    }

    #[test]
    fn test_sort_items_orders_and_keeps_items_without_the_field() {
        let mut routes = vec![
            route(Uuid::nil(), "/v1/alpha", 20),
            route(Uuid::nil(), "/v1/beta", 5),
            route(Uuid::nil(), "/v1/gamma", 5),
        ];
        let lookup = |route: &Route, field: &str| match field {
            "priority" => vec![FilterValue::number(u64::from(route.config.priority))],
            _ => Vec::new(),
        };

        ResolvedList {
            order: vec![OrderClause {
                field: "priority".to_owned(),
                descending: false,
            }],
            ..ResolvedList::default()
        }
        .sort_items(&mut routes, lookup);
        let ascending: Vec<u32> = routes.iter().map(|route| route.config.priority).collect();
        assert_eq!(ascending, [5, 5, 20]);

        ResolvedList {
            order: vec![OrderClause {
                field: "priority".to_owned(),
                descending: true,
            }],
            ..ResolvedList::default()
        }
        .sort_items(&mut routes, lookup);
        let descending: Vec<u32> = routes.iter().map(|route| route.config.priority).collect();
        assert_eq!(descending, [20, 5, 5]);
    }

    #[test]
    fn test_page_applies_skip_and_reports_the_total() {
        let items = vec![1_u32, 2, 3, 4, 5];
        let resolved = ResolvedList {
            top: 2,
            skip: 1,
            ..ResolvedList::default()
        };

        let (page, total) = resolved.page(items);

        assert_eq!(page, [2, 3]);
        assert_eq!(total, 5);
    }

    #[test]
    fn test_list_page_reports_count_total_and_projection() {
        let upstream = upstream("api.openai.com");
        let document = list_page(
            vec![UpstreamDto::from(&upstream)],
            3,
            "upstreams",
            Some(&["id".to_owned(), "alias".to_owned()]),
        );

        assert_eq!(document["count"].as_u64(), Some(1));
        assert_eq!(document["total"].as_u64(), Some(3));
        assert_eq!(document["upstreams"][0]["alias"], "api.openai.com");
        assert!(document["upstreams"][0].get("server").is_none());
    }

    #[test]
    fn test_upstream_request_resolves_an_explicit_alias_and_a_spec() {
        let body: UpstreamRequest = request(json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.openai.com", "port": 443 }] },
            "protocol": PROTOCOL_HTTP,
        }));

        assert_eq!(body.resolved_alias("stored"), Some("stored".to_owned()));
        assert_eq!(
            body.to_spec(Some("derived".to_owned())).alias,
            Some("derived".to_owned())
        );
    }

    #[test]
    fn test_upstream_dto_reports_the_wire_identifier() {
        let stored = upstream("api.openai.com");
        let document = UpstreamDto::from(&stored);

        assert_eq!(document.id, gts_upstream_id(stored.id));
        assert_eq!(document.alias, "api.openai.com");
        assert_eq!(document.tenant_id, TENANT);
        assert_eq!(document.tags, ["llm".to_owned()]);
    }

    #[test]
    fn test_route_dto_renames_match_and_names_the_upstream() {
        let upstream_id = Uuid::new_v4();
        let stored = route(upstream_id, "/v1/chat/completions", 4);
        let rendered = serde_json::to_value(RouteDto::from(&stored)).unwrap();

        assert_eq!(rendered["id"], gts_route_id(stored.id));
        assert_eq!(rendered["upstream_id"], gts_upstream_id(upstream_id));
        assert_eq!(rendered["match"]["http"]["path"], "/v1/chat/completions");
        assert_eq!(rendered["priority"], 4);
        assert!(rendered.get("rate_limit").is_none());
    }

    #[test]
    fn test_plugin_dto_and_source_document_carry_the_gts_identifier() {
        let record = StoredPlugin {
            id: Uuid::from_u128(0x0D71),
            tenant_id: TENANT,
            name: "redact_pii".to_owned(),
            description: Some("removes PII".to_owned()),
            kind: PluginKind::Transform,
            phases: vec!["on_request".to_owned()],
            config_schema: None,
            source_code: "def on_request(ctx):\n    return ctx.next()\n".to_owned(),
        };
        let view = StoredPluginView::from(&record);

        assert_eq!(
            PluginDto::from(view).id,
            gts_plugin_id(PluginKind::Transform, record.id)
        );
        assert_eq!(
            serde_json::to_value(PluginSourceDto::from(StoredPluginView::from(&record))).unwrap(),
            json!({
                "plugin_id": gts_plugin_id(PluginKind::Transform, record.id),
                "name": "redact_pii",
                "plugin_type": "transform",
                "source_code": "def on_request(ctx):\n    return ctx.next()\n",
            })
        );
    }
}
