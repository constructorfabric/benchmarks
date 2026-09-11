//! Route Match Shape and gRPC-Branch Validation
//! (`cpt-cf-oagw-algo-route-match-validate`).
//!
//! Accumulates every shape violation found in a create/replace request body
//! rather than stopping at the first (`route.v1.schema.json`'s `match`
//! `oneOf`, `http`/`grpc` sub-shapes, `tags[]` pattern, `plugins`,
//! `rate_limit`, and the schema-compatible `enabled`/`priority` additions),
//! and returns a normalized, defaulted field set on success.

use serde_json::Value;
use uuid::Uuid;

use crate::model::route::{GrpcMatch, HttpMatch, PathSuffixMode, RouteMatch, RoutePluginsBinding};
use crate::model::upstream::{BurstConfig, RateLimitConfig, Sharing};

/// One accumulated field-level violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldViolation {
    pub field: &'static str,
    pub message: String,
}

impl FieldViolation {
    fn new(field: &'static str, message: impl Into<String>) -> Self {
        Self {
            field,
            message: message.into(),
        }
    }
}

/// Render an accumulated violation list as one `detail` string for
/// `OagwError`'s `400 ValidationError` body
/// (`inst-match-validate-catch-return`).
#[must_use]
pub fn violations_detail(violations: &[FieldViolation]) -> String {
    violations
        .iter()
        .map(|v| format!("{}: {}", v.field, v.message))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The normalized, defaulted field set `cpt-cf-oagw-algo-route-match-validate`
/// returns on success (`inst-match-validate-return`). Deliberately excludes
/// `id`/`tenant_id`/`upstream_id`, which the calling flow (create/replace)
/// resolves separately.
#[derive(Debug, Clone)]
pub struct NormalizedRouteShape {
    pub tags: Vec<String>,
    pub route_match: RouteMatch,
    pub plugins: RoutePluginsBinding,
    pub rate_limit: Option<RateLimitConfig>,
    pub enabled: bool,
    pub priority: Option<i64>,
}

// @cpt-algo:cpt-cf-oagw-algo-route-match-validate:p2
// @cpt-dod:cpt-cf-oagw-dod-route-schema-validation:p1
// @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-try
/// Validate a create/replace request body, accumulating every violation
/// found rather than stopping at the first.
///
/// # Errors
/// Returns every accumulated [`FieldViolation`] when the body fails shape
/// validation.
pub fn validate_route_shape(body: &Value) -> Result<NormalizedRouteShape, Vec<FieldViolation>> {
    let mut violations = Vec::new();

    let route_match = validate_match(body, &mut violations);
    let tags = validate_tags(body, &mut violations);
    let plugins = validate_plugins(body, &mut violations);
    let rate_limit = validate_rate_limit(body, &mut violations);
    let enabled = validate_enabled(body, &mut violations);
    let priority = validate_priority(body, &route_match, &mut violations);

    if !violations.is_empty() {
        // @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-catch
        // @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-catch-return
        return Err(violations);
        // @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-catch-return
        // @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-catch
    }

    // A `cors` object, if the request body contains one, is accepted at the
    // shape-validation level above (the Route root object has no
    // `additionalProperties: false` constraint) but is never read out of
    // `body` into any field here -- `NormalizedRouteShape`, constructed
    // immediately below, carries no `cors` field at all, so an accepted
    // `cors` object is never persisted as route-level CORS configuration.
    // @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-cors-if
    // @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-cors-ignore
    // @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-return
    Ok(NormalizedRouteShape {
        tags,
        route_match,
        plugins,
        rate_limit,
        enabled,
        priority,
    })
    // @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-return
    // @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-cors-ignore
    // @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-cors-if
}
// @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-try

// @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-oneof
fn validate_match(body: &Value, violations: &mut Vec<FieldViolation>) -> RouteMatch {
    let Some(match_value) = body.get("match") else {
        violations.push(FieldViolation::new("match", "match is required"));
        return RouteMatch::default();
    };
    let Some(obj) = match_value.as_object() else {
        violations.push(FieldViolation::new("match", "match must be an object"));
        return RouteMatch::default();
    };

    if obj.keys().any(|k| k != "http" && k != "grpc") {
        violations.push(FieldViolation::new(
            "match",
            "match must not contain properties other than http or grpc",
        ));
    }
    // @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-oneof

    match (obj.get("http"), obj.get("grpc")) {
        (Some(_), Some(_)) => {
            violations.push(FieldViolation::new(
                "match",
                "match must contain exactly one of http or grpc, not both",
            ));
            RouteMatch::default()
        }
        (None, None) => {
            violations.push(FieldViolation::new(
                "match",
                "match must contain one of http or grpc",
            ));
            RouteMatch::default()
        }
        // @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-http-if
        (Some(http), None) => RouteMatch {
            http: Some(validate_http_match(http, violations)),
            grpc: None,
        },
        // @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-http-if
        // @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-grpc-else
        (None, Some(grpc)) => RouteMatch {
            http: None,
            grpc: Some(validate_grpc_match(grpc, violations)),
        },
        // @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-grpc-else
    }
}

fn validate_http_match(value: &Value, violations: &mut Vec<FieldViolation>) -> HttpMatch {
    let obj = value.as_object();
    if obj.is_none() {
        violations.push(FieldViolation::new(
            "match.http",
            "match.http must be an object",
        ));
    }

    // @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-http-methods
    let methods = obj
        .and_then(|o| o.get("methods"))
        .and_then(Value::as_array)
        .map(|arr| parse_http_methods(arr, violations))
        .unwrap_or_else(|| {
            violations.push(FieldViolation::new(
                "match.http.methods",
                "methods must be a non-empty array of GET/POST/PUT/DELETE/PATCH",
            ));
            Vec::new()
        });
    // @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-http-methods

    // @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-http-path
    let path = obj
        .and_then(|o| o.get("path"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            violations.push(FieldViolation::new(
                "match.http.path",
                "path must be a non-empty string",
            ));
            String::new()
        });
    // @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-http-path

    // @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-http-query-allowlist
    let query_allowlist = match obj.and_then(|o| o.get("query_allowlist")) {
        None => Vec::new(),
        Some(v) => parse_string_array(v, "match.http.query_allowlist", violations),
    };
    // @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-http-query-allowlist

    // @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-http-suffix-mode
    let path_suffix_mode = match obj.and_then(|o| o.get("path_suffix_mode")) {
        None => PathSuffixMode::Append,
        Some(Value::String(s)) if s == "append" => PathSuffixMode::Append,
        Some(Value::String(s)) if s == "disabled" => PathSuffixMode::Disabled,
        Some(_) => {
            violations.push(FieldViolation::new(
                "match.http.path_suffix_mode",
                "path_suffix_mode must be one of disabled or append",
            ));
            PathSuffixMode::Append
        }
    };
    // @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-http-suffix-mode

    HttpMatch {
        methods,
        path,
        query_allowlist,
        path_suffix_mode,
    }
}

fn parse_http_methods(
    arr: &[Value],
    violations: &mut Vec<FieldViolation>,
) -> Vec<crate::model::route::HttpMethod> {
    if arr.is_empty() {
        violations.push(FieldViolation::new(
            "match.http.methods",
            "methods must be a non-empty array of GET/POST/PUT/DELETE/PATCH",
        ));
        return Vec::new();
    }
    let mut methods = Vec::new();
    let mut has_bad_verb = false;
    for entry in arr {
        match entry.as_str().and_then(parse_http_method) {
            Some(method) => methods.push(method),
            None => has_bad_verb = true,
        }
    }
    if has_bad_verb {
        violations.push(FieldViolation::new(
            "match.http.methods",
            "methods must only contain GET/POST/PUT/DELETE/PATCH",
        ));
    }
    methods
}

fn parse_http_method(raw: &str) -> Option<crate::model::route::HttpMethod> {
    serde_json::from_value(Value::String(raw.to_owned())).ok()
}

fn parse_string_array(
    value: &Value,
    field: &'static str,
    violations: &mut Vec<FieldViolation>,
) -> Vec<String> {
    let Some(arr) = value.as_array() else {
        violations.push(FieldViolation::new(field, "must be an array of strings"));
        return Vec::new();
    };
    let mut items = Vec::new();
    let mut has_bad_entry = false;
    for entry in arr {
        match entry.as_str() {
            Some(s) => items.push(s.to_owned()),
            None => has_bad_entry = true,
        }
    }
    if has_bad_entry {
        violations.push(FieldViolation::new(field, "every entry must be a string"));
    }
    items
}

// @cpt-dod:cpt-cf-oagw-dod-route-grpc-persist:p1
// @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-grpc-fields
// @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-grpc-persist
fn validate_grpc_match(value: &Value, violations: &mut Vec<FieldViolation>) -> GrpcMatch {
    let obj = value.as_object();
    if obj.is_none() {
        violations.push(FieldViolation::new(
            "match.grpc",
            "match.grpc must be an object",
        ));
    }
    let service = non_empty_string(obj, "service", "match.grpc.service", violations);
    let method = non_empty_string(obj, "method", "match.grpc.method", violations);
    // Accepted and persisted schema-conformantly; no gRPC proxy-time
    // matching/uniqueness behavior is evaluated, required, or reserved for
    // it anywhere in this module -- there is no gRPC proxy code path to
    // reach (`DESIGN.md` §3.1, §4.7 item 7; `PRD.md` §4.2).
    GrpcMatch { service, method }
}
// @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-grpc-persist
// @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-grpc-fields

fn non_empty_string(
    obj: Option<&serde_json::Map<String, Value>>,
    key: &str,
    field: &'static str,
    violations: &mut Vec<FieldViolation>,
) -> String {
    obj.and_then(|o| o.get(key))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            violations.push(FieldViolation::new(field, "must be a non-empty string"));
            String::new()
        })
}

// @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-tags
fn validate_tags(body: &Value, violations: &mut Vec<FieldViolation>) -> Vec<String> {
    let Some(tags_value) = body.get("tags") else {
        return Vec::new();
    };
    let Some(arr) = tags_value.as_array() else {
        violations.push(FieldViolation::new("tags", "tags must be an array"));
        return Vec::new();
    };
    let mut tags = Vec::new();
    let mut has_invalid_tag = false;
    for entry in arr {
        match entry.as_str() {
            Some(s) if is_valid_tag(s) => tags.push(s.to_owned()),
            _ => has_invalid_tag = true,
        }
    }
    if has_invalid_tag {
        violations.push(FieldViolation::new(
            "tags",
            "every tag must match ^[a-z0-9_-]+$",
        ));
    }
    tags
}

fn is_valid_tag(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}
// @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-tags

// @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-plugins
fn validate_plugins(body: &Value, violations: &mut Vec<FieldViolation>) -> RoutePluginsBinding {
    let Some(plugins_value) = body.get("plugins") else {
        return RoutePluginsBinding::default();
    };
    let obj = plugins_value.as_object();
    if obj.is_none() {
        violations.push(FieldViolation::new("plugins", "plugins must be an object"));
    }

    let sharing = match obj.and_then(|o| o.get("sharing")) {
        None => Sharing::Private,
        Some(v) => match v.as_str().and_then(parse_sharing) {
            Some(s) => s,
            None => {
                violations.push(FieldViolation::new(
                    "plugins.sharing",
                    "must be one of private, inherit, enforce",
                ));
                Sharing::Private
            }
        },
    };

    let items = match obj.and_then(|o| o.get("items")) {
        None => Vec::new(),
        Some(v) => parse_plugin_items(v, violations),
    };

    RoutePluginsBinding { sharing, items }
}

fn parse_sharing(raw: &str) -> Option<Sharing> {
    serde_json::from_value(Value::String(raw.to_owned())).ok()
}

fn parse_plugin_items(value: &Value, violations: &mut Vec<FieldViolation>) -> Vec<String> {
    let Some(arr) = value.as_array() else {
        violations.push(FieldViolation::new(
            "plugins.items",
            "items must be an array",
        ));
        return Vec::new();
    };
    let mut items = Vec::new();
    let mut has_malformed_item = false;
    for entry in arr {
        match entry.as_str() {
            Some(s) if gts::GtsId::try_new(s).is_ok() => items.push(s.to_owned()),
            _ => has_malformed_item = true,
        }
    }
    if has_malformed_item {
        violations.push(FieldViolation::new(
            "plugins.items",
            "every entry must be a syntactically well-formed GTS identifier",
        ));
    }
    items
}
// @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-plugins

// @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-ratelimit-if
// @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-ratelimit-fields
fn validate_rate_limit(
    body: &Value,
    violations: &mut Vec<FieldViolation>,
) -> Option<RateLimitConfig> {
    let value = body.get("rate_limit")?;
    match serde_json::from_value::<RateLimitConfig>(value.clone()) {
        Ok(mut rate_limit) => {
            if rate_limit.sustained.rate < 1 {
                violations.push(FieldViolation::new(
                    "rate_limit.sustained.rate",
                    "must be present and >= 1",
                ));
            }
            let default_capacity = rate_limit.sustained.rate.max(1);
            let capacity = rate_limit
                .burst
                .as_ref()
                .and_then(|b| b.capacity)
                .unwrap_or(default_capacity);
            rate_limit.burst = Some(BurstConfig {
                capacity: Some(capacity),
            });
            Some(rate_limit)
        }
        Err(err) => {
            violations.push(FieldViolation::new(
                "rate_limit",
                format!("invalid rate_limit shape: {err}"),
            ));
            None
        }
    }
}
// @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-ratelimit-fields
// @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-ratelimit-if

// @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-enabled-default
fn validate_enabled(body: &Value, violations: &mut Vec<FieldViolation>) -> bool {
    match body.get("enabled") {
        None => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            violations.push(FieldViolation::new("enabled", "enabled must be a boolean"));
            true
        }
    }
}
// @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-enabled-default

// @cpt-begin:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-http-priority
fn validate_priority(
    body: &Value,
    route_match: &RouteMatch,
    violations: &mut Vec<FieldViolation>,
) -> Option<i64> {
    let priority_value = body.get("priority").and_then(Value::as_i64);
    if route_match.http.is_some() && priority_value.is_none() {
        violations.push(FieldViolation::new(
            "priority",
            "priority is required alongside match.http",
        ));
    }
    priority_value
}
// @cpt-end:cpt-cf-oagw-algo-route-match-validate:p1:inst-match-validate-http-priority

/// Parse and validate `upstream_id` off the raw request body. Shared by the
/// create flow (required) and the replace flow's immutability check
/// (optional; see `route_api::handlers`).
#[must_use]
pub fn parse_upstream_id(body: &Value) -> Option<Uuid> {
    body.get("upstream_id")
        .and_then(Value::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn minimal_http_body() -> Value {
        json!({
            "upstream_id": Uuid::new_v4(),
            "match": { "http": { "methods": ["GET"], "path": "/v1/models" } },
            "priority": 10,
        })
    }

    #[test]
    fn accepts_a_minimal_valid_http_body() {
        let shape = validate_route_shape(&minimal_http_body()).unwrap();
        assert!(shape.route_match.http.is_some());
        assert!(shape.enabled);
        assert_eq!(shape.priority, Some(10));
        assert_eq!(
            shape.route_match.http.unwrap().query_allowlist,
            Vec::<String>::new()
        );
    }

    #[test]
    fn rejects_match_with_both_http_and_grpc() {
        let body = json!({
            "upstream_id": Uuid::new_v4(),
            "match": {
                "http": { "methods": ["GET"], "path": "/v1/models" },
                "grpc": { "service": "svc", "method": "m" },
            },
            "priority": 1,
        });
        let violations = validate_route_shape(&body).unwrap_err();
        assert!(violations.iter().any(|v| v.field == "match"));
    }

    #[test]
    fn rejects_match_with_neither_http_nor_grpc() {
        let body = json!({ "upstream_id": Uuid::new_v4(), "match": {} });
        let violations = validate_route_shape(&body).unwrap_err();
        assert!(violations.iter().any(|v| v.field == "match"));
    }

    #[test]
    fn rejects_match_with_extra_properties() {
        let body = json!({
            "upstream_id": Uuid::new_v4(),
            "match": { "http": { "methods": ["GET"], "path": "/p" }, "foo": 1 },
            "priority": 1,
        });
        let violations = validate_route_shape(&body).unwrap_err();
        assert!(violations.iter().any(|v| v.field == "match"));
    }

    #[test]
    fn rejects_empty_methods_array() {
        let body = json!({
            "upstream_id": Uuid::new_v4(),
            "match": { "http": { "methods": [], "path": "/p" } },
            "priority": 1,
        });
        let violations = validate_route_shape(&body).unwrap_err();
        assert!(violations.iter().any(|v| v.field == "match.http.methods"));
    }

    #[test]
    fn rejects_an_unsupported_verb() {
        let body = json!({
            "upstream_id": Uuid::new_v4(),
            "match": { "http": { "methods": ["TRACE"], "path": "/p" } },
            "priority": 1,
        });
        let violations = validate_route_shape(&body).unwrap_err();
        assert!(violations.iter().any(|v| v.field == "match.http.methods"));
    }

    #[test]
    fn defaults_query_allowlist_to_empty_and_suffix_mode_to_append() {
        let shape = validate_route_shape(&minimal_http_body()).unwrap();
        let http = shape.route_match.http.unwrap();
        assert!(http.query_allowlist.is_empty());
        assert_eq!(http.path_suffix_mode, PathSuffixMode::Append);
    }

    #[test]
    fn persists_an_explicit_disabled_suffix_mode() {
        let mut body = minimal_http_body();
        body["match"]["http"]["path_suffix_mode"] = json!("disabled");
        let shape = validate_route_shape(&body).unwrap();
        assert_eq!(
            shape.route_match.http.unwrap().path_suffix_mode,
            PathSuffixMode::Disabled
        );
    }

    #[test]
    fn rejects_http_match_missing_priority() {
        let body = json!({
            "upstream_id": Uuid::new_v4(),
            "match": { "http": { "methods": ["GET"], "path": "/p" } },
        });
        let violations = validate_route_shape(&body).unwrap_err();
        assert!(violations.iter().any(|v| v.field == "priority"));
    }

    #[test]
    fn accepts_a_grpc_only_match_without_priority() {
        let body = json!({
            "upstream_id": Uuid::new_v4(),
            "match": { "grpc": { "service": "pkg.Service", "method": "Get" } },
        });
        let shape = validate_route_shape(&body).unwrap();
        assert!(shape.route_match.grpc.is_some());
        assert!(shape.priority.is_none());
    }

    #[test]
    fn rejects_a_tag_that_fails_the_pattern() {
        let mut body = minimal_http_body();
        body["tags"] = json!(["Not-Lowercase"]);
        let violations = validate_route_shape(&body).unwrap_err();
        assert!(violations.iter().any(|v| v.field == "tags"));
    }

    #[test]
    fn accepts_valid_tags() {
        let mut body = minimal_http_body();
        body["tags"] = json!(["ok-tag_1"]);
        let shape = validate_route_shape(&body).unwrap();
        assert_eq!(shape.tags, vec!["ok-tag_1".to_owned()]);
    }

    #[test]
    fn defaults_plugins_sharing_to_private_and_items_to_empty() {
        let shape = validate_route_shape(&minimal_http_body()).unwrap();
        assert_eq!(shape.plugins.sharing, Sharing::Private);
        assert!(shape.plugins.items.is_empty());
    }

    #[test]
    fn rejects_a_malformed_plugin_item() {
        let mut body = minimal_http_body();
        body["plugins"] = json!({ "items": ["not a gts id"] });
        let violations = validate_route_shape(&body).unwrap_err();
        assert!(violations.iter().any(|v| v.field == "plugins.items"));
    }

    #[test]
    fn accepts_a_well_formed_gts_plugin_item() {
        let mut body = minimal_http_body();
        body["plugins"] = json!({ "items": ["gts.cf.core.oagw.plugin.v1~"] });
        let shape = validate_route_shape(&body).unwrap();
        assert_eq!(
            shape.plugins.items,
            vec!["gts.cf.core.oagw.plugin.v1~".to_owned()]
        );
    }

    #[test]
    fn defaults_rate_limit_burst_capacity_to_sustained_rate() {
        let mut body = minimal_http_body();
        body["rate_limit"] = json!({ "sustained": { "rate": 42 } });
        let shape = validate_route_shape(&body).unwrap();
        let rate_limit = shape.rate_limit.unwrap();
        assert_eq!(rate_limit.burst.unwrap().capacity, Some(42));
        assert_eq!(
            rate_limit.sustained.window,
            crate::model::upstream::RateLimitWindow::Second
        );
        assert_eq!(
            rate_limit.scope,
            crate::model::upstream::RateLimitScope::Tenant
        );
        assert_eq!(
            rate_limit.strategy,
            crate::model::upstream::RateLimitStrategy::Reject
        );
        assert_eq!(rate_limit.cost, 1);
    }

    #[test]
    fn rejects_rate_limit_missing_sustained_rate() {
        let mut body = minimal_http_body();
        body["rate_limit"] = json!({ "sustained": {} });
        let violations = validate_route_shape(&body).unwrap_err();
        assert!(violations.iter().any(|v| v.field == "rate_limit"));
    }

    #[test]
    fn enabled_defaults_to_true_and_accepts_explicit_false() {
        let shape = validate_route_shape(&minimal_http_body()).unwrap();
        assert!(shape.enabled);

        let mut body = minimal_http_body();
        body["enabled"] = json!(false);
        let shape = validate_route_shape(&body).unwrap();
        assert!(!shape.enabled);
    }

    #[test]
    fn ignores_a_cors_object_without_failing_or_persisting_it() {
        let mut body = minimal_http_body();
        body["cors"] = json!({ "enabled": true, "allowed_origins": ["*"] });
        let shape = validate_route_shape(&body).unwrap();
        // NormalizedRouteShape has no cors field at all: this line simply
        // demonstrates the body parsed successfully despite the extra key.
        assert!(shape.route_match.http.is_some());
    }

    #[test]
    fn accumulates_multiple_violations_in_one_pass() {
        let body = json!({
            "upstream_id": Uuid::new_v4(),
            "match": { "http": { "methods": [], "path": "" } },
            "tags": ["Bad Tag"],
        });
        let violations = validate_route_shape(&body).unwrap_err();
        assert!(
            violations.len() >= 3,
            "expected multiple accumulated violations, got {violations:?}"
        );
    }

    #[test]
    fn parse_upstream_id_reads_a_valid_uuid_string() {
        let id = Uuid::new_v4();
        let body = json!({ "upstream_id": id.to_string() });
        assert_eq!(parse_upstream_id(&body), Some(id));
    }

    #[test]
    fn parse_upstream_id_rejects_a_missing_or_malformed_value() {
        assert_eq!(parse_upstream_id(&json!({})), None);
        assert_eq!(parse_upstream_id(&json!({ "upstream_id": "nope" })), None);
    }
}
