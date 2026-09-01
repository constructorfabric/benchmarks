//! Write-path validation for upstreams, routes and plugins.
//!
//! Implements the constraints published in `schemas/upstream.v1.schema.json`
//! and `schemas/route.v1.schema.json`, the alias enforcement rules (DESIGN
//! §3.2) and the SSRF/policy gates that the gear configuration enables.

use crate::config::OagwConfig;
use crate::domain::error::{DomainError, DomainResult};
use crate::domain::model::{
    CorsConfig, Endpoint, HttpMatch, MatchConfig, PluginBinding, PluginsConfig, Route, Upstream,
};

/// Serde default helper for optional booleans that default to `true`.
#[must_use]
pub const fn default_true() -> bool {
    true
}

/// Valid tag pattern: `^[a-z0-9_-]+$`.
fn validate_tag(tag: &str) -> DomainResult<()> {
    let valid = !tag.is_empty()
        && tag
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(DomainError::validation(format!(
            "tag {tag:?} must match ^[a-z0-9_-]+$"
        )))
    }
}

fn validate_tags(tags: &[String]) -> DomainResult<()> {
    for tag in tags {
        validate_tag(tag)?;
    }
    Ok(())
}

/// Validates one endpoint against the gear configuration.
///
/// # Errors
///
/// Returns a validation error when the scheme is not allowed by the plaintext
/// upstream policy, the host is not a valid hostname/IP, or the port is out of
/// range.
pub fn validate_endpoint(endpoint: &Endpoint, config: &OagwConfig) -> DomainResult<()> {
    if endpoint.scheme.is_plaintext() && !config.plaintext_upstreams_allowed() {
        return Err(DomainError::validation(format!(
            "endpoint scheme {:?} requires allow_http_upstream to be enabled",
            endpoint.scheme.as_str()
        )));
    }
    let host =
        crate::domain::alias::validate_hostname(&endpoint.host).map_err(DomainError::validation)?;
    if host != endpoint.host {
        return Err(DomainError::validation(format!(
            "endpoint host must be normalized (lowercase, no trailing dot): {:?}",
            endpoint.host
        )));
    }
    if endpoint.port == 0 {
        return Err(DomainError::validation(
            "endpoint port must be between 1 and 65535",
        ));
    }
    crate::infra::ssrf::check_host(&endpoint.host, config)?;
    Ok(())
}

fn validate_endpoint_pool(endpoints: &[Endpoint], config: &OagwConfig) -> DomainResult<()> {
    if endpoints.is_empty() {
        return Err(DomainError::validation(
            "upstream must declare at least one endpoint",
        ));
    }
    if endpoints.len() > 64 {
        return Err(DomainError::validation(
            "upstream endpoint pool is limited to 64 endpoints",
        ));
    }
    let first = &endpoints[0];
    for endpoint in endpoints {
        validate_endpoint(endpoint, config)?;
        if endpoint.scheme != first.scheme || endpoint.port != first.port {
            return Err(DomainError::validation(
                "all endpoints of an upstream must share the same scheme and port",
            ));
        }
    }
    Ok(())
}

/// Validates the plugin chain shape (references and contiguity).
///
/// # Errors
///
/// Returns a validation error when a reference is malformed or positions are
/// not contiguous from zero.
pub fn validate_plugins(plugins: &PluginsConfig) -> DomainResult<()> {
    for (position, binding) in plugins.items.iter().enumerate() {
        validate_plugin_binding(binding, position)?;
    }
    Ok(())
}

fn validate_plugin_binding(binding: &PluginBinding, position: usize) -> DomainResult<()> {
    if binding.plugin_ref.trim().is_empty() {
        return Err(DomainError::validation(format!(
            "plugins.items[{position}] must carry a plugin_ref"
        )));
    }
    if let Some(uuid) = binding.plugin_uuid {
        let parsed = binding
            .plugin_ref
            .rsplit('~')
            .next()
            .and_then(|part| uuid::Uuid::parse_str(part).ok());
        if parsed != Some(uuid) {
            return Err(DomainError::validation(
                "plugin_uuid must match the instance part of plugin_ref",
            ));
        }
    }
    Ok(())
}

/// Validates the alias for a new or updated upstream.
///
/// Returns the normalized alias.
///
/// # Errors
///
/// Returns a validation error when the alias violates the endpoint-type rules
/// documented in DESIGN §3.2 "Alias Enforcement Rules".
pub fn validate_alias(provided: Option<&str>, endpoints: &[Endpoint]) -> DomainResult<String> {
    let derived = crate::domain::alias::compute_derived_alias(endpoints);
    let Some(provided) = provided.map(str::trim).filter(|value| !value.is_empty()) else {
        return match derived {
            crate::domain::alias::DerivedAlias::Derived(value) => Ok(value),
            crate::domain::alias::DerivedAlias::NotDerivable => Err(DomainError::validation(
                "alias is required for IP-based or non-derivable endpoint sets",
            )),
        };
    };
    let normalized_input = crate::domain::alias::normalize_alias(provided);
    if !normalized_input
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
    {
        return Err(DomainError::validation(format!(
            "alias {provided:?} must match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$"
        )));
    }
    if !normalized_input
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == ':' || c == '-')
    {
        return Err(DomainError::validation(format!(
            "alias {provided:?} must match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$"
        )));
    }
    let last_char = normalized_input.chars().next_back();
    if !last_char.is_some_and(|last| last.is_ascii_lowercase() || last.is_ascii_digit()) {
        return Err(DomainError::validation(format!(
            "alias {provided:?} must end with an alphanumeric character"
        )));
    }
    let normalized = normalized_input;
    match derived {
        crate::domain::alias::DerivedAlias::Derived(value) => {
            if normalized == value {
                Ok(normalized)
            } else {
                Err(DomainError::validation(format!(
                    "alias {provided:?} does not match the value derived from the endpoints ({value:?})"
                )))
            }
        }
        crate::domain::alias::DerivedAlias::NotDerivable => Ok(normalized),
    }
}

/// Validates an upstream write.
///
/// # Errors
///
/// Aggregates every validation failure of the upstream body.
pub fn validate_upstream_body(upstream: &Upstream, config: &OagwConfig) -> DomainResult<String> {
    validate_endpoint_pool(&upstream.server.endpoints, config)?;
    validate_tags(&upstream.tags)?;
    validate_plugins(&upstream.plugins)?;
    if let Some(cors) = &upstream.cors {
        validate_cors(cors)?;
    }
    if let Some(limit) = &upstream.rate_limit {
        validate_rate_limit(limit)?;
    }
    validate_alias(Some(upstream.alias.as_str()), &upstream.server.endpoints)
}

fn validate_rate_limit(limit: &crate::domain::model::RateLimitConfig) -> DomainResult<()> {
    if limit.sustained.rate == 0 {
        return Err(DomainError::validation(
            "rate_limit.sustained.rate must be at least 1",
        ));
    }
    if limit.cost == 0 {
        return Err(DomainError::validation(
            "rate_limit.cost must be at least 1",
        ));
    }
    if let Some(capacity) = limit.burst.capacity
        && capacity == 0
    {
        return Err(DomainError::validation(
            "rate_limit.burst.capacity must be at least 1",
        ));
    }
    Ok(())
}

fn validate_cors(cors: &crate::domain::model::CorsConfig) -> DomainResult<()> {
    if cors.allow_credentials && cors.allowed_origins.iter().any(|origin| origin == "*") {
        return Err(DomainError::validation(
            "cors.allow_credentials cannot be combined with the wildcard origin '*'",
        ));
    }
    for origin in &cors.allowed_origins {
        if origin == "*" {
            continue;
        }
        let Some((scheme, authority)) = origin.split_once("://") else {
            return Err(DomainError::validation(format!(
                "cors.allowed_origins entry {origin:?} must be an exact origin (scheme://host[:port]) or '*'"
            )));
        };
        if !matches!(scheme, "http" | "https") {
            return Err(DomainError::validation(format!(
                "cors.allowed_origins entry {origin:?} must use the http or https scheme"
            )));
        }
        // Port-sensitive matching (ADR-0004): the host is validated apart from
        // the optional `:port` suffix, which must be a valid port number.
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        };
        if host.is_empty()
            || crate::domain::alias::validate_hostname(host).is_err()
            || port.is_some_and(|port| port.is_empty() || port.parse::<u16>().is_err())
        {
            return Err(DomainError::validation(format!(
                "cors.allowed_origins entry {origin:?} must be an exact origin (scheme://host[:port]) or '*'"
            )));
        }
    }
    for method in &cors.allowed_methods {
        if crate::domain::model::HttpMethod::parse(method.as_str()).is_none()
            && !matches!(method.as_str(), "HEAD" | "OPTIONS")
        {
            return Err(DomainError::validation(format!(
                "cors.allowed_methods entry {method:?} is not a supported HTTP method"
            )));
        }
    }
    Ok(())
}

fn validate_http_match(
    match_rule: &HttpMatch,
    protocol: crate::domain::model::Protocol,
) -> DomainResult<()> {
    if matches!(protocol, crate::domain::model::Protocol::Grpc) {
        return Err(DomainError::validation(
            "route with a grpc upstream protocol must declare a grpc match rule",
        ));
    }
    if match_rule.methods.is_empty() {
        return Err(DomainError::validation(
            "match.http.methods must declare at least one method",
        ));
    }
    if match_rule.path.is_empty() {
        return Err(DomainError::validation("match.http.path must not be empty"));
    }
    if !match_rule.path.starts_with('/') {
        return Err(DomainError::validation(
            "match.http.path must start with '/'",
        ));
    }
    for param in &match_rule.query_allowlist {
        if param.trim().is_empty() {
            return Err(DomainError::validation(
                "match.http.query_allowlist entries must not be empty",
            ));
        }
    }
    Ok(())
}

fn validate_match(
    match_rule: &MatchConfig,
    protocol: crate::domain::model::Protocol,
) -> DomainResult<()> {
    match match_rule {
        MatchConfig::Http(http) => validate_http_match(http, protocol),
        MatchConfig::Grpc(grpc) => {
            if matches!(protocol, crate::domain::model::Protocol::Http) {
                return Err(DomainError::validation(
                    "route with an http upstream protocol must declare an http match rule",
                ));
            }
            if grpc.service.trim().is_empty() || grpc.method.trim().is_empty() {
                return Err(DomainError::validation(
                    "match.grpc.service and match.grpc.method must not be empty",
                ));
            }
            Ok(())
        }
    }
}

/// Validates a new route body against its upstream.
///
/// # Errors
///
/// Returns a validation error for schema violations, protocol/match mismatch or
/// an unknown upstream.
pub fn validate_route_body(
    route: &Route,
    upstream: Option<&Upstream>,
    config: &OagwConfig,
    require_enabled_upstream: bool,
) -> DomainResult<()> {
    let Some(upstream) = upstream else {
        if route.upstream_id.is_none() {
            return Err(DomainError::validation(
                "route.upstream_id is required and must reference an own upstream",
            ));
        }
        return Err(DomainError::not_found(
            "upstream",
            route
                .upstream_id
                .map_or_else(|| "unknown".to_owned(), |id| id.to_string()),
        ));
    };
    if require_enabled_upstream && !upstream.enabled {
        return Err(DomainError::validation(
            "routes cannot be attached to a disabled upstream",
        ));
    }
    validate_tags(&route.tags)?;
    validate_match(&route.r#match, upstream.protocol)?;
    validate_plugins(&route.plugins)?;
    if let Some(limit) = &route.rate_limit {
        validate_rate_limit(limit)?;
    }
    if let Some(cors) = &route.cors {
        validate_cors(&CorsConfig::from(cors))?;
    }
    let _ = config;
    Ok(())
}

/// Validates a custom-plugin write (DESIGN §3.4 POST `/plugins`).
///
/// Plugins are immutable after creation, carry a tenant-unique name and, when
/// they are custom, must ship Starlark source.
///
/// # Errors
///
/// Returns a validation error for an unknown type category, a missing name or
/// source, or a config schema that is not a JSON object.
pub fn validate_plugin_body(plugin: &crate::domain::model::Plugin) -> DomainResult<()> {
    if !matches!(plugin.plugin_type.as_str(), "auth" | "guard" | "transform") {
        return Err(DomainError::validation(format!(
            "plugin_type must be one of 'auth', 'guard' or 'transform', got {:?}",
            plugin.plugin_type
        )));
    }
    let name = plugin.name.trim();
    if name.is_empty() || name.len() > 128 {
        return Err(DomainError::validation(
            "plugin.name must be between 1 and 128 characters",
        ));
    }
    if name
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.'))
    {
        return Err(DomainError::validation(
            "plugin.name may only contain letters, digits, '-', '_' and '.'",
        ));
    }
    if let Some(schema) = &plugin.config_schema
        && !schema.is_object()
    {
        return Err(DomainError::validation(
            "plugin.config_schema must be a JSON object",
        ));
    }
    let empty_source = plugin
        .source_code
        .as_ref()
        .is_none_or(|source| source.trim().is_empty());
    if empty_source {
        return Err(DomainError::validation(
            "custom plugins must carry non-empty Starlark source_code",
        ));
    }
    Ok(())
}

/// `true` when two match rules select the same request key.
///
/// Route-match determinism (DESIGN §3.6) forbids two enabled routes under the
/// same upstream from sharing `(path prefix, methods)` — the caller adds the
/// priority term.
#[must_use]
pub fn same_match_key(left: &MatchConfig, right: &MatchConfig) -> bool {
    match (left, right) {
        (MatchConfig::Http(a), MatchConfig::Http(b)) => {
            a.path == b.path && a.methods.iter().any(|method| b.methods.contains(method))
        }
        (MatchConfig::Grpc(a), MatchConfig::Grpc(b)) => {
            a.service == b.service && a.method == b.method
        }
        _ => false,
    }
}

/// Human-readable rendering of a match rule, used in conflict messages.
#[must_use]
pub fn describe_match(match_rules: &MatchConfig) -> String {
    match match_rules {
        MatchConfig::Http(http) => format!(
            "{} {}",
            http.methods
                .iter()
                .map(|method| method.as_str())
                .collect::<Vec<_>>()
                .join("|"),
            http.path
        ),
        MatchConfig::Grpc(grpc) => format!("{}::{}", grpc.service, grpc.method),
    }
}

/// Validates an upstream update against the previously stored entity.
///
/// # Errors
///
/// Returns a validation error when the alias is immutable for the new endpoint
/// set or the update would break an existing bind.
pub fn validate_upstream_update(existing: &Upstream, next: &Upstream) -> DomainResult<()> {
    let derived_next = crate::domain::alias::compute_derived_alias(&next.server.endpoints);
    let derived_existing = crate::domain::alias::compute_derived_alias(&existing.server.endpoints);
    if derived_next != derived_existing {
        return Err(DomainError::validation(
            "upstream alias is immutable: the endpoint change would change the derived alias; delete and re-create the upstream",
        ));
    }
    // DESIGN §3.2 "Alias Update Behavior": an alias that differs from the
    // stored one is never accepted, including for non-derivable (IP) pools.
    let normalized_existing = crate::domain::alias::normalize_alias(&existing.alias);
    let normalized_next = crate::domain::alias::normalize_alias(&next.alias);
    if normalized_next != normalized_existing {
        return Err(DomainError::validation(format!(
            "upstream alias is immutable: {existing:?} cannot be renamed to {next:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, Protocol, Scheme, ServerConfig, Upstream};

    fn http_endpoint(host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: Scheme::Http,
            host: host.to_owned(),
            port,
        }
    }

    fn upstream(endpoints: Vec<Endpoint>) -> Upstream {
        Upstream {
            id: uuid::Uuid::new_v4(),
            tenant_id: uuid::Uuid::new_v4(),
            alias: "alias".to_owned(),
            protocol: Protocol::Http,
            enabled: true,
            server: ServerConfig { endpoints },
            auth: crate::domain::model::AuthConfig::default(),
            headers: crate::domain::model::HeadersConfig::default(),
            rate_limit: None,
            cors: None,
            plugins: crate::domain::model::PluginsConfig::default(),
            tags: Vec::new(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn plaintext_schemes_require_the_config_flag() {
        let config = OagwConfig::default();
        let error = validate_endpoint(&http_endpoint("mock.internal", 8080), &config)
            .expect_err("rejected");
        assert_eq!(error.status_code(), 400);

        let permissive = OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        };
        validate_endpoint(&http_endpoint("mock.internal", 8080), &permissive).expect("accepted");
    }

    #[test]
    fn endpoint_pools_must_share_scheme_and_port() {
        let permissive = OagwConfig {
            allow_http_upstream: true,
            ..OagwConfig::default()
        };
        let mixed = vec![
            http_endpoint("a.internal", 8080),
            http_endpoint("b.internal", 9090),
        ];
        assert!(validate_endpoint_pool(&mixed, &permissive).is_err());
        let same = vec![
            http_endpoint("a.internal", 8080),
            http_endpoint("b.internal", 8080),
        ];
        validate_endpoint_pool(&same, &permissive).expect("accepted");
        assert!(validate_endpoint_pool(&[], &permissive).is_err());
    }

    #[test]
    fn hostname_endpoints_reject_user_alias_mismatch() {
        let endpoints = vec![http_endpoint("api.openai.com", 80)];
        assert!(validate_alias(Some("other"), &endpoints).is_err());
        assert_eq!(
            validate_alias(Some("api.openai.com"), &endpoints).expect("derived"),
            "api.openai.com"
        );
        assert_eq!(
            validate_alias(None, &endpoints).expect("derived"),
            "api.openai.com"
        );
    }

    #[test]
    fn ip_endpoints_require_an_explicit_alias() {
        let endpoints = vec![http_endpoint("127.0.0.1", 8080)];
        assert!(validate_alias(None, &endpoints).is_err());
        assert_eq!(
            validate_alias(Some("My-Service."), &endpoints).expect("normalized"),
            "my-service"
        );
    }

    #[test]
    fn route_match_must_agree_with_the_upstream_protocol() {
        let config = OagwConfig::default();
        let target = upstream(vec![http_endpoint("api.openai.com", 80)]);
        let mut route = Route {
            id: uuid::Uuid::new_v4(),
            tenant_id: uuid::Uuid::new_v4(),
            upstream_id: Some(target.id),
            r#match: MatchConfig::Grpc(crate::domain::model::GrpcMatch {
                service: "svc".to_owned(),
                method: "Get".to_owned(),
            }),
            priority: 0,
            enabled: true,
            rate_limit: None,
            cors: None,
            plugins: crate::domain::model::PluginsConfig::default(),
            tags: Vec::new(),
            created_at: 0,
            updated_at: 0,
        };
        assert!(validate_route_body(&route, Some(&target), &config, true).is_err());
        route.r#match = MatchConfig::Http(crate::domain::model::HttpMatch {
            methods: vec![crate::domain::model::HttpMethod::Get],
            path: "/v1".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
        });
        validate_route_body(&route, Some(&target), &config, true).expect("accepted");
        assert!(validate_route_body(&route, None, &config, true).is_err());
    }

    #[test]
    fn wildcard_origin_with_credentials_is_rejected() {
        let cors = crate::domain::model::CorsConfig {
            allow_credentials: true,
            allowed_origins: vec!["*".to_owned()],
            ..Default::default()
        };
        assert!(validate_cors(&cors).is_err());
    }

    #[test]
    fn invalid_tags_are_rejected() {
        assert!(validate_tags(&["valid_tag".to_owned()]).is_ok());
        assert!(validate_tags(&["Invalid Tag".to_owned()]).is_err());
    }

    #[test]
    fn plugin_uuid_must_match_the_reference() {
        let binding = PluginBinding {
            plugin_ref: crate::domain::model::gts::instance(
                crate::domain::model::gts::GUARD_PLUGIN,
                &uuid::Uuid::new_v4(),
            ),
            plugin_uuid: Some(uuid::Uuid::new_v4()),
            config: serde_json::Value::Null,
        };
        let plugins = PluginsConfig {
            sharing: crate::domain::model::Sharing::Private,
            items: vec![binding],
        };
        assert!(validate_plugins(&plugins).is_err());
    }

    #[test]
    fn upstream_update_rejects_alias_changes() {
        let existing = upstream(vec![http_endpoint("api.openai.com", 80)]);
        let mut next = existing.clone();
        next.server.endpoints = vec![http_endpoint("other.internal", 80)];
        assert!(validate_upstream_update(&existing, &next).is_err());
    }
}
