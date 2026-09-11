//! Conversions between the wire DTOs and the domain model.

use std::collections::BTreeMap;

use uuid::Uuid;

use crate::api::rest::dto as wire;
use crate::domain::dto as domain;
use crate::domain::error::DomainError;

/// Parses a scheme name.
fn parse_scheme(name: &str) -> Result<domain::Scheme, DomainError> {
    match name {
        "http" => Ok(domain::Scheme::Http),
        "https" => Ok(domain::Scheme::Https),
        "ws" => Ok(domain::Scheme::Ws),
        "wss" => Ok(domain::Scheme::Wss),
        "wt" => Ok(domain::Scheme::Wt),
        "grpc" => Ok(domain::Scheme::Grpc),
        other => Err(DomainError::Validation(format!(
            "endpoint scheme `{other}` is not one of https, wss, wt, grpc, http"
        ))),
    }
}

fn parse_protocol(id: &str) -> Result<domain::Protocol, DomainError> {
    domain::Protocol::from_gts_id(id).ok_or_else(|| {
        DomainError::Validation(format!(
            "protocol `{id}` is not a known oagw protocol identifier"
        ))
    })
}

fn parse_sharing(name: &str, field: &str) -> Result<domain::Sharing, DomainError> {
    match name {
        "" | "private" => Ok(domain::Sharing::Private),
        "inherit" => Ok(domain::Sharing::Inherit),
        "enforce" => Ok(domain::Sharing::Enforce),
        other => Err(DomainError::Validation(format!(
            "`{field}` must be private, inherit or enforce, not `{other}`"
        ))),
    }
}

fn parse_passthrough(name: &str) -> Result<domain::HeaderPassthrough, DomainError> {
    match name {
        "" | "none" => Ok(domain::HeaderPassthrough::None),
        "allowlist" => Ok(domain::HeaderPassthrough::Allowlist),
        "all" => Ok(domain::HeaderPassthrough::All),
        other => Err(DomainError::Validation(format!(
            "passthrough must be none, allowlist or all, not `{other}`"
        ))),
    }
}

fn parse_window(name: &str) -> Result<domain::RateWindow, DomainError> {
    match name {
        "" | "second" => Ok(domain::RateWindow::Second),
        "minute" => Ok(domain::RateWindow::Minute),
        "hour" => Ok(domain::RateWindow::Hour),
        "day" => Ok(domain::RateWindow::Day),
        other => Err(DomainError::Validation(format!(
            "rate window must be second, minute, hour or day, not `{other}`"
        ))),
    }
}

fn parse_algorithm(name: &str) -> Result<domain::RateAlgorithm, DomainError> {
    match name {
        "" | "token_bucket" => Ok(domain::RateAlgorithm::TokenBucket),
        "sliding_window" => Ok(domain::RateAlgorithm::SlidingWindow),
        other => Err(DomainError::Validation(format!(
            "rate algorithm must be token_bucket or sliding_window, not `{other}`"
        ))),
    }
}

fn parse_scope(name: &str) -> Result<domain::RateScope, DomainError> {
    match name {
        "" | "tenant" => Ok(domain::RateScope::Tenant),
        "global" => Ok(domain::RateScope::Global),
        "user" => Ok(domain::RateScope::User),
        "ip" => Ok(domain::RateScope::Ip),
        "route" => Ok(domain::RateScope::Route),
        other => Err(DomainError::Validation(format!(
            "rate scope must be global, tenant, user, ip or route, not `{other}`"
        ))),
    }
}

fn parse_strategy(name: &str) -> Result<domain::RateStrategy, DomainError> {
    match name {
        "" | "reject" => Ok(domain::RateStrategy::Reject),
        "queue" => Ok(domain::RateStrategy::Queue),
        "degrade" => Ok(domain::RateStrategy::Degrade),
        other => Err(DomainError::Validation(format!(
            "rate strategy must be reject, queue or degrade, not `{other}`"
        ))),
    }
}

fn parse_methods(values: &[String]) -> Result<Vec<domain::HttpMethod>, DomainError> {
    if values.is_empty() {
        return Err(DomainError::Validation(
            "match.http.methods must name at least one method".into(),
        ));
    }
    values
        .iter()
        .map(|value| {
            domain::HttpMethod::parse(value).ok_or_else(|| {
                DomainError::Validation(format!(
                    "method `{value}` is not one of GET, POST, PUT, DELETE, PATCH"
                ))
            })
        })
        .collect()
}

fn parse_suffix_mode(name: &str) -> Result<domain::PathSuffixMode, DomainError> {
    match name {
        "" | "append" => Ok(domain::PathSuffixMode::Append),
        "disabled" => Ok(domain::PathSuffixMode::Disabled),
        other => Err(DomainError::Validation(format!(
            "path_suffix_mode must be append or disabled, not `{other}`"
        ))),
    }
}

fn json_object(value: &serde_json::Value) -> BTreeMap<String, serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => map.clone().into_iter().collect(),
        serde_json::Value::Null => BTreeMap::new(),
        other => {
            let mut map = BTreeMap::new();
            map.insert("value".to_owned(), other.clone());
            map
        }
    }
}

fn json_value(map: &BTreeMap<String, serde_json::Value>) -> serde_json::Value {
    serde_json::Value::Object(map.clone().into_iter().collect())
}

/// DTO → domain.
///
/// # Errors
///
/// Returns a validation error when `scheme` is not one of the supported
/// protocol schemes.
pub fn to_endpoint(source: &wire::EndpointDto) -> Result<domain::Endpoint, DomainError> {
    Ok(domain::Endpoint {
        scheme: parse_scheme(&source.scheme)?,
        host: source.host.clone(),
        port: source.port,
    })
}

/// DTO → domain.
///
/// # Errors
///
/// Returns a validation error when any endpoint of the server carries an
/// unsupported scheme.
pub fn to_server(source: &wire::ServerDto) -> Result<domain::ServerConfig, DomainError> {
    let endpoints: Result<Vec<domain::Endpoint>, DomainError> =
        source.endpoints.iter().map(to_endpoint).collect();
    Ok(domain::ServerConfig {
        endpoints: endpoints?,
    })
}

/// DTO → domain.
///
/// # Errors
///
/// Returns a validation error when `auth.sharing` is not one of `private`,
/// `inherit` or `enforce`.
pub fn to_auth(source: &wire::AuthDto) -> Result<domain::AuthConfig, DomainError> {
    Ok(domain::AuthConfig {
        auth_type: source.auth_type.clone(),
        sharing: parse_sharing(&source.sharing, "auth.sharing")?,
        config: json_value(&source.config),
    })
}

/// DTO → domain.
///
/// # Errors
///
/// Returns a validation error when `passthrough` is not one of `none`,
/// `allowlist` or `all`.
pub fn to_request_rules(
    source: &wire::RequestHeaderRulesDto,
) -> Result<domain::RequestHeaderRules, DomainError> {
    Ok(domain::RequestHeaderRules {
        set: source.set.clone(),
        add: source.add.clone(),
        remove: source.remove.clone(),
        passthrough: parse_passthrough(&source.passthrough)?,
        passthrough_allowlist: source.passthrough_allowlist.clone(),
    })
}

/// DTO → domain.
///
/// # Errors
///
/// Returns a validation error when either the request or the response header
/// rules carry an unsupported `passthrough` value.
pub fn to_headers(source: &wire::HeadersDto) -> Result<domain::HeadersConfig, DomainError> {
    Ok(domain::HeadersConfig {
        request: source
            .request
            .as_ref()
            .map(to_request_rules)
            .transpose()?,
        response: source
            .response
            .as_ref()
            .map(|rules| domain::ResponseHeaderRules {
                set: rules.set.clone(),
                add: rules.add.clone(),
                remove: rules.remove.clone(),
            }),
    })
}

/// DTO → domain.
///
/// An item may be a bare reference or an object carrying that plugin's
/// configuration; the ADR-0009 form is the object one, the published schema's
/// is the bare one, and both are admitted here.
///
/// # Errors
///
/// Returns a validation error when a `plugins.items[]` entry is neither a
/// reference nor an object carrying a `plugin_ref`, when the object form lacks
/// a usable reference, or when `sharing` is not one of `private`, `inherit` or
/// `enforce`.
pub fn to_plugins(source: &wire::PluginsDto) -> Result<domain::PluginsConfig, DomainError> {
    let mut items = Vec::with_capacity(source.items.len());
    let mut config = BTreeMap::new();
    for item in &source.items {
        let (reference, config_document) = plugin_item(item)?;
        items.push(reference.to_owned());
        if let Some(document) = config_document {
            config.insert(reference.to_owned(), document);
        }
    }
    Ok(domain::PluginsConfig {
        sharing: parse_sharing(&source.sharing, "plugins.sharing")?,
        items,
        config,
    })
}

/// Splits one `plugins.items[]` entry into its reference and config document.
fn plugin_item(item: &serde_json::Value) -> Result<(&str, Option<serde_json::Value>), DomainError> {
    match item {
        serde_json::Value::String(reference) => Ok((reference.as_str(), None)),
        serde_json::Value::Object(fields) => {
            let reference = fields
                .get("plugin_ref")
                .or_else(|| fields.get("ref"))
                .or_else(|| fields.get("id"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    DomainError::Validation(
                        "a plugin item object requires a `plugin_ref` string".into(),
                    )
                })?;
            let config = fields
                .get("config")
                .filter(|config| !config.is_null())
                .cloned();
            Ok((reference, config))
        }
        _ => Err(DomainError::Validation(
            "a plugin item must be a reference or an object with a `plugin_ref`".into(),
        )),
    }
}

/// DTO → domain.
///
/// # Errors
///
/// Returns a validation error when `sharing`, `algorithm`, `window`, `scope`
/// or `strategy` names an unsupported value.
pub fn to_rate_limit(source: &wire::RateLimitDto) -> Result<domain::RateLimitConfig, DomainError> {
    Ok(domain::RateLimitConfig {
        sharing: parse_sharing(&source.sharing, "rate_limit.sharing")?,
        algorithm: parse_algorithm(&source.algorithm)?,
        sustained: domain::SustainedRate {
            rate: source.sustained.rate,
            window: parse_window(&source.sustained.window)?,
        },
        burst: source.burst.as_ref().map(|burst| domain::Burst {
            capacity: burst.capacity,
        }),
        scope: parse_scope(&source.scope)?,
        strategy: parse_strategy(&source.strategy)?,
        cost: source.cost,
    })
}

/// DTO → domain.
///
/// # Errors
///
/// Returns a validation error when `cors.sharing` is not one of `private`,
/// `inherit` or `enforce`.
pub fn to_cors(source: &wire::CorsDto) -> Result<domain::CorsConfig, DomainError> {
    Ok(domain::CorsConfig {
        sharing: parse_sharing(&source.sharing, "cors.sharing")?,
        enabled: source.enabled,
        allowed_origins: source.allowed_origins.clone(),
        allowed_methods: source.allowed_methods.clone(),
        expose_headers: source.expose_headers.clone(),
        allow_credentials: source.allow_credentials,
    })
}

/// DTO → domain.
///
/// # Errors
///
/// Returns a validation error when the match carries both or neither of `http`
/// and `grpc`, when the HTTP match names no method or an unknown one, when the
/// gRPC match omits `service` or `method`, or when `path_suffix_mode` is
/// unsupported.
pub fn to_match(source: &wire::MatchDto) -> Result<domain::MatchRule, DomainError> {
    match (&source.http, &source.grpc) {
        // Both arms reject the same two shapes (both set, neither set) with the
        // same message, so they stay a single arm.
        (Some(_), Some(_)) | (None, None) => Err(DomainError::Validation(
            "match must carry exactly one of `http` or `grpc`".into(),
        )),
        (Some(http), None) => Ok(domain::MatchRule::Http(domain::HttpMatch {
            methods: parse_methods(&http.methods)?,
            path: http.path.clone(),
            query_allowlist: http.query_allowlist.clone(),
            path_suffix_mode: parse_suffix_mode(&http.path_suffix_mode)?,
        })),
        (None, Some(grpc)) => {
            if grpc.service.is_empty() || grpc.method.is_empty() {
                return Err(DomainError::Validation(
                    "match.grpc requires both `service` and `method`".into(),
                ));
            }
            Ok(domain::MatchRule::Grpc(domain::GrpcMatch {
                service: grpc.service.clone(),
                method: grpc.method.clone(),
            }))
        }
    }
}

fn from_endpoint(source: &domain::Endpoint) -> wire::EndpointDto {
    wire::EndpointDto {
        scheme: source.scheme.as_str().to_owned(),
        host: source.host.clone(),
        port: source.port,
    }
}

fn from_server(source: &domain::ServerConfig) -> wire::ServerDto {
    wire::ServerDto {
        endpoints: source
            .endpoints
            .iter()
            .map(from_endpoint)
            .collect(),
    }
}

fn from_auth(source: &domain::AuthConfig) -> wire::AuthDto {
    wire::AuthDto {
        auth_type: source.auth_type.clone(),
        sharing: source.sharing.as_wire().to_owned(),
        config: json_object(&source.config),
    }
}

fn from_request_rules(source: &domain::RequestHeaderRules) -> wire::RequestHeaderRulesDto {
    wire::RequestHeaderRulesDto {
        set: source.set.clone(),
        add: source.add.clone(),
        remove: source.remove.clone(),
        passthrough: match source.passthrough {
            domain::HeaderPassthrough::None => "none".to_owned(),
            domain::HeaderPassthrough::Allowlist => "allowlist".to_owned(),
            domain::HeaderPassthrough::All => "all".to_owned(),
        },
        passthrough_allowlist: source.passthrough_allowlist.clone(),
    }
}

fn from_headers(source: &domain::HeadersConfig) -> wire::HeadersDto {
    wire::HeadersDto {
        request: source.request.as_ref().map(from_request_rules),
        response: source.response.as_ref().map(|rules| wire::ResponseHeaderRulesDto {
            set: rules.set.clone(),
            add: rules.add.clone(),
            remove: rules.remove.clone(),
        }),
    }
}

fn from_plugins(source: &domain::PluginsConfig) -> wire::PluginsDto {
    // A reference with a configuration document round-trips as the object form;
    // a bare one stays bare, matching the published schema's primary shape. A
    // UUID-backed reference always carries the `plugin_uuid` extracted from it,
    // which is how a custom plugin's binding is keyed in storage.
    let items = source
        .items
        .iter()
        .map(|reference| match (source.config.get(reference), crate::domain::gts_helpers::plugin_uuid_of(reference)) {
            (Some(config), _) => serde_json::json!({"plugin_ref": reference, "config": config}),
            (None, Some(uuid)) => serde_json::json!({"plugin_ref": reference, "plugin_uuid": uuid}),
            (None, None) => serde_json::Value::String(reference.clone()),
        })
        .collect();
    wire::PluginsDto {
        sharing: source.sharing.as_wire().to_owned(),
        items,
    }
}

fn from_rate_limit(source: &domain::RateLimitConfig) -> wire::RateLimitDto {
    wire::RateLimitDto {
        sharing: source.sharing.as_wire().to_owned(),
        algorithm: match source.algorithm {
            domain::RateAlgorithm::TokenBucket => "token_bucket".to_owned(),
            domain::RateAlgorithm::SlidingWindow => "sliding_window".to_owned(),
        },
        sustained: wire::SustainedRateDto {
            rate: source.sustained.rate,
            window: match source.sustained.window {
                domain::RateWindow::Second => "second".to_owned(),
                domain::RateWindow::Minute => "minute".to_owned(),
                domain::RateWindow::Hour => "hour".to_owned(),
                domain::RateWindow::Day => "day".to_owned(),
            },
        },
        burst: source.burst.as_ref().map(|burst| wire::BurstDto {
            capacity: burst.capacity,
        }),
        scope: match source.scope {
            domain::RateScope::Global => "global".to_owned(),
            domain::RateScope::Tenant => "tenant".to_owned(),
            domain::RateScope::User => "user".to_owned(),
            domain::RateScope::Ip => "ip".to_owned(),
            domain::RateScope::Route => "route".to_owned(),
        },
        strategy: match source.strategy {
            domain::RateStrategy::Reject => "reject".to_owned(),
            domain::RateStrategy::Queue => "queue".to_owned(),
            domain::RateStrategy::Degrade => "degrade".to_owned(),
        },
        cost: source.cost,
    }
}

fn from_cors(source: &domain::CorsConfig) -> wire::CorsDto {
    wire::CorsDto {
        sharing: source.sharing.as_wire().to_owned(),
        enabled: source.enabled,
        allowed_origins: source.allowed_origins.clone(),
        allowed_methods: source.allowed_methods.clone(),
        expose_headers: source.expose_headers.clone(),
        allow_credentials: source.allow_credentials,
    }
}

fn from_match(source: &domain::MatchRule) -> wire::MatchDto {
    match source {
        domain::MatchRule::Http(http) => wire::MatchDto {
            http: Some(wire::HttpMatchDto {
                methods: http.methods.iter().map(|method| method.as_str().to_owned()).collect(),
                path: http.path.clone(),
                query_allowlist: http.query_allowlist.clone(),
                path_suffix_mode: match http.path_suffix_mode {
                    domain::PathSuffixMode::Append => "append".to_owned(),
                    domain::PathSuffixMode::Disabled => "disabled".to_owned(),
                },
            }),
            grpc: None,
        },
        domain::MatchRule::Grpc(grpc) => wire::MatchDto {
            http: None,
            grpc: Some(wire::GrpcMatchDto {
                service: grpc.service.clone(),
                method: grpc.method.clone(),
            }),
        },
    }
}

/// Domain → wire.
#[must_use]
pub fn from_upstream(source: &domain::Upstream) -> wire::UpstreamDto {
    wire::UpstreamDto {
        id: source.id,
        tenant_id: source.tenant_id,
        enabled: source.enabled,
        alias: source.alias.clone(),
        tags: source.tags.clone(),
        server: from_server(&source.server),
        protocol: source.protocol.gts_id().to_owned(),
        auth: source.auth.as_ref().map(from_auth),
        headers: source.headers.as_ref().map(from_headers),
        plugins: source.plugins.as_ref().map(from_plugins),
        rate_limit: source.rate_limit.as_ref().map(from_rate_limit),
        cors: source.cors.as_ref().map(from_cors),
        created_at: source.created_at.clone(),
        updated_at: source.updated_at.clone(),
    }
}

/// Domain → wire.
#[must_use]
pub fn from_route(source: &domain::Route) -> wire::RouteDto {
    wire::RouteDto {
        id: source.id,
        tenant_id: source.tenant_id,
        upstream_id: source.upstream_id,
        enabled: source.enabled,
        tags: source.tags.clone(),
        match_rule: from_match(&source.match_rule),
        plugins: source.plugins.as_ref().map(from_plugins),
        rate_limit: source.rate_limit.as_ref().map(from_rate_limit),
        cors: source.cors.as_ref().map(from_cors),
        created_at: source.created_at.clone(),
        updated_at: source.updated_at.clone(),
    }
}

/// Domain → wire.
#[must_use]
pub fn from_plugin(source: &domain::Plugin) -> wire::PluginDto {
    wire::PluginDto {
        id: source.id,
        tenant_id: source.tenant_id,
        plugin_type: source.plugin_type.clone(),
        name: source.name.clone(),
        source: source.source.clone(),
        config: json_object(&source.config),
        gc_eligible_at: source.gc_eligible_at.clone(),
        created_at: source.created_at.clone(),
    }
}

/// Builds a domain upstream from a create request.
///
/// # Errors
///
/// Returns a validation error when the protocol identifier, endpoint schemes or
/// any nested auth, header, plugin, rate-limit or CORS configuration is
/// invalid.
pub fn to_upstream(
    request: &wire::CreateUpstreamRequest,
    id: Uuid,
    tenant_id: Uuid,
) -> Result<domain::Upstream, DomainError> {
    Ok(domain::Upstream {
        id,
        tenant_id,
        enabled: request.enabled,
        alias: request.alias.clone().unwrap_or_default(),
        tags: request.tags.clone(),
        server: to_server(&request.server)?,
        protocol: parse_protocol(&request.protocol)?,
        auth: request.auth.as_ref().map(to_auth).transpose()?,
        headers: request.headers.as_ref().map(to_headers).transpose()?,
        plugins: request.plugins.as_ref().map(to_plugins).transpose()?,
        rate_limit: request.rate_limit.as_ref().map(to_rate_limit).transpose()?,
        cors: request.cors.as_ref().map(to_cors).transpose()?,
        created_at: None,
        updated_at: None,
    })
}

/// Builds a domain upstream from a replace request.
///
/// # Errors
///
/// Returns a validation error when a field the request overrides (protocol,
/// server, auth, headers, plugins, rate limit or CORS) fails to convert; fields
/// the request omits keep the base upstream's values and cannot fail.
pub fn to_upstream_update(
    request: &wire::UpdateUpstreamRequest,
    base: &domain::Upstream,
) -> Result<domain::Upstream, DomainError> {
    Ok(domain::Upstream {
        id: base.id,
        tenant_id: base.tenant_id,
        enabled: request.enabled.unwrap_or(base.enabled),
        alias: base.alias.clone(),
        tags: request.tags.clone().unwrap_or_else(|| base.tags.clone()),
        server: match &request.server {
            Some(server) => to_server(server)?,
            None => base.server.clone(),
        },
        protocol: match &request.protocol {
            Some(protocol) => parse_protocol(protocol)?,
            None => base.protocol,
        },
        auth: match &request.auth {
            Some(auth) => Some(to_auth(auth)?),
            None => base.auth.clone(),
        },
        headers: match &request.headers {
            Some(headers) => Some(to_headers(headers)?),
            None => base.headers.clone(),
        },
        plugins: match &request.plugins {
            Some(plugins) => Some(to_plugins(plugins)?),
            None => base.plugins.clone(),
        },
        rate_limit: match &request.rate_limit {
            Some(rate_limit) => Some(to_rate_limit(rate_limit)?),
            None => base.rate_limit.clone(),
        },
        cors: match &request.cors {
            Some(cors) => Some(to_cors(cors)?),
            None => base.cors.clone(),
        },
        created_at: base.created_at.clone(),
        updated_at: base.updated_at.clone(),
    })
}

/// Builds a domain route from a create request.
///
/// # Errors
///
/// Returns a validation error when the match rule is not exactly one of `http`
/// or `grpc`, or when a nested plugin, rate-limit or CORS configuration is
/// invalid.
pub fn to_route(
    request: &wire::CreateRouteRequest,
    id: Uuid,
    tenant_id: Uuid,
) -> Result<domain::Route, DomainError> {
    Ok(domain::Route {
        id,
        tenant_id,
        upstream_id: request.upstream_id,
        enabled: request.enabled,
        tags: request.tags.clone(),
        match_rule: to_match(&request.match_rule)?,
        plugins: request.plugins.as_ref().map(to_plugins).transpose()?,
        rate_limit: request.rate_limit.as_ref().map(to_rate_limit).transpose()?,
        cors: request.cors.as_ref().map(to_cors).transpose()?,
        created_at: None,
        updated_at: None,
    })
}

/// Builds a domain route from a replace request.
///
/// # Errors
///
/// Returns a validation error when a field the request overrides (match rule,
/// plugins, rate limit or CORS) fails to convert; fields the request omits keep
/// the base route's values and cannot fail.
pub fn to_route_update(
    request: &wire::UpdateRouteRequest,
    base: &domain::Route,
) -> Result<domain::Route, DomainError> {
    Ok(domain::Route {
        id: base.id,
        tenant_id: base.tenant_id,
        upstream_id: base.upstream_id,
        enabled: request.enabled.unwrap_or(base.enabled),
        tags: request.tags.clone().unwrap_or_else(|| base.tags.clone()),
        match_rule: match &request.match_rule {
            Some(match_rule) => to_match(match_rule)?,
            None => base.match_rule.clone(),
        },
        plugins: match &request.plugins {
            Some(plugins) => Some(to_plugins(plugins)?),
            None => base.plugins.clone(),
        },
        rate_limit: match &request.rate_limit {
            Some(rate_limit) => Some(to_rate_limit(rate_limit)?),
            None => base.rate_limit.clone(),
        },
        cors: match &request.cors {
            Some(cors) => Some(to_cors(cors)?),
            None => base.cors.clone(),
        },
        created_at: base.created_at.clone(),
        updated_at: base.updated_at.clone(),
    })
}

/// Domain → the replacement request a `PUT` echoes back.
#[must_use]
pub fn upstream_to_update_request(source: &domain::Upstream) -> wire::UpdateUpstreamRequest {
    wire::UpdateUpstreamRequest {
        enabled: Some(source.enabled),
        tags: Some(source.tags.clone()),
        server: Some(from_server(&source.server)),
        protocol: Some(source.protocol.gts_id().to_owned()),
        auth: source.auth.as_ref().map(from_auth),
        headers: source.headers.as_ref().map(from_headers),
        plugins: source.plugins.as_ref().map(from_plugins),
        rate_limit: source.rate_limit.as_ref().map(from_rate_limit),
        cors: source.cors.as_ref().map(from_cors),
    }
}

/// Domain → the replacement request a `PUT` echoes back.
#[must_use]
pub fn route_to_update_request(source: &domain::Route) -> wire::UpdateRouteRequest {
    wire::UpdateRouteRequest {
        enabled: Some(source.enabled),
        tags: Some(source.tags.clone()),
        match_rule: Some(from_match(&source.match_rule)),
        plugins: source.plugins.as_ref().map(from_plugins),
        rate_limit: source.rate_limit.as_ref().map(from_rate_limit),
        cors: source.cors.as_ref().map(from_cors),
    }
}

/// Builds a domain plugin from a create request.
///
/// # Errors
///
/// Returns a validation error when `name` is blank or when `plugin_type` is not
/// one of `auth_plugin`, `guard_plugin` or `transform_plugin`.
pub fn to_plugin(
    request: &wire::CreatePluginRequest,
    id: Uuid,
    tenant_id: Uuid,
) -> Result<domain::Plugin, DomainError> {
    if request.name.trim().is_empty() {
        return Err(DomainError::Validation(
            "a plugin requires a non-empty `name`".into(),
        ));
    }
    if !matches!(
        request.plugin_type.as_str(),
        "auth_plugin" | "guard_plugin" | "transform_plugin"
    ) {
        return Err(DomainError::Validation(format!(
            "plugin_type must be auth_plugin, guard_plugin or transform_plugin, not `{}`",
            request.plugin_type
        )));
    }
    Ok(domain::Plugin {
        id,
        tenant_id,
        plugin_type: request.plugin_type.clone(),
        name: request.name.clone(),
        source: request.source.clone(),
        config: json_value(&request.config),
        gc_eligible_at: None,
        created_at: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(scheme: &str, host: &str, port: u16) -> wire::EndpointDto {
        wire::EndpointDto {
            scheme: scheme.to_owned(),
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn round_trips_an_upstream() {
        let request = wire::CreateUpstreamRequest {
            server: wire::ServerDto {
                endpoints: vec![endpoint("https", "api.openai.com", 443)],
            },
            protocol: crate::domain::gts_helpers::PROTOCOL_HTTP.to_owned(),
            ..wire::CreateUpstreamRequest::default()
        };
        let upstream = to_upstream(&request, Uuid::new_v4(), Uuid::new_v4()).expect("converts");
        let back = from_upstream(&upstream);
        assert_eq!(back.protocol, crate::domain::gts_helpers::PROTOCOL_HTTP);
        assert_eq!(back.server.endpoints.len(), 1);
        assert_eq!(back.server.endpoints[0].scheme, "https");
    }

    #[test]
    fn rejects_an_unknown_scheme() {
        let request = wire::EndpointDto {
            scheme: "ftp".to_owned(),
            host: "a.com".to_owned(),
            port: 21,
        };
        let error = to_endpoint(&request).expect_err("rejected");
        assert_eq!(error.status(), 400);
    }

    #[test]
    fn rejects_both_match_variants() {
        let dto = wire::MatchDto {
            http: Some(wire::HttpMatchDto {
                methods: vec!["GET".to_owned()],
                path: "/v1".to_owned(),
                query_allowlist: vec![],
                path_suffix_mode: "append".to_owned(),
            }),
            grpc: Some(wire::GrpcMatchDto {
                service: "svc".to_owned(),
                method: "Get".to_owned(),
            }),
        };
        let error = to_match(&dto).expect_err("rejected");
        assert_eq!(error.status(), 400);
    }
}
