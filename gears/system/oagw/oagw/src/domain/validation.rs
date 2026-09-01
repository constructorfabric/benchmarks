//! Structural and semantic validation of upstream and route payloads.
//!
//! The JSON Schemas in `docs/schemas/` define the accepted wire shape; serde
//! (`deny_unknown_fields`, enums, `minItems`) enforces most of it. What serde
//! cannot express — host syntax, pool homogeneity, match-rule well-formedness,
//! plugin binding resolvability — lives here, so a payload either round-trips
//! into a valid row or fails with a single `400`.

use std::collections::BTreeSet;

use crate::domain::error::DomainError;
use crate::domain::model::{
    AuthConfig, CorsConfig, Endpoint, EndpointScheme, GrpcMatch, HttpMatch, MatchConfig,
    PluginsConfig, Protocol, RateLimitConfig, Upstream,
};
use crate::domain::plugin;
use crate::domain::repo::{PluginRepository, RouteRepository};

/// Maximum accepted length of a Starlark plugin source.
pub const MAX_PLUGIN_SOURCE_BYTES: usize = 128 * 1024;

/// Maximum number of endpoints in one pool.
pub const MAX_ENDPOINTS: usize = 64;

/// The HTTP methods a CORS configuration may allow (`upstream.v1` enum).
pub const CORS_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/// Validate a hostname per RFC 1123 and normalize it.
///
/// Accepts an optional trailing root dot (FQDN notation) and returns the
/// normalized, lowercased host.
///
/// # Errors
/// Returns [`DomainError::Validation`] for an empty, oversized, or malformed
/// label sequence.
pub fn validate_hostname(host: &str) -> Result<String, DomainError> {
    let normalized = host.trim().to_ascii_lowercase();
    let trimmed = normalized.strip_suffix('.').unwrap_or(&normalized);
    if trimmed.is_empty() {
        return Err(DomainError::validation("endpoint host must not be empty"));
    }
    if trimmed.len() > 253 {
        return Err(DomainError::validation(
            "endpoint host must be at most 253 characters",
        ));
    }
    if crate::domain::alias::is_ip(trimmed) {
        return Ok(trimmed.to_owned());
    }
    for label in trimmed.split('.') {
        validate_label(label, trimmed)?;
    }
    Ok(trimmed.to_owned())
}

fn validate_label(label: &str, full_host: &str) -> Result<(), DomainError> {
    if label.is_empty() {
        return Err(DomainError::validation(format!(
            "endpoint host has an empty label: {full_host}"
        )));
    }
    if label.len() > 63 {
        return Err(DomainError::validation(format!(
            "endpoint host label is longer than 63 characters: {full_host}"
        )));
    }
    if label.starts_with('-') || label.ends_with('-') {
        return Err(DomainError::validation(format!(
            "endpoint host label must not start or end with a hyphen: {full_host}"
        )));
    }
    for c in label.chars() {
        if !(c.is_ascii_alphanumeric() || c == '-') {
            return Err(DomainError::validation(format!(
                "endpoint host label contains an invalid character '{c}': {full_host}"
            )));
        }
    }
    Ok(())
}

/// Validate the endpoint pool of an upstream.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the pool is empty, exceeds
/// [`MAX_ENDPOINTS`], carries a malformed host, or is not homogeneous
/// (all endpoints must share `scheme` and `port`).
pub fn validate_endpoints(endpoints: &[Endpoint]) -> Result<(), DomainError> {
    if endpoints.is_empty() {
        return Err(DomainError::validation(
            "server.endpoints must contain at least one endpoint",
        ));
    }
    if endpoints.len() > MAX_ENDPOINTS {
        return Err(DomainError::validation(format!(
            "server.endpoints must contain at most {MAX_ENDPOINTS} endpoints"
        )));
    }
    for endpoint in endpoints {
        validate_hostname(&endpoint.host)?;
    }
    let first = endpoints
        .first()
        .ok_or_else(|| DomainError::validation("server.endpoints must not be empty"))?;
    for endpoint in endpoints {
        if endpoint.scheme != first.scheme || endpoint.port != first.port {
            return Err(DomainError::validation(
                "all endpoints of an upstream must share the same scheme and port",
            ));
        }
    }
    Ok(())
}

/// Validate a plugin chain binding.
///
/// # Errors
/// Returns [`DomainError::Validation`] when a bound plugin is neither a
/// resolvable built-in nor a resolvable custom row reference, or names a row
/// whose kind the chain does not accept.
pub async fn validate_plugin_chain(
    kind: plugin::ChainKind,
    config: &PluginsConfig,
    plugins: &dyn PluginRepository,
    tenant: uuid::Uuid,
) -> Result<(), DomainError> {
    for item in &config.items {
        validate_plugin_ref(kind, item.plugin_ref(), plugins, tenant).await?;
    }
    Ok(())
}

/// Validate one plugin reference.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the reference is not a well-formed
/// GTS id of an accepted base type, is a catalog-only identifier with no
/// backing implementation, or names a custom plugin that does not exist in the
/// calling tenant.
pub async fn validate_plugin_ref(
    kind: plugin::ChainKind,
    plugin_ref: &str,
    plugins: &dyn PluginRepository,
    tenant: uuid::Uuid,
) -> Result<(), DomainError> {
    if !plugin::is_bindable(kind, plugin_ref) {
        let expected = plugin::CHAIN_BASE_TYPES.join(" or ");
        return Err(DomainError::validation(format!(
            "unknown {kind} plugin: '{plugin_ref}'; {kind} chains bind {expected} plugins"
        )));
    }
    let Some(uuid) = plugin::uuid_tail(plugin_ref) else {
        return Ok(());
    };
    let Some(row) = plugins.find(tenant, uuid).await? else {
        return Err(DomainError::validation(format!(
            "unknown custom plugin: {plugin_ref}"
        )));
    };
    validate_row_kind(plugin_ref, &row)
}

/// Validate that a resolved custom plugin row is of a kind the chain accepts.
///
/// A row is addressed by its UUID tail, so a reference carrying the guard base
/// type can still name a transform row; that reference must not resolve,
/// otherwise the same row would be bindable under a base type it does not use.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the row's kind is not bindable on
/// a chain.
fn validate_row_kind(
    plugin_ref: &str,
    row: &crate::domain::model::Plugin,
) -> Result<(), DomainError> {
    if plugin::allows_row(row.plugin_type) {
        return Ok(());
    }
    Err(DomainError::validation(format!(
        "plugin '{plugin_ref}' is a {} row and cannot be bound to a chain",
        row.plugin_type.gts_base_type()
    )))
}

/// Validate the outbound auth configuration of an upstream.
///
/// `auth.type` is a *named* built-in auth plugin or a UUID-backed auth plugin
/// row of the calling tenant; it is never a `plugins.items` entry.
///
/// # Errors
/// Returns [`DomainError::Validation`] when `auth.type` is not a typed
/// `auth_plugin` GTS id, names a catalog-only identifier with no backing
/// implementation, or names a custom row that does not exist.
pub async fn validate_auth(
    auth: &AuthConfig,
    plugins: &dyn PluginRepository,
    tenant: uuid::Uuid,
) -> Result<(), DomainError> {
    // `auth.type` is optional: an upstream without it is forwarded as-is.
    let Some(reference) = auth.plugin_type.as_deref() else {
        return Ok(());
    };
    if !reference.starts_with(crate::domain::model::AUTH_PLUGIN_TYPE) {
        return Err(unknown_auth_plugin(reference));
    }
    let Some(uuid) = plugin::uuid_tail(reference) else {
        // A named built-in must have a backing implementation.
        if crate::domain::plugin::AUTH_BUILTINS
            .iter()
            .any(|entry| entry.bindable && entry.gts_id == reference)
        {
            return Ok(());
        }
        return Err(unknown_auth_plugin(reference));
    };
    let Some(row) = plugins.find(tenant, uuid).await? else {
        return Err(unknown_auth_plugin(reference));
    };
    if row.plugin_type != crate::domain::model::PluginType::Auth {
        return Err(unknown_auth_plugin(reference));
    }
    Ok(())
}

fn unknown_auth_plugin(reference: &str) -> DomainError {
    DomainError::validation(format!("unknown auth plugin: {reference}"))
}

/// Validate a CORS configuration against the `cors` definition of the schemas.
///
/// # Errors
/// Returns [`DomainError::Validation`] when a list field is set while CORS is
/// disabled, when `allow_credentials` is combined with a wildcard or an empty
/// origin set, when an origin is neither `*` nor a URI, or when an allowed
/// method is outside the documented enum.
pub fn validate_cors(cors: &CorsConfig) -> Result<(), DomainError> {
    let lists_are_set = !cors.allowed_origins.is_empty()
        || !cors.allowed_methods.is_empty()
        || !cors.expose_headers.is_empty()
        || cors.allow_credentials;
    if !cors.enabled && lists_are_set {
        return Err(DomainError::validation(
            "cors.allowed_origins, cors.allowed_methods and cors.allow_credentials \
             require cors.enabled",
        ));
    }
    if cors.allow_credentials {
        if cors.allowed_origins.is_empty() {
            return Err(DomainError::validation(
                "cors.allow_credentials requires at least one allowed origin",
            ));
        }
        if cors.allowed_origins.iter().any(|origin| origin == "*") {
            return Err(DomainError::validation(
                "cors.allowed_origins must not contain '*' while \
                 cors.allow_credentials is true",
            ));
        }
    }
    for origin in &cors.allowed_origins {
        if origin == "*" {
            continue;
        }
        // The schema types an origin as `format: uri`, so anything that is not
        // the wildcard must parse as one.
        if url::Url::parse(origin).is_err() {
            return Err(DomainError::validation(format!(
                "cors.allowed_origins entry must be '*' or a URI: {origin}"
            )));
        }
    }
    for method in &cors.allowed_methods {
        if !CORS_METHODS.contains(&method.as_str()) {
            return Err(DomainError::validation(format!(
                "cors.allowed_methods must be one of {}, got '{method}'",
                CORS_METHODS.join(", ")
            )));
        }
    }
    Ok(())
}

/// Validate a rate-limit configuration (`DESIGN` §3.2).
///
/// # Errors
/// Returns [`DomainError::Validation`] when the sustained rate is zero or the
/// burst capacity is smaller than the sustained rate.
pub fn validate_rate_limit(rate_limit: &RateLimitConfig) -> Result<(), DomainError> {
    if rate_limit.sustained.rate < 1 {
        return Err(DomainError::validation(
            "rate_limit.sustained.rate must be at least 1",
        ));
    }
    if let Some(burst) = &rate_limit.burst
        && burst.capacity < rate_limit.sustained.rate
    {
        return Err(DomainError::validation(format!(
            "rate_limit.burst.capacity ({}) must be at least \
             rate_limit.sustained.rate ({})",
            burst.capacity, rate_limit.sustained.rate
        )));
    }
    Ok(())
}

/// The shared, optional part of an upstream or route payload, borrowed from the
/// command under validation.
///
/// Both resource kinds carry a plugin chain, a rate limit, a CORS
/// configuration and tags, and both validate them the same way, so the rules
/// travel as one value instead of as four positional arguments. The default is
/// the empty payload: nothing is configured, so nothing is checked.
#[derive(Default)]
pub struct PayloadRules<'a> {
    /// Plugin chain to bind, whose references must resolve in the tenant.
    pub plugins: Option<&'a PluginsConfig>,
    /// Rate-limit override.
    pub rate_limit: Option<&'a RateLimitConfig>,
    /// CORS configuration.
    pub cors: Option<&'a CorsConfig>,
    /// Flat categorization tags.
    pub tags: &'a [String],
}

impl PayloadRules<'_> {
    /// Validate the rate limit, the CORS configuration and the tags.
    fn validate(&self) -> Result<(), DomainError> {
        if let Some(rate_limit) = self.rate_limit {
            validate_rate_limit(rate_limit)?;
        }
        if let Some(cors) = self.cors {
            validate_cors(cors)?;
        }
        validate_tags(self.tags)
    }
}

/// Validate the upstream payload of a create or replace.
///
/// # Errors
/// Aggregates every upstream-specific rule: endpoint pool, protocol/endpoint
/// agreement, alias, tags, auth plugin, CORS, rate limit, and the
/// resolvability of the bound plugins.
pub async fn validate_upstream(
    endpoints: &[Endpoint],
    protocol: Protocol,
    auth: Option<&AuthConfig>,
    rules: PayloadRules<'_>,
    plugins_repo: &dyn PluginRepository,
    tenant: uuid::Uuid,
) -> Result<(), DomainError> {
    validate_upstream_with_posture(
        endpoints,
        protocol,
        auth,
        rules,
        plugins_repo,
        tenant,
        false,
    )
    .await
}

/// [`validate_upstream`] under a deployment's cleartext posture.
///
/// # Errors
/// Same rules as [`validate_upstream`]; when `allow_cleartext` is set the
/// endpoint pool may carry `http`/`ws` endpoints, which the deployment opted
/// into through `allow_http_upstream`.
pub async fn validate_upstream_with_posture(
    endpoints: &[Endpoint],
    protocol: Protocol,
    auth: Option<&AuthConfig>,
    rules: PayloadRules<'_>,
    plugins_repo: &dyn PluginRepository,
    tenant: uuid::Uuid,
    allow_cleartext: bool,
) -> Result<(), DomainError> {
    validate_endpoints(endpoints)?;
    validate_protocol_agreement(endpoints, protocol, allow_cleartext)?;
    rules.validate()?;
    if let Some(auth) = auth {
        validate_auth(auth, plugins_repo, tenant).await?;
    }
    if let Some(chain) = rules.plugins {
        validate_plugin_chain(plugin::ChainKind::Upstream, chain, plugins_repo, tenant).await?;
    }
    Ok(())
}

/// Validate the route payload of a create or replace.
///
/// # Errors
/// Aggregates the match-rule, tag, CORS, rate-limit and plugin-chain rules.
pub async fn validate_route(
    upstream: &Upstream,
    route_match: &MatchConfig,
    priority: u32,
    rules: PayloadRules<'_>,
    plugins_repo: &dyn PluginRepository,
    tenant: uuid::Uuid,
) -> Result<(), DomainError> {
    validate_match_rules(route_match, upstream.protocol)?;
    rules.validate()?;
    if let Some(chain) = rules.plugins {
        validate_plugin_chain(plugin::ChainKind::Route, chain, plugins_repo, tenant).await?;
    }
    // `priority` is part of the uniqueness key; keep it in its wire domain.
    if priority > 1_000_000 {
        return Err(DomainError::validation(
            "route priority must be at most 1000000",
        ));
    }
    Ok(())
}

/// Validate a `match` payload against the protocol of its upstream.
///
/// # Errors
/// Returns [`DomainError::Validation`] unless exactly one of `http`/`grpc` is
/// set, that side is well-formed, and it agrees with the upstream protocol.
fn validate_match_rules(route_match: &MatchConfig, protocol: Protocol) -> Result<(), DomainError> {
    if route_match.http.is_some() == route_match.grpc.is_some() {
        return Err(DomainError::validation(
            "match must carry exactly one of 'http' or 'grpc'",
        ));
    }
    if let Some(http) = &route_match.http {
        if protocol == Protocol::Grpc {
            return Err(DomainError::validation(
                "route.match.http is not allowed on a grpc-protocol upstream",
            ));
        }
        validate_http_match(http)?;
    }
    if let Some(grpc) = &route_match.grpc {
        if protocol == Protocol::Http {
            return Err(DomainError::validation(
                "route match.grpc is not allowed on an http-protocol upstream",
            ));
        }
        validate_grpc_match(grpc)?;
    }
    Ok(())
}

/// # Errors
/// Returns [`DomainError::Validation`] for a malformed HTTP match rule.
pub fn validate_http_match(http: &HttpMatch) -> Result<(), DomainError> {
    if http.methods.is_empty() {
        return Err(DomainError::validation(
            "match.http.methods must contain at least one method",
        ));
    }
    let mut unique = BTreeSet::new();
    for method in &http.methods {
        if !unique.insert(method.as_str()) {
            return Err(DomainError::validation(format!(
                "match.http.methods contains a duplicate method: {}",
                method.as_str()
            )));
        }
    }
    let path = http.path.trim();
    if path.is_empty() {
        return Err(DomainError::validation("match.http.path must not be empty"));
    }
    if !path.starts_with('/') {
        return Err(DomainError::validation(
            "match.http.path must start with '/'",
        ));
    }
    for name in &http.query_allowlist {
        if name.trim().is_empty() {
            return Err(DomainError::validation(
                "match.http.query_allowlist must not contain empty names",
            ));
        }
    }
    Ok(())
}

/// # Errors
/// Returns [`DomainError::Validation`] for a malformed gRPC match rule.
pub fn validate_grpc_match(grpc: &GrpcMatch) -> Result<(), DomainError> {
    if grpc.service.trim().is_empty() {
        return Err(DomainError::validation(
            "match.grpc.service must not be empty",
        ));
    }
    if grpc.method.trim().is_empty() {
        return Err(DomainError::validation(
            "match.grpc.method must not be empty",
        ));
    }
    Ok(())
}

/// # Errors
/// Returns [`DomainError::Validation`] when a tag is not
/// `^[a-z0-9_-]+$` or the set exceeds 32 tags.
pub fn validate_tags(tags: &[String]) -> Result<(), DomainError> {
    if tags.len() > 32 {
        return Err(DomainError::validation(
            "tags must contain at most 32 items",
        ));
    }
    for tag in tags {
        let valid = !tag.is_empty()
            && tag.len() <= 64
            && tag
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        if !valid {
            return Err(DomainError::validation(format!(
                "tag must match ^[a-z0-9_-]+$: {tag}"
            )));
        }
    }
    Ok(())
}

fn validate_protocol_agreement(
    endpoints: &[Endpoint],
    protocol: Protocol,
    allow_cleartext: bool,
) -> Result<(), DomainError> {
    let first = endpoints
        .first()
        .ok_or_else(|| DomainError::validation("server.endpoints must not be empty"))?;
    let expected = match protocol {
        Protocol::Http => EndpointScheme::Https,
        Protocol::Grpc => EndpointScheme::Grpc,
    };
    if allow_cleartext && matches!(first.scheme, EndpointScheme::Http | EndpointScheme::Ws) {
        // An explicit deployment opt-in (`allow_http_upstream`): the endpoint
        // pool may speak cleartext, which the data plane still refuses unless
        // the same posture is configured.
        return Ok(());
    }
    if first.scheme != expected {
        return Err(DomainError::validation(format!(
            "protocol '{}' requires '{}' endpoints, got '{}'",
            protocol.gts_id(),
            expected.as_str(),
            first.scheme.as_str(),
        )));
    }
    Ok(())
}

/// Validate the Starlark source of a custom plugin.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the source is empty or exceeds
/// [`MAX_PLUGIN_SOURCE_BYTES`].
pub fn validate_plugin_source(source: &str) -> Result<(), DomainError> {
    if source.trim().is_empty() {
        return Err(DomainError::validation(
            "source_code must not be empty: a plugin without a body cannot be interpreted",
        ));
    }
    if source.len() > MAX_PLUGIN_SOURCE_BYTES {
        return Err(DomainError::validation(format!(
            "source_code exceeds {MAX_PLUGIN_SOURCE_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Ensure a route's match rules do not collide with an existing route of the
/// same upstream (`DESIGN` §3.3, POST (create): "same path + priority + method
/// → 409").
///
/// # Errors
/// Returns [`DomainError::Conflict`] when an existing route of the upstream
/// carries the same match key.
pub async fn ensure_match_rule_unique(
    routes: &dyn RouteRepository,
    tenant: uuid::Uuid,
    upstream_id: uuid::Uuid,
    candidate: &MatchConfig,
    priority: u32,
    ignore_route_id: Option<uuid::Uuid>,
) -> Result<(), DomainError> {
    for existing in routes.list_by_upstream(tenant, upstream_id).await? {
        if Some(existing.id) == ignore_route_id {
            continue;
        }
        if existing.priority != priority {
            continue;
        }
        if match_keys_equal(&existing.r#match, candidate) {
            return Err(DomainError::Conflict {
                detail: format!(
                    "route already defines this match rule on upstream \
                     {} at priority {priority}",
                    crate::domain::model::resource_gts_id(
                        crate::domain::model::ROUTE_TYPE,
                        upstream_id
                    )
                ),
            });
        }
    }
    Ok(())
}

/// Compare two match configurations for match-key equality, ignoring the
/// suffix mode and the query allowlist (which shape handling, not identity).
fn match_keys_equal(left: &MatchConfig, right: &MatchConfig) -> bool {
    match (&left.http, &right.http) {
        (Some(a), Some(b)) => {
            let mut lhs = a.methods.clone();
            let mut rhs = b.methods.clone();
            lhs.sort_by_key(|m| m.as_str());
            rhs.sort_by_key(|m| m.as_str());
            a.path == b.path && lhs == rhs
        }
        (None, None) => left.grpc == right.grpc,
        _ => false,
    }
}

/// `true` when every endpoint of the pool is a hostname (no IP literal).
#[must_use]
pub fn all_hostnames(endpoints: &[Endpoint]) -> bool {
    endpoints
        .iter()
        .all(|e| !crate::domain::alias::is_ip(&e.host))
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "validation_tests.rs"]
mod tests;
