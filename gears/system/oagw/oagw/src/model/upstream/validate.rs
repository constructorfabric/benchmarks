//! `cpt-cf-oagw-algo-validate-upstream-schema`: full-body validation of a
//! `POST`/`PUT /oagw/v1/upstreams[/{id}]` request against
//! `docs/schemas/upstream.v1.schema.json`, including the `http`/`ws`
//! plaintext-scheme override documented in
//! `cpt-cf-oagw-dod-http-scheme-acceptance`.

use std::collections::HashMap;

use serde_json::{Map, Value};
use uuid::Uuid;

use super::host::classify_host;
use super::ident;
use super::{
    AuthConfig, BurstConfig, CorsConfig, CorsMethod, Endpoint, EndpointScheme, HeadersConfig,
    PassthroughMode, PluginsBinding, RateLimitAlgorithm, RateLimitConfig, RateLimitScope,
    RateLimitStrategy, RequestHeaderRules, ResponseHeaderRules, ServerConfig, Sharing,
    SustainedRate, Upstream,
};

/// A `400 ValidationError`-worthy schema-validation failure, carrying an
/// occurrence-specific detail message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaValidationError(pub String);

fn err(detail: impl Into<String>) -> SchemaValidationError {
    SchemaValidationError(detail.into())
}

fn require_object<'a>(
    value: &'a Value,
    field: &str,
) -> Result<&'a Map<String, Value>, SchemaValidationError> {
    value
        .as_object()
        .ok_or_else(|| err(format!("`{field}` must be an object")))
}

fn check_unknown_keys(
    map: &Map<String, Value>,
    allowed: &[&str],
    field: &str,
) -> Result<(), SchemaValidationError> {
    for key in map.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(err(format!("`{field}` has unknown property `{key}`")));
        }
    }
    Ok(())
}

/// Top-level `additionalProperties: false` set (excludes the read-only,
/// server-generated `id`, which is rejected separately).
const TOP_LEVEL_KEYS: &[&str] = &[
    "enabled",
    "alias",
    "tags",
    "server",
    "protocol",
    "auth",
    "headers",
    "plugins",
    "rate_limit",
    "cors",
];

/// Validate a raw JSON request body and return a normalized, schema-valid
/// Upstream draft (`id` unset, `tenant_id` unset -- both are set by the
/// caller after tenant/alias resolution).
///
/// # Errors
/// Returns [`SchemaValidationError`] describing the first violation found.
// @cpt-algo:cpt-cf-oagw-algo-validate-upstream-schema:p2
// @cpt-dod:cpt-cf-oagw-dod-schema-validation:p1
// @cpt-dod:cpt-cf-oagw-dod-http-scheme-acceptance:p1
pub fn validate_upstream_body(value: &Value) -> Result<Upstream, SchemaValidationError> {
    // @cpt-begin:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-parse-json
    let obj = value
        .as_object()
        .ok_or_else(|| err("request body must be a JSON object"))?;
    // @cpt-end:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-parse-json

    // @cpt-begin:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-reject-client-id
    if obj.contains_key("id") {
        // @cpt-begin:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-reject-client-id-return
        return Err(err(
            "`id` is read-only and server-generated; it must not be supplied",
        ));
        // @cpt-end:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-reject-client-id-return
    }
    // @cpt-end:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-reject-client-id

    // @cpt-begin:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-top-level-additional-props
    check_unknown_keys(obj, TOP_LEVEL_KEYS, "")?;
    // @cpt-end:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-top-level-additional-props

    let enabled = parse_enabled(obj)?;
    let alias = parse_alias_literal(obj)?;
    let server = parse_server(obj)?;
    let protocol = parse_protocol(obj)?;
    let tags = parse_tags(obj)?;
    let auth = parse_auth(obj)?;
    let headers = parse_headers(obj)?;
    let plugins = parse_plugins(obj)?;
    let rate_limit = parse_rate_limit(obj)?;
    let cors = parse_cors(obj)?;

    // @cpt-begin:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-return
    Ok(Upstream {
        id: None,
        enabled,
        alias,
        tags,
        server,
        protocol,
        auth,
        headers,
        plugins,
        rate_limit,
        cors,
        tenant_id: Uuid::nil(),
    })
    // @cpt-end:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-return
}

fn parse_enabled(obj: &Map<String, Value>) -> Result<bool, SchemaValidationError> {
    match obj.get("enabled") {
        None => Ok(true),
        Some(v) => v
            .as_bool()
            .ok_or_else(|| err("`enabled` must be a boolean")),
    }
}

fn parse_alias_literal(obj: &Map<String, Value>) -> Result<Option<String>, SchemaValidationError> {
    match obj.get("alias") {
        None => Ok(None),
        Some(v) => Ok(Some(
            v.as_str()
                .ok_or_else(|| err("`alias` must be a string"))?
                .to_owned(),
        )),
    }
}

// @cpt-begin:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-endpoints-shape
// @cpt-begin:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-pool-consistency
fn parse_server(obj: &Map<String, Value>) -> Result<ServerConfig, SchemaValidationError> {
    let server_val = obj
        .get("server")
        .ok_or_else(|| err("`server` is required"))?;
    let server_obj = require_object(server_val, "server")?;
    check_unknown_keys(server_obj, &["endpoints"], "server")?;

    let endpoints_val = server_obj
        .get("endpoints")
        .ok_or_else(|| err("`server.endpoints` is required"))?;
    let endpoints_arr = endpoints_val
        .as_array()
        .ok_or_else(|| err("`server.endpoints` must be an array"))?;
    if endpoints_arr.is_empty() {
        return Err(err("`server.endpoints` must contain at least one item"));
    }

    // @cpt-begin:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-endpoints-foreach
    let mut endpoints = Vec::with_capacity(endpoints_arr.len());
    for (i, item) in endpoints_arr.iter().enumerate() {
        endpoints.push(parse_endpoint(item, i)?);
    }
    // @cpt-end:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-endpoints-foreach

    let first = endpoints[0].clone();
    if endpoints
        .iter()
        .any(|e| e.scheme != first.scheme || e.port != first.port)
    {
        return Err(err(
            "every endpoint in `server.endpoints` must share the same `scheme` and `port`",
        ));
    }

    Ok(ServerConfig { endpoints })
}
// @cpt-end:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-pool-consistency
// @cpt-end:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-endpoints-shape

fn parse_endpoint(value: &Value, index: usize) -> Result<Endpoint, SchemaValidationError> {
    let field = format!("server.endpoints[{index}]");
    let obj = require_object(value, &field)?;
    check_unknown_keys(obj, &["scheme", "host", "port"], &field)?;

    // @cpt-begin:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-scheme-enum
    let scheme_str = obj
        .get("scheme")
        .and_then(Value::as_str)
        .ok_or_else(|| err(format!("`{field}.scheme` is required")))?;
    let scheme = parse_scheme(scheme_str, &field)?;
    // @cpt-end:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-scheme-enum

    // @cpt-begin:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-host-format
    let host = obj
        .get("host")
        .and_then(Value::as_str)
        .ok_or_else(|| err(format!("`{field}.host` is required")))?;
    if classify_host(host).is_none() {
        return Err(err(format!(
            "`{field}.host` `{host}` is not a valid hostname or IP literal"
        )));
    }
    // @cpt-end:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-host-format

    // @cpt-begin:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-port-range
    let port = match obj.get("port") {
        None => super::default_port(),
        Some(v) => {
            let n = v
                .as_u64()
                .ok_or_else(|| err(format!("`{field}.port` must be an integer")))?;
            if n < 1 || n > u64::from(u16::MAX) {
                return Err(err(format!("`{field}.port` must be in [1, 65535]")));
            }
            u16::try_from(n).map_err(|_| err(format!("`{field}.port` must be in [1, 65535]")))?
        }
    };
    // @cpt-end:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-port-range

    Ok(Endpoint {
        scheme,
        host: host.to_owned(),
        port,
    })
}

fn parse_scheme(raw: &str, field: &str) -> Result<EndpointScheme, SchemaValidationError> {
    match raw {
        "https" => Ok(EndpointScheme::Https),
        "wss" => Ok(EndpointScheme::Wss),
        "wt" => Ok(EndpointScheme::Wt),
        "grpc" => Ok(EndpointScheme::Grpc),
        // `http`/`ws` are the plaintext family this feature accepts
        // unconditionally at the management layer -- see
        // `cpt-cf-oagw-dod-http-scheme-acceptance`.
        "http" => Ok(EndpointScheme::Http),
        "ws" => Ok(EndpointScheme::Ws),
        other => Err(err(format!("`{field}.scheme` `{other}` is not recognized"))),
    }
}

// @cpt-begin:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-protocol
fn parse_protocol(obj: &Map<String, Value>) -> Result<String, SchemaValidationError> {
    let raw = obj
        .get("protocol")
        .and_then(Value::as_str)
        .ok_or_else(|| err("`protocol` is required"))?;
    if !ident::is_valid_protocol(raw) {
        return Err(err(format!("`protocol` `{raw}` is not recognized")));
    }
    Ok(raw.to_owned())
}
// @cpt-end:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-protocol

// @cpt-begin:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-tags
fn parse_tags(obj: &Map<String, Value>) -> Result<Vec<String>, SchemaValidationError> {
    let Some(v) = obj.get("tags") else {
        return Ok(Vec::new());
    };
    let arr = v.as_array().ok_or_else(|| err("`tags` must be an array"))?;
    let mut tags = Vec::with_capacity(arr.len());
    for item in arr {
        let s = item
            .as_str()
            .ok_or_else(|| err("`tags` items must be strings"))?;
        if !is_valid_tag(s) {
            return Err(err(format!("tag `{s}` does not match ^[a-z0-9_-]+$")));
        }
        tags.push(s.to_owned());
    }
    Ok(tags)
}
// @cpt-end:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-tags

fn is_valid_tag(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn parse_sharing(value: Option<&Value>, field: &str) -> Result<Sharing, SchemaValidationError> {
    match value {
        None => Ok(Sharing::Private),
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| err(format!("`{field}` must be a string")))?;
            match s {
                "private" => Ok(Sharing::Private),
                "inherit" => Ok(Sharing::Inherit),
                "enforce" => Ok(Sharing::Enforce),
                other => Err(err(format!(
                    "`{field}` `{other}` is not one of private/inherit/enforce"
                ))),
            }
        }
    }
}

// @cpt-begin:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-auth
fn parse_auth(obj: &Map<String, Value>) -> Result<Option<AuthConfig>, SchemaValidationError> {
    let Some(v) = obj.get("auth") else {
        return Ok(None);
    };
    let auth_obj = require_object(v, "auth")?;

    let auth_type = match auth_obj.get("type") {
        None => None,
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| err("`auth.type` must be a string"))?;
            if !ident::is_well_formed_gts_identifier(s) {
                return Err(err(format!(
                    "`auth.type` `{s}` is not a well-formed GTS identifier"
                )));
            }
            Some(s.to_owned())
        }
    };
    let sharing = parse_sharing(auth_obj.get("sharing"), "auth.sharing")?;
    let config = match auth_obj.get("config") {
        None => Value::Object(Map::new()),
        Some(v) => {
            if !v.is_object() {
                return Err(err("`auth.config` must be an object"));
            }
            v.clone()
        }
    };
    Ok(Some(AuthConfig {
        auth_type,
        sharing,
        config,
    }))
}
// @cpt-end:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-auth

// @cpt-begin:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-headers
fn parse_headers(obj: &Map<String, Value>) -> Result<Option<HeadersConfig>, SchemaValidationError> {
    let Some(v) = obj.get("headers") else {
        return Ok(None);
    };
    let headers_obj = require_object(v, "headers")?;
    check_unknown_keys(headers_obj, &["request", "response"], "headers")?;

    let request = match headers_obj.get("request") {
        None => RequestHeaderRules::default(),
        Some(v) => parse_request_headers(v)?,
    };
    let response = match headers_obj.get("response") {
        None => ResponseHeaderRules::default(),
        Some(v) => parse_response_headers(v)?,
    };
    Ok(Some(HeadersConfig { request, response }))
}

fn parse_request_headers(value: &Value) -> Result<RequestHeaderRules, SchemaValidationError> {
    let obj = require_object(value, "headers.request")?;
    check_unknown_keys(
        obj,
        &[
            "set",
            "add",
            "remove",
            "passthrough",
            "passthrough_allowlist",
        ],
        "headers.request",
    )?;
    let set = parse_string_map(obj.get("set"), "headers.request.set")?;
    let add = parse_string_map(obj.get("add"), "headers.request.add")?;
    let remove = parse_string_array(obj.get("remove"), "headers.request.remove")?;
    let passthrough = match obj.get("passthrough") {
        None => PassthroughMode::None,
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| err("`headers.request.passthrough` must be a string"))?;
            match s {
                "none" => PassthroughMode::None,
                "allowlist" => PassthroughMode::Allowlist,
                "all" => PassthroughMode::All,
                other => {
                    return Err(err(format!(
                        "`headers.request.passthrough` `{other}` is invalid"
                    )));
                }
            }
        }
    };
    let passthrough_allowlist = parse_string_array(
        obj.get("passthrough_allowlist"),
        "headers.request.passthrough_allowlist",
    )?;
    Ok(RequestHeaderRules {
        set,
        add,
        remove,
        passthrough,
        passthrough_allowlist,
    })
}

fn parse_response_headers(value: &Value) -> Result<ResponseHeaderRules, SchemaValidationError> {
    let obj = require_object(value, "headers.response")?;
    check_unknown_keys(obj, &["set", "add", "remove"], "headers.response")?;
    let set = parse_string_map(obj.get("set"), "headers.response.set")?;
    let add = parse_string_map(obj.get("add"), "headers.response.add")?;
    let remove = parse_string_array(obj.get("remove"), "headers.response.remove")?;
    Ok(ResponseHeaderRules { set, add, remove })
}
// @cpt-end:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-headers

fn parse_string_map(
    value: Option<&Value>,
    field: &str,
) -> Result<HashMap<String, String>, SchemaValidationError> {
    let Some(v) = value else {
        return Ok(HashMap::new());
    };
    let obj = v
        .as_object()
        .ok_or_else(|| err(format!("`{field}` must be an object")))?;
    let mut map = HashMap::with_capacity(obj.len());
    for (k, val) in obj {
        let s = val
            .as_str()
            .ok_or_else(|| err(format!("`{field}.{k}` must be a string")))?;
        map.insert(k.clone(), s.to_owned());
    }
    Ok(map)
}

fn parse_string_array(
    value: Option<&Value>,
    field: &str,
) -> Result<Vec<String>, SchemaValidationError> {
    let Some(v) = value else {
        return Ok(Vec::new());
    };
    let arr = v
        .as_array()
        .ok_or_else(|| err(format!("`{field}` must be an array")))?;
    arr.iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| err(format!("`{field}` items must be strings")))
        })
        .collect()
}

// @cpt-begin:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-plugins
fn parse_plugins(
    obj: &Map<String, Value>,
) -> Result<Option<PluginsBinding>, SchemaValidationError> {
    let Some(v) = obj.get("plugins") else {
        return Ok(None);
    };
    let plugins_obj = require_object(v, "plugins")?;
    let sharing = parse_sharing(plugins_obj.get("sharing"), "plugins.sharing")?;
    let items = match plugins_obj.get("items") {
        None => Vec::new(),
        Some(v) => {
            let arr = v
                .as_array()
                .ok_or_else(|| err("`plugins.items` must be an array"))?;
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                let s = item
                    .as_str()
                    .ok_or_else(|| err("`plugins.items` entries must be strings"))?;
                if !ident::is_well_formed_plugin_ref(s) {
                    return Err(err(format!(
                        "`plugins.items` entry `{s}` is neither a well-formed GTS \
                         identifier nor a UUID"
                    )));
                }
                out.push(s.to_owned());
            }
            out
        }
    };
    Ok(Some(PluginsBinding { sharing, items }))
}
// @cpt-end:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-plugins

// @cpt-begin:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-rate-limit
fn parse_rate_limit(
    obj: &Map<String, Value>,
) -> Result<Option<RateLimitConfig>, SchemaValidationError> {
    let Some(v) = obj.get("rate_limit") else {
        return Ok(None);
    };
    let rl_obj = require_object(v, "rate_limit")?;
    check_unknown_keys(
        rl_obj,
        &[
            "sharing",
            "algorithm",
            "sustained",
            "burst",
            "scope",
            "strategy",
            "cost",
        ],
        "rate_limit",
    )?;

    let sharing = parse_sharing(rl_obj.get("sharing"), "rate_limit.sharing")?;
    let algorithm = parse_rate_limit_algorithm(rl_obj.get("algorithm"))?;

    let sustained_val = rl_obj
        .get("sustained")
        .ok_or_else(|| err("`rate_limit.sustained` is required"))?;
    let sustained = parse_sustained(sustained_val)?;

    let default_burst_capacity = sustained.rate;
    let burst = Some(parse_burst(rl_obj.get("burst"), default_burst_capacity)?);

    let scope = parse_rate_limit_scope(rl_obj.get("scope"))?;
    let strategy = parse_rate_limit_strategy(rl_obj.get("strategy"))?;
    let cost = match rl_obj.get("cost") {
        None => 1,
        Some(v) => parse_positive_u32(v, "rate_limit.cost")?,
    };

    Ok(Some(RateLimitConfig {
        sharing,
        algorithm,
        sustained,
        burst,
        scope,
        strategy,
        cost,
    }))
}

fn parse_rate_limit_algorithm(
    value: Option<&Value>,
) -> Result<RateLimitAlgorithm, SchemaValidationError> {
    match value {
        None => Ok(RateLimitAlgorithm::TokenBucket),
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| err("`rate_limit.algorithm` must be a string"))?;
            match s {
                "token_bucket" => Ok(RateLimitAlgorithm::TokenBucket),
                "sliding_window" => Ok(RateLimitAlgorithm::SlidingWindow),
                other => Err(err(format!("`rate_limit.algorithm` `{other}` is invalid"))),
            }
        }
    }
}

fn parse_rate_limit_scope(value: Option<&Value>) -> Result<RateLimitScope, SchemaValidationError> {
    match value {
        None => Ok(RateLimitScope::Tenant),
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| err("`rate_limit.scope` must be a string"))?;
            match s {
                "global" => Ok(RateLimitScope::Global),
                "tenant" => Ok(RateLimitScope::Tenant),
                "user" => Ok(RateLimitScope::User),
                "ip" => Ok(RateLimitScope::Ip),
                "route" => Ok(RateLimitScope::Route),
                other => Err(err(format!("`rate_limit.scope` `{other}` is invalid"))),
            }
        }
    }
}

fn parse_rate_limit_strategy(
    value: Option<&Value>,
) -> Result<RateLimitStrategy, SchemaValidationError> {
    match value {
        None => Ok(RateLimitStrategy::Reject),
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| err("`rate_limit.strategy` must be a string"))?;
            match s {
                "reject" => Ok(RateLimitStrategy::Reject),
                "queue" => Ok(RateLimitStrategy::Queue),
                "degrade" => Ok(RateLimitStrategy::Degrade),
                other => Err(err(format!("`rate_limit.strategy` `{other}` is invalid"))),
            }
        }
    }
}

fn parse_sustained(value: &Value) -> Result<SustainedRate, SchemaValidationError> {
    let obj = require_object(value, "rate_limit.sustained")?;
    let rate_val = obj
        .get("rate")
        .ok_or_else(|| err("`rate_limit.sustained.rate` is required"))?;
    let rate = parse_positive_u32(rate_val, "rate_limit.sustained.rate")?;
    let window = match obj.get("window") {
        None => super::RateLimitWindow::Second,
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| err("`rate_limit.sustained.window` must be a string"))?;
            match s {
                "second" => super::RateLimitWindow::Second,
                "minute" => super::RateLimitWindow::Minute,
                "hour" => super::RateLimitWindow::Hour,
                "day" => super::RateLimitWindow::Day,
                other => {
                    return Err(err(format!(
                        "`rate_limit.sustained.window` `{other}` is invalid"
                    )));
                }
            }
        }
    };
    Ok(SustainedRate { rate, window })
}

fn parse_burst(
    value: Option<&Value>,
    default_capacity: u32,
) -> Result<BurstConfig, SchemaValidationError> {
    let Some(v) = value else {
        return Ok(BurstConfig {
            capacity: Some(default_capacity),
        });
    };
    let obj = require_object(v, "rate_limit.burst")?;
    let capacity = match obj.get("capacity") {
        None => default_capacity,
        Some(v) => parse_positive_u32(v, "rate_limit.burst.capacity")?,
    };
    Ok(BurstConfig {
        capacity: Some(capacity),
    })
}

fn parse_positive_u32(value: &Value, field: &str) -> Result<u32, SchemaValidationError> {
    let n = value
        .as_u64()
        .ok_or_else(|| err(format!("`{field}` must be a positive integer")))?;
    if n < 1 {
        return Err(err(format!("`{field}` must be >= 1")));
    }
    u32::try_from(n).map_err(|_| err(format!("`{field}` is out of range")))
}
// @cpt-end:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-rate-limit

// @cpt-begin:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-cors
fn parse_cors(obj: &Map<String, Value>) -> Result<Option<CorsConfig>, SchemaValidationError> {
    let Some(v) = obj.get("cors") else {
        return Ok(None);
    };
    let cors_obj = require_object(v, "cors")?;
    check_unknown_keys(
        cors_obj,
        &[
            "sharing",
            "enabled",
            "allowed_origins",
            "allowed_methods",
            "expose_headers",
            "allow_credentials",
        ],
        "cors",
    )?;

    let sharing = parse_sharing(cors_obj.get("sharing"), "cors.sharing")?;
    let enabled = match cors_obj.get("enabled") {
        None => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| err("`cors.enabled` must be a boolean"))?,
    };
    let allowed_origins = parse_allowed_origins(cors_obj.get("allowed_origins"))?;
    let allowed_methods = parse_allowed_methods(cors_obj.get("allowed_methods"))?;
    let expose_headers = parse_string_array(cors_obj.get("expose_headers"), "cors.expose_headers")?;
    let allow_credentials = match cors_obj.get("allow_credentials") {
        None => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| err("`cors.allow_credentials` must be a boolean"))?,
    };

    // @cpt-begin:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-cors-credentials-conflict
    if allow_credentials && allowed_origins.iter().any(|o| o == "*") {
        // @cpt-begin:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-cors-credentials-conflict-return
        return Err(err(
            "`cors.allow_credentials: true` forbids a wildcard `allowed_origins` entry",
        ));
        // @cpt-end:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-cors-credentials-conflict-return
    }
    // @cpt-end:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-cors-credentials-conflict

    Ok(Some(CorsConfig {
        sharing,
        enabled,
        allowed_origins,
        allowed_methods,
        expose_headers,
        allow_credentials,
    }))
}

fn parse_allowed_origins(value: Option<&Value>) -> Result<Vec<String>, SchemaValidationError> {
    let Some(v) = value else {
        return Ok(Vec::new());
    };
    let arr = v
        .as_array()
        .ok_or_else(|| err("`cors.allowed_origins` must be an array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let s = item
            .as_str()
            .ok_or_else(|| err("`cors.allowed_origins` items must be strings"))?;
        if s != "*" && url::Url::parse(s).is_err() {
            return Err(err(format!(
                "`cors.allowed_origins` item `{s}` is neither `*` nor a valid URI"
            )));
        }
        out.push(s.to_owned());
    }
    Ok(out)
}

fn parse_allowed_methods(value: Option<&Value>) -> Result<Vec<CorsMethod>, SchemaValidationError> {
    let Some(v) = value else {
        return Ok(vec![CorsMethod::Get, CorsMethod::Post]);
    };
    let arr = v
        .as_array()
        .ok_or_else(|| err("`cors.allowed_methods` must be an array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let s = item
            .as_str()
            .ok_or_else(|| err("`cors.allowed_methods` items must be strings"))?;
        out.push(parse_cors_method(s)?);
    }
    Ok(out)
}

fn parse_cors_method(s: &str) -> Result<CorsMethod, SchemaValidationError> {
    match s {
        "GET" => Ok(CorsMethod::Get),
        "POST" => Ok(CorsMethod::Post),
        "PUT" => Ok(CorsMethod::Put),
        "PATCH" => Ok(CorsMethod::Patch),
        "DELETE" => Ok(CorsMethod::Delete),
        "HEAD" => Ok(CorsMethod::Head),
        "OPTIONS" => Ok(CorsMethod::Options),
        other => Err(err(format!(
            "`cors.allowed_methods` item `{other}` is invalid"
        ))),
    }
}
// @cpt-end:cpt-cf-oagw-algo-validate-upstream-schema:p2:inst-validate-cors

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn minimal_body() -> Value {
        json!({
            "server": { "endpoints": [ { "scheme": "https", "host": "api.example.com" } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        })
    }

    #[test]
    fn accepts_a_minimal_valid_body_and_applies_defaults() {
        let draft = validate_upstream_body(&minimal_body()).unwrap();
        assert!(draft.enabled);
        assert_eq!(draft.server.endpoints.len(), 1);
    }

    #[test]
    fn accepts_plaintext_http_scheme_with_explicit_port_80() {
        let body = json!({
            "server": { "endpoints": [ { "scheme": "http", "host": "example-plaintext.internal", "port": 80 } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
            "alias": "example-plaintext-svc",
        });
        let draft = validate_upstream_body(&body).unwrap();
        assert_eq!(draft.server.endpoints[0].scheme, EndpointScheme::Http);
        assert_eq!(draft.server.endpoints[0].port, 80);
    }

    #[test]
    fn rejects_a_client_supplied_id() {
        let mut body = minimal_body();
        body["id"] = json!(Uuid::new_v4().to_string());
        assert!(validate_upstream_body(&body).is_err());
    }

    #[test]
    fn rejects_an_unknown_top_level_property() {
        let mut body = minimal_body();
        body["foo"] = json!("bar");
        assert!(validate_upstream_body(&body).is_err());
    }

    #[test]
    fn rejects_missing_protocol() {
        let body = json!({
            "server": { "endpoints": [ { "scheme": "https", "host": "api.example.com" } ] },
        });
        assert!(validate_upstream_body(&body).is_err());
    }

    #[test]
    fn rejects_endpoints_with_differing_scheme() {
        let body = json!({
            "server": { "endpoints": [
                { "scheme": "https", "host": "a.example.com" },
                { "scheme": "wss", "host": "b.example.com" },
            ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        });
        assert!(validate_upstream_body(&body).is_err());
    }

    #[test]
    fn rejects_endpoints_with_differing_port() {
        let body = json!({
            "server": { "endpoints": [
                { "scheme": "https", "host": "a.example.com", "port": 443 },
                { "scheme": "https", "host": "b.example.com", "port": 8443 },
            ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        });
        assert!(validate_upstream_body(&body).is_err());
    }

    #[test]
    fn rejects_out_of_range_port() {
        let body = json!({
            "server": { "endpoints": [ { "scheme": "https", "host": "a.example.com", "port": 0 } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        });
        assert!(validate_upstream_body(&body).is_err());
    }

    #[test]
    fn rejects_cors_credentials_with_wildcard_origin() {
        let mut body = minimal_body();
        body["cors"] = json!({
            "enabled": true,
            "allow_credentials": true,
            "allowed_origins": ["*"],
        });
        assert!(validate_upstream_body(&body).is_err());
    }

    #[test]
    fn rejects_plugins_item_that_is_neither_gts_id_nor_uuid() {
        let mut body = minimal_body();
        body["plugins"] = json!({ "items": ["not-a-valid-ref"] });
        assert!(validate_upstream_body(&body).is_err());
    }

    #[test]
    fn defaults_sustained_window_to_second_when_omitted() {
        let mut body = minimal_body();
        body["rate_limit"] = json!({ "sustained": { "rate": 10 } });
        let draft = validate_upstream_body(&body).unwrap();
        let rate_limit = draft.rate_limit.unwrap();
        assert_eq!(
            rate_limit.sustained.window,
            super::super::RateLimitWindow::Second
        );
        assert_eq!(rate_limit.burst.unwrap().capacity, Some(10));
    }

    #[test]
    fn rejects_unknown_property_in_server_object() {
        let body = json!({
            "server": { "endpoints": [ { "scheme": "https", "host": "a.example.com" } ], "extra": 1 },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        });
        assert!(validate_upstream_body(&body).is_err());
    }

    #[test]
    fn rejects_invalid_tag_pattern() {
        let mut body = minimal_body();
        body["tags"] = json!(["Not_Lowercase!"]);
        assert!(validate_upstream_body(&body).is_err());
    }
}
