//! Field-level route schema validation
//! (`cpt-cf-oagw-dod-route-schema-validation`,
//! `cpt-cf-oagw-algo-route-schema-validation`).
//!
//! Parses a create/replace request body against
//! [`crate::domain::model::RouteRequest`], the strongly-typed mirror of
//! `route.v1.schema.json`. Every nested type derives
//! `#[serde(deny_unknown_fields)]`, so an unknown property
//! (`additionalProperties: false`) or an out-of-enum value (including
//! `match.http.methods` and `match.http.path_suffix_mode`) is already
//! rejected by `serde` itself. This module adds the checks `serde` cannot
//! express as types: `upstream_id`/`match` presence (parameterized by
//! caller, since `upstream_id` is required on create but retainable on
//! replace), the `match` "exactly one of `http`/`grpc`" rule, `minItems`/
//! `minLength` bounds, and the tag pattern.
//!
//! See `crate::domain::service`'s module doc for why
//! `clippy::result_large_err` is allowed here: `OagwError` is returned
//! unboxed everywhere in this crate, including the handler layer.
#![allow(clippy::result_large_err)]

use serde_json::Value;

use super::alias::matches_tag_pattern;
use super::model::{GrpcMatch, HttpMatch, MatchConfig, RouteRequest};
use crate::error::OagwError;

/// Parses and validates a create/replace route request body
/// (`cpt-cf-oagw-algo-route-schema-validation`).
///
/// `require_upstream_id` is `true` for create (`upstream_id` is required)
/// and `false` for replace (an omitted `upstream_id` is resolved by the
/// caller via `cpt-cf-oagw-algo-upstream-id-immutability-check` instead).
///
/// # Errors
///
/// Returns [`OagwError::validation_error`] when the body fails to parse
/// against the schema mirror, or fails a semantic check `serde` cannot
/// express: a missing `upstream_id` (when required) or `match`, `match`
/// carrying both or neither of `http`/`grpc`, an `http` branch whose
/// `methods` is empty or whose `path` is empty, a `grpc` branch whose
/// `service` or `method` is empty, or a tag violating the declared pattern.
/// Every violation found is listed together in a single error.
pub fn parse_route_request(
    body: Value,
    require_upstream_id: bool,
) -> Result<RouteRequest, OagwError> {
    // @cpt-begin:cpt-cf-oagw-dod-route-schema-validation:p1:inst-schema-route-parse-01
    let request: RouteRequest = serde_json::from_value(body).map_err(|error| {
        OagwError::validation_error(format!("request validation failed: {error}"))
    })?;
    // @cpt-end:cpt-cf-oagw-dod-route-schema-validation:p1:inst-schema-route-parse-01

    let errors = validate_route_semantics(&request, require_upstream_id);
    if !errors.is_empty() {
        return Err(OagwError::validation_error(format!(
            "request validation failed: {}",
            errors.join("; ")
        )));
    }

    Ok(request)
}

/// Collects every semantic violation `serde`'s type-level parsing cannot
/// express, returning an empty vector when the request is otherwise valid.
fn validate_route_semantics(request: &RouteRequest, require_upstream_id: bool) -> Vec<String> {
    let mut errors = Vec::new();

    if require_upstream_id && request.upstream_id.is_none() {
        errors.push("upstream_id: field is required".to_owned());
    }

    // @cpt-begin:cpt-cf-oagw-dod-exactly-one-match:p1:inst-exactly-one-validate-01
    match &request.match_config {
        None => errors.push("match: field is required".to_owned()),
        Some(match_config) => validate_match(match_config, &mut errors),
    }
    // @cpt-end:cpt-cf-oagw-dod-exactly-one-match:p1:inst-exactly-one-validate-01

    for tag in &request.tags {
        if !matches_tag_pattern(tag) {
            errors.push(format!(
                "tags: '{tag}' does not match pattern ^[a-z0-9_-]+$"
            ));
        }
    }

    validate_rate_limit(request, &mut errors);

    errors
}

/// Enforces "exactly one of `http` or `grpc`"
/// (`cpt-cf-oagw-algo-exactly-one-match-enforcement`), then validates
/// whichever single branch is present.
fn validate_match(match_config: &MatchConfig, errors: &mut Vec<String>) {
    match (&match_config.http, &match_config.grpc) {
        (None, None) => {
            errors.push("match: exactly one of 'http' or 'grpc' is required".to_owned());
        }
        (Some(_), Some(_)) => {
            errors
                .push("match: exactly one of 'http' or 'grpc' may be present, not both".to_owned());
        }
        // @cpt-begin:cpt-cf-oagw-dod-http-match-validation:p1:inst-schema-http-validate-01
        (Some(http), None) => validate_http_match(http, errors),
        // @cpt-end:cpt-cf-oagw-dod-http-match-validation:p1:inst-schema-http-validate-01
        // @cpt-begin:cpt-cf-oagw-dod-grpc-match-validation:p2:inst-schema-grpc-validate-01
        (None, Some(grpc)) => validate_grpc_match(grpc, errors),
        // @cpt-end:cpt-cf-oagw-dod-grpc-match-validation:p2:inst-schema-grpc-validate-01
    }
}

/// Validates `http_match.methods`' `minItems: 1` and `http_match.path`'s
/// `minLength: 1` (`cpt-cf-oagw-dod-http-match-validation`); the method
/// enum itself and `path_suffix_mode`'s closed set are already enforced by
/// `serde` via [`super::model::RouteMethod`] and
/// [`super::model::PathSuffixMode`].
fn validate_http_match(http: &HttpMatch, errors: &mut Vec<String>) {
    if http.methods.is_empty() {
        errors.push("match.http.methods: must contain at least 1 item".to_owned());
    }
    if http.path.is_empty() {
        errors.push("match.http.path: must have a minimum length of 1".to_owned());
    }
}

/// Validates `grpc_match.service` and `grpc_match.method`'s `minLength: 1`
/// (`cpt-cf-oagw-dod-grpc-match-validation`); their presence is already
/// enforced by `serde` since both are non-`Option` fields on
/// [`GrpcMatch`].
fn validate_grpc_match(grpc: &GrpcMatch, errors: &mut Vec<String>) {
    if grpc.service.is_empty() {
        errors.push("match.grpc.service: must have a minimum length of 1".to_owned());
    }
    if grpc.method.is_empty() {
        errors.push("match.grpc.method: must have a minimum length of 1".to_owned());
    }
}

fn validate_rate_limit(request: &RouteRequest, errors: &mut Vec<String>) {
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

#[cfg(test)]
mod tests {
    use super::parse_route_request;
    use serde_json::json;

    fn http_match(path: &str) -> serde_json::Value {
        json!({"http": {"methods": ["GET"], "path": path}})
    }

    #[test]
    fn accepts_a_minimal_http_route() {
        let body = json!({
            "upstream_id": "11111111-1111-1111-1111-111111111111",
            "match": http_match("/v1/widgets"),
        });
        let request = parse_route_request(body, true).expect("must parse");
        assert_eq!(
            request.upstream_id.unwrap().to_string(),
            "11111111-1111-1111-1111-111111111111"
        );
    }

    #[test]
    fn rejects_a_body_omitting_both_upstream_id_and_match_naming_both() {
        let body = json!({});
        let error = parse_route_request(body, true).expect_err("must reject");
        let detail = error.to_problem().detail;
        assert!(detail.contains("upstream_id"), "detail was: {detail}");
        assert!(detail.contains("match"), "detail was: {detail}");
    }

    #[test]
    fn rejects_a_match_carrying_both_http_and_grpc() {
        let body = json!({
            "upstream_id": "11111111-1111-1111-1111-111111111111",
            "match": {
                "http": {"methods": ["GET"], "path": "/v1/widgets"},
                "grpc": {"service": "foo.v1.Svc", "method": "Get"},
            },
        });
        let error = parse_route_request(body, true).expect_err("must reject");
        assert!(error.to_problem().detail.contains("match"));
    }

    #[test]
    fn rejects_a_match_carrying_neither_http_nor_grpc() {
        let body = json!({
            "upstream_id": "11111111-1111-1111-1111-111111111111",
            "match": {},
        });
        let error = parse_route_request(body, true).expect_err("must reject");
        assert!(error.to_problem().detail.contains("match"));
    }

    #[test]
    fn rejects_empty_http_methods() {
        let body = json!({
            "upstream_id": "11111111-1111-1111-1111-111111111111",
            "match": {"http": {"methods": [], "path": "/v1/widgets"}},
        });
        let error = parse_route_request(body, true).expect_err("must reject");
        assert!(error.to_problem().detail.contains("methods"));
    }

    #[test]
    fn rejects_a_method_outside_the_route_enum() {
        let body = json!({
            "upstream_id": "11111111-1111-1111-1111-111111111111",
            "match": {"http": {"methods": ["HEAD"], "path": "/v1/widgets"}},
        });
        assert!(parse_route_request(body, true).is_err());
    }

    #[test]
    fn rejects_an_empty_http_path() {
        let body = json!({
            "upstream_id": "11111111-1111-1111-1111-111111111111",
            "match": {"http": {"methods": ["GET"], "path": ""}},
        });
        let error = parse_route_request(body, true).expect_err("must reject");
        assert!(error.to_problem().detail.contains("path"));
    }

    #[test]
    fn defaults_query_allowlist_to_empty_and_path_suffix_mode_to_append() {
        let body = json!({
            "upstream_id": "11111111-1111-1111-1111-111111111111",
            "match": http_match("/v1/widgets"),
        });
        let request = parse_route_request(body, true).expect("must parse");
        let http = request
            .match_config
            .expect("match present")
            .http
            .expect("http present");
        assert!(http.query_allowlist.is_empty());
        assert_eq!(
            http.path_suffix_mode,
            super::super::model::PathSuffixMode::Append
        );
    }

    #[test]
    fn persists_an_explicit_disabled_path_suffix_mode_unchanged() {
        let body = json!({
            "upstream_id": "11111111-1111-1111-1111-111111111111",
            "match": {
                "http": {"methods": ["GET"], "path": "/v1/widgets", "path_suffix_mode": "disabled"},
            },
        });
        let request = parse_route_request(body, true).expect("must parse");
        let http = request
            .match_config
            .expect("match present")
            .http
            .expect("http present");
        assert_eq!(
            http.path_suffix_mode,
            super::super::model::PathSuffixMode::Disabled
        );
    }

    #[test]
    fn rejects_grpc_match_missing_service() {
        let body = json!({
            "upstream_id": "11111111-1111-1111-1111-111111111111",
            "match": {"grpc": {"method": "GetUser"}},
        });
        assert!(parse_route_request(body, true).is_err());
    }

    #[test]
    fn rejects_grpc_match_with_empty_service() {
        let body = json!({
            "upstream_id": "11111111-1111-1111-1111-111111111111",
            "match": {"grpc": {"service": "", "method": "GetUser"}},
        });
        let error = parse_route_request(body, true).expect_err("must reject");
        assert!(error.to_problem().detail.contains("service"));
    }

    #[test]
    fn rejects_grpc_match_with_empty_method() {
        let body = json!({
            "upstream_id": "11111111-1111-1111-1111-111111111111",
            "match": {"grpc": {"service": "foo.v1.Svc", "method": ""}},
        });
        let error = parse_route_request(body, true).expect_err("must reject");
        assert!(error.to_problem().detail.contains("method"));
    }

    #[test]
    fn accepts_a_valid_grpc_match() {
        let body = json!({
            "upstream_id": "11111111-1111-1111-1111-111111111111",
            "match": {"grpc": {"service": "foo.v1.UserService", "method": "GetUser"}},
        });
        assert!(parse_route_request(body, true).is_ok());
    }

    #[test]
    fn replace_does_not_require_upstream_id() {
        let body = json!({"match": http_match("/v1/widgets")});
        assert!(parse_route_request(body, false).is_ok());
    }

    #[test]
    fn rejects_an_unknown_top_level_property() {
        let body = json!({
            "upstream_id": "11111111-1111-1111-1111-111111111111",
            "match": http_match("/v1/widgets"),
            "unexpected": true,
        });
        assert!(parse_route_request(body, true).is_err());
    }
}
