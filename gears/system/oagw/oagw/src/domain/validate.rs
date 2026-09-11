//! Field-level schema validation
//! (`cpt-cf-oagw-dod-schema-validation`, `cpt-cf-oagw-algo-schema-validation`).
//!
//! Parses a create/replace request body against [`crate::domain::model`]'s
//! strongly-typed mirror of `upstream.v1.schema.json`. Every nested type
//! derives `#[serde(deny_unknown_fields)]`, so `serde`'s own deserialization
//! error already reports a missing required field, an unknown property
//! (`additionalProperties: false`), or an invalid enum variant (including
//! the `scheme` extension) with the offending field name in its message.
//! This module adds the handful of checks `serde` cannot express as types:
//! non-empty `server.endpoints`, host format, port range, tag/alias
//! patterns, and the CORS wildcard-plus-credentials conditional.
//!
//! See `crate::domain::service`'s module doc for why
//! `clippy::result_large_err` is allowed here: `OagwError` is returned
//! unboxed everywhere in this crate, including the handler layer.
#![allow(clippy::result_large_err)]

use super::model::UpstreamRequest;
use crate::domain::alias::{is_valid_host, matches_alias_pattern, matches_tag_pattern};
use crate::error::OagwError;

/// Parses and validates a create/replace request body
/// (`cpt-cf-oagw-algo-schema-validation`).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] when the body fails to parse
/// against the schema mirror, or fails a semantic check `serde` cannot
/// express (non-empty endpoints, host format, port range, patterns, or the
/// CORS wildcard-plus-credentials conditional).
pub fn parse_request(body: serde_json::Value) -> Result<UpstreamRequest, OagwError> {
    // @cpt-begin:cpt-cf-oagw-algo-schema-validation:p1:inst-schema-validate-parse-01
    let mut request: UpstreamRequest = serde_json::from_value(body).map_err(|error| {
        OagwError::validation_error(format!("request validation failed: {error}"))
    })?;
    // @cpt-end:cpt-cf-oagw-algo-schema-validation:p1:inst-schema-validate-parse-01

    // @cpt-begin:cpt-cf-oagw-algo-schema-validation:p1:inst-schema-validate-semantic-01
    let errors = validate_semantics(&request);
    if !errors.is_empty() {
        return Err(OagwError::validation_error(format!(
            "request validation failed: {}",
            errors.join("; ")
        )));
    }
    // @cpt-end:cpt-cf-oagw-algo-schema-validation:p1:inst-schema-validate-semantic-01

    // @cpt-begin:cpt-cf-oagw-dod-schema-validation:p1:inst-schema-validate-default-port-01
    resolve_ports(&mut request);
    // @cpt-end:cpt-cf-oagw-dod-schema-validation:p1:inst-schema-validate-default-port-01

    Ok(request)
}

/// Collects every semantic violation `serde`'s type-level parsing cannot
/// express, returning an empty vector when the request is otherwise valid.
fn validate_semantics(request: &UpstreamRequest) -> Vec<String> {
    let mut errors = Vec::new();

    if request.server.endpoints.is_empty() {
        errors.push("server.endpoints: must contain at least 1 item".to_owned());
    }
    for (index, endpoint) in request.server.endpoints.iter().enumerate() {
        if !is_valid_host(&endpoint.host) {
            errors.push(format!(
                "server.endpoints[{index}].host: '{}' is not a valid hostname, IPv4, or IPv6 address",
                endpoint.host
            ));
        }
        if endpoint.port == Some(0) {
            errors.push(format!(
                "server.endpoints[{index}].port: must be between 1 and 65535"
            ));
        }
    }

    for tag in &request.tags {
        if !matches_tag_pattern(tag) {
            errors.push(format!(
                "tags: '{tag}' does not match pattern ^[a-z0-9_-]+$"
            ));
        }
    }

    if let Some(alias) = &request.alias
        && !matches_alias_pattern(alias)
    {
        errors.push(format!(
            "alias: '{alias}' does not match pattern ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$"
        ));
    }

    validate_rate_limit(request, &mut errors);
    validate_cors(request, &mut errors);

    errors
}

fn validate_rate_limit(request: &UpstreamRequest, errors: &mut Vec<String>) {
    let Some(rate_limit) = &request.rate_limit else {
        return;
    };
    if rate_limit.sustained.rate < 1 {
        errors.push("rate_limit.sustained.rate: must be >= 1".to_owned());
    }
    if let Some(burst) = &rate_limit.burst
        && burst.capacity < 1
    {
        errors.push("rate_limit.burst.capacity: must be >= 1".to_owned());
    }
}

fn validate_cors(request: &UpstreamRequest, errors: &mut Vec<String>) {
    let Some(cors) = &request.cors else {
        return;
    };
    let has_wildcard_origin = cors
        .allowed_origins
        .iter()
        .any(|origin| origin == super::model::WILDCARD_ORIGIN);
    if cors.allow_credentials && has_wildcard_origin {
        errors.push(
            "cors: allow_credentials cannot be combined with a wildcard allowed_origins entry"
                .to_owned(),
        );
    }

    // CODE1-F-001: the schema's `allowed_origins` items are `oneOf [{"const":
    // "*"}, {"format": "uri"}]`; reject any entry that is neither.
    for (index, origin) in cors.allowed_origins.iter().enumerate() {
        if origin != super::model::WILDCARD_ORIGIN && url::Url::parse(origin).is_err() {
            errors.push(format!(
                "cors.allowed_origins[{index}]: '{origin}' must be '*' or an absolute URI"
            ));
        }
    }
}

/// Applies the per-`scheme` default port to every endpoint whose `port` was
/// omitted (`cpt-cf-oagw-dod-schema-validation`).
fn resolve_ports(request: &mut UpstreamRequest) {
    for endpoint in &mut request.server.endpoints {
        if endpoint.port.is_none() {
            endpoint.port = Some(endpoint.scheme.standard_port());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_request;
    use serde_json::json;

    #[test]
    fn accepts_an_http_scheme_upstream_with_default_port() {
        let body = json!({
            "server": {"endpoints": [{"scheme": "http", "host": "internal.example.com"}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        });
        let request = parse_request(body).expect("http scheme must be accepted");
        assert_eq!(request.server.endpoints[0].port, Some(80));
    }

    #[test]
    fn accepts_a_ws_scheme_upstream_with_default_port() {
        let body = json!({
            "server": {"endpoints": [{"scheme": "ws", "host": "internal.example.com"}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        });
        let request = parse_request(body).expect("ws scheme must be accepted");
        assert_eq!(request.server.endpoints[0].port, Some(80));
    }

    #[test]
    fn defaults_to_443_for_the_tls_family() {
        let body = json!({
            "server": {"endpoints": [{"scheme": "https", "host": "api.example.com"}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        });
        let request = parse_request(body).expect("https scheme must be accepted");
        assert_eq!(request.server.endpoints[0].port, Some(443));
    }

    #[test]
    fn rejects_a_scheme_outside_the_accepted_set() {
        let body = json!({
            "server": {"endpoints": [{"scheme": "ftp", "host": "api.example.com"}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        });
        assert!(parse_request(body).is_err());
    }

    #[test]
    fn rejects_a_missing_server_field() {
        let body = json!({"protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"});
        let error = parse_request(body).expect_err("missing server must be rejected");
        assert!(error.to_problem().detail.contains("server"));
    }

    #[test]
    fn rejects_a_missing_protocol_field() {
        let body = json!({
            "server": {"endpoints": [{"scheme": "https", "host": "api.example.com"}]},
        });
        let error = parse_request(body).expect_err("missing protocol must be rejected");
        assert!(error.to_problem().detail.contains("protocol"));
    }

    #[test]
    fn rejects_an_unknown_top_level_property() {
        let body = json!({
            "server": {"endpoints": [{"scheme": "https", "host": "api.example.com"}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "unexpected": true,
        });
        assert!(parse_request(body).is_err());
    }

    #[test]
    fn rejects_an_empty_endpoints_array() {
        let body = json!({
            "server": {"endpoints": []},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        });
        assert!(parse_request(body).is_err());
    }

    #[test]
    fn rejects_cors_wildcard_origin_with_credentials() {
        let body = json!({
            "server": {"endpoints": [{"scheme": "https", "host": "api.example.com"}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "cors": {"enabled": true, "allowed_origins": ["*"], "allow_credentials": true},
        });
        assert!(parse_request(body).is_err());
    }

    // CODE1-F-001: an `allowed_origins` entry that is neither `"*"` nor a
    // parseable absolute URI must be rejected.
    #[test]
    fn rejects_a_cors_allowed_origin_that_is_neither_wildcard_nor_a_valid_uri() {
        let body = json!({
            "server": {"endpoints": [{"scheme": "https", "host": "api.example.com"}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "cors": {"enabled": true, "allowed_origins": ["not a url"]},
        });
        assert!(parse_request(body).is_err());
    }

    #[test]
    fn accepts_cors_allowed_origins_that_are_valid_absolute_uris() {
        let body = json!({
            "server": {"endpoints": [{"scheme": "https", "host": "api.example.com"}]},
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "cors": {"enabled": true, "allowed_origins": ["https://app.example.com"]},
        });
        assert!(parse_request(body).is_ok());
    }
}
