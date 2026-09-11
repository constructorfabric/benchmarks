//! Schema-faithful validation of upstream and route bodies.
//!
//! The `JSON` Schemas in `docs/schemas/` are enforced structurally by serde
//! (`deny_unknown_fields`, closed enums, required members); this module adds
//! the semantic rules the schemas express only in prose: hostname shape,
//! header-name/value safety, tags, `CORS` contradictions and match sanity.

use crate::domain::alias;
use crate::domain::model::{HttpMatch, MatchRules, Route, Upstream};
use crate::domain::repo::ControlPlaneError;

/// Maximum number of tags accepted on one resource.
const MAX_TAGS: usize = 32;

/// Validates a header name.
///
/// # Errors
///
/// Returns a reason when the name is not a valid `HTTP` header name.
pub fn validate_header_name(name: &str) -> Result<(), ControlPlaneError> {
    if name.is_empty() {
        return Err(ControlPlaneError::Validation("header name is empty".to_owned()));
    }
    if name.len() > 128 {
        return Err(ControlPlaneError::Validation("header name is too long".to_owned()));
    }
    if !name.bytes().all(is_token_byte) {
        return Err(ControlPlaneError::Validation(format!(
            "'{name}' is not a valid header name"
        )));
    }
    Ok(())
}

/// Validates a header value.
///
/// # Errors
///
/// Returns a reason when the value contains control characters or a CRLF
/// sequence.
pub fn validate_header_value(name: &str, value: &str) -> Result<(), ControlPlaneError> {
    if value.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err(ControlPlaneError::Validation(format!(
            "value of header '{name}' contains control characters"
        )));
    }
    Ok(())
}

fn is_token_byte(b: u8) -> bool {
    matches!(b,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.'
        | b'^' | b'_' | b'`' | b'|' | b'~'
        | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z')
}

/// Validates the tag list.
///
/// # Errors
///
/// Returns a reason when a tag does not match `^[a-z0-9_-]+$`.
pub fn validate_tags(tags: &[String]) -> Result<(), ControlPlaneError> {
    if tags.len() > MAX_TAGS {
        return Err(ControlPlaneError::Validation("too many tags".to_owned()));
    }
    for tag in tags {
        if tag.is_empty() || !tag.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-') {
            return Err(ControlPlaneError::Validation(format!(
                "tag '{tag}' must match ^[a-z0-9_-]+$"
            )));
        }
    }
    Ok(())
}

/// Validates the request-side header rule set.
///
/// # Errors
///
/// Returns a reason for invalid header names or values.
pub fn validate_header_rules(
    set: &crate::domain::model::MapCodec,
    add: &crate::domain::model::MapCodec,
    remove: &[String],
) -> Result<(), ControlPlaneError> {
    for (name, value) in set {
        validate_header_name_value(name, value)?;
    }
    for (name, value) in add {
        validate_header_name_value(name, value)?;
    }
    for name in remove {
        validate_header_name(name)?;
    }
    Ok(())
}

/// Validates the response-side header rule set, whose `add` carries a list of
/// values per name.
///
/// # Errors
///
/// Returns a reason for invalid header names or values.
pub fn validate_response_header_rules(
    set: &crate::domain::model::MapCodec,
    add: &crate::domain::model::VecMapCodec,
    remove: &[String],
) -> Result<(), ControlPlaneError> {
    for (name, value) in set {
        validate_header_name_value(name, value)?;
    }
    for name in add.keys() {
        validate_header_name(name)?;
    }
    for name in remove {
        validate_header_name(name)?;
    }
    Ok(())
}

fn validate_header_name_value(name: &str, value: &str) -> Result<(), ControlPlaneError> {
    validate_header_name(name)?;
    validate_header_value(name, value)
}

/// Validates an upstream body.
///
/// # Errors
///
/// Returns [`ControlPlaneError::Validation`] with a human-readable reason.
///
/// `allow_http` mirrors the gear's `allow_http_upstream` setting: the `http`
/// scheme is always a legal *field value*, but a plaintext dial is only
/// permitted when the deployment opted in.
pub fn validate_upstream(
    upstream: &Upstream,
    allow_http: bool,
) -> Result<(), ControlPlaneError> {
    if upstream.server.endpoints.is_empty() {
        return Err(ControlPlaneError::Validation(
            "server.endpoints requires at least one endpoint".to_owned(),
        ));
    }
    for endpoint in &upstream.server.endpoints {
        if endpoint.port == 0 {
            return Err(ControlPlaneError::Validation(
                "endpoint port must be between 1 and 65535".to_owned(),
            ));
        }
        if endpoint.host.is_empty() {
            return Err(ControlPlaneError::Validation(
                "endpoint host is required".to_owned(),
            ));
        }
        let host = endpoint.host.trim().trim_end_matches('.');
        if !alias::is_ip_literal(host) {
            alias::validate_hostname(host).map_err(ControlPlaneError::Validation)?;
        }
        if endpoint.scheme == crate::domain::model::EndpointScheme::Http && !allow_http {
            return Err(ControlPlaneError::Validation(
                "endpoint scheme 'http' is rejected because allow_http_upstream is disabled"
                    .to_owned(),
            ));
        }
    }
    validate_endpoint_pool(&upstream.server.endpoints)?;
    validate_tags(&upstream.tags)?;
    validate_plugin_bindings(&upstream.plugins)?;

    if let Some(rules) = &upstream.headers {
        if let Some(request) = &rules.request {
            validate_header_rules(&request.set, &request.add, &request.remove)?;
            for name in &request.passthrough_allowlist {
                validate_header_name(name)?;
            }
        }
        if let Some(response) = &rules.response {
            validate_response_header_rules(&response.set, &response.add, &response.remove)?;
        }
    }

    if let Some(cors) = &upstream.cors {
        let wildcard = cors
            .allowed_origins
            .iter()
            .any(|origin| origin == "*");
        if cors.allow_credentials && wildcard {
            return Err(ControlPlaneError::Validation(
                "cors.allow_credentials cannot be combined with a wildcard origin".to_owned(),
            ));
        }
        for origin in &cors.allowed_origins {
            if origin == "*" {
                continue;
            }
            if url::Url::parse(origin).is_err() {
                return Err(ControlPlaneError::Validation(format!(
                    "cors.allowed_origins entry '{origin}' is not a valid URI"
                )));
            }
        }
        for method in &cors.allowed_methods {
            if !matches!(
                method.as_str(),
                "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
            ) {
                return Err(ControlPlaneError::Validation(format!(
                    "cors.allowed_methods entry '{method}' is not supported"
                )));
            }
        }
    }

    Ok(())
}

/// Validates that every endpoint of a pool shares scheme and port.
///
/// # Errors
///
/// Returns a reason when the pool is heterogeneous.
pub fn validate_endpoint_pool(
    endpoints: &[crate::domain::model::Endpoint],
) -> Result<(), ControlPlaneError> {
    let Some(first) = endpoints.first() else {
        return Ok(());
    };
    for endpoint in endpoints.iter().skip(1) {
        if endpoint.scheme != first.scheme {
            return Err(ControlPlaneError::Validation(
                "all endpoints of an upstream must share the same scheme".to_owned(),
            ));
        }
        if endpoint.port != first.port {
            return Err(ControlPlaneError::Validation(
                "all endpoints of an upstream must share the same port".to_owned(),
            ));
        }
    }
    Ok(())
}

/// Validates the plugin binding list.
///
/// # Errors
///
/// Returns a reason when positions are declared partially or are not
/// contiguous from zero.
pub fn validate_plugin_bindings(
    plugins: &crate::domain::model::PluginsConfig,
) -> Result<(), ControlPlaneError> {
    let declared: Vec<Option<u32>> = plugins.items.iter().map(crate::domain::model::PluginBinding::position).collect();
    let any = declared.iter().any(Option::is_some);
    let all = declared.iter().all(Option::is_some);
    if any && !all {
        return Err(ControlPlaneError::Validation(
            "plugins.items positions must be declared on every binding".to_owned(),
        ));
    }
    if all {
        let mut positions: Vec<u32> = declared.into_iter().flatten().collect();
        positions.sort_unstable();
        for (index, position) in positions.iter().enumerate() {
            if *position != u32::try_from(index).unwrap_or(u32::MAX) {
                return Err(ControlPlaneError::Validation(
                    "plugins.items positions must be contiguous starting at 0".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

/// Validates an `HTTP` match.
///
/// # Errors
///
/// Returns a reason for empty method lists, empty paths and unsupported
/// methods.
pub fn validate_http_match(match_rules: &HttpMatch) -> Result<(), ControlPlaneError> {
    if match_rules.methods.is_empty() {
        return Err(ControlPlaneError::Validation(
            "match.http.methods requires at least one method".to_owned(),
        ));
    }
    for method in &match_rules.methods {
        if method.as_str().is_empty() {
            return Err(ControlPlaneError::Validation(
                "match.http.methods entries must not be empty".to_owned(),
            ));
        }
    }
    if match_rules.path.is_empty() {
        return Err(ControlPlaneError::Validation(
            "match.http.path must not be empty".to_owned(),
        ));
    }
    if !match_rules.path.starts_with('/') {
        return Err(ControlPlaneError::Validation(
            "match.http.path must start with '/'".to_owned(),
        ));
    }
    Ok(())
}

/// Validates a route.
///
/// # Errors
///
/// Returns a reason for invalid matches and references.
pub fn validate_route(route: &Route) -> Result<(), ControlPlaneError> {
    validate_tags(&route.tags)?;
    validate_plugin_bindings(&route.plugins)?;
    if route.upstream_id.is_empty() {
        return Err(ControlPlaneError::Validation(
            "upstream_id is required".to_owned(),
        ));
    }
    let http_present = route.match_rules.http.is_some();
    let grpc_present = route.match_rules.grpc.is_some();
    if http_present == grpc_present {
        return Err(ControlPlaneError::Validation(
            "match must carry exactly one of http or grpc".to_owned(),
        ));
    }
    if let Some(http) = &route.match_rules.http {
        validate_http_match(http)?;
    }
    if let Some(grpc) = &route.match_rules.grpc
        && (grpc.service.is_empty() || grpc.method.is_empty())
    {
        return Err(ControlPlaneError::Validation(
            "match.grpc.service and match.grpc.method are required".to_owned(),
        ));
    }
    Ok(())
}

/// Returns the route path for matching, or `None` for a `gRPC` route.
#[must_use]
pub fn route_path(match_rules: &MatchRules) -> Option<&str> {
    match_rules.http.as_ref().map(|http| http.path.as_str())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::domain::model::{
        CorsConfig, EndpointScheme, GrpcMatch, HttpMethod, PluginsConfig, Protocol,
        RequestHeaderRules, ResponseHeaderRules, ServerConfig,
    };
    use crate::domain::repo::ControlPlaneError;
    use serde_json::json;

    fn endpoint(scheme: EndpointScheme, host: &str, port: u16) -> crate::domain::model::Endpoint {
        crate::domain::model::Endpoint { scheme, host: host.to_owned(), port }
    }

    fn upstream(endpoints: Vec<crate::domain::model::Endpoint>) -> Upstream {
        Upstream {
            id: None,
            enabled: true,
            alias: None,
            tags: Vec::new(),
            server: ServerConfig { endpoints },
            protocol: Protocol::Http,
            auth: None,
            headers: None,
            plugins: PluginsConfig::default(),
            rate_limit: None,
            cors: None,
        }
    }

    fn cors_config() -> CorsConfig {
        CorsConfig {
            sharing: crate::domain::model::Sharing::default(),
            enabled: true,
            allowed_origins: Vec::new(),
            allowed_methods: Vec::new(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }

    fn route(match_rules: MatchRules) -> Route {
        Route {
            id: None,
            upstream_id: "gts.cf.core.oagw.upstream.v1~00000000-0000-0000-0000-000000000001"
                .to_owned(),
            tags: Vec::new(),
            match_rules,
            plugins: PluginsConfig::default(),
            rate_limit: None,
            enabled: true,
        }
    }

    fn http_match(methods: &[HttpMethod], path: &str) -> MatchRules {
        MatchRules {
            http: Some(HttpMatch {
                methods: methods.to_vec(),
                path: path.to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
            }),
            grpc: None,
        }
    }

    fn validation_message(error: ControlPlaneError) -> String {
        match error {
            ControlPlaneError::Validation(message) => message,
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    #[test]
    fn header_names_are_token_checked() {
        assert!(validate_header_name("x-request-id").is_ok());
        assert!(validate_header_name("X-Request-ID_1.~").is_ok());
        assert!(validate_header_name("").is_err());
        assert!(validate_header_name("has space").is_err());
        assert!(validate_header_name("with\rcr").is_err());
        assert!(validate_header_name("\u{e9}").is_err());
        assert!(validate_header_name(&"a".repeat(129)).is_err());
    }

    #[test]
    fn header_values_reject_control_characters() {
        assert!(validate_header_value("x-a", "ok value").is_ok());
        assert!(validate_header_value("x-a", "bad\nvalue").is_err());
        assert!(validate_header_value("x-a", "bad\tvalue").is_err());
        assert!(validate_header_value("x-a", "\u{7f}").is_err());
    }

    #[test]
    fn tags_match_the_documented_pattern() {
        assert!(validate_tags(&["prod".to_owned(), "tier-1".to_owned(), "a_b".to_owned()]).is_ok());
        assert!(validate_tags(&[String::new()]).is_err());
        assert!(validate_tags(&["Prod".to_owned()]).is_err());
        assert!(validate_tags(&["a b".to_owned()]).is_err());
        let many = vec!["t".to_owned(); 33];
        assert!(validate_tags(&many).is_err());
    }

    #[test]
    fn an_upstream_needs_endpoints() {
        let error = validation_message(validate_upstream(&upstream(Vec::new()), true).unwrap_err());
        assert!(error.contains("at least one endpoint"), "{error}");
    }

    #[test]
    fn endpoint_hosts_and_ports_are_validated() {
        assert!(validate_upstream(&upstream(vec![endpoint(EndpointScheme::Https, "api.openai.com", 443)]), false).is_ok());
        assert!(validate_upstream(&upstream(vec![endpoint(EndpointScheme::Https, "api.openai.com.", 443)]), false).is_ok());
        assert!(validate_upstream(&upstream(vec![endpoint(EndpointScheme::Https, "10.0.0.1", 443)]), false).is_ok());

        let message = validation_message(
            validate_upstream(&upstream(vec![endpoint(EndpointScheme::Https, "api.openai.com", 0)]), false)
                .unwrap_err(),
        );
        assert!(message.contains("between 1 and 65535"), "{message}");

        let message = validation_message(
            validate_upstream(&upstream(vec![endpoint(EndpointScheme::Https, "", 443)]), false)
                .unwrap_err(),
        );
        assert!(message.contains("host is required"), "{message}");

        let message = validation_message(
            validate_upstream(&upstream(vec![endpoint(EndpointScheme::Https, "-bad-.example", 443)]), false)
                .unwrap_err(),
        );
        assert!(!message.is_empty());
    }

    #[test]
    fn the_http_scheme_is_a_legal_field_value_gated_by_configuration() {
        // `http` is always a legal field value: with the deployment opt-in it
        // validates, without it the schema rejects the plaintext dial.
        assert!(validate_upstream(&upstream(vec![endpoint(EndpointScheme::Http, "127.0.0.1", 8080)]), true).is_ok());
        let message = validation_message(
            validate_upstream(&upstream(vec![endpoint(EndpointScheme::Http, "127.0.0.1", 8080)]), false)
                .unwrap_err(),
        );
        assert!(message.contains("allow_http_upstream is disabled"), "{message}");
    }

    #[test]
    fn an_endpoint_pool_must_be_homogeneous() {
        let mixed_scheme = upstream(vec![
            endpoint(EndpointScheme::Https, "a.example.com", 443),
            endpoint(EndpointScheme::Http, "b.example.com", 443),
        ]);
        let message = validation_message(validate_upstream(&mixed_scheme, true).unwrap_err());
        assert!(message.contains("same scheme"), "{message}");

        let mixed_port = upstream(vec![
            endpoint(EndpointScheme::Https, "a.example.com", 443),
            endpoint(EndpointScheme::Https, "b.example.com", 8443),
        ]);
        let message = validation_message(validate_upstream(&mixed_port, false).unwrap_err());
        assert!(message.contains("same port"), "{message}");

        let pool = upstream(vec![
            endpoint(EndpointScheme::Https, "a.example.com", 443),
            endpoint(EndpointScheme::Https, "b.example.com", 443),
        ]);
        assert!(validate_upstream(&pool, false).is_ok());
    }

    #[test]
    fn plugin_positions_must_be_complete_and_contiguous() {
        let mut config = upstream(vec![endpoint(EndpointScheme::Https, "a.example.com", 443)]);
        config.plugins.items = vec![
            crate::domain::model::PluginBinding::Ref("gts.cf.core.oagw.plugin.v1~abc".to_owned()),
            crate::domain::model::PluginBinding::Bound {
                plugin_ref: "gts.cf.core.oagw.plugin.v1~def".to_owned(),
                config: json!({}),
                position: Some(1),
            },
        ];
        let message = validation_message(validate_upstream(&config, false).unwrap_err());
        assert!(message.contains("declared on every binding"), "{message}");

        config.plugins.items = vec![
            crate::domain::model::PluginBinding::Bound {
                plugin_ref: "gts.cf.core.oagw.plugin.v1~abc".to_owned(),
                config: json!({}),
                position: Some(1),
            },
            crate::domain::model::PluginBinding::Bound {
                plugin_ref: "gts.cf.core.oagw.plugin.v1~def".to_owned(),
                config: json!({}),
                position: Some(2),
            },
        ];
        let message = validation_message(validate_upstream(&config, false).unwrap_err());
        assert!(message.contains("contiguous starting at 0"), "{message}");

        config.plugins.items = vec![
            crate::domain::model::PluginBinding::Bound {
                plugin_ref: "gts.cf.core.oagw.plugin.v1~abc".to_owned(),
                config: json!({}),
                position: Some(0),
            },
            crate::domain::model::PluginBinding::Bound {
                plugin_ref: "gts.cf.core.oagw.plugin.v1~def".to_owned(),
                config: json!({}),
                position: Some(1),
            },
        ];
        assert!(validate_upstream(&config, false).is_ok());
    }

    #[test]
    fn cors_contradictions_are_rejected() {
        let mut config = upstream(vec![endpoint(EndpointScheme::Https, "a.example.com", 443)]);
        config.cors = Some(CorsConfig {
            allowed_origins: vec!["*".to_owned()],
            allow_credentials: true,
            ..cors_config()
        });
        let message = validation_message(validate_upstream(&config, false).unwrap_err());
        assert!(message.contains("wildcard origin"), "{message}");

        config.cors = Some(CorsConfig {
            enabled: true,
            allowed_origins: vec!["not a uri".to_owned()],
            ..cors_config()
        });
        let message = validation_message(validate_upstream(&config, false).unwrap_err());
        assert!(message.contains("valid URI"), "{message}");

        config.cors = Some(CorsConfig {
            enabled: true,
            allowed_methods: vec!["TRACE".to_owned()],
            ..cors_config()
        });
        let message = validation_message(validate_upstream(&config, false).unwrap_err());
        assert!(message.contains("not supported"), "{message}");
    }

    #[test]
    fn header_rules_are_validated_on_both_sides() {
        let request_rules = RequestHeaderRules {
            add: [("'bad name'".to_owned(), "value".to_owned())].into_iter().collect(),
            ..RequestHeaderRules::default()
        };
        let message = match validate_header_rules(&request_rules.set, &request_rules.add, &request_rules.remove) {
            Err(ControlPlaneError::Validation(message)) => message,
            other => panic!("expected a validation error, got {other:?}"),
        };
        assert!(message.contains("valid header name"), "{message}");

        let response_rules = ResponseHeaderRules {
            add: [("valid".to_owned(), vec!["value".to_owned()])].into_iter().collect(),
            ..ResponseHeaderRules::default()
        };
        assert!(validate_response_header_rules(&response_rules.set, &response_rules.add, &[]).is_ok());
        let response_rules = ResponseHeaderRules {
            add: [("bad name".to_owned(), vec!["value".to_owned()])].into_iter().collect(),
            ..ResponseHeaderRules::default()
        };
        assert!(validate_response_header_rules(&response_rules.set, &response_rules.add, &[]).is_err());
    }

    #[test]
    fn http_matches_need_methods_and_a_rooted_path() {
        let with_get = http_match(&[HttpMethod::Get], "/v1");
        assert!(validate_http_match(with_get.http.as_ref().expect("an http match")).is_ok());

        let empty_methods = http_match(&[], "/v1");
        let message = validation_message(validate_http_match(
            empty_methods.http.as_ref().expect("an http match"),
        )
        .unwrap_err());
        assert!(message.contains("at least one method"), "{message}");

        let relative = http_match(&[HttpMethod::Get], "v1");
        let message = validation_message(validate_http_match(
            relative.http.as_ref().expect("an http match"),
        )
        .unwrap_err());
        assert!(message.contains("must start with '/'"), "{message}");
    }

    #[test]
    fn a_route_matches_exactly_one_protocol() {
        assert!(validate_route(&route(http_match(&[HttpMethod::Get], "/v1"))).is_ok());

        let neither = route(MatchRules { http: None, grpc: None });
        let message = validation_message(validate_route(&neither).unwrap_err());
        assert!(message.contains("exactly one of http or grpc"), "{message}");

        let both = route(MatchRules {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: "/v1".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
            }),
            grpc: Some(GrpcMatch { service: "svc".to_owned(), method: "m".to_owned() }),
        });
        let message = validation_message(validate_route(&both).unwrap_err());
        assert!(message.contains("exactly one of http or grpc"), "{message}");

        let grpc = route(MatchRules {
            http: None,
            grpc: Some(GrpcMatch { service: String::new(), method: "m".to_owned() }),
        });
        let message = validation_message(validate_route(&grpc).unwrap_err());
        assert!(message.contains("service and match.grpc.method are required"), "{message}");

        let unlinked = route(http_match(&[HttpMethod::Get], "/v1"));
        let message = validation_message(validate_route(&Route {
            upstream_id: String::new(),
            ..unlinked
        })
        .unwrap_err());
        assert!(message.contains("upstream_id is required"), "{message}");
    }

    #[test]
    fn route_path_extracts_the_http_path() {
        assert_eq!(route_path(&http_match(&[HttpMethod::Get], "/v1")), Some("/v1"));
        assert_eq!(
            route_path(&MatchRules { http: None, grpc: Some(GrpcMatch { service: "s".to_owned(), method: "m".to_owned() }) }),
            None
        );
    }
}
