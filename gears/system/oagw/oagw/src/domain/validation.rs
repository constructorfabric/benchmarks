//! Payload validation for upstreams, routes, and plugin bindings.
//!
//! Enforces the JSON schemas (`schemas/upstream.v1.schema.json`,
//! `schemas/route.v1.schema.json`) plus the alias-derivation rules of
//! `DESIGN.md` §3.2 "Alias Resolution".

use crate::domain::alias::{derive_alias, valid_alias, validate_host};
use crate::domain::error::{ErrorKind, OagwError};
use crate::domain::model::{
    AuthConfig, CorsConfig, Endpoint, EndpointScheme, PROTOCOL_GRPC, PROTOCOL_HTTP,
    PluginBindingDto, PluginsConfig, RateLimitConfig, Route, Upstream,
};
use crate::domain::plugin::{AuthPluginRegistry, ResolvedPlugin, classify_plugin_ref};

/// Validation helper: a 400 problem with a `field` context member.
fn invalid(field: &str, detail: impl Into<String>) -> OagwError {
    OagwError::new(ErrorKind::Validation, detail).with_context("field", serde_json::json!(field))
}

/// Whether `tag` matches the schema pattern `^[a-z0-9_-]+$`.
#[must_use]
fn valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Validate `tags`, reporting the offending entry.
///
/// # Errors
///
/// Returns a validation error when a tag is malformed.
pub fn validate_tags(tags: &[String]) -> Result<(), OagwError> {
    for tag in tags {
        if !valid_tag(tag) {
            return Err(invalid("tags", format!("invalid tag '{tag}'")));
        }
    }
    Ok(())
}

/// Validate one endpoint: legal scheme, RFC 1123 host or IP, port in range.
///
/// # Errors
///
/// Returns a validation error naming the offending property.
pub fn validate_endpoint(endpoint: &Endpoint) -> Result<(), OagwError> {
    if let Some(reason) = validate_host(&endpoint.host) {
        return Err(invalid(
            "server.endpoints[].host",
            format!("invalid endpoint host '{}': {reason}", endpoint.host),
        ));
    }
    if endpoint.port.is_some_and(|port| port == 0) {
        return Err(invalid(
            "server.endpoints[].port",
            "endpoint port must be at least 1",
        ));
    }
    Ok(())
}

/// Validate an endpoint pool: at least one endpoint, each well-formed, and the
/// whole pool uniform (`DESIGN.md` "Multi-Endpoint Load Balancing": all
/// endpoints must have the same `protocol`, `scheme`, and `port`).
///
/// `protocol` is a property of the upstream, not of an endpoint, so it is
/// uniform by construction; the per-endpoint members a pool must agree on are
/// the URI scheme and the effective port.
///
/// # Errors
///
/// Returns a validation error when the pool is empty, malformed, or mixes
/// schemes or ports.
pub fn validate_endpoints(endpoints: &[Endpoint]) -> Result<(), OagwError> {
    if endpoints.is_empty() {
        return Err(invalid(
            "server.endpoints",
            "at least one endpoint is required",
        ));
    }
    for endpoint in endpoints {
        validate_endpoint(endpoint)?;
    }
    let first = &endpoints[0];
    let scheme = first.scheme;
    let port = first.effective_port();
    for endpoint in &endpoints[1..] {
        if endpoint.scheme != scheme {
            return Err(invalid(
                "server.endpoints",
                "all endpoints of a pool must use the same scheme",
            ));
        }
        if endpoint.effective_port() != port {
            return Err(invalid(
                "server.endpoints",
                "all endpoints of a pool must use the same port",
            ));
        }
    }
    Ok(())
}

/// The key a plugin implementation is registered under.
///
/// A binding may name a plugin by its short id (`required_headers`), by the
/// instance part of its GTS identifier (`cf.core.oagw.required_headers.v1`) or
/// by the full identifier
/// (`gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1`). The
/// registries key their implementations by the short form, so every spelling is
/// reduced to it before a lookup — the same reduction at validation time and at
/// relay time, or a chain accepted on creation would fail closed at runtime.
#[must_use]
pub fn plugin_registry_key(reference: &str) -> &str {
    let instance = crate::domain::model::gts_instance(reference.trim());
    let mut segments: Vec<&str> = instance
        .split('.')
        .filter(|part| !part.is_empty())
        .collect();
    if segments.len() > 1
        && segments
            .last()
            .is_some_and(|last| crate::domain::model::is_version_segment(last))
    {
        segments.pop();
    }
    segments.pop().unwrap_or(instance)
}

/// Validate a plugin binding chain against the registries.
///
/// # Errors
///
/// Returns a validation error when a `plugin_ref` is not a resolvable
/// identifier, or when it names a tenant-defined plugin: a custom plugin has no
/// interpreter in this build, so a binding that would be silently skipped at
/// relay time is refused here instead.
pub fn validate_plugin_chain(
    plugins: &PluginsConfig,
    guards: &crate::domain::plugin::GuardPluginRegistry,
    transforms: &crate::domain::plugin::TransformPluginRegistry,
    custom_exists: &dyn Fn(uuid::Uuid) -> bool,
) -> Result<(), OagwError> {
    for binding in &plugins.items {
        let (reference, _) = match binding {
            PluginBindingDto::Ref(reference) => (reference, None),
            PluginBindingDto::Detailed { plugin_ref, config } => (plugin_ref, Some(config)),
        };
        match classify_plugin_ref(reference) {
            ResolvedPlugin::Custom(id) => {
                if !custom_exists(id) {
                    return Err(invalid(
                        "plugins.items[]",
                        format!("plugin '{reference}' does not exist for this tenant"),
                    ));
                }
                return Err(invalid(
                    "plugins.items[]",
                    format!(
                        "plugin '{reference}' is a tenant-defined plugin and custom plugin \
                         execution is not available in this build"
                    ),
                ));
            }
            ResolvedPlugin::Named => {
                let key = plugin_registry_key(reference);
                if guards.resolve(key).is_none() && transforms.resolve(key).is_none() {
                    return Err(invalid(
                        "plugins.items[]",
                        format!("unknown plugin '{reference}'"),
                    ));
                }
            }
            ResolvedPlugin::Unknown => {
                return Err(invalid("plugins.items[]", "empty plugin reference"));
            }
        }
    }
    Ok(())
}

/// Validate the `auth` binding of an upstream.
///
/// # Errors
///
/// Returns a validation error when the plugin type is unknown or its config
/// is incomplete.
pub fn validate_auth(
    auth: &AuthConfig,
    auth_plugins: &AuthPluginRegistry,
    custom_exists: &dyn Fn(uuid::Uuid) -> bool,
) -> Result<(), OagwError> {
    let Some(plugin_type) = auth.plugin_type.as_deref().filter(|t| !t.trim().is_empty()) else {
        return Err(invalid(
            "auth.type",
            "auth.type is required when auth is configured",
        ));
    };
    match classify_plugin_ref(plugin_type) {
        ResolvedPlugin::Custom(id) => {
            if !custom_exists(id) {
                return Err(invalid(
                    "auth.type",
                    format!("plugin '{plugin_type}' does not exist for this tenant"),
                ));
            }
        }
        ResolvedPlugin::Named => {
            if auth_plugins.resolve(plugin_type).is_none() {
                return Err(invalid(
                    "auth.type",
                    format!("unknown auth plugin '{plugin_type}'"),
                ));
            }
        }
        ResolvedPlugin::Unknown => {
            return Err(invalid("auth.type", "auth.type must not be empty"));
        }
    }
    Ok(())
}

/// Validate the OAuth2-specific configuration keys (`ADR 0008`).
///
/// # Errors
///
/// Returns a validation error when neither `token_endpoint` nor `issuer_url`
/// is configured, or when a credential reference is missing.
pub fn validate_oauth2_config(config: &serde_json::Value) -> Result<(), OagwError> {
    let Some(object) = config.as_object() else {
        return Err(invalid("auth.config", "auth.config must be an object"));
    };
    let endpoint = object.contains_key("token_endpoint");
    let issuer = object.contains_key("issuer_url");
    if endpoint == issuer {
        return Err(invalid(
            "auth.config",
            "exactly one of auth.config.token_endpoint or auth.config.issuer_url is required",
        ));
    }
    for key in ["client_id_ref", "client_secret_ref"] {
        let present = object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.trim().is_empty());
        if !present {
            return Err(invalid(
                "auth.config",
                format!("auth.config.{key} is required"),
            ));
        }
    }
    Ok(())
}

/// Validate a CORS configuration (`ADR 0004` "Credentials restriction").
///
/// # Errors
///
/// Returns a validation error when `allow_credentials` is combined with a
/// wildcard origin.
pub fn validate_cors(cors: &CorsConfig) -> Result<(), OagwError> {
    if cors.allow_credentials && cors.allowed_origins.iter().any(|origin| origin == "*") {
        return Err(invalid(
            "cors.allowed_origins",
            "cannot use allow_credentials with wildcard origin",
        ));
    }
    Ok(())
}

/// Validate a rate-limit configuration.
///
/// # Errors
///
/// Returns a validation error when the sustained rate is below 1, when the
/// cost is below 1, or when the configured overload strategy has no
/// implementation: `queue` and `degrade` fall through to `reject` at relay
/// time, so a configuration that cannot be honoured is refused here instead of
/// being accepted and silently ignored.
pub fn validate_rate_limit(config: &RateLimitConfig) -> Result<(), OagwError> {
    if config.sustained.rate < 1 {
        return Err(invalid(
            "rate_limit.sustained.rate",
            "rate_limit.sustained.rate must be at least 1",
        ));
    }
    if config.cost < 1 {
        return Err(invalid(
            "rate_limit.cost",
            "rate_limit.cost must be at least 1",
        ));
    }
    if let crate::domain::model::RateStrategy::Reject = config.strategy {
        return Ok(());
    }
    Err(invalid(
        "rate_limit.strategy",
        format!(
            "rate_limit.strategy '{}' is not supported; only 'reject' is implemented",
            strategy_name(config.strategy)
        ),
    ))
}

/// The wire spelling of a rate-limit strategy.
#[must_use]
fn strategy_name(strategy: crate::domain::model::RateStrategy) -> &'static str {
    match strategy {
        crate::domain::model::RateStrategy::Reject => "reject",
        crate::domain::model::RateStrategy::Queue => "queue",
        crate::domain::model::RateStrategy::Degrade => "degrade",
    }
}

/// Resolve and validate the alias of an upstream from its endpoint pool.
///
/// Hostname-based pools always derive the alias (`DESIGN.md`: a user-provided
/// alias is rejected); IP-based or non-derivable pools require an explicit
/// alias.
///
/// # Errors
///
/// Returns a validation error when the alias is missing, malformed, or
/// attempts to override a derived alias.
pub fn resolve_alias(alias_in: &str, endpoints: &[Endpoint]) -> Result<String, OagwError> {
    validate_endpoints(endpoints)?;
    let derived = derive_alias(endpoints);
    let supplied = alias_in.trim();
    if supplied.is_empty() {
        return derived.ok_or_else(|| {
            invalid(
                "alias",
                "an explicit alias is required for IP-based or non-derivable endpoints",
            )
        });
    }
    let normalized = crate::domain::alias::normalize(supplied);
    if !valid_alias(&normalized) {
        return Err(invalid("alias", format!("invalid alias '{normalized}'")));
    }
    if let Some(derived) = derived
        && derived != normalized
    {
        return Err(invalid(
            "alias",
            format!(
                "alias is derived from the endpoint pool ('{derived}') and may not be overridden"
            ),
        ));
    }
    Ok(normalized)
}

/// Validate an upstream on create or replace.
///
/// # Errors
///
/// Returns a validation error for every schema violation, including an
/// unknown `protocol`, an unusable alias, or an unresolvable plugin chain.
pub fn validate_upstream(
    upstream: &Upstream,
    auth_plugins: &AuthPluginRegistry,
    guards: &crate::domain::plugin::GuardPluginRegistry,
    transforms: &crate::domain::plugin::TransformPluginRegistry,
    custom_exists: &dyn Fn(uuid::Uuid) -> bool,
) -> Result<(), OagwError> {
    if upstream.protocol != PROTOCOL_HTTP && upstream.protocol != PROTOCOL_GRPC {
        return Err(invalid(
            "protocol",
            format!("unsupported protocol '{}'", upstream.protocol),
        ));
    }
    validate_endpoints(&upstream.server.endpoints)?;
    validate_tags(&upstream.tags)?;
    if upstream.alias.is_empty() {
        return Err(invalid("alias", "an explicit alias is required"));
    }
    if !valid_alias(&upstream.alias) {
        return Err(invalid(
            "alias",
            format!("invalid alias '{}'", upstream.alias),
        ));
    }
    if let Some(auth) = &upstream.auth {
        validate_auth(auth, auth_plugins, custom_exists)?;
        let instance =
            crate::domain::model::gts_instance(auth.plugin_type.as_deref().unwrap_or_default());
        if instance == crate::domain::model::auth_plugin_ids::OAUTH2_CC
            || instance == crate::domain::model::auth_plugin_ids::OAUTH2_CC_BASIC
        {
            validate_oauth2_config(&auth.config)?;
        }
    }
    if let Some(cors) = &upstream.cors {
        validate_cors(cors)?;
    }
    if let Some(rate_limit) = &upstream.rate_limit {
        validate_rate_limit(rate_limit)?;
    }
    validate_plugin_chain(&upstream.plugins, guards, transforms, custom_exists)?;
    Ok(())
}

/// Validate a route on create or replace.
///
/// # Errors
///
/// Returns a validation error for missing `upstream_id`, an ambiguous match
/// rule, an unresolvable plugin chain, or a CORS block that pairs credentials
/// with a wildcard origin.
pub fn validate_route(
    route: &Route,
    guards: &crate::domain::plugin::GuardPluginRegistry,
    transforms: &crate::domain::plugin::TransformPluginRegistry,
    custom_exists: &dyn Fn(uuid::Uuid) -> bool,
) -> Result<(), OagwError> {
    if route.upstream_id.is_nil() {
        return Err(invalid("upstream_id", "upstream_id is required"));
    }
    let http = route.match_rule.http.as_ref();
    let grpc = route.match_rule.grpc.as_ref();
    if http.is_some() == grpc.is_some() {
        return Err(invalid(
            "match",
            "exactly one of match.http or match.grpc is required",
        ));
    }
    if let Some(http) = http {
        if http.methods.is_empty() {
            return Err(invalid(
                "match.http.methods",
                "at least one HTTP method is required",
            ));
        }
        for method in &http.methods {
            if method.to_http() == http::Method::OPTIONS {
                return Err(invalid(
                    "match.http.methods",
                    "OPTIONS is handled by the gateway's CORS layer",
                ));
            }
        }
        if http.path.trim().is_empty() {
            return Err(invalid("match.http.path", "match.http.path is required"));
        }
        if !http.path.starts_with('/') {
            return Err(invalid(
                "match.http.path",
                "match.http.path must start with '/'",
            ));
        }
    }
    if let Some(grpc) = grpc
        && (grpc.service.trim().is_empty() || grpc.method.trim().is_empty())
    {
        return Err(invalid(
            "match.grpc",
            "match.grpc.service and match.grpc.method are required",
        ));
    }
    validate_tags(&route.tags)?;
    if let Some(rate_limit) = &route.rate_limit {
        validate_rate_limit(rate_limit)?;
    }
    if let Some(cors) = &route.cors {
        validate_cors(cors)?;
    }
    validate_plugin_chain(&route.plugins, guards, transforms, custom_exists)?;
    Ok(())
}

/// Whether `scheme` is a plaintext (non-TLS) scheme.
#[must_use]
pub fn scheme_is_plaintext(scheme: EndpointScheme) -> bool {
    !scheme.is_tls()
}

/// Validate a custom plugin definition on create.
///
/// The kind must be one the data plane knows how to dispatch (`auth`, `guard`,
/// or `transform`), and a Starlark plugin carries the source it will be run
/// with: an empty source is a definition nothing can execute.
///
/// # Errors
///
/// Returns a validation error naming the offending member.
pub fn validate_plugin(plugin: &crate::domain::model::CustomPlugin) -> Result<(), OagwError> {
    if !matches!(plugin.plugin_type.as_str(), "auth" | "guard" | "transform") {
        return Err(invalid(
            "plugin_type",
            format!(
                "plugin_type must be one of 'auth', 'guard', 'transform', got '{}'",
                plugin.plugin_type
            ),
        ));
    }
    if plugin.name.trim().is_empty() {
        return Err(invalid("name", "name is required"));
    }
    if plugin.source_code.trim().is_empty() {
        return Err(invalid("source_code", "source_code is required"));
    }
    Ok(())
}

/// Merge two plugin chains: upstream bindings first, then route bindings
/// (`ADR 0002` "Upstream plugins execute before route plugins").
#[must_use]
pub fn merge_plugins(upstream: &PluginsConfig, route: &PluginsConfig) -> Vec<PluginBindingDto> {
    let mut merged = Vec::with_capacity(upstream.items.len() + route.items.len());
    merged.extend(upstream.items.iter().cloned());
    merged.extend(route.items.iter().cloned());
    merged
}
