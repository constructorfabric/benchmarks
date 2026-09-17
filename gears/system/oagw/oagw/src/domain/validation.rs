//! Configuration validation (DESIGN "Body Validation Rules", schemas).

use crate::domain::alias;
use crate::domain::error::{OagwError, OagwResult};
use crate::domain::gts_helpers;
use crate::domain::model::{CorsConfig, Endpoint, EndpointScheme, PluginInput, PluginType, RouteInput, UpstreamInput};

/// Validates an upstream create/replace payload.
///
/// # Errors
///
/// [`OagwError::Validation`] describing the first problem found.
pub fn validate_upstream(input: &UpstreamInput, allow_http: bool) -> OagwResult<()> {
    if input.server.endpoints.is_empty() {
        return Err(OagwError::Validation(
            "server.endpoints must contain at least one endpoint".to_owned(),
        ));
    }
    for endpoint in &input.server.endpoints {
        validate_endpoint(endpoint, allow_http)?;
    }

    if input.protocol != gts_helpers::PROTOCOL_HTTP && input.protocol != gts_helpers::PROTOCOL_GRPC {
        return Err(OagwError::Validation(format!(
            "protocol '{}' is not a supported OAGW protocol identifier",
            input.protocol
        )));
    }

    if let Some(auth) = &input.auth {
        if auth.plugin_type.trim().is_empty() {
            return Err(OagwError::Validation(
                "auth.type must be an auth plugin GTS identifier".to_owned(),
            ));
        }
        if gts_helpers::plugin_kind(&auth.plugin_type) != Some("auth") {
            return Err(OagwError::Validation(format!(
                "auth.type '{}' is not an auth plugin identifier",
                auth.plugin_type
            )));
        }
    }

    validate_tags(&input.tags)?;
    if let Some(rate) = &input.rate_limit {
        validate_rate_limit(rate)?;
    }
    if let Some(cors) = &input.cors {
        validate_cors(cors)?;
    }

    // Alias: explicit must be valid; absent requires a derivable endpoint set.
    match &input.alias {
        Some(explicit) => {
            let normalized = alias::normalize(explicit);
            if !alias::is_valid_alias(&normalized) {
                return Err(OagwError::Validation(format!(
                    "alias '{explicit}' is not a valid alias"
                )));
            }
        }
        None => {
            match alias::derive_alias(&input.server.endpoints)? {
                Some(_) => {}
                None => {
                    return Err(OagwError::Validation(
                        "alias is required: endpoints are IP-based or not derivable".to_owned(),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Validates a single endpoint.
///
/// # Errors
///
/// [`OagwError::Validation`] when the scheme/host/port combination is invalid.
pub fn validate_endpoint(endpoint: &Endpoint, allow_http: bool) -> OagwResult<()> {
    if endpoint.host.trim().is_empty() {
        return Err(OagwError::Validation("endpoint.host is required".to_owned()));
    }
    let host = alias::normalize(&endpoint.host);
    if !alias::is_ip(&host) && !is_hostname(&host) {
        return Err(OagwError::Validation(format!(
            "endpoint host '{}' is not a hostname or IP address",
            endpoint.host
        )));
    }
    if matches!(endpoint.scheme, EndpointScheme::Http) && !allow_http {
        return Err(OagwError::Validation(
            "scheme 'http' is not allowed: allow_http_upstream is disabled".to_owned(),
        ));
    }
    if endpoint.port == 0 {
        return Err(OagwError::Validation("endpoint.port must be >= 1".to_owned()));
    }
    Ok(())
}

fn is_hostname(host: &str) -> bool {
    // RFC 1123: labels of 1..=63 alnum/hyphen chars, no leading/trailing hyphen,
    // total length <= 253.
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        if label.is_empty() || label.len() > 63 {
            return false;
        }
        let bytes = label.as_bytes();
        if bytes[0] == b'-' || bytes[label.len() - 1] == b'-' {
            return false;
        }
        bytes.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-')
    })
}

/// Validates a route create/replace payload.
///
/// # Errors
///
/// [`OagwError::Validation`] describing the first problem found.
pub fn validate_route(input: &RouteInput) -> OagwResult<()> {
    if input.upstream_id.is_nil() {
        return Err(OagwError::Validation(
            "upstream_id must reference an existing upstream".to_owned(),
        ));
    }
    validate_tags(&input.tags)?;

    let http = input.r#match.http.as_ref();
    let grpc = input.r#match.grpc.as_ref();
    match (http, grpc) {
        (Some(_), Some(_)) => {
            return Err(OagwError::Validation(
                "match must contain exactly one of {http, grpc}".to_owned(),
            ));
        }
        (None, None) => {
            return Err(OagwError::Validation(
                "match must contain http or grpc rules".to_owned(),
            ));
        }
        (Some(http), None) => {
            if http.methods.is_empty() {
                return Err(OagwError::Validation(
                    "match.http.methods must list at least one method".to_owned(),
                ));
            }
            if http.path.is_empty() {
                return Err(OagwError::Validation(
                    "match.http.path must not be empty".to_owned(),
                ));
            }
            if !http.path.starts_with('/') {
                return Err(OagwError::Validation(
                    "match.http.path must start with '/'".to_owned(),
                ));
            }
        }
        (None, Some(grpc)) => {
            if grpc.service.is_empty() || grpc.method.is_empty() {
                return Err(OagwError::Validation(
                    "match.grpc requires both service and method".to_owned(),
                ));
            }
        }
    }

    for item in &input.plugins.items {
        if item.id().trim().is_empty() {
            return Err(OagwError::Validation(
                "plugins.items must not contain empty identifiers".to_owned(),
            ));
        }
    }
    if let Some(rate) = &input.rate_limit {
        validate_rate_limit(rate)?;
    }
    Ok(())
}

/// Validates a plugin create payload.
///
/// # Errors
///
/// [`OagwError::Validation`] describing the first problem found.
pub fn validate_plugin(input: &PluginInput) -> OagwResult<()> {
    if input.name.trim().is_empty() {
        return Err(OagwError::Validation("name must not be empty".to_owned()));
    }
    if input.source_code.trim().is_empty() {
        return Err(OagwError::Validation(
            "source_code must not be empty for custom plugins".to_owned(),
        ));
    }
    if !matches!(input.plugin_type, PluginType::Auth | PluginType::Guard | PluginType::Transform) {
        return Err(OagwError::Validation("plugin_type is not supported".to_owned()));
    }
    Ok(())
}

/// Validates tag values against `^[a-z0-9_-]+$`.
///
/// # Errors
///
/// [`OagwError::Validation`] on the first invalid tag.
pub fn validate_tags(tags: &[String]) -> OagwResult<()> {
    for tag in tags {
        let valid = !tag.is_empty()
            && tag.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        if !valid {
            return Err(OagwError::Validation(format!(
                "tag '{tag}' must match ^[a-z0-9_-]+$"
            )));
        }
    }
    Ok(())
}

/// Validates a rate-limit configuration (ADR-0003).
///
/// # Errors
///
/// [`OagwError::Validation`] when the configuration is not usable.
pub fn validate_rate_limit(config: &crate::domain::model::RateLimitConfig) -> OagwResult<()> {
    if config.sustained.rate == 0 {
        return Err(OagwError::Validation(
            "rate_limit.sustained.rate must be >= 1".to_owned(),
        ));
    }
    if config.capacity() == 0 {
        return Err(OagwError::Validation(
            "rate_limit.burst.capacity must be >= 1".to_owned(),
        ));
    }
    if config.cost == 0 {
        return Err(OagwError::Validation("rate_limit.cost must be >= 1".to_owned()));
    }
    if config.algorithm == crate::domain::model::RateAlgorithm::SlidingWindow {
        return Err(OagwError::Validation(
            "rate_limit.algorithm 'sliding_window' is not implemented; use token_bucket"
                .to_owned(),
        ));
    }
    Ok(())
}

/// Validates a CORS configuration (ADR-0004).
///
/// # Errors
///
/// [`OagwError::Validation`] when `allow_credentials` is combined with a
/// wildcard origin or when an origin is not a valid origin.
pub fn validate_cors(config: &CorsConfig) -> OagwResult<()> {
    if config.allow_credentials && config.allowed_origins.iter().any(|o| o == "*") {
        return Err(OagwError::Validation(
            "cors.allow_credentials cannot be combined with the wildcard origin '*'".to_owned(),
        ));
    }
    for origin in &config.allowed_origins {
        if origin == "*" {
            continue;
        }
        let parsed = url::Url::parse(origin)
            .ok()
            .filter(|u| u.host_str().is_some() && !u.path().is_empty());
        if parsed.is_none() {
            return Err(OagwError::Validation(format!(
                "cors origin '{origin}' is not a valid origin"
            )));
        }
    }
    for method in &config.allowed_methods {
        if crate::domain::model::HttpMethod::parse(&http::Method::from_bytes(method.as_bytes()).unwrap_or(http::Method::GET))
            .is_none()
            && method != "HEAD"
            && method != "OPTIONS"
        {
            return Err(OagwError::Validation(format!(
                "cors method '{method}' is not allowed"
            )));
        }
    }
    Ok(())
}

/// Validates that a plugin reference resolves to a builtin or a custom plugin.
///
/// # Errors
///
/// [`OagwError::Validation`] when the reference is neither a GTS builtin id nor
/// a UUID.
pub fn validate_plugin_reference(id: &str) -> OagwResult<()> {
    if gts_helpers::plugin_kind(id).is_some() {
        return Ok(());
    }
    if uuid::Uuid::parse_str(id).is_ok() {
        return Ok(());
    }
    Err(OagwError::Validation(format!(
        "plugin reference '{id}' is neither a builtin GTS id nor a plugin UUID"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, MatchRule, UpstreamServer};
    use uuid::Uuid;

    fn https(host: &str) -> Endpoint {
        Endpoint {
            scheme: EndpointScheme::Https,
            host: host.to_owned(),
            port: 443,
        }
    }

    fn valid_input() -> UpstreamInput {
        UpstreamInput {
            server: UpstreamServer {
                endpoints: vec![https("api.openai.com")],
            },
            ..UpstreamInput::default()
        }
    }

    #[test]
    fn accepts_derivable_upstream() {
        assert!(validate_upstream(&valid_input(), false).is_ok());
    }

    #[test]
    fn rejects_empty_endpoints() {
        let mut input = valid_input();
        input.server.endpoints.clear();
        assert!(validate_upstream(&input, false).is_err());
    }

    #[test]
    fn rejects_http_when_disallowed() {
        let mut input = valid_input();
        input.server.endpoints[0] = Endpoint {
            scheme: EndpointScheme::Http,
            host: "localhost".to_owned(),
            port: 8080,
        };
        input.alias = Some("localhost:8080".to_owned());
        let err = validate_upstream(&input, false).unwrap_err();
        assert!(err.detail().contains("allow_http_upstream"));
        assert!(validate_upstream(&input, true).is_ok());
    }

    #[test]
    fn rejects_bad_protocol() {
        let mut input = valid_input();
        input.protocol = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.ftp.v1".to_owned();
        assert!(validate_upstream(&input, false).is_err());
    }

    #[test]
    fn rejects_route_without_match() {
        let input = RouteInput {
            upstream_id: Uuid::new_v4(),
            r#match: MatchRule {
                http: None,
                grpc: None,
            },
            ..RouteInput::default()
        };
        assert!(validate_route(&input).is_err());
    }

    #[test]
    fn rejects_cors_credentials_with_wildcard() {
        let cors = CorsConfig {
            enabled: true,
            allowed_origins: vec!["*".to_owned()],
            allow_credentials: true,
            ..CorsConfig::default()
        };
        assert!(validate_cors(&cors).is_err());
    }
}
