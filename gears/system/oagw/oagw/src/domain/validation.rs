//! Validation of the `oagw` domain model (`cpt-cf-oagw-dod-validation-rules`).
//!
//! Two phases, so a payload that violates several rules reports all of them
//! (`cpt-cf-oagw-flow-resource-validation` `inst-rv-16`, `inst-sv-14`):
//!
//! 1. the **shape** pass strips every field the declared field set does not
//!    declare ([`ViolationKind::UnknownField`]), then checks the declared type
//!    and the presence of every required field, replacing a wrongly typed value
//!    with a well-formed representative of the declared type so the typed parse
//!    always succeeds and each field is reported exactly once;
//! 2. the **value** pass runs the rules of
//!    `cpt-cf-oagw-algo-endpoint-validation` and
//!    `cpt-cf-oagw-algo-shape-validation` over the typed aggregate — the
//!    scheme, host and port rules, pool homogeneity, the alias and tag
//!    patterns, the rate-limit, CORS, headers, auth, plugin and route-match
//!    blocks, and the tenant-scoped referential integrity of `upstream_id`.
//!
//! The collector then renders every violated rule in one [`DomainError`], in
//! field order.
// @cpt-begin:cpt-cf-oagw-dod-validation-rules:p1:inst-full

use std::collections::BTreeSet;
use std::net::IpAddr;

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use url::Url;
use uuid::Uuid;

use crate::domain::error::{DomainError, ViolationKind, Violations};
use crate::domain::model::{
    ALGORITHM_TOKEN_BUCKET, AuthConfig, CORS_METHODS, CORS_WILDCARD_ORIGIN, CRED_REF_SCHEME,
    CorsConfig, DEFAULT_ENDPOINT_PORT, DEFAULT_RATE_COST, ENDPOINT_SCHEMES, Endpoint, GrpcMatch,
    HTTP_METHODS, HeadersConfig, HttpMatch, INLINE_SECRET_KEYS, MAX_PORT, MIN_PORT, MatchConfig,
    PASSTHROUGH_ALLOWLIST, PASSTHROUGH_MODES, PASSTHROUGH_NONE, PATH_SUFFIX_APPEND,
    PATH_SUFFIX_MODES, PLUGIN_TYPE_IDS, PROTECTED_HEADERS, PROTOCOL_GRPC, PROTOCOL_HTTP,
    PROTOCOL_INSTANCES, Plugin, PluginBinding, PluginsConfig, RATE_ALGORITHMS, RATE_SCOPES,
    RATE_STRATEGIES, RATE_WINDOWS, RateLimitConfig, Route, SCOPE_TENANT, SHARING_MODES,
    SHARING_PRIVATE, STRATEGY_REJECT, Upstream, WINDOW_SECOND, plugin_ref_uuid,
    split_plugin_reference, strip_trailing_dot,
};

/// The canonical GTS auth-plugin reference, a valid representative.
const PLACEHOLDER_AUTH_PLUGIN: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
/// The canonical GTS guard-plugin reference, a valid representative.
const PLACEHOLDER_GUARD_PLUGIN: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
/// The canonical absolute origin URI, a valid representative.
const PLACEHOLDER_ORIGIN: &str = "https://placeholder.example";
/// The canonical hostname, a valid representative.
const PLACEHOLDER_HOST: &str = "placeholder.example";
/// The canonical route path, a valid representative.
const PLACEHOLDER_PATH: &str = "/placeholder";
/// The canonical alias, a valid representative.
const PLACEHOLDER_ALIAS: &str = "placeholder-alias";
/// The canonical tag, a valid representative.
const PLACEHOLDER_TAG: &str = "placeholder-tag";
/// The canonical header name, a valid representative.
const PLACEHOLDER_HEADER: &str = "x-placeholder";
/// The canonical gRPC service name, a valid representative.
const PLACEHOLDER_GRPC_SERVICE: &str = "placeholder.v1.PlaceholderService";
/// The canonical gRPC method name, a valid representative.
const PLACEHOLDER_GRPC_METHOD: &str = "Placeholder";
/// The canonical plugin name, a valid representative.
const PLACEHOLDER_PLUGIN_NAME: &str = "placeholder-plugin";
/// The canonical Starlark source, a valid representative.
const PLACEHOLDER_SOURCE: &str = "def on_request(context):\n    return context\n";

/// A complete, well-formed endpoint, the placeholder of the endpoint pool.
fn placeholder_endpoints() -> Value {
    serde_json::json!([{
        "scheme": "https",
        "host": PLACEHOLDER_HOST,
        "port": DEFAULT_ENDPOINT_PORT,
    }])
}

/// A complete, well-formed HTTP match, the placeholder of the route `match`
/// block.
fn placeholder_match() -> Value {
    serde_json::json!({
        "http": {
            "methods": ["GET"],
            "path": PLACEHOLDER_PATH,
            "path_suffix_mode": PATH_SUFFIX_APPEND,
        }
    })
}

/// A complete, well-formed method list, the placeholder of the required
/// `match.http.methods`.
fn placeholder_methods() -> Value {
    serde_json::json!(["GET"])
}

/// A complete, well-formed sustained rate, the placeholder of the rate-limit
/// block.
fn placeholder_sustained() -> Value {
    serde_json::json!({ "rate": 1, "window": WINDOW_SECOND })
}

/// The longest prefix of a payload value a violation message echoes, so an
/// over-long field cannot inflate a problem detail or a log entry.
const ECHOED_PREFIX: usize = 32;

/// Renders the payload value a violation message echoes: a value longer than
/// [`ECHOED_PREFIX`] is cut to its first characters and its length is named, so
/// a 1 MiB alias, host or origin stays a bounded message instead of becoming a
/// ~1 MiB problem detail and log entry.
fn echoed(value: &str) -> String {
    let prefix: String = value.chars().take(ECHOED_PREFIX).collect();
    if value.chars().count() <= ECHOED_PREFIX {
        return prefix;
    }
    format!("{prefix}...(len={})", value.chars().count())
}

/// The declared JSON shape of one payload field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// A JSON object whose keys are the declared child fields.
    Object,
    /// A JSON array.
    Array,
    /// A JSON string; the argument is the valid representative that replaces a
    /// wrongly typed value.
    Text(&'static str),
    /// A JSON integer; the argument is the valid representative.
    Integer(i64),
    /// A JSON boolean; the argument is the valid representative.
    Bool(bool),
    /// An object of header name to header value.
    NameMap,
    /// An array of names; the argument is the valid representative.
    NameList(&'static str),
    /// A UUID string.
    Uuid,
    /// An object whose keys are not closed, such as the opaque auth `config`.
    Opaque,
}

impl Shape {
    /// The well-formed representative that replaces a wrongly typed value, so
    /// the typed parse succeeds and the field is reported exactly once.
    fn placeholder(self) -> Value {
        match self {
            Self::Object | Self::Opaque => Value::Object(Map::new()),
            Self::Array => Value::Array(Vec::new()),
            Self::Text(placeholder) => Value::String((*placeholder).to_owned()),
            Self::Integer(placeholder) => Value::from(placeholder),
            Self::Bool(placeholder) => Value::Bool(placeholder),
            Self::NameMap => Value::Object(Map::new()),
            Self::NameList(placeholder) => {
                Value::Array(vec![Value::String((*placeholder).to_owned())])
            }
            Self::Uuid => Value::String(Uuid::nil().to_string()),
        }
    }

    /// Whether a value of the payload is of the declared type.
    fn accepts(self, value: &Value) -> bool {
        match self {
            Self::Object | Self::Opaque => value.is_object(),
            Self::Array => value.is_array(),
            Self::Text(_) => value.is_string(),
            Self::Integer(_) => value.is_i64(),
            Self::Bool(_) => value.is_boolean(),
            Self::NameMap => value
                .as_object()
                .is_some_and(|map| map.values().all(Value::is_string)),
            Self::NameList(_) => value
                .as_array()
                .is_some_and(|items| items.iter().all(Value::is_string)),
            Self::Uuid => value
                .as_str()
                .is_some_and(|value| Uuid::parse_str(value).is_ok()),
        }
    }

    /// Whether a required field of this shape must also carry an entry.
    fn is_empty(self, value: &Value) -> bool {
        match self {
            Self::Array | Self::NameList(_) => value.as_array().is_some_and(Vec::is_empty),
            Self::NameMap => value.as_object().is_some_and(Map::is_empty),
            Self::Text(_) => value.as_str().is_some_and(str::is_empty),
            Self::Object | Self::Opaque | Self::Integer(_) | Self::Bool(_) | Self::Uuid => false,
        }
    }

    /// Whether the unknown-field walk descends into a value of this shape.
    fn is_container(self) -> bool {
        matches!(self, Self::Object | Self::Array)
    }
}

/// One declared field of a payload: its path, its shape and the violation kind
/// a failure of the field is reported as.
#[derive(Debug, Clone, Copy)]
struct ShapeRule {
    /// Dotted field path, with `name[]` for the entries of an array.
    path: &'static str,
    /// The declared shape of the field.
    shape: Shape,
    /// The kind a violation of this field is reported as.
    kind: ViolationKind,
    /// Whether the field must be present, and non-empty for a collection.
    required: bool,
    /// The value a wrongly typed or absent field is replaced with.
    placeholder: Placeholder,
}

/// The value a wrongly typed or absent field is replaced with, so the typed
/// parse of the payload always succeeds and the field is reported exactly once.
#[derive(Debug, Clone, Copy)]
enum Placeholder {
    /// A valid representative of the declared shape.
    FromShape,
    /// A complete value of a structural field whose children are required too.
    Complete(fn() -> Value),
}

const fn rule(path: &'static str, shape: Shape, kind: ViolationKind) -> ShapeRule {
    ShapeRule {
        path,
        shape,
        kind,
        required: false,
        placeholder: Placeholder::FromShape,
    }
}

const fn complete(mut field: ShapeRule, placeholder: fn() -> Value) -> ShapeRule {
    field.placeholder = Placeholder::Complete(placeholder);
    field
}

impl ShapeRule {
    /// The replacement value of the field.
    fn placeholder(&self) -> Value {
        match self.placeholder {
            Placeholder::FromShape => self.shape.placeholder(),
            Placeholder::Complete(build) => build(),
        }
    }
}

const fn required(mut field: ShapeRule) -> ShapeRule {
    field.required = true;
    field
}

/// The declared field set of an upstream payload, in table order — parents
/// before children, so a required parent is materialised before its children
/// are checked.
const UPSTREAM_SHAPE: &[ShapeRule] = &[
    rule("id", Shape::Uuid, ViolationKind::MalformedUuid),
    // `enabled` is a declared field of `schemas/upstream.v1.schema.json`, so a
    // value outside its declared shape is a shape violation, not an unknown
    // field. The closed vocabulary of `cpt-cf-oagw-dod-domain-error` carries no
    // kind for a shape failure of a top-level upstream field outside the
    // declared blocks — the enum that owns the kinds lives in
    // `src/domain/error.rs` — so the shape-pass kind stays here.
    rule("enabled", Shape::Bool(true), ViolationKind::UnknownField),
    rule(
        "alias",
        Shape::Text(PLACEHOLDER_ALIAS),
        ViolationKind::MalformedAlias,
    ),
    rule("tags", Shape::Array, ViolationKind::MalformedTag),
    rule(
        "tags[]",
        Shape::Text(PLACEHOLDER_TAG),
        ViolationKind::MalformedTag,
    ),
    required(rule(
        "protocol",
        Shape::Text(PROTOCOL_HTTP),
        ViolationKind::EndpointRule,
    )),
    required(rule("server", Shape::Object, ViolationKind::EndpointRule)),
    required(complete(
        rule(
            "server.endpoints",
            Shape::Array,
            ViolationKind::EndpointRule,
        ),
        placeholder_endpoints,
    )),
    rule(
        "server.endpoints[]",
        Shape::Object,
        ViolationKind::EndpointRule,
    ),
    required(rule(
        "server.endpoints[].scheme",
        Shape::Text("https"),
        ViolationKind::EndpointRule,
    )),
    required(rule(
        "server.endpoints[].host",
        Shape::Text(PLACEHOLDER_HOST),
        ViolationKind::EndpointRule,
    )),
    rule(
        "server.endpoints[].port",
        Shape::Integer(DEFAULT_ENDPOINT_PORT),
        ViolationKind::OutOfRangePort,
    ),
    rule("auth", Shape::Object, ViolationKind::AuthShape),
    rule(
        "auth.type",
        Shape::Text(PLACEHOLDER_AUTH_PLUGIN),
        ViolationKind::AuthShape,
    ),
    rule(
        "auth.sharing",
        Shape::Text(SHARING_PRIVATE),
        ViolationKind::AuthShape,
    ),
    rule("auth.config", Shape::Opaque, ViolationKind::AuthShape),
    rule(
        "auth.config.secret_ref",
        Shape::Text("cred://placeholder"),
        ViolationKind::AuthShape,
    ),
    rule("headers", Shape::Object, ViolationKind::HeadersShape),
    rule(
        "headers.request",
        Shape::Object,
        ViolationKind::HeadersShape,
    ),
    rule(
        "headers.request.set",
        Shape::NameMap,
        ViolationKind::HeadersShape,
    ),
    rule(
        "headers.request.add",
        Shape::NameMap,
        ViolationKind::HeadersShape,
    ),
    rule(
        "headers.request.remove",
        Shape::NameList(PLACEHOLDER_HEADER),
        ViolationKind::HeadersShape,
    ),
    rule(
        "headers.request.remove[]",
        Shape::Text(PLACEHOLDER_HEADER),
        ViolationKind::HeadersShape,
    ),
    rule(
        "headers.request.passthrough",
        Shape::Text(PASSTHROUGH_NONE),
        ViolationKind::HeadersShape,
    ),
    rule(
        "headers.request.passthrough_allowlist",
        Shape::NameList(PLACEHOLDER_HEADER),
        ViolationKind::HeadersShape,
    ),
    rule(
        "headers.request.passthrough_allowlist[]",
        Shape::Text(PLACEHOLDER_HEADER),
        ViolationKind::HeadersShape,
    ),
    rule(
        "headers.response",
        Shape::Object,
        ViolationKind::HeadersShape,
    ),
    rule(
        "headers.response.set",
        Shape::NameMap,
        ViolationKind::HeadersShape,
    ),
    rule(
        "headers.response.add",
        Shape::NameMap,
        ViolationKind::HeadersShape,
    ),
    rule(
        "headers.response.remove",
        Shape::NameList(PLACEHOLDER_HEADER),
        ViolationKind::HeadersShape,
    ),
    rule(
        "headers.response.remove[]",
        Shape::Text(PLACEHOLDER_HEADER),
        ViolationKind::HeadersShape,
    ),
    rule("plugins", Shape::Object, ViolationKind::PluginShape),
    rule(
        "plugins.sharing",
        Shape::Text(SHARING_PRIVATE),
        ViolationKind::PluginShape,
    ),
    rule("plugins.items", Shape::Array, ViolationKind::PluginShape),
    rule(
        "plugins.items[]",
        Shape::Text(PLACEHOLDER_GUARD_PLUGIN),
        ViolationKind::PluginShape,
    ),
    rule("rate_limit", Shape::Object, ViolationKind::RateLimitShape),
    rule(
        "rate_limit.sharing",
        Shape::Text(SHARING_PRIVATE),
        ViolationKind::RateLimitShape,
    ),
    rule(
        "rate_limit.algorithm",
        Shape::Text(ALGORITHM_TOKEN_BUCKET),
        ViolationKind::RateLimitShape,
    ),
    required(complete(
        rule(
            "rate_limit.sustained",
            Shape::Object,
            ViolationKind::RateLimitShape,
        ),
        placeholder_sustained,
    )),
    required(rule(
        "rate_limit.sustained.rate",
        Shape::Integer(1),
        ViolationKind::RateLimitShape,
    )),
    rule(
        "rate_limit.sustained.window",
        Shape::Text(WINDOW_SECOND),
        ViolationKind::RateLimitShape,
    ),
    rule(
        "rate_limit.burst",
        Shape::Object,
        ViolationKind::RateLimitShape,
    ),
    rule(
        "rate_limit.burst.capacity",
        Shape::Integer(1),
        ViolationKind::RateLimitShape,
    ),
    rule(
        "rate_limit.scope",
        Shape::Text(SCOPE_TENANT),
        ViolationKind::RateLimitShape,
    ),
    rule(
        "rate_limit.strategy",
        Shape::Text(STRATEGY_REJECT),
        ViolationKind::RateLimitShape,
    ),
    rule(
        "rate_limit.cost",
        Shape::Integer(DEFAULT_RATE_COST),
        ViolationKind::RateLimitShape,
    ),
    rule("cors", Shape::Object, ViolationKind::CorsShape),
    rule(
        "cors.sharing",
        Shape::Text(SHARING_PRIVATE),
        ViolationKind::CorsShape,
    ),
    required(rule(
        "cors.enabled",
        Shape::Bool(false),
        ViolationKind::CorsShape,
    )),
    rule(
        "cors.allowed_origins",
        Shape::Array,
        ViolationKind::CorsShape,
    ),
    rule(
        "cors.allowed_origins[]",
        Shape::Text(PLACEHOLDER_ORIGIN),
        ViolationKind::CorsShape,
    ),
    rule(
        "cors.allowed_methods",
        Shape::Array,
        ViolationKind::CorsShape,
    ),
    rule(
        "cors.allowed_methods[]",
        Shape::Text("GET"),
        ViolationKind::CorsShape,
    ),
    rule(
        "cors.expose_headers",
        Shape::NameList(PLACEHOLDER_HEADER),
        ViolationKind::CorsShape,
    ),
    rule(
        "cors.expose_headers[]",
        Shape::Text(PLACEHOLDER_HEADER),
        ViolationKind::CorsShape,
    ),
    rule(
        "cors.allow_credentials",
        Shape::Bool(false),
        ViolationKind::CorsShape,
    ),
];

/// The declared field set of a route payload: the field set of
/// `schemas/route.v1.schema.json` plus the `enabled` and `priority` fields
/// declared in FEATURE §1.5. The `cors`, `headers` and `auth` blocks are
/// upstream-only and are declared nowhere here, so a route payload carrying
/// them is reported as an unknown field.
const ROUTE_SHAPE: &[ShapeRule] = &[
    rule("id", Shape::Uuid, ViolationKind::MalformedUuid),
    // The two §1.5 extension fields are declared members, so a value outside
    // their declared shape is a route-shape violation — the kind the sibling
    // `priority` rule of `validate_route_values` reports — and not an unknown
    // field, which a field the field set does not declare is reported as.
    rule("enabled", Shape::Bool(true), ViolationKind::RouteMatchShape),
    rule(
        "priority",
        Shape::Integer(0),
        ViolationKind::RouteMatchShape,
    ),
    rule("tags", Shape::Array, ViolationKind::MalformedTag),
    rule(
        "tags[]",
        Shape::Text(PLACEHOLDER_TAG),
        ViolationKind::MalformedTag,
    ),
    required(rule(
        "upstream_id",
        Shape::Uuid,
        ViolationKind::MalformedUuid,
    )),
    required(complete(
        rule("match", Shape::Object, ViolationKind::RouteMatchShape),
        placeholder_match,
    )),
    rule("match.http", Shape::Object, ViolationKind::RouteMatchShape),
    required(complete(
        rule(
            "match.http.methods",
            Shape::Array,
            ViolationKind::RouteMatchShape,
        ),
        placeholder_methods,
    )),
    rule(
        "match.http.methods[]",
        Shape::Text("GET"),
        ViolationKind::RouteMatchShape,
    ),
    required(rule(
        "match.http.path",
        Shape::Text(PLACEHOLDER_PATH),
        ViolationKind::RouteMatchShape,
    )),
    rule(
        "match.http.query_allowlist",
        Shape::NameList("placeholder"),
        ViolationKind::RouteMatchShape,
    ),
    rule(
        "match.http.query_allowlist[]",
        Shape::Text("placeholder"),
        ViolationKind::RouteMatchShape,
    ),
    rule(
        "match.http.path_suffix_mode",
        Shape::Text(PATH_SUFFIX_APPEND),
        ViolationKind::RouteMatchShape,
    ),
    rule("match.grpc", Shape::Object, ViolationKind::RouteMatchShape),
    required(rule(
        "match.grpc.service",
        Shape::Text(PLACEHOLDER_GRPC_SERVICE),
        ViolationKind::RouteMatchShape,
    )),
    required(rule(
        "match.grpc.method",
        Shape::Text(PLACEHOLDER_GRPC_METHOD),
        ViolationKind::RouteMatchShape,
    )),
    rule("plugins", Shape::Object, ViolationKind::PluginShape),
    rule(
        "plugins.sharing",
        Shape::Text(SHARING_PRIVATE),
        ViolationKind::PluginShape,
    ),
    rule("plugins.items", Shape::Array, ViolationKind::PluginShape),
    rule(
        "plugins.items[]",
        Shape::Text(PLACEHOLDER_GUARD_PLUGIN),
        ViolationKind::PluginShape,
    ),
    rule("rate_limit", Shape::Object, ViolationKind::RateLimitShape),
    rule(
        "rate_limit.sharing",
        Shape::Text(SHARING_PRIVATE),
        ViolationKind::RateLimitShape,
    ),
    rule(
        "rate_limit.algorithm",
        Shape::Text(ALGORITHM_TOKEN_BUCKET),
        ViolationKind::RateLimitShape,
    ),
    required(complete(
        rule(
            "rate_limit.sustained",
            Shape::Object,
            ViolationKind::RateLimitShape,
        ),
        placeholder_sustained,
    )),
    required(rule(
        "rate_limit.sustained.rate",
        Shape::Integer(1),
        ViolationKind::RateLimitShape,
    )),
    rule(
        "rate_limit.sustained.window",
        Shape::Text(WINDOW_SECOND),
        ViolationKind::RateLimitShape,
    ),
    rule(
        "rate_limit.burst",
        Shape::Object,
        ViolationKind::RateLimitShape,
    ),
    rule(
        "rate_limit.burst.capacity",
        Shape::Integer(1),
        ViolationKind::RateLimitShape,
    ),
    rule(
        "rate_limit.scope",
        Shape::Text(SCOPE_TENANT),
        ViolationKind::RateLimitShape,
    ),
    rule(
        "rate_limit.strategy",
        Shape::Text(STRATEGY_REJECT),
        ViolationKind::RateLimitShape,
    ),
    rule(
        "rate_limit.cost",
        Shape::Integer(DEFAULT_RATE_COST),
        ViolationKind::RateLimitShape,
    ),
];

/// The declared field set of a `Plugin` payload
/// (`cpt-cf-oagw-dod-plugin-identity`).
const PLUGIN_SHAPE: &[ShapeRule] = &[
    rule("id", Shape::Uuid, ViolationKind::MalformedUuid),
    rule(
        "plugin_type",
        Shape::Text(PLACEHOLDER_GUARD_PLUGIN),
        ViolationKind::PluginShape,
    ),
    rule(
        "name",
        Shape::Text(PLACEHOLDER_PLUGIN_NAME),
        ViolationKind::PluginShape,
    ),
    rule(
        "description",
        Shape::Text("A placeholder plugin description."),
        ViolationKind::PluginShape,
    ),
    rule("config_schema", Shape::Opaque, ViolationKind::PluginShape),
    rule(
        "source_code",
        Shape::Text(PLACEHOLDER_SOURCE),
        ViolationKind::PluginShape,
    ),
];

/// Hook the route validation consults for the tenant-scoped referential
/// integrity of `upstream_id` (`inst-rv-14`, `inst-sv-08`).
///
/// The in-memory upstream store implements it, so a reference into another or
/// an ancestor tenant fails the check instead of reaching a foreign resource.
pub trait UpstreamExistence {
    /// Whether an upstream with this identifier exists in the caller's tenant.
    fn upstream_exists(&self, tenant_id: Uuid, upstream_id: Uuid) -> bool;

    /// The protocol of that upstream, when it exists, so the route match is
    /// checked against the protocol it scopes.
    fn upstream_protocol(&self, _tenant_id: Uuid, _upstream_id: Uuid) -> Option<String> {
        None
    }
}

/// A hook that answers "absent" for every upstream, for a payload validated
/// before any store exists.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoUpstreams;

impl UpstreamExistence for NoUpstreams {
    fn upstream_exists(&self, _tenant_id: Uuid, _upstream_id: Uuid) -> bool {
        false
    }
}

/// Validates an upstream payload and returns the aggregate stamped with the
/// caller's tenant (`inst-rv-01`).
///
/// # Errors
/// Returns one [`DomainError`] carrying every violated rule, in field order.
pub fn validate_upstream_payload(
    payload: &Value,
    tenant_id: Uuid,
) -> Result<Upstream, DomainError> {
    // @cpt-begin:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-01
    // The payload shape to validate — `UPSTREAM_SHAPE`, the field list of
    // `schemas/upstream.v1.schema.json` — received together with the caller's
    // tenant context.
    // @cpt-end:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-01
    let mut violations = Violations::new();
    let mut shaped = payload.clone();
    strip_unknown_fields(&mut shaped, "", UPSTREAM_SHAPE, &mut violations);
    apply_shape_rules(&mut shaped, UPSTREAM_SHAPE, &mut violations);
    let mut upstream: Upstream = parse_shaped(&shaped, &mut violations);
    upstream.tenant_id = Some(tenant_id);
    validate_identity(upstream.tenant_id, &mut violations);
    validate_upstream_values(&upstream, &mut violations);
    // @cpt-begin:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-15
    // IF any violation was recorded in the preceding steps.
    // @cpt-begin:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-16
    // RETURN one `DomainError` carrying every violated rule id and its message,
    // in field order, so the caller sees all failures of one payload at once.
    violations.into_result(Upstream::FIELD_ORDER)?;
    // @cpt-end:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-16
    // @cpt-end:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-15
    // @cpt-begin:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-17
    // ELSE the payload violated no rule of the flow, so ...
    // @cpt-end:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-17
    // @cpt-begin:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-18
    // ... RETURN the validated aggregate, ready for the tenant-scoped write of
    // `cpt-cf-oagw-flow-tenant-scoped-lookup`.
    Ok(upstream)
    // @cpt-end:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-18
}

/// Validates a route payload and returns the aggregate stamped with the
/// caller's tenant (`inst-rv-01`).
///
/// # Errors
/// Returns one [`DomainError`] carrying every violated rule, in field order.
pub fn validate_route_payload(
    payload: &Value,
    tenant_id: Uuid,
    upstreams: &dyn UpstreamExistence,
) -> Result<Route, DomainError> {
    let mut violations = Violations::new();
    let mut shaped = payload.clone();
    strip_unknown_fields(&mut shaped, "", ROUTE_SHAPE, &mut violations);
    apply_shape_rules(&mut shaped, ROUTE_SHAPE, &mut violations);
    let mut route: Route = parse_shaped(&shaped, &mut violations);
    route.tenant_id = Some(tenant_id);
    validate_identity(route.tenant_id, &mut violations);
    validate_route_values(&route, Some(tenant_id), upstreams, &mut violations);
    violations.into_result(Route::FIELD_ORDER)?;
    Ok(route)
}

/// Validates a `Plugin` payload and returns the aggregate stamped with the
/// caller's tenant (`cpt-cf-oagw-dod-plugin-identity`).
///
/// # Errors
/// Returns one [`DomainError`] carrying every violated rule, in field order.
pub fn validate_plugin_payload(payload: &Value, tenant_id: Uuid) -> Result<Plugin, DomainError> {
    let mut violations = Violations::new();
    let mut shaped = payload.clone();
    strip_unknown_fields(&mut shaped, "", PLUGIN_SHAPE, &mut violations);
    apply_shape_rules(&mut shaped, PLUGIN_SHAPE, &mut violations);
    let mut plugin: Plugin = parse_shaped(&shaped, &mut violations);
    plugin.tenant_id = Some(tenant_id);
    validate_identity(plugin.tenant_id, &mut violations);
    validate_plugin(&plugin, &mut violations);
    violations.into_result(Plugin::FIELD_ORDER)?;
    Ok(plugin)
}

/// Validates an already-typed upstream aggregate, the check every repository
/// write runs before it mutates (`inst-mr-03`).
///
/// # Errors
/// Returns one [`DomainError`] carrying every violated rule, in field order.
pub fn validate_upstream(upstream: &Upstream) -> Result<(), DomainError> {
    let mut violations = Violations::new();
    validate_identity(upstream.tenant_id, &mut violations);
    validate_upstream_values(upstream, &mut violations);
    violations.into_result(Upstream::FIELD_ORDER)
}

/// Validates an already-typed route aggregate, the check every repository write
/// runs before it mutates (`inst-mr-03`).
///
/// # Errors
/// Returns one [`DomainError`] carrying every violated rule, in field order.
pub fn validate_route(route: &Route, upstreams: &dyn UpstreamExistence) -> Result<(), DomainError> {
    let mut violations = Violations::new();
    validate_identity(route.tenant_id, &mut violations);
    validate_route_values(route, route.tenant_id, upstreams, &mut violations);
    violations.into_result(Route::FIELD_ORDER)
}

/// Validates an already-typed `Plugin` aggregate (`inst-mr-03`).
///
/// # Errors
/// Returns one [`DomainError`] carrying every violated rule, in field order.
pub fn validate_plugin_aggregate(plugin: &Plugin) -> Result<(), DomainError> {
    let mut violations = Violations::new();
    validate_identity(plugin.tenant_id, &mut violations);
    validate_plugin(plugin, &mut violations);
    violations.into_result(Plugin::FIELD_ORDER)
}

/// The identity check of `inst-rv-02`: the presence of the caller's
/// `tenant_id`, which the caller supplies and no payload carries.
fn validate_identity(tenant_id: Option<Uuid>, violations: &mut Violations) {
    // @cpt-begin:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-02
    // The caller's tenant context; `id` and, for an upstream, the `protocol`
    // GTS identifier are checked by the caller and by `validate_protocol`.
    if tenant_id.is_none() {
        violations.record(
            ViolationKind::MalformedUuid,
            "tenant_id",
            "the caller's tenant context is required",
        );
    }
    // @cpt-end:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-02
}

/// The value rules of an upstream: the endpoint pool, the alias, the tags and
/// every block of `cpt-cf-oagw-algo-shape-validation` an upstream carries.
fn validate_upstream_values(upstream: &Upstream, violations: &mut Violations) {
    validate_protocol(upstream.protocol.as_deref(), violations);
    let endpoints = upstream
        .server
        .as_ref()
        .map(|server| server.endpoints.as_slice())
        .unwrap_or_default();
    validate_endpoints(endpoints, violations);
    // @cpt-begin:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-09
    // The alias value against the alias pattern and the tags against the tag
    // pattern with per-resource uniqueness; the alias is an upstream-only
    // check, and whether it was derived or explicitly supplied is the concern
    // of `cpt-cf-oagw-feature-alias-resolution`, not of this check.
    if let Some(alias) = upstream.alias.as_deref() {
        validate_alias(alias, violations);
    }
    validate_tags(&upstream.tags, violations);
    // @cpt-end:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-09
    // @cpt-begin:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-10
    // The rate-limit block through `cpt-cf-oagw-algo-shape-validation`.
    if let Some(rate_limit) = &upstream.rate_limit {
        validate_rate_limit(rate_limit, violations);
    }
    // @cpt-end:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-10
    // @cpt-begin:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-11
    // The CORS block through the same algorithm.
    if let Some(cors) = &upstream.cors {
        validate_cors(cors, violations);
    }
    // @cpt-end:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-11
    if let Some(headers) = &upstream.headers {
        validate_headers(headers, violations);
    }
    if let Some(auth) = &upstream.auth {
        validate_auth(auth, violations);
    }
    // @cpt-begin:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-12
    // The plugin list shape per aggregate: an upstream accepts the
    // custom-plugin UUID branch of the schema `oneOf`.
    if let Some(plugins) = &upstream.plugins {
        validate_plugins(plugins, true, violations);
    }
    // @cpt-end:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-12
}

/// The `protocol` GTS identifier of an upstream (`inst-rv-02`).
fn validate_protocol(protocol: Option<&str>, violations: &mut Violations) {
    match protocol {
        None => violations.record(
            ViolationKind::EndpointRule,
            "protocol",
            "a protocol GTS identifier is required",
        ),
        Some(protocol) => {
            if !PROTOCOL_INSTANCES.contains(&protocol) {
                violations.record(
                    ViolationKind::EndpointRule,
                    "protocol",
                    format!(
                        "'{}' is not one of the declared protocol instances {}",
                        echoed(protocol),
                        PROTOCOL_INSTANCES.join(", ")
                    ),
                );
            }
        }
    }
}

/// The value rules of a route (`inst-rv-09` to `inst-rv-14`): tags, the
/// rate-limit and plugin blocks, the `match` block and the tenant-scoped
/// `upstream_id` reference.
fn validate_route_values(
    route: &Route,
    tenant_id: Option<Uuid>,
    upstreams: &dyn UpstreamExistence,
    violations: &mut Violations,
) {
    if route.priority < 0 {
        violations.record(
            ViolationKind::RouteMatchShape,
            "priority",
            format!("'{}' must not be negative", route.priority),
        );
    }
    validate_tags(&route.tags, violations);
    if let Some(rate_limit) = &route.rate_limit {
        validate_rate_limit(rate_limit, violations);
    }
    if let Some(plugins) = &route.plugins {
        validate_plugins(plugins, false, violations);
    }
    // @cpt-begin:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-13
    // The route `match` block when the payload is a route, per
    // `schemas/route.v1.schema.json`.
    if let (Some(tenant_id), Some(match_config)) = (tenant_id, &route.match_config) {
        let protocol = route
            .upstream_id
            .filter(|upstream_id| upstreams.upstream_exists(tenant_id, *upstream_id))
            .and_then(|upstream_id| upstreams.upstream_protocol(tenant_id, upstream_id));
        validate_match(match_config, protocol.as_deref(), violations);
    }
    // @cpt-end:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-13
    // @cpt-begin:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-14
    // The route `upstream_id` when the payload is a route: present, a UUID, and
    // referring to an upstream that exists in the caller's tenant.
    if let (Some(tenant_id), false) = (tenant_id, violations.has_field("upstream_id")) {
        validate_upstream_reference(tenant_id, route.upstream_id, upstreams, violations);
    }
    // @cpt-end:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-14
}

/// `cpt-cf-oagw-algo-endpoint-validation`: the scheme, host and port rules of
/// every endpoint (`inst-ev-01` to `inst-ev-06`), the minimum list size
/// (`inst-ev-11`) and the homogeneity of the pool (`inst-ev-07` to
/// `inst-ev-10`).
pub fn validate_endpoints(endpoints: &[Endpoint], violations: &mut Violations) {
    // @cpt-begin:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-03
    // FOR EACH endpoint in `server.endpoints` ...
    for (position, endpoint) in endpoints.iter().enumerate() {
        // @cpt-begin:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-04
        // ... validate `scheme`, `host` and `port` through
        // `cpt-cf-oagw-algo-endpoint-validation`.
        validate_endpoint(endpoint, position, violations);
        // @cpt-end:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-04
    }
    // @cpt-end:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-03
    if endpoints.is_empty() {
        violations.record(
            ViolationKind::EndpointRule,
            "server.endpoints",
            "at least one endpoint is required",
        );
        return;
    }
    // @cpt-begin:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-05
    // IF the endpoint list is heterogeneous in protocol, scheme or port, which
    // the pairwise comparison of `cpt-cf-oagw-algo-endpoint-validation` below
    // decides.
    // @cpt-end:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-05
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-09
    // IF any pair differs in scheme or port.
    let diverging = diverging_positions(endpoints);
    if !diverging.is_empty() {
        // @cpt-begin:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-06
        // One homogeneity violation naming the diverging endpoint positions, so
        // a pool mixing `http` and `https` (or ports 443 and 8443) is rejected
        // as a single violation.
        // @cpt-end:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-06
        // @cpt-begin:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-10
        // The one upstream-level `protocol` is constant across the pool, so the
        // pairwise comparison covers scheme and port per endpoint and the
        // protocol through the owning upstream.
        violations.record(
            ViolationKind::EndpointHeterogeneity,
            "server.endpoints",
            format!(
                "endpoints {diverging:?} diverge from endpoint 0 in scheme or port; one endpoint pool shares one protocol, scheme and port"
            ),
        );
        // @cpt-end:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-10
    } else {
        // @cpt-begin:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-07
        // ELSE the endpoint list is one homogeneous pool, so it contributes no
        // violation of its own.
        // @cpt-end:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-07
    }
    // @cpt-end:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-09
    // @cpt-begin:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-08
    // The pool is accepted and the remaining value rules continue with the
    // aggregate: the empty-list and per-endpoint checks above have already
    // spoken for it.
    // @cpt-end:cpt-cf-oagw-flow-resource-validation:p1:inst-rv-08
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-11
    // RETURN the validated endpoint list, or the collected violations when the
    // list is empty (`minItems: 1` in the schema) or any rule failed.
    // @cpt-end:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-11
}

/// `inst-ev-07` and `inst-ev-08`: the pairwise comparison of the pool.
fn diverging_positions(endpoints: &[Endpoint]) -> Vec<usize> {
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-07
    // FOR EACH pair of endpoints in the list, the positions whose scheme or
    // port differs from endpoint 0.
    // @cpt-end:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-07
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-08
    // An endpoint carries no `protocol` of its own in
    // `schemas/upstream.v1.schema.json`, so its protocol is the owning
    // upstream's single `protocol` value, constant across the pool, and only
    // scheme and port are compared per endpoint.
    endpoints
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, endpoint)| {
            endpoint.scheme != endpoints[0].scheme || endpoint.port != endpoints[0].port
        })
        .map(|(position, _)| position)
        .collect()
    // @cpt-end:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-08
}

/// Validates one endpoint of the pool against the endpoint algorithm.
fn validate_endpoint(endpoint: &Endpoint, position: usize, violations: &mut Violations) {
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-01
    // The endpoint is already parsed into `scheme`, `host` and `port`, with the
    // schema defaults of `https` and 443 applied by the typed aggregate.
    // @cpt-end:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-01
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-02
    // The correction-2 scheme enum, which extends the declared schema enum.
    if endpoint.scheme_enum().is_none() {
        violations.record(
            ViolationKind::EndpointRule,
            format!("server.endpoints[{position}].scheme"),
            format!(
                "'{}' is outside the accepted scheme enum {}",
                endpoint.scheme,
                ENDPOINT_SCHEMES.join(", ")
            ),
        );
    }
    // @cpt-end:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-02
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-03
    // The RFC 1123 hostname or IP literal, carrying no embedded port and no
    // path segment.
    let host_field = format!("server.endpoints[{position}].host");
    match endpoint.host.as_deref() {
        None => {
            violations.record(
                ViolationKind::EndpointRule,
                &host_field,
                "a host is required",
            );
        }
        Some(host) => {
            if let Err(message) = validate_hostname(host) {
                violations.record(ViolationKind::EndpointRule, &host_field, &message);
            }
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-03
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-04
    // The port, an integer in the range 1 to 65535.
    if !(MIN_PORT..=MAX_PORT).contains(&endpoint.port) {
        violations.record(
            ViolationKind::OutOfRangePort,
            format!("server.endpoints[{position}].port"),
            format!(
                "'{}' is outside the port range {MIN_PORT}..={MAX_PORT}",
                endpoint.port
            ),
        );
    }
    // @cpt-end:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-04
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-05
    // IF the scheme is `wt`: accepted as valid configuration and flagged
    // not-proxied ...
    if endpoint
        .scheme_enum()
        .is_some_and(|scheme| scheme.is_proxied())
    {
        // @cpt-end:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-05
        // @cpt-begin:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-06
        // ... so a proxy request routed to a non-proxied scheme is answered with
        // the gateway `RouteError`/`ProtocolError` semantics of the error
        // mapping table, which this feature does not consult.
        // @cpt-end:cpt-cf-oagw-algo-endpoint-validation:p1:inst-ev-06
    }
}

/// Validates a `host` value per RFC 1123 or as an IP literal (`inst-ev-03`): a
/// trailing dot is tolerated and stripped, an embedded port or path segment is
/// rejected.
fn validate_hostname(host: &str) -> Result<(), String> {
    let host = strip_trailing_dot(host);
    if host.is_empty() {
        return Err("a host is required".to_owned());
    }
    if host.contains('/') {
        return Err(format!("'{}' embeds a path segment", echoed(host)));
    }
    if host.parse::<IpAddr>().is_ok() {
        return Ok(());
    }
    if host.contains(':') {
        return Err(format!(
            "'{}' embeds a port; declare it in the endpoint's 'port' field",
            echoed(host)
        ));
    }
    // The RFC 1123 hostname-length bound: a fully qualified hostname is at most
    // 255 octets, which is 253 characters in the textual form this check
    // validates (no trailing dot). Endpoint hosts are bounded by it here, and
    // the alias — which a hostname endpoint derives from — carries the same
    // ceiling in `validate_alias`.
    if host.len() > 253 {
        return Err(format!("'{}' is longer than 253 characters", echoed(host)));
    }
    for label in host.split('.') {
        if label.is_empty() {
            return Err(format!("'{}' carries an empty label", echoed(host)));
        }
        if label.len() > 63 {
            return Err(format!(
                "the label '{}' is longer than 63 characters",
                echoed(label)
            ));
        }
        if !label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(format!(
                "the label '{}' carries a character outside the RFC 1123 label set",
                echoed(label)
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!(
                "the label '{}' starts or ends with a hyphen",
                echoed(label)
            ));
        }
    }
    Ok(())
}

/// Validates an alias against the pattern
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$` of `schemas/upstream.v1.schema.json`
/// (`inst-rv-09`).
///
/// The alias is an upstream-only check, since
/// `schemas/route.v1.schema.json` declares no `alias`; whether the alias was
/// derived or explicitly supplied is the concern of
/// `cpt-cf-oagw-feature-alias-resolution`, not of this check.
pub fn validate_alias(alias: &str, violations: &mut Violations) {
    let field = "alias";
    if alias.is_empty() {
        violations.record(ViolationKind::MalformedAlias, field, "an alias is required");
        return;
    }
    // The 253-character ceiling mirrors the RFC 1123 hostname-length bound — a
    // fully qualified hostname is at most 255 octets, 253 characters in the
    // textual form without the trailing dot — because an alias of a
    // hostname-based endpoint is derived from that host and is resolved as one
    // (`{METHOD} /oagw/v1/proxy/{alias}/{path}`). The schema pattern bounds the
    // character set only, so the length bound is carried here, where the alias
    // is enforced, and not in the pattern.
    if alias.len() > 253 {
        violations.record(
            ViolationKind::MalformedAlias,
            field,
            format!("'{}' is longer than 253 characters", echoed(alias)),
        );
    }
    let characters: Vec<char> = alias.chars().collect();
    let allowed = |character: char| character.is_ascii_lowercase() || character.is_ascii_digit();
    if !allowed(characters[0]) {
        violations.record(
            ViolationKind::MalformedAlias,
            field,
            format!(
                "'{}' must start with a lowercase letter or a digit",
                echoed(alias)
            ),
        );
    }
    if characters.len() > 1 && !allowed(characters[characters.len() - 1]) {
        violations.record(
            ViolationKind::MalformedAlias,
            field,
            format!(
                "'{}' must end with a lowercase letter or a digit",
                echoed(alias)
            ),
        );
    }
    for character in characters.iter().take(characters.len() - 1).skip(1) {
        if !allowed(*character) && !crate::domain::model::ALIAS_INNER_CHARS.contains(character) {
            violations.record(
                ViolationKind::MalformedAlias,
                field,
                format!(
                    "'{}' carries '{character}', which the alias pattern does not allow",
                    echoed(alias)
                ),
            );
        }
    }
}

/// Validates the tags against `^[a-z0-9_-]+$` and per-resource uniqueness
/// (`inst-rv-09`, `inst-sv-11`).
pub fn validate_tags(tags: &[String], violations: &mut Violations) {
    // @cpt-begin:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-11
    // Each tag matches the tag pattern and the tags are unique within the
    // resource.
    for (index, tag) in tags.iter().enumerate() {
        let allowed = !tag.is_empty()
            && tag.chars().all(|character| {
                character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || crate::domain::model::TAG_EXTRA_CHARS.contains(&character)
            });
        if !allowed {
            violations.record(
                ViolationKind::MalformedTag,
                format!("tags[{index}]"),
                format!("tag '{}' must match ^[a-z0-9_-]+$", echoed(tag)),
            );
        }
    }
    let mut seen = BTreeSet::new();
    for (index, tag) in tags.iter().enumerate() {
        if !seen.insert(tag.as_str()) {
            violations.record(
                ViolationKind::MalformedTag,
                format!("tags[{index}]"),
                format!("tag '{}' is not unique within the resource", echoed(tag)),
            );
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-11
}

/// `inst-sv-01`: the rate-limit block, shared by upstreams and routes.
pub fn validate_rate_limit(rate_limit: &RateLimitConfig, violations: &mut Violations) {
    // @cpt-begin:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-01
    // `sharing`, `algorithm`, `sustained.rate` and `sustained.window`,
    // `burst.capacity`, `scope`, `cost` and `strategy`, with `sliding_window`
    // accepted as the fixed-window approximation of correction 8 and `queue`
    // and `degrade` accepted as valid configuration whose behaviour is not
    // implemented (correction 6).
    enum_check(
        &rate_limit.sharing,
        SHARING_MODES,
        "rate_limit.sharing",
        ViolationKind::RateLimitShape,
        violations,
    );
    enum_check(
        &rate_limit.algorithm,
        RATE_ALGORITHMS,
        "rate_limit.algorithm",
        ViolationKind::RateLimitShape,
        violations,
    );
    match &rate_limit.sustained {
        None => violations.record(
            ViolationKind::RateLimitShape,
            "rate_limit.sustained",
            "a sustained rate is required",
        ),
        Some(sustained) => {
            match sustained.rate {
                None => violations.record(
                    ViolationKind::RateLimitShape,
                    "rate_limit.sustained.rate",
                    "a sustained rate is required",
                ),
                Some(rate) if rate < 1 => violations.record(
                    ViolationKind::RateLimitShape,
                    "rate_limit.sustained.rate",
                    format!("'{rate}' must be at least 1"),
                ),
                Some(_) => {}
            }
            enum_check(
                &sustained.window,
                RATE_WINDOWS,
                "rate_limit.sustained.window",
                ViolationKind::RateLimitShape,
                violations,
            );
        }
    }
    if let Some(burst) = &rate_limit.burst
        && burst.capacity.is_some_and(|capacity| capacity < 1)
    {
        violations.record(
            ViolationKind::RateLimitShape,
            "rate_limit.burst.capacity",
            format!(
                "'{}' must be at least 1",
                burst.capacity.unwrap_or_default()
            ),
        );
    }
    enum_check(
        &rate_limit.scope,
        RATE_SCOPES,
        "rate_limit.scope",
        ViolationKind::RateLimitShape,
        violations,
    );
    enum_check(
        &rate_limit.strategy,
        RATE_STRATEGIES,
        "rate_limit.strategy",
        ViolationKind::RateLimitShape,
        violations,
    );
    if rate_limit.cost.is_some_and(|cost| cost < 1) {
        violations.record(
            ViolationKind::RateLimitShape,
            "rate_limit.cost",
            format!(
                "'{}' must be at least 1",
                rate_limit.cost.unwrap_or_default()
            ),
        );
    }
    // @cpt-end:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-01
}

/// `inst-sv-02` to `inst-sv-04`: the CORS block, an upstream-only block.
pub fn validate_cors(cors: &CorsConfig, violations: &mut Violations) {
    // @cpt-begin:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-02
    // The CORS block, an upstream-only block: `enabled`, `allowed_origins`,
    // `allowed_methods` and `expose_headers`.
    enum_check(
        &cors.sharing,
        SHARING_MODES,
        "cors.sharing",
        ViolationKind::CorsShape,
        violations,
    );
    if cors.enabled.is_none() {
        violations.record(
            ViolationKind::CorsShape,
            "cors.enabled",
            "an enabled flag is required",
        );
    }
    for (index, origin) in cors.allowed_origins.iter().enumerate() {
        if origin == CORS_WILDCARD_ORIGIN {
            continue;
        }
        if let Err(message) = validate_origin(origin) {
            violations.record(
                ViolationKind::CorsShape,
                format!("cors.allowed_origins[{index}]"),
                &message,
            );
        }
    }
    for (index, method) in cors.allowed_methods.iter().enumerate() {
        if !CORS_METHODS.contains(&method.as_str()) {
            violations.record(
                ViolationKind::CorsShape,
                format!("cors.allowed_methods[{index}]"),
                format!(
                    "'{}' is outside the closed method enum {CORS_METHODS:?}",
                    echoed(method)
                ),
            );
        }
    }
    for (index, header) in cors.expose_headers.iter().enumerate() {
        if let Err(message) = validate_header_name(header) {
            violations.record(
                ViolationKind::CorsShape,
                format!("cors.expose_headers[{index}]"),
                &message,
            );
        }
    }
    // @cpt-begin:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-03
    // IF `allow_credentials` is true and `allowed_origins` contains `*`.
    if cors.allow_credentials
        && let Some(index) = cors
            .allowed_origins
            .iter()
            .position(|origin| origin == CORS_WILDCARD_ORIGIN)
    {
        // @cpt-begin:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-04
        // The credentials-with-wildcard violation: both JSON Schemas express
        // this as a conditional constraint and `cpt-cf-oagw-adr-cors` rejects
        // it at validation time rather than at request time.
        violations.record(
            ViolationKind::CorsShape,
            format!("cors.allowed_origins[{index}]"),
            "allow_credentials must not be combined with the wildcard origin (ADR 0004)",
        );
        // @cpt-end:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-04
    }
    // @cpt-end:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-03
    // @cpt-end:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-02
}

/// Validates a CORS origin: an absolute `http` or `https` origin URI, carrying
/// no path, query or fragment.
fn validate_origin(origin: &str) -> Result<(), String> {
    let parsed = Url::parse(origin)
        .map_err(|error| format!("'{}' is not an absolute URI: {error}", echoed(origin)))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!(
            "'{}' must be an http or https origin",
            echoed(origin)
        ));
    }
    if parsed.host_str().is_none() {
        return Err(format!("'{}' must carry a host", echoed(origin)));
    }
    if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(format!(
            "'{}' must be an origin, not a URL carrying a path, query or fragment",
            echoed(origin)
        ));
    }
    Ok(())
}

/// `inst-sv-05`: the upstream-only headers block, including the two
/// feature-local rules of FEATURE §1.5 — the `passthrough_allowlist`
/// requirement and the protection of the well-known headers.
pub fn validate_headers(headers: &HeadersConfig, violations: &mut Violations) {
    // @cpt-begin:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-05
    // The upstream-only headers block: the six name maps, the `passthrough`
    // mode, the feature-local `passthrough_allowlist` requirement and the
    // protection of the well-known headers.
    if let Some(request) = &headers.request {
        validate_header_map(&request.set, "headers.request.set", violations);
        validate_header_map(&request.add, "headers.request.add", violations);
        validate_header_names(&request.remove, "headers.request.remove", violations);
        enum_check(
            &request.passthrough,
            PASSTHROUGH_MODES,
            "headers.request.passthrough",
            ViolationKind::HeadersShape,
            violations,
        );
        validate_header_names(
            &request.passthrough_allowlist,
            "headers.request.passthrough_allowlist",
            violations,
        );
        if request.passthrough == PASSTHROUGH_ALLOWLIST && request.passthrough_allowlist.is_empty()
        {
            violations.record(
                ViolationKind::HeadersShape,
                "headers.request.passthrough_allowlist",
                "is required when passthrough is 'allowlist'",
            );
        }
        record_protected_removals(&request.remove, "headers.request.remove", violations);
    }
    if let Some(response) = &headers.response {
        validate_header_map(&response.set, "headers.response.set", violations);
        validate_header_map(&response.add, "headers.response.add", violations);
        validate_header_names(&response.remove, "headers.response.remove", violations);
        record_protected_removals(&response.remove, "headers.response.remove", violations);
    }
    // @cpt-end:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-05
}

/// The feature-local rule that removing a well-known header such as
/// `Content-Length` or `Content-Type` is a violation, while setting or
/// adjusting one stays permitted.
fn record_protected_removals(names: &[String], field: &str, violations: &mut Violations) {
    for (index, name) in names.iter().enumerate() {
        if PROTECTED_HEADERS.contains(&name.to_lowercase().as_str()) {
            violations.record(
                ViolationKind::HeadersShape,
                format!("{field}[{index}]"),
                format!(
                    "removing the well-known header '{}' is not permitted; setting or adjusting it is",
                    echoed(name)
                ),
            );
        }
    }
}

fn validate_header_map(
    headers: &std::collections::BTreeMap<String, String>,
    field: &str,
    violations: &mut Violations,
) {
    for (name, value) in headers {
        if let Err(message) = validate_header_name(name) {
            violations.record(ViolationKind::HeadersShape, field, &message);
        }
        if value.is_empty() {
            violations.record(
                ViolationKind::HeadersShape,
                format!("{field}.{name}"),
                "a header value is required",
            );
        }
    }
}

fn validate_header_names(names: &[String], field: &str, violations: &mut Violations) {
    for (index, name) in names.iter().enumerate() {
        if let Err(message) = validate_header_name(name) {
            violations.record(
                ViolationKind::HeadersShape,
                format!("{field}[{index}]"),
                &message,
            );
        }
    }
}

/// Validates a header name: non-empty printable ASCII without a colon.
fn validate_header_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a header name is required".to_owned());
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_graphic() && byte != b':')
    {
        return Err(format!("'{}' is not a valid header name", echoed(name)));
    }
    Ok(())
}

/// `inst-sv-06`: the upstream-only auth block, including the `cred://` form of
/// the secret reference and the rejection of inline secret material.
pub fn validate_auth(auth: &AuthConfig, violations: &mut Violations) {
    // @cpt-begin:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-06
    // The upstream-only auth block: the GTS auth-plugin identifier, the sharing
    // mode, the opaque `config` object and the `cred://` secret reference it
    // carries, with inline secret material rejected.
    match auth.plugin_type.as_deref() {
        None => violations.record(
            ViolationKind::AuthShape,
            "auth.type",
            "an auth plugin identifier is required",
        ),
        Some(plugin_type) => {
            let in_family = PLUGIN_TYPE_IDS
                .iter()
                .any(|family| plugin_type.starts_with(family) && plugin_type.len() > family.len());
            if !in_family {
                violations.record(
                    ViolationKind::AuthShape,
                    "auth.type",
                    format!(
                        "'{}' is not a member of the gts.cf.core.oagw.{{type}}_plugin.v1~ family",
                        echoed(plugin_type)
                    ),
                );
            }
        }
    }
    enum_check(
        &auth.sharing,
        SHARING_MODES,
        "auth.sharing",
        ViolationKind::AuthShape,
        violations,
    );
    let Some(config) = &auth.config else {
        return;
    };
    match config.get("secret_ref") {
        Some(Value::String(reference)) => {
            if !reference.starts_with(CRED_REF_SCHEME) {
                // The value the field carries is secret material, so the message
                // names the field path and the expected form only, exactly as
                // the inline-secret branch below names only its key — the value
                // a caller submits must never reach a problem detail or a log.
                violations.record(
                    ViolationKind::AuthShape,
                    "auth.config.secret_ref",
                    format!(
                        "must be a '{CRED_REF_SCHEME}' reference that the cred-store resolves at request time"
                    ),
                );
            }
        }
        Some(_) => violations.record(
            ViolationKind::AuthShape,
            "auth.config.secret_ref",
            format!("must be a string carrying a '{CRED_REF_SCHEME}' reference"),
        ),
        None => {}
    }
    for key in INLINE_SECRET_KEYS {
        let Some(value) = config.get(*key) else {
            continue;
        };
        let inline = value
            .as_str()
            .is_some_and(|value| !value.starts_with(CRED_REF_SCHEME));
        if inline || !value.is_string() {
            violations.record(
                ViolationKind::AuthShape,
                format!("auth.config.{key}"),
                format!(
                    "carries inline secret material; use a '{CRED_REF_SCHEME}' reference that the cred-store resolves"
                ),
            );
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-06
}

/// `inst-sv-07`, `inst-rv-13`: the route `match` block, exactly one of `http`
/// or `grpc`, checked against the protocol of the referenced upstream when it
/// is known.
pub fn validate_match(
    match_config: &MatchConfig,
    protocol: Option<&str>,
    violations: &mut Violations,
) {
    // @cpt-begin:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-07
    // Exactly one of `http` or `grpc` present (the `oneOf` of
    // `schemas/route.v1.schema.json`), the http methods and path, the grpc
    // service and method, the query allowlist and the path-suffix-mode enum.
    match (&match_config.http, &match_config.grpc) {
        (Some(_), Some(_)) => violations.record(
            ViolationKind::RouteMatchShape,
            "match",
            "carries both 'http' and 'grpc'; exactly one of the two is required",
        ),
        (None, None) => violations.record(
            ViolationKind::RouteMatchShape,
            "match",
            "carries neither 'http' nor 'grpc'; exactly one of the two is required",
        ),
        (Some(http), None) => {
            check_match_protocol(protocol, false, violations);
            validate_http_match(http, violations);
        }
        (None, Some(grpc)) => {
            check_match_protocol(protocol, true, violations);
            validate_grpc_match(grpc, violations);
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-07
}

/// Checks that the one flavour the `match` block carries is the one the
/// referenced upstream's protocol scopes.
fn check_match_protocol(protocol: Option<&str>, grpc: bool, violations: &mut Violations) {
    let Some(protocol) = protocol else {
        return;
    };
    if (protocol == PROTOCOL_GRPC) != grpc {
        violations.record(
            ViolationKind::RouteMatchShape,
            "match",
            format!(
                "must be the {} match of the upstream protocol '{protocol}'",
                if protocol == PROTOCOL_GRPC {
                    "grpc"
                } else {
                    "http"
                }
            ),
        );
    }
}

fn validate_http_match(http: &HttpMatch, violations: &mut Violations) {
    // The shape pass already reported an absent or empty method list and
    // materialised its default, so the value pass reports only the entries.
    if http.methods.is_empty() && !violations.has_field("match.http.methods") {
        violations.record(
            ViolationKind::RouteMatchShape,
            "match.http.methods",
            "at least one method is required",
        );
    }
    for (index, method) in http.methods.iter().enumerate() {
        if !HTTP_METHODS.contains(&method.as_str()) {
            violations.record(
                ViolationKind::RouteMatchShape,
                format!("match.http.methods[{index}]"),
                format!(
                    "'{}' is outside the closed method enum {HTTP_METHODS:?}",
                    echoed(method)
                ),
            );
        }
    }
    match http.path.as_deref() {
        None => violations.record(
            ViolationKind::RouteMatchShape,
            "match.http.path",
            "a path is required",
        ),
        Some("") => violations.record(
            ViolationKind::RouteMatchShape,
            "match.http.path",
            "must not be empty",
        ),
        Some(_) => {}
    }
    for (index, name) in http.query_allowlist.iter().enumerate() {
        if name.is_empty() {
            violations.record(
                ViolationKind::RouteMatchShape,
                format!("match.http.query_allowlist[{index}]"),
                "a query-parameter name is required",
            );
        }
    }
    enum_check(
        &http.path_suffix_mode,
        PATH_SUFFIX_MODES,
        "match.http.path_suffix_mode",
        ViolationKind::RouteMatchShape,
        violations,
    );
}

fn validate_grpc_match(grpc: &GrpcMatch, violations: &mut Violations) {
    for (field, value) in [
        ("match.grpc.service", grpc.service.as_deref()),
        ("match.grpc.method", grpc.method.as_deref()),
    ] {
        match value {
            None => violations.record(ViolationKind::RouteMatchShape, field, "a value is required"),
            Some("") => {
                violations.record(ViolationKind::RouteMatchShape, field, "must not be empty");
            }
            Some(_) => {}
        }
    }
}

/// `inst-rv-14`, `inst-sv-08`: the tenant-scoped referential integrity of
/// `upstream_id`, a not-found violation so a reference into another or an
/// ancestor tenant is indistinguishable from an absent upstream.
pub fn validate_upstream_reference(
    tenant_id: Uuid,
    upstream_id: Option<Uuid>,
    upstreams: &dyn UpstreamExistence,
    violations: &mut Violations,
) {
    // @cpt-begin:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-08
    // Present, a UUID, and referring to an upstream that exists in the caller's
    // tenant, so referential integrity is checked across the tenant store and
    // not merely the identifier's form.
    let Some(upstream_id) = upstream_id else {
        violations.record(
            ViolationKind::MalformedUuid,
            "upstream_id",
            "an upstream_id is required",
        );
        return;
    };
    if !upstreams.upstream_exists(tenant_id, upstream_id) {
        violations.record(
            ViolationKind::NotFound,
            "upstream_id",
            format!("no upstream '{upstream_id}' exists in the caller's tenant"),
        );
    }
    // @cpt-end:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-08
}

/// `inst-sv-10`, `inst-rv-12`: the plugins block of an aggregate, where an
/// upstream accepts the custom-plugin UUID branch of the schema `oneOf` and a
/// route binds GTS plugin identifiers only.
pub fn validate_plugins(
    plugins: &PluginsConfig,
    allow_uuid_items: bool,
    violations: &mut Violations,
) {
    // @cpt-begin:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-10
    // `items` entries are either a named GTS plugin identifier or, for an
    // upstream only, a custom-plugin UUID (the schema `oneOf`);
    // `plugin_ref` is always present and `plugin_uuid` derived only when the
    // reference is UUID-backed, with binding positions contiguous from 0.
    enum_check(
        &plugins.sharing,
        SHARING_MODES,
        "plugins.sharing",
        ViolationKind::PluginShape,
        violations,
    );
    for (position, reference) in plugins.items.iter().enumerate() {
        let field = format!("plugins.items[{position}]");
        // A named reference carries a family prefix *and* an instance, so a bare
        // family prefix names no plugin — the same requirement the auth check
        // applies to `auth.type`. A custom-plugin UUID is the other `oneOf`
        // branch and stays `uuid_backed`, so a route keeps rejecting it.
        let named =
            split_plugin_reference(reference).is_some_and(|(_, instance)| !instance.is_empty());
        let uuid_backed = Uuid::parse_str(reference).is_ok();
        if named || (uuid_backed && allow_uuid_items) {
            continue;
        }
        violations.record(
            ViolationKind::PluginShape,
            &field,
            format!(
                "'{}' is {}",
                echoed(reference),
                if uuid_backed {
                    "a custom-plugin UUID, and a route binds GTS plugin identifiers only"
                } else {
                    "neither a GTS plugin identifier nor a custom-plugin UUID"
                }
            ),
        );
    }
    validate_bindings(&plugins.bindings(), violations);
    // @cpt-end:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-10
}

/// `cpt-cf-oagw-dod-plugin-identity`: every binding carries a `plugin_ref`,
/// `plugin_uuid` is derived only for a UUID-backed reference and is verified
/// against it, and the positions are contiguous from 0.
pub fn validate_bindings(bindings: &[PluginBinding], violations: &mut Violations) {
    for (position, binding) in bindings.iter().enumerate() {
        let field = format!("plugins.bindings[{position}]");
        if binding.plugin_ref.is_empty() {
            violations.record(
                ViolationKind::PluginShape,
                &field,
                "a plugin_ref is always present",
            );
        }
        if binding.position != position {
            violations.record(
                ViolationKind::PluginShape,
                &field,
                format!(
                    "binding positions must be contiguous from 0, found {}",
                    binding.position
                ),
            );
        }
        if let Some(uuid) = binding.plugin_uuid
            && plugin_ref_uuid(&binding.plugin_ref) != Some(uuid)
        {
            violations.record(
                ViolationKind::PluginShape,
                format!("{field}.plugin_uuid"),
                format!(
                    "does not match the reference '{}'",
                    echoed(&binding.plugin_ref)
                ),
            );
        }
    }
}

/// `inst-sv-09`: the `Plugin` aggregate, a Starlark custom plugin of the
/// `gts.cf.core.oagw.{{type}}_plugin.v1~` family.
pub fn validate_plugin(plugin: &Plugin, violations: &mut Violations) {
    // @cpt-begin:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-09
    // `plugin_type` a member of the `gts.cf.core.oagw.{{type}}_plugin.v1~`
    // family, `name` present (its per-tenant uniqueness is the `(tenant_id,
    // name)` constraint of `cpt-cf-oagw-db-schema`, which the store enforces),
    // and for a Starlark plugin a JSON Schema object `config_schema` plus a
    // non-empty `source_code`.
    match plugin.plugin_type.as_deref() {
        None => violations.record(
            ViolationKind::PluginShape,
            "plugin_type",
            "a plugin type is required",
        ),
        Some(plugin_type) => {
            if !PLUGIN_TYPE_IDS.contains(&plugin_type) {
                violations.record(
                    ViolationKind::PluginShape,
                    "plugin_type",
                    format!(
                        "'{}' is not a member of the gts.cf.core.oagw.{{type}}_plugin.v1~ family",
                        echoed(plugin_type)
                    ),
                );
            }
        }
    }
    match plugin.name.as_deref() {
        None => violations.record(
            ViolationKind::PluginShape,
            "name",
            "a plugin name is required",
        ),
        Some(name) if name.trim().is_empty() => {
            violations.record(ViolationKind::PluginShape, "name", "must not be blank");
        }
        Some(_) => {}
    }
    if plugin
        .config_schema
        .as_ref()
        .is_some_and(|schema| !schema.is_object())
    {
        violations.record(
            ViolationKind::PluginShape,
            "config_schema",
            "must be a JSON Schema object",
        );
    }
    match plugin.source_code.as_deref() {
        None => violations.record(
            ViolationKind::PluginShape,
            "source_code",
            "a Starlark plugin carries its source",
        ),
        Some(source) if source.trim().is_empty() => {
            violations.record(
                ViolationKind::PluginShape,
                "source_code",
                "must not be blank",
            );
        }
        Some(_) => {}
    }
    // @cpt-end:cpt-cf-oagw-algo-shape-validation:p1:inst-sv-09
}

fn enum_check(
    value: &str,
    allowed: &[&str],
    field: &str,
    kind: ViolationKind,
    violations: &mut Violations,
) {
    if !allowed.contains(&value) {
        violations.record(
            kind,
            field,
            format!("'{}' is outside the closed enum {allowed:?}", echoed(value)),
        );
    }
}

/// Parses the shaped payload into the aggregate, reporting the one violation
/// that remains when a shape table and the model drift apart.
///
/// The fallback is the typed default, never a panic: the aggregate every model
/// declares deserialises from an empty object (all of its fields carry a serde
/// default), so a drift between a shape table and the model is a recorded
/// violation on a request path, not an aborted request.
fn parse_shaped<T: DeserializeOwned + Default>(shaped: &Value, violations: &mut Violations) -> T {
    match serde_json::from_value(shaped.clone()) {
        Ok(aggregate) => aggregate,
        Err(error) => {
            violations.record(
                ViolationKind::UnknownField,
                "",
                format!("the payload is not shaped as the declared field set: {error}"),
            );
            T::default()
        }
    }
}

/// Removes every field the declared field set does not declare, reporting each
/// one as an unknown-field violation at its dotted path.
fn strip_unknown_fields(
    payload: &mut Value,
    path: &str,
    table: &[ShapeRule],
    violations: &mut Violations,
) {
    if let Some(shape) = declared_shape(table, path) {
        // An open subtree (the opaque auth `config`, a name map) or a
        // non-container contributes no walk.
        if matches!(shape, Shape::Opaque | Shape::NameMap) || !shape.is_container() {
            return;
        }
        // A declared array or object whose payload value is not of that kind is
        // left unwalked: the shape rule reports the one type error, and walking
        // a mistyped container would first report each of its keys as an unknown
        // field the payload never declared.
        if matches!(shape, Shape::Object) != payload.is_object() {
            return;
        }
    }
    match payload {
        Value::Object(map) => {
            let undeclared: Vec<String> = map
                .keys()
                .filter(|key| declared_shape(table, &join(path, key)).is_none())
                .cloned()
                .collect();
            for key in undeclared {
                map.remove(&key);
                violations.record(
                    ViolationKind::UnknownField,
                    join(path, &key),
                    "the field set of the aggregate does not declare it",
                );
            }
            for (key, child) in map.iter_mut() {
                strip_unknown_fields(child, &join(path, key), table, violations);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter_mut().enumerate() {
                strip_unknown_fields(child, &index_path(path, index), table, violations);
            }
        }
        _ => {}
    }
}

/// Checks the declared shape and the required presence of every declared field,
/// replacing a wrongly typed or absent value with a well-formed representative.
fn apply_shape_rules(payload: &mut Value, table: &[ShapeRule], violations: &mut Violations) {
    for entry in table {
        let path = segments(entry.path);
        let found = instances(payload, &path);
        if found.is_empty() {
            // A required field of a block the payload does not carry at all is
            // not missing: the parent rule, which the table declares first,
            // already handled that.
            if entry.required && !instances(payload, &path[..path.len() - 1]).is_empty() {
                violations.record(entry.kind, entry.path, "a value is required");
                insert_placeholder(payload, &template(&path), entry.placeholder());
            }
            continue;
        }
        for path in found {
            let field = render(&path);
            let Some(value) = value_at_mut(payload, &path) else {
                continue;
            };
            if !entry.shape.accepts(value) {
                violations.record(entry.kind, &field, shape_message(entry.shape, value));
                *value = entry.placeholder();
            } else if entry.required
                && entry.shape.is_empty(value)
                // A required scalar of shape `Text` carries its emptiness to the
                // value pass, whose "must not be empty" branch names the empty
                // string precisely; the placeholder here would erase it and
                // replace the caller's value with a path that was never sent.
                && !matches!(entry.shape, Shape::Text(_))
            {
                violations.record(entry.kind, &field, "must carry at least one entry");
                *value = entry.placeholder();
            }
        }
    }
}

/// The declared shape of a resolved field path, if the table declares it.
fn declared_shape(table: &[ShapeRule], path: &str) -> Option<Shape> {
    table
        .iter()
        .find(|entry| matches_pattern(entry.path, path))
        .map(|entry| entry.shape)
}

/// Reports whether a declared path matches a resolved one, where a declared
/// `name[]` matches any `name[index]`.
///
/// The array wildcard needs a segment boundary: `tags[]` matches `tags[0]` but
/// not `tagsfoo`, so an undeclared key that merely begins with the name of an
/// array field stays undeclared and is stripped as an unknown field instead of
/// surviving the strip pass and breaking the typed parse.
fn matches_pattern(pattern: &str, path: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('.').collect();
    let path: Vec<&str> = path.split('.').collect();
    pattern.len() == path.len()
        && pattern.iter().zip(&path).all(|(declared, found)| {
            *declared == *found
                || (declared.ends_with("[]")
                    && found
                        .strip_prefix(declared.trim_end_matches("[]"))
                        .is_some_and(|rest| rest.starts_with('[')))
        })
}

/// One segment of a declared field path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Segment {
    /// A JSON object key.
    Key(&'static str),
    /// Each entry of a JSON array, written `name[]` in a declared path.
    Each,
}

/// The concrete path a required field is materialised at: the first entry of an
/// array stands in for its `name[]` segment.
fn template(path: &[Segment]) -> Vec<Step> {
    path.iter()
        .map(|segment| match segment {
            Segment::Key(key) => Step::Key(key),
            Segment::Each => Step::Index(0),
        })
        .collect()
}

fn segments(path: &'static str) -> Vec<Segment> {
    path.split('.')
        .flat_map(|part| match part.strip_suffix("[]") {
            Some(name) => vec![Segment::Key(name), Segment::Each],
            None => vec![Segment::Key(part)],
        })
        .collect()
}

/// One step of a resolved field path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// A JSON object key.
    Key(&'static str),
    /// An index into a JSON array.
    Index(usize),
}

/// Resolves every instance of a declared path in the payload: a `name[]`
/// segment yields one instance per array entry.
fn instances(value: &Value, path: &[Segment]) -> Vec<Vec<Step>> {
    let Some((first, rest)) = path.split_first() else {
        return vec![Vec::new()];
    };
    match first {
        Segment::Key(key) => match value.get(*key) {
            None => Vec::new(),
            Some(child) => instances(child, rest)
                .into_iter()
                .map(|mut tail| {
                    tail.insert(0, Step::Key(key));
                    tail
                })
                .collect(),
        },
        Segment::Each => match value.as_array() {
            None => Vec::new(),
            Some(items) => items
                .iter()
                .enumerate()
                .flat_map(|(index, child)| {
                    instances(child, rest)
                        .into_iter()
                        .map(move |mut tail| {
                            tail.insert(0, Step::Index(index));
                            tail
                        })
                        .collect::<Vec<_>>()
                })
                .collect(),
        },
    }
}

fn value_at_mut<'a>(payload: &'a mut Value, path: &[Step]) -> Option<&'a mut Value> {
    let mut current = payload;
    for step in path {
        current = match step {
            Step::Key(key) => current.get_mut(*key)?,
            Step::Index(index) => current.get_mut(*index)?,
        };
    }
    Some(current)
}

/// Renders a resolved path as the dotted field path a violation names.
fn render(path: &[Step]) -> String {
    let mut rendered = String::new();
    for step in path {
        match step {
            Step::Key(key) => {
                if !rendered.is_empty() {
                    rendered.push('.');
                }
                rendered.push_str(key);
            }
            Step::Index(index) => {
                rendered.push('[');
                rendered.push_str(&index.to_string());
                rendered.push(']');
            }
        }
    }
    rendered
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

fn index_path(path: &str, index: usize) -> String {
    format!("{path}[{index}]")
}

/// Materialises the placeholder of a required field, creating any missing
/// parent object and appending to a missing or empty parent array.
fn insert_placeholder(payload: &mut Value, path: &[Step], placeholder: Value) {
    let Some((first, rest)) = path.split_first() else {
        *payload = placeholder;
        return;
    };
    match first {
        Step::Key(key) => {
            if !payload.is_object() {
                *payload = Value::Object(Map::new());
            }
            let Some(object) = payload.as_object_mut() else {
                return;
            };
            match object.entry((*key).to_owned()) {
                serde_json::map::Entry::Occupied(occupied) => {
                    insert_placeholder(occupied.into_mut(), rest, placeholder);
                }
                serde_json::map::Entry::Vacant(vacant) => {
                    if rest.is_empty() {
                        vacant.insert(placeholder);
                    } else {
                        let mut child = container_for(rest);
                        insert_placeholder(&mut child, rest, placeholder);
                        vacant.insert(child);
                    }
                }
            }
        }
        Step::Index(index) => {
            if !payload.is_array() {
                *payload = Value::Array(Vec::new());
            }
            let Some(array) = payload.as_array_mut() else {
                return;
            };
            while array.len() <= *index {
                array.push(Value::Object(Map::new()));
            }
            insert_placeholder(&mut array[*index], rest, placeholder);
        }
    }
}

/// The container a missing parent of `path` must be, so its children resolve.
fn container_for(path: &[Step]) -> Value {
    match path.first() {
        Some(Step::Index(_)) => Value::Array(Vec::new()),
        _ => Value::Object(Map::new()),
    }
}

/// The message of a shape violation, naming the type that was found and the
/// type the field declares.
fn shape_message(shape: Shape, value: &Value) -> String {
    let found = match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    };
    let expected = match shape {
        Shape::Object | Shape::Opaque => "an object",
        Shape::Array => "an array",
        Shape::Text(_) => "a string",
        Shape::Integer(_) => "an integer",
        Shape::Bool(_) => "a boolean",
        Shape::NameMap => "an object of header name to header value",
        Shape::NameList(_) => "an array of names",
        Shape::Uuid => "a UUID string",
    };
    format!("found {found}, but the field declares {expected}")
}

// @cpt-end:cpt-cf-oagw-dod-validation-rules:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::error::{OagwError, Violation};
    use crate::domain::model::{PLUGIN_TYPE_IDS, PROTOCOL_HTTP, ROUTE_TYPE_ID, UPSTREAM_TYPE_ID};

    const TENANT: &str = "7f0a1b2c-3d4e-4f50-8617-8899aabbccdd";
    const UPSTREAM_ID: &str = "0f0a1b2c-3d4e-4f50-8617-8899aabbccdd";
    const PLUGIN_ID: &str = "3f0a1b2c-3d4e-4f50-8617-8899aabbccdd";
    const AUTH_PLUGIN: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
    const GUARD_PLUGIN: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";

    fn tenant() -> Uuid {
        Uuid::parse_str(TENANT).unwrap()
    }

    /// A store stub answering for exactly one upstream of one tenant.
    struct KnownUpstreams {
        tenant_id: Uuid,
        upstream_id: Uuid,
        protocol: &'static str,
    }

    impl UpstreamExistence for KnownUpstreams {
        fn upstream_exists(&self, tenant_id: Uuid, upstream_id: Uuid) -> bool {
            tenant_id == self.tenant_id && upstream_id == self.upstream_id
        }

        fn upstream_protocol(&self, tenant_id: Uuid, upstream_id: Uuid) -> Option<String> {
            self.upstream_exists(tenant_id, upstream_id)
                .then(|| self.protocol.to_owned())
        }
    }

    /// A complete, well-formed upstream payload: every schema field populated.
    fn valid_upstream_json() -> Value {
        serde_json::json!({
            "id": UPSTREAM_ID,
            "enabled": true,
            "alias": "payments.vendor.com",
            "tags": ["openai", "llm"],
            "server": {
                "endpoints": [
                    { "scheme": "https", "host": "api.vendor.com", "port": 443 },
                    { "scheme": "https", "host": "eu.vendor.com.", "port": 443 }
                ]
            },
            "protocol": PROTOCOL_HTTP,
            "auth": {
                "type": AUTH_PLUGIN,
                "sharing": "private",
                "config": { "secret_ref": "cred://vendor/openai/api_key" }
            },
            "headers": {
                "request": {
                    "set": { "x-forwarded-by": "oagw" },
                    "add": { "x-request-id": "req-1" },
                    "remove": ["x-internal-token"],
                    "passthrough": "allowlist",
                    "passthrough_allowlist": ["accept", "content-type"]
                },
                "response": {
                    "set": { "x-gateway": "oagw" },
                    "add": { "x-trace-id": "trace-1" },
                    "remove": ["server"]
                }
            },
            "plugins": { "sharing": "private", "items": [GUARD_PLUGIN, PLUGIN_ID] },
            "rate_limit": {
                "sharing": "private",
                "algorithm": "token_bucket",
                "sustained": { "rate": 100, "window": "minute" },
                "burst": { "capacity": 200 },
                "scope": "tenant",
                "strategy": "reject",
                "cost": 2
            },
            "cors": {
                "sharing": "private",
                "enabled": true,
                "allowed_origins": ["https://console.vendor.com"],
                "allowed_methods": ["GET", "POST"],
                "expose_headers": ["x-request-id"],
                "allow_credentials": false
            }
        })
    }

    /// A complete, well-formed route payload: the schema field set plus the
    /// declared `enabled` and `priority` extension.
    fn valid_route_json() -> Value {
        serde_json::json!({
            "id": "1f0a1b2c-3d4e-4f50-8617-8899aabbccdd",
            "tags": ["chat"],
            "upstream_id": UPSTREAM_ID,
            "match": {
                "http": {
                    "methods": ["POST", "GET"],
                    "path": "/v1/chat",
                    "query_allowlist": ["model"],
                    "path_suffix_mode": "append"
                }
            },
            "plugins": { "sharing": "private", "items": [GUARD_PLUGIN] },
            "rate_limit": {
                "sharing": "private",
                "algorithm": "sliding_window",
                "sustained": { "rate": 10, "window": "second" },
                "burst": { "capacity": 40 },
                "scope": "user",
                "strategy": "queue",
                "cost": 1
            },
            "enabled": false,
            "priority": 10
        })
    }

    /// The same well-formed route, matching its upstream on the gRPC flavour
    /// instead, so both declared `match` branches carry a fixture.
    fn valid_grpc_route_json() -> Value {
        serde_json::json!({
            "id": "2f0a1b2c-3d4e-4f50-8617-8899aabbccdd",
            "tags": ["rpc"],
            "upstream_id": UPSTREAM_ID,
            "match": {
                "grpc": {
                    "service": "vendor.chat.v1.ChatService",
                    "method": "Send"
                }
            },
            "plugins": { "sharing": "private", "items": [GUARD_PLUGIN] },
            "rate_limit": {
                "sharing": "private",
                "algorithm": "fixed_window",
                "sustained": { "rate": 20, "window": "minute" },
                "burst": { "capacity": 60 },
                "scope": "tenant",
                "strategy": "reject",
                "cost": 1
            },
            "enabled": true,
            "priority": 5
        })
    }

    /// A well-formed `Plugin` payload.
    fn valid_plugin_json() -> Value {
        serde_json::json!({
            "id": PLUGIN_ID,
            "plugin_type": PLUGIN_TYPE_IDS[1],
            "name": "redact-headers",
            "description": "Removes headers the tenant declares.",
            "config_schema": {
                "type": "object",
                "properties": { "headers": { "type": "array" } }
            },
            "source_code": "def on_request(context):\n    return context\n"
        })
    }

    fn fields_of(error: &DomainError) -> Vec<String> {
        error
            .to_violations()
            .into_iter()
            .map(|violation: Violation| violation.field)
            .collect()
    }

    fn kinds_of(error: &DomainError) -> Vec<ViolationKind> {
        error
            .to_violations()
            .into_iter()
            .map(|violation| violation.kind)
            .collect()
    }

    /// Every leaf of a serialised payload, at its dotted path. A subtree the
    /// declared shape leaves open, such as the opaque auth `config`, is one
    /// leaf.
    fn leaf_paths(value: &Value, path: &str, table: &[ShapeRule], leaves: &mut Vec<String>) {
        match declared_shape(table, path) {
            Some(Shape::Opaque) | Some(Shape::NameMap) => {
                leaves.push(path.to_owned());
                return;
            }
            _ => {}
        }
        match value {
            Value::Object(map) => {
                if map.is_empty() {
                    leaves.push(path.to_owned());
                }
                for (key, child) in map {
                    leaf_paths(child, &join(path, key), table, leaves);
                }
            }
            Value::Array(items) => {
                if items.is_empty() {
                    leaves.push(path.to_owned());
                }
                for (index, child) in items.iter().enumerate() {
                    leaf_paths(child, &index_path(path, index), table, leaves);
                }
            }
            _ => leaves.push(path.to_owned()),
        }
    }

    /// The JSON pointer of a dotted field path, with the first array entry
    /// standing in for the `[]` wildcard.
    fn json_pointer(path: &str) -> String {
        let mut pointer = String::new();
        for segment in path.replace("[]", "[0]").split('.') {
            match segment.split_once('[') {
                Some((name, index)) => {
                    pointer.push('/');
                    pointer.push_str(name);
                    pointer.push('/');
                    pointer.push_str(index.trim_end_matches(']'));
                }
                None => {
                    pointer.push('/');
                    pointer.push_str(segment);
                }
            }
        }
        pointer
    }

    // §6: the aggregates serialise to and from payloads that satisfy the two
    // schemas exactly, for the documented happy-path examples.

    #[test]
    fn the_documented_happy_paths_validate() {
        let tenant = tenant();
        let upstream = validate_upstream_payload(&valid_upstream_json(), tenant).unwrap();
        assert_eq!(upstream.tenant_id, Some(tenant));
        assert_eq!(upstream.alias.as_deref(), Some("payments.vendor.com"));
        assert_eq!(upstream.server.as_ref().unwrap().endpoints.len(), 2);

        let route = validate_route_payload(&valid_route_json(), tenant, &NoUpstreams);
        assert!(
            route.is_err(),
            "the referenced upstream does not exist in the caller's tenant yet"
        );
        let upstreams = KnownUpstreams {
            tenant_id: tenant,
            upstream_id: Uuid::parse_str(UPSTREAM_ID).unwrap(),
            protocol: PROTOCOL_HTTP,
        };
        let route = validate_route_payload(&valid_route_json(), tenant, &upstreams).unwrap();
        assert!(!route.enabled);
        assert_eq!(route.priority, 10);
        assert_eq!(
            route.match_config.unwrap().http.unwrap().methods,
            ["POST", "GET"]
        );

        let plugin = validate_plugin_payload(&valid_plugin_json(), tenant).unwrap();
        assert_eq!(plugin.name.as_deref(), Some("redact-headers"));
        assert_eq!(plugin.tenant_id, Some(tenant));
    }

    #[test]
    fn a_validated_payload_round_trips_through_the_aggregate() {
        let upstream = validate_upstream_payload(&valid_upstream_json(), tenant()).unwrap();
        // `tenant_id` is assigned by the caller and `#[serde(skip)]`-ped, so the
        // round trip compares the payload-carried shape only.
        let mut stripped = upstream.clone();
        stripped.tenant_id = None;
        let reparsed: Upstream =
            serde_json::from_value(serde_json::to_value(&upstream).unwrap()).unwrap();
        assert_eq!(reparsed, stripped);
        assert!(
            validate_upstream(&upstream).is_ok(),
            "the aggregate revalidates"
        );

        let upstreams = KnownUpstreams {
            tenant_id: tenant(),
            upstream_id: Uuid::parse_str(UPSTREAM_ID).unwrap(),
            protocol: PROTOCOL_HTTP,
        };
        let route = validate_route_payload(&valid_route_json(), tenant(), &upstreams).unwrap();
        let mut stripped = route.clone();
        stripped.tenant_id = None;
        let reparsed: Route =
            serde_json::from_value(serde_json::to_value(&route).unwrap()).unwrap();
        assert_eq!(reparsed, stripped);
        assert!(validate_route(&route, &upstreams).is_ok());
    }

    #[test]
    fn the_shape_tables_declare_exactly_the_serialised_field_sets() {
        for (table, payload) in [
            (UPSTREAM_SHAPE, valid_upstream_json()),
            (ROUTE_SHAPE, valid_route_json()),
            (ROUTE_SHAPE, valid_grpc_route_json()),
            (PLUGIN_SHAPE, valid_plugin_json()),
        ] {
            let serialized = serde_json::to_value(&payload).unwrap();
            let mut leaves = Vec::new();
            leaf_paths(&serialized, "", table, &mut leaves);
            for leaf in &leaves {
                assert!(
                    table.iter().any(|entry| matches_pattern(entry.path, leaf)),
                    "the serialised leaf '{leaf}' is declared by no shape rule"
                );
            }
            for entry in table {
                // The two `match` flavours are a `oneOf`: a payload carries
                // exactly one branch, so the other branch's declared paths are
                // exempt from the resolution check.
                if entry.path.starts_with("match.http")
                    && serialized.pointer("/match/grpc").is_some()
                {
                    continue;
                }
                if entry.path.starts_with("match.grpc")
                    && serialized.pointer("/match/http").is_some()
                {
                    continue;
                }
                assert!(
                    serialized.pointer(&json_pointer(entry.path)).is_some(),
                    "the declared path '{}' is absent from the serialised payload",
                    entry.path
                );
            }
        }
    }

    #[test]
    fn the_declared_shape_table_is_the_closed_field_set_of_the_schemas() {
        assert_eq!(UPSTREAM_SHAPE.len(), 56);
        assert_eq!(ROUTE_SHAPE.len(), 32);
        assert_eq!(PLUGIN_SHAPE.len(), 6);
        let mut roots: Vec<&str> = UPSTREAM_SHAPE
            .iter()
            .chain(ROUTE_SHAPE.iter())
            .map(|entry| entry.path.split('.').next().unwrap_or(entry.path))
            .collect();
        roots.sort_unstable();
        roots.dedup();
        assert_eq!(
            roots,
            [
                "alias",
                "auth",
                "cors",
                "enabled",
                "headers",
                "id",
                "match",
                "plugins",
                "priority",
                "protocol",
                "rate_limit",
                "server",
                "tags",
                "tags[]",
                "upstream_id"
            ]
        );
    }

    #[test]
    fn an_unknown_field_at_any_level_is_reported_with_its_path() {
        let mut root = valid_upstream_json();
        root["nope"] = serde_json::json!(1);
        let mut server = valid_upstream_json();
        server["server"]["nope"] = serde_json::json!(1);
        let mut endpoint = valid_upstream_json();
        endpoint["server"]["endpoints"][0]["nope"] = serde_json::json!(1);
        let mut rate_limit = valid_upstream_json();
        rate_limit["rate_limit"]["nope"] = serde_json::json!(1);
        for payload in [root, server, endpoint.clone(), rate_limit] {
            let error = validate_upstream_payload(&payload, tenant()).unwrap_err();
            assert_eq!(
                kinds_of(&error),
                vec![ViolationKind::UnknownField],
                "{error}"
            );
        }

        let error = validate_upstream_payload(&endpoint, tenant()).unwrap_err();
        assert_eq!(fields_of(&error), ["server.endpoints[0].nope"], "{error}");
    }

    #[test]
    fn a_route_cannot_carry_the_upstream_only_blocks() {
        let payload = serde_json::json!({
            "upstream_id": UPSTREAM_ID,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } },
            "cors": { "enabled": true },
            "alias": "routes-have-no-alias"
        });
        let error = validate_route_payload(&payload, tenant(), &NoUpstreams).unwrap_err();
        let mut fields = fields_of(&error);
        fields.sort();
        assert_eq!(fields, ["alias", "cors", "upstream_id"], "{error}");
        assert!(kinds_of(&error).contains(&ViolationKind::UnknownField));
    }

    #[test]
    fn a_wrongly_typed_field_is_reported_once() {
        let payload = serde_json::json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com", "port": "443" }] },
            "protocol": PROTOCOL_HTTP,
            "tags": [7],
            "rate_limit": { "sustained": { "rate": "10" } },
            "cors": { "enabled": "yes" },
            "headers": { "request": { "set": { "x-a": 1 } } }
        });
        let error = validate_upstream_payload(&payload, tenant()).unwrap_err();
        let kinds = kinds_of(&error);
        assert_eq!(kinds.len(), 5, "{error}");
        assert!(kinds.contains(&ViolationKind::OutOfRangePort));
        assert!(kinds.contains(&ViolationKind::MalformedTag));
        assert!(kinds.contains(&ViolationKind::RateLimitShape));
        assert!(kinds.contains(&ViolationKind::CorsShape), "{error}");
        assert!(kinds.contains(&ViolationKind::HeadersShape), "{error}");
    }

    #[test]
    fn a_required_field_is_reported_and_the_parse_still_succeeds() {
        let payload = serde_json::json!({ "alias": "payments" });
        let error = validate_upstream_payload(&payload, tenant()).unwrap_err();
        let mut fields = fields_of(&error);
        fields.retain(|field| field != "protocol" && field != "server.endpoints");
        assert!(fields.contains(&"server".to_owned()), "{error}");
        assert!(kinds_of(&error).contains(&ViolationKind::EndpointRule));

        let route = serde_json::json!({ "tags": ["chat"] });
        let error = validate_route_payload(&route, tenant(), &NoUpstreams).unwrap_err();
        let fields = fields_of(&error);
        assert!(fields.contains(&"upstream_id".to_owned()), "{error}");
        assert!(fields.contains(&"match".to_owned()), "{error}");
    }

    #[test]
    fn a_missing_tenant_context_is_reported() {
        let upstream = validate_upstream_payload(&valid_upstream_json(), tenant()).unwrap();
        let mut unstamped = upstream.clone();
        unstamped.tenant_id = None;
        let error = validate_upstream(&unstamped).unwrap_err();
        assert_eq!(fields_of(&error), ["tenant_id"]);
        assert_eq!(kinds_of(&error), [ViolationKind::MalformedUuid]);

        let mut plugin = validate_plugin_payload(&valid_plugin_json(), tenant()).unwrap();
        plugin.tenant_id = None;
        assert_eq!(
            fields_of(&validate_plugin_aggregate(&plugin).unwrap_err()),
            ["tenant_id"]
        );
    }

    #[test]
    fn a_non_uuid_identifier_is_reported_as_malformed() {
        let payload = serde_json::json!({
            "id": "not-a-uuid",
            "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com" }] },
            "protocol": PROTOCOL_HTTP
        });
        let error = validate_upstream_payload(&payload, tenant()).unwrap_err();
        assert_eq!(fields_of(&error), ["id"]);
        assert_eq!(kinds_of(&error), [ViolationKind::MalformedUuid]);

        let route = serde_json::json!({
            "upstream_id": "not-a-uuid",
            "match": { "http": { "methods": ["GET"], "path": "/v1" } }
        });
        let error = validate_route_payload(&route, tenant(), &NoUpstreams).unwrap_err();
        assert_eq!(fields_of(&error), ["upstream_id"]);
        assert_eq!(kinds_of(&error), [ViolationKind::MalformedUuid]);
    }

    // §6: an endpoint list that mixes http and https is heterogeneous, while a
    // pure http list is accepted (correction 2).

    #[test]
    fn a_mixed_scheme_pool_is_heterogeneous_and_a_pure_http_pool_is_not() {
        let tenant = tenant();
        let mixed = serde_json::json!({
            "server": { "endpoints": [
                { "scheme": "http", "host": "api.vendor.com", "port": 443 },
                { "scheme": "https", "host": "eu.vendor.com", "port": 443 }
            ] },
            "protocol": PROTOCOL_HTTP
        });
        let error = validate_upstream_payload(&mixed, tenant).unwrap_err();
        assert_eq!(kinds_of(&error), [ViolationKind::EndpointHeterogeneity]);
        assert_eq!(fields_of(&error), ["server.endpoints"]);

        let pure_http = serde_json::json!({
            "server": { "endpoints": [
                { "scheme": "http", "host": "api.vendor.com", "port": 443 },
                { "scheme": "http", "host": "eu.vendor.com", "port": 443 }
            ] },
            "protocol": PROTOCOL_HTTP
        });
        let upstream = validate_upstream_payload(&pure_http, tenant).unwrap();
        assert!(validate_upstream(&upstream).is_ok());
        assert!(
            upstream
                .server
                .as_ref()
                .unwrap()
                .endpoints
                .iter()
                .all(crate::domain::model::Endpoint::is_proxied)
        );
    }

    #[test]
    fn a_diverging_port_is_one_heterogeneity_violation_naming_the_positions() {
        let tenant = tenant();
        let payload = serde_json::json!({
            "server": { "endpoints": [
                { "scheme": "https", "host": "api.vendor.com", "port": 443 },
                { "scheme": "https", "host": "eu.vendor.com", "port": 8443 },
                { "scheme": "https", "host": "ap.vendor.com", "port": 8443 }
            ] },
            "protocol": PROTOCOL_HTTP
        });
        let error = validate_upstream_payload(&payload, tenant).unwrap_err();
        let violations = error.to_violations();
        assert_eq!(violations.len(), 1, "{error}");
        assert_eq!(violations[0].kind, ViolationKind::EndpointHeterogeneity);
        assert!(
            violations[0].message.contains("[1, 2]"),
            "the diverging positions are named: {}",
            violations[0].message
        );
    }

    #[test]
    fn an_empty_endpoints_list_is_rejected() {
        let payload = serde_json::json!({
            "server": { "endpoints": [] },
            "protocol": PROTOCOL_HTTP
        });
        let error = validate_upstream_payload(&payload, tenant()).unwrap_err();
        assert_eq!(fields_of(&error), ["server.endpoints"]);
        assert_eq!(kinds_of(&error), [ViolationKind::EndpointRule]);
    }

    #[test]
    fn an_unknown_scheme_is_rejected() {
        let tenant = tenant();
        let payload = serde_json::json!({
            "server": { "endpoints": [{ "scheme": "ftp", "host": "api.vendor.com", "port": 443 }] },
            "protocol": PROTOCOL_HTTP
        });
        let error = validate_upstream_payload(&payload, tenant).unwrap_err();
        assert_eq!(fields_of(&error), ["server.endpoints[0].scheme"]);
        assert_eq!(kinds_of(&error), [ViolationKind::EndpointRule]);

        for scheme in ["http", "https", "wss", "grpc", "wt"] {
            let payload = serde_json::json!({
                "server": { "endpoints": [{ "scheme": scheme, "host": "api.vendor.com", "port": 443 }] },
                "protocol": PROTOCOL_HTTP
            });
            let upstream = validate_upstream_payload(&payload, tenant).unwrap();
            assert_eq!(
                upstream.server.as_ref().unwrap().endpoints[0]
                    .scheme
                    .as_str(),
                scheme,
                "{scheme} is accepted by correction 2"
            );
            assert!(
                validate_upstream(&upstream).is_ok(),
                "{scheme} is accepted by correction 2"
            );
        }
    }

    // §6: an embedded port or an underscore host is rejected, a trailing dot is
    // accepted and stripped.

    #[test]
    fn an_embedded_port_or_underscore_host_is_rejected_and_a_trailing_dot_is_stripped() {
        let tenant = tenant();
        for host in [
            "api.openai.com:8443",
            "api_openai.com",
            "api.openai.com/v1",
            "-api.com",
            "api..com",
        ] {
            let payload = serde_json::json!({
                "server": { "endpoints": [{ "scheme": "https", "host": host, "port": 443 }] },
                "protocol": PROTOCOL_HTTP
            });
            let error = validate_upstream_payload(&payload, tenant).unwrap_err();
            assert_eq!(fields_of(&error), ["server.endpoints[0].host"], "{host}");
            assert_eq!(kinds_of(&error), [ViolationKind::EndpointRule], "{host}");
        }

        for host in [
            "api.openai.com.",
            "10.0.0.1",
            "2001:db8::1",
            "api.openai.com",
            "localhost",
        ] {
            let payload = serde_json::json!({
                "server": { "endpoints": [{ "scheme": "https", "host": host, "port": 443 }] },
                "protocol": PROTOCOL_HTTP
            });
            let upstream = validate_upstream_payload(&payload, tenant).unwrap();
            assert_eq!(
                upstream.server.as_ref().unwrap().endpoints[0].host_stripped(),
                Some(host.trim_end_matches('.')),
                "{host} is accepted and stripped"
            );
        }
    }

    #[test]
    fn a_port_outside_the_range_is_reported() {
        let tenant = tenant();
        for port in [0, 65_536, -1] {
            let payload = serde_json::json!({
                "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com", "port": port }] },
                "protocol": PROTOCOL_HTTP
            });
            let error = validate_upstream_payload(&payload, tenant).unwrap_err();
            assert_eq!(fields_of(&error), ["server.endpoints[0].port"], "{port}");
            assert_eq!(kinds_of(&error), [ViolationKind::OutOfRangePort], "{port}");
        }
        for port in [1, 443, 65_535] {
            let payload = serde_json::json!({
                "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com", "port": port }] },
                "protocol": PROTOCOL_HTTP
            });
            let upstream = validate_upstream_payload(&payload, tenant).unwrap();
            assert!(validate_upstream(&upstream).is_ok());
        }
    }

    // §6: a wt endpoint validates as configuration and is flagged not-proxied.

    #[test]
    fn a_wt_endpoint_validates_as_configuration_and_is_flagged_not_proxied() {
        let tenant = tenant();
        let payload = serde_json::json!({
            "server": { "endpoints": [{ "scheme": "wt", "host": "api.vendor.com", "port": 443 }] },
            "protocol": PROTOCOL_HTTP
        });
        let upstream = validate_upstream_payload(&payload, tenant).unwrap();
        assert!(
            validate_upstream(&upstream).is_ok(),
            "wt validates as configuration"
        );
        assert!(
            !upstream.server.as_ref().unwrap().endpoints[0].is_proxied(),
            "correction 7: a wt endpoint is not proxied"
        );
    }

    #[test]
    fn an_alias_follows_its_pattern() {
        let mut violations = Violations::new();
        validate_alias("payments.vendor.com", &mut violations);
        validate_alias("a", &mut violations);
        validate_alias("gateway-1:8443", &mut violations);
        assert!(violations.is_empty(), "{}", violations);

        for alias in [
            "",
            "-payments",
            "payments-",
            "Payments",
            "pay ments",
            "pay/ments",
        ] {
            let mut violations = Violations::new();
            validate_alias(alias, &mut violations);
            assert!(
                !violations.is_empty(),
                "the alias '{alias}' must be rejected"
            );
        }

        let payload = serde_json::json!({
            "alias": "Payments",
            "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com" }] },
            "protocol": PROTOCOL_HTTP
        });
        let error = validate_upstream_payload(&payload, tenant()).unwrap_err();
        assert_eq!(fields_of(&error), ["alias"]);
        assert_eq!(kinds_of(&error), [ViolationKind::MalformedAlias]);
    }

    #[test]
    fn tags_are_pattern_checked_and_unique() {
        let mut violations = Violations::new();
        validate_tags(
            &[
                "openai".to_owned(),
                "llm-v1".to_owned(),
                "tier_1".to_owned(),
            ],
            &mut violations,
        );
        assert!(violations.is_empty(), "{}", violations);

        let mut violations = Violations::new();
        validate_tags(
            &[
                "LLM".to_owned(),
                "a b".to_owned(),
                "openai".to_owned(),
                "openai".to_owned(),
            ],
            &mut violations,
        );
        assert_eq!(violations.len(), 3, "{}", violations);
        assert_eq!(
            violations.as_slice()[0].field,
            "tags[0]",
            "the violations name the offending positions"
        );

        let payload = serde_json::json!({
            "tags": ["openai", "openai"],
            "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com" }] },
            "protocol": PROTOCOL_HTTP
        });
        let error = validate_upstream_payload(&payload, tenant()).unwrap_err();
        assert_eq!(fields_of(&error), ["tags[1]"]);
    }

    #[test]
    fn an_unknown_protocol_instance_is_rejected() {
        let tenant = tenant();
        let payload = serde_json::json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com" }] },
            "protocol": "gts.cf.core.oagw.upstream.v1~not-a-protocol"
        });
        let error = validate_upstream_payload(&payload, tenant).unwrap_err();
        assert_eq!(fields_of(&error), ["protocol"]);
        assert_eq!(kinds_of(&error), [ViolationKind::EndpointRule]);

        for protocol in PROTOCOL_INSTANCES {
            let payload = serde_json::json!({
                "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com" }] },
                "protocol": protocol
            });
            let upstream = validate_upstream_payload(&payload, tenant).unwrap();
            assert!(
                validate_upstream(&upstream).is_ok(),
                "{protocol} is accepted"
            );
        }
    }

    // §6: an unknown rate-limit strategy or algorithm fails, as does a rate
    // below 1 or a window outside the enum.

    #[test]
    fn an_unknown_rate_limit_value_is_rejected() {
        let mut violations = Violations::new();
        validate_rate_limit(
            &serde_json::from_value(serde_json::json!({
                "algorithm": "leaky_bucket",
                "sustained": { "rate": 0, "window": "fortnight" },
                "scope": "universe",
                "strategy": "throttle",
                "cost": 0,
                "burst": { "capacity": 0 }
            }))
            .unwrap(),
            &mut violations,
        );
        let fields: Vec<String> = violations
            .as_slice()
            .iter()
            .map(|violation| violation.field.clone())
            .collect();
        assert_eq!(
            fields,
            [
                "rate_limit.algorithm",
                "rate_limit.sustained.rate",
                "rate_limit.sustained.window",
                "rate_limit.burst.capacity",
                "rate_limit.scope",
                "rate_limit.strategy",
                "rate_limit.cost"
            ],
            "{}",
            violations
        );
        assert!(
            violations
                .as_slice()
                .iter()
                .all(|violation| violation.kind == ViolationKind::RateLimitShape)
        );
    }

    // §6: reject, queue and degrade are accepted strategies and no other value
    // passes (correction 6).

    #[test]
    fn queue_and_degrade_are_accepted_strategies() {
        for strategy in ["reject", "queue", "degrade"] {
            let mut violations = Violations::new();
            let rate_limit: RateLimitConfig = serde_json::from_value(serde_json::json!({
                "sustained": { "rate": 5 },
                "strategy": strategy
            }))
            .unwrap();
            validate_rate_limit(&rate_limit, &mut violations);
            assert!(
                violations.is_empty(),
                "'{strategy}' validates: {}",
                violations
            );
        }

        let payload = serde_json::json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com" }] },
            "protocol": PROTOCOL_HTTP,
            "rate_limit": { "sustained": { "rate": 5 }, "strategy": "throttle" }
        });
        let error = validate_upstream_payload(&payload, tenant()).unwrap_err();
        assert_eq!(fields_of(&error), ["rate_limit.strategy"]);
    }

    // §6: allow_credentials with a wildcard origin is rejected at validation
    // time.

    #[test]
    fn credentials_with_a_wildcard_origin_are_rejected() {
        let mut violations = Violations::new();
        let cors: CorsConfig = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "allowed_origins": ["*"],
            "allow_credentials": true
        }))
        .unwrap();
        validate_cors(&cors, &mut violations);
        assert_eq!(violations.len(), 1, "{}", violations);
        assert_eq!(violations.as_slice()[0].kind, ViolationKind::CorsShape);
        assert_eq!(violations.as_slice()[0].field, "cors.allowed_origins[0]");

        let mut violations = Violations::new();
        let cors: CorsConfig = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "allowed_origins": ["https://console.vendor.com"],
            "allow_credentials": true
        }))
        .unwrap();
        validate_cors(&cors, &mut violations);
        assert!(
            violations.is_empty(),
            "a concrete origin with credentials is allowed"
        );

        let payload = serde_json::json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com" }] },
            "protocol": PROTOCOL_HTTP,
            "cors": {
                "enabled": true,
                "allowed_origins": ["*"],
                "allow_credentials": true
            }
        });
        let error = validate_upstream_payload(&payload, tenant()).unwrap_err();
        assert_eq!(fields_of(&error), ["cors.allowed_origins[0]"]);

        for origin in ["https://console.vendor.com", "http://localhost:3000"] {
            let mut violations = Violations::new();
            let cors: CorsConfig = serde_json::from_value(
                serde_json::json!({ "enabled": true, "allowed_origins": [origin] }),
            )
            .unwrap();
            validate_cors(&cors, &mut violations);
            assert!(violations.is_empty(), "'{origin}' is an absolute origin");
        }
        for origin in ["console.vendor.com", "https://x/y", "ftp://x", "not a uri"] {
            let mut violations = Violations::new();
            let cors: CorsConfig = serde_json::from_value(
                serde_json::json!({ "enabled": true, "allowed_origins": [origin] }),
            )
            .unwrap();
            validate_cors(&cors, &mut violations);
            assert!(!violations.is_empty(), "'{origin}' is not an origin");
        }
    }

    #[test]
    fn an_unknown_cors_method_is_rejected() {
        let mut violations = Violations::new();
        let cors: CorsConfig = serde_json::from_value(serde_json::json!({
            "enabled": true,
            "allowed_methods": ["GET", "TRACE"]
        }))
        .unwrap();
        validate_cors(&cors, &mut violations);
        assert_eq!(violations.as_slice()[0].field, "cors.allowed_methods[1]");
    }

    #[test]
    fn a_cors_block_without_an_enabled_flag_is_rejected() {
        let payload = serde_json::json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com" }] },
            "protocol": PROTOCOL_HTTP,
            "cors": { "allowed_origins": ["*"] }
        });
        let error = validate_upstream_payload(&payload, tenant()).unwrap_err();
        assert_eq!(fields_of(&error), ["cors.enabled"]);
        assert_eq!(kinds_of(&error), [ViolationKind::CorsShape]);
    }

    // The two feature-local headers rules of FEATURE §1.5.

    #[test]
    fn a_passthrough_of_allowlist_requires_its_allowlist() {
        let mut violations = Violations::new();
        let headers: HeadersConfig = serde_json::from_value(serde_json::json!({
            "request": { "passthrough": "allowlist" }
        }))
        .unwrap();
        validate_headers(&headers, &mut violations);
        assert_eq!(violations.len(), 1, "{}", violations);
        assert_eq!(
            violations.as_slice()[0].field,
            "headers.request.passthrough_allowlist"
        );

        let mut violations = Violations::new();
        let headers: HeadersConfig = serde_json::from_value(serde_json::json!({
            "request": {
                "passthrough": "allowlist",
                "passthrough_allowlist": ["accept"]
            }
        }))
        .unwrap();
        validate_headers(&headers, &mut violations);
        assert!(violations.is_empty(), "{}", violations);
    }

    #[test]
    fn removing_a_well_known_header_is_a_violation_and_setting_one_is_not() {
        let mut violations = Violations::new();
        let headers: HeadersConfig = serde_json::from_value(serde_json::json!({
            "request": { "remove": ["Content-Length", "x-internal-token"] },
            "response": { "set": { "content-type": "application/json" }, "remove": ["Content-Type"] }
        }))
        .unwrap();
        validate_headers(&headers, &mut violations);
        assert_eq!(violations.len(), 2, "{}", violations);
        assert_eq!(violations.as_slice()[0].field, "headers.request.remove[0]");
        assert_eq!(violations.as_slice()[1].field, "headers.response.remove[0]");
        assert!(
            violations
                .as_slice()
                .iter()
                .all(|violation| violation.kind == ViolationKind::HeadersShape)
        );

        let mut violations = Violations::new();
        let headers: HeadersConfig = serde_json::from_value(serde_json::json!({
            "request": { "set": { "content-length": "0" }, "passthrough": "none" }
        }))
        .unwrap();
        validate_headers(&headers, &mut violations);
        assert!(
            violations.is_empty(),
            "setting a protected header is permitted"
        );
    }

    #[test]
    fn an_unknown_passthrough_mode_is_rejected() {
        let payload = serde_json::json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com" }] },
            "protocol": PROTOCOL_HTTP,
            "headers": { "request": { "passthrough": "everything" } }
        });
        let error = validate_upstream_payload(&payload, tenant()).unwrap_err();
        assert_eq!(fields_of(&error), ["headers.request.passthrough"]);
        assert_eq!(kinds_of(&error), [ViolationKind::HeadersShape]);
    }

    // §1.5: the auth block, its cred:// reference form and its inline secrets.

    #[test]
    fn a_secret_reference_must_match_the_cred_form() {
        let mut violations = Violations::new();
        let auth: AuthConfig = serde_json::from_value(serde_json::json!({
            "type": AUTH_PLUGIN,
            "config": { "secret_ref": "vault://tenant/openai" }
        }))
        .unwrap();
        validate_auth(&auth, &mut violations);
        assert_eq!(violations.as_slice()[0].field, "auth.config.secret_ref");
        assert_eq!(violations.as_slice()[0].kind, ViolationKind::AuthShape);

        let mut violations = Violations::new();
        let auth: AuthConfig = serde_json::from_value(serde_json::json!({
            "type": AUTH_PLUGIN,
            "config": { "secret_ref": "cred://tenant/openai", "header": "x-api-key" }
        }))
        .unwrap();
        validate_auth(&auth, &mut violations);
        assert!(violations.is_empty(), "{}", violations);
    }

    #[test]
    fn inline_secret_material_is_rejected() {
        for (key, value) in [
            ("api_key", "sk-live-123"),
            ("password", "hunter2"),
            ("access_token", "eyJhbGciOi"),
        ] {
            let mut violations = Violations::new();
            let auth: AuthConfig = serde_json::from_value(serde_json::json!({
                "type": AUTH_PLUGIN,
                "config": { key: value }
            }))
            .unwrap();
            validate_auth(&auth, &mut violations);
            assert_eq!(
                violations.as_slice()[0].field,
                format!("auth.config.{key}"),
                "the inline secret '{key}' is rejected"
            );
        }

        let mut violations = Violations::new();
        let auth: AuthConfig = serde_json::from_value(serde_json::json!({
            "type": AUTH_PLUGIN,
            "config": { "api_key": "cred://tenant/openai" }
        }))
        .unwrap();
        validate_auth(&auth, &mut violations);
        assert!(
            violations.is_empty(),
            "a cred:// reference is not inline material"
        );
    }

    #[test]
    fn an_auth_type_outside_the_plugin_family_is_rejected() {
        let payload = serde_json::json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com" }] },
            "protocol": PROTOCOL_HTTP,
            "auth": { "type": "gts.cf.core.oagw.upstream.v1~cf.core.oagw.apikey.v1" }
        });
        let error = validate_upstream_payload(&payload, tenant()).unwrap_err();
        assert_eq!(fields_of(&error), ["auth.type"]);
        assert_eq!(kinds_of(&error), [ViolationKind::AuthShape]);
    }

    // §6: the route match block per the schema oneOf.

    #[test]
    fn a_route_match_block_is_validated_per_the_one_of() {
        let upstreams = KnownUpstreams {
            tenant_id: tenant(),
            upstream_id: Uuid::parse_str(UPSTREAM_ID).unwrap(),
            protocol: PROTOCOL_HTTP,
        };
        let both = serde_json::json!({
            "upstream_id": UPSTREAM_ID,
            "match": {
                "http": { "methods": ["GET"], "path": "/v1" },
                "grpc": { "service": "foo.v1.S", "method": "Get" }
            }
        });
        let error = validate_route_payload(&both, tenant(), &upstreams).unwrap_err();
        assert_eq!(fields_of(&error), ["match"]);
        assert_eq!(kinds_of(&error), [ViolationKind::RouteMatchShape]);

        let neither = serde_json::json!({ "upstream_id": UPSTREAM_ID, "match": {} });
        let error = validate_route_payload(&neither, tenant(), &upstreams).unwrap_err();
        assert_eq!(fields_of(&error), ["match"]);

        let absent = serde_json::json!({ "upstream_id": UPSTREAM_ID });
        let error = validate_route_payload(&absent, tenant(), &upstreams).unwrap_err();
        assert!(fields_of(&error).contains(&"match".to_owned()), "{error}");
    }

    #[test]
    fn an_http_match_is_validated() {
        let upstreams = KnownUpstreams {
            tenant_id: tenant(),
            upstream_id: Uuid::parse_str(UPSTREAM_ID).unwrap(),
            protocol: PROTOCOL_HTTP,
        };
        for (field, block) in [
            (
                "match.http.methods",
                serde_json::json!({ "methods": [], "path": "/v1" }),
            ),
            (
                "match.http.methods[1]",
                serde_json::json!({ "methods": ["GET", "TRACE"], "path": "/v1" }),
            ),
            (
                "match.http.path",
                serde_json::json!({ "methods": ["GET"], "path": "" }),
            ),
            (
                "match.http.path_suffix_mode",
                serde_json::json!({ "methods": ["GET"], "path": "/v1", "path_suffix_mode": "prefix" }),
            ),
        ] {
            let payload =
                serde_json::json!({ "upstream_id": UPSTREAM_ID, "match": { "http": block } });
            let error = validate_route_payload(&payload, tenant(), &upstreams).unwrap_err();
            assert_eq!(fields_of(&error), [field], "{block}");
            assert_eq!(
                kinds_of(&error),
                [ViolationKind::RouteMatchShape],
                "{field}"
            );
        }

        let payload = serde_json::json!({
            "upstream_id": UPSTREAM_ID,
            "match": { "http": { "methods": ["GET", "POST", "PUT", "DELETE", "PATCH"], "path": "/v1" } }
        });
        let route = validate_route_payload(&payload, tenant(), &upstreams).unwrap();
        assert!(validate_route(&route, &upstreams).is_ok());
    }

    #[test]
    fn a_grpc_match_missing_its_service_is_reported_like_its_method() {
        let upstreams = KnownUpstreams {
            tenant_id: tenant(),
            upstream_id: Uuid::parse_str(UPSTREAM_ID).unwrap(),
            protocol: PROTOCOL_GRPC,
        };
        let absent = serde_json::json!({
            "upstream_id": UPSTREAM_ID,
            "match": { "grpc": {} }
        });
        let error = validate_route_payload(&absent, tenant(), &upstreams).unwrap_err();
        assert_eq!(
            fields_of(&error),
            ["match.grpc.service", "match.grpc.method"],
            "both required fields are named: {error}"
        );
        assert_eq!(
            kinds_of(&error),
            [
                ViolationKind::RouteMatchShape,
                ViolationKind::RouteMatchShape
            ],
            "both required fields are reported: {error}"
        );

        for field in ["service", "method"] {
            let mut grpc = serde_json::json!({ "service": "foo.v1.UserService", "method": "Get" });
            grpc[field] = serde_json::json!("");
            let payload = serde_json::json!({
                "upstream_id": UPSTREAM_ID,
                "match": { "grpc": grpc }
            });
            let error = validate_route_payload(&payload, tenant(), &upstreams).unwrap_err();
            assert_eq!(
                fields_of(&error),
                [format!("match.grpc.{field}")],
                "{error}"
            );
        }

        let payload = serde_json::json!({
            "upstream_id": UPSTREAM_ID,
            "match": { "grpc": { "service": "foo.v1.UserService", "method": "Get" } }
        });
        let route = validate_route_payload(&payload, tenant(), &upstreams).unwrap();
        assert!(validate_route(&route, &upstreams).is_ok());
    }

    #[test]
    fn a_grpc_match_is_validated() {
        let upstreams = KnownUpstreams {
            tenant_id: tenant(),
            upstream_id: Uuid::parse_str(UPSTREAM_ID).unwrap(),
            protocol: PROTOCOL_GRPC,
        };
        let payload = serde_json::json!({
            "upstream_id": UPSTREAM_ID,
            "match": { "grpc": { "service": "foo.v1.UserService" } }
        });
        let error = validate_route_payload(&payload, tenant(), &upstreams).unwrap_err();
        assert_eq!(fields_of(&error), ["match.grpc.method"]);

        let payload = serde_json::json!({
            "upstream_id": UPSTREAM_ID,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } }
        });
        let error = validate_route_payload(&payload, tenant(), &upstreams).unwrap_err();
        assert_eq!(
            fields_of(&error),
            ["match"],
            "the match must scope the grpc protocol"
        );
    }

    // §6: the tenant-scoped referential integrity of upstream_id.

    #[test]
    fn an_upstream_reference_is_checked_across_the_tenant_store() {
        let tenant = tenant();
        let other_tenant = Uuid::parse_str("8f0a1b2c-3d4e-4f50-8617-8899aabbccdd").unwrap();
        let upstream_id = Uuid::parse_str(UPSTREAM_ID).unwrap();
        let foreign = KnownUpstreams {
            tenant_id: other_tenant,
            upstream_id,
            protocol: PROTOCOL_HTTP,
        };
        let payload = serde_json::json!({
            "upstream_id": UPSTREAM_ID,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } }
        });
        let error = validate_route_payload(&payload, tenant, &foreign).unwrap_err();
        assert_eq!(fields_of(&error), ["upstream_id"]);
        assert_eq!(kinds_of(&error), [ViolationKind::NotFound], "{error}");
        assert_eq!(
            OagwError::from(error).status(),
            404,
            "the caller surfaces 404, never 403"
        );

        let mine = KnownUpstreams {
            tenant_id: tenant,
            upstream_id,
            protocol: PROTOCOL_HTTP,
        };
        let route = validate_route_payload(&payload, tenant, &mine).unwrap();
        assert!(validate_route(&route, &mine).is_ok());
    }

    #[test]
    fn a_route_match_must_scope_the_upstream_protocol() {
        let tenant = tenant();
        let upstream_id = Uuid::parse_str(UPSTREAM_ID).unwrap();
        let grpc_upstream = KnownUpstreams {
            tenant_id: tenant,
            upstream_id,
            protocol: PROTOCOL_GRPC,
        };
        let payload = serde_json::json!({
            "upstream_id": UPSTREAM_ID,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } }
        });
        let error = validate_route_payload(&payload, tenant, &grpc_upstream).unwrap_err();
        assert_eq!(fields_of(&error), ["match"]);

        let payload = serde_json::json!({
            "upstream_id": UPSTREAM_ID,
            "match": { "grpc": { "service": "foo.v1.UserService", "method": "GetUser" } }
        });
        let route = validate_route_payload(&payload, tenant, &grpc_upstream).unwrap();
        assert!(validate_route(&route, &grpc_upstream).is_ok());
    }

    // The plugins block and the plugin binding projection.

    #[test]
    fn a_route_binds_gts_plugin_identifiers_only() {
        let upstreams = KnownUpstreams {
            tenant_id: tenant(),
            upstream_id: Uuid::parse_str(UPSTREAM_ID).unwrap(),
            protocol: PROTOCOL_HTTP,
        };
        let payload = serde_json::json!({
            "upstream_id": UPSTREAM_ID,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } },
            "plugins": { "items": [PLUGIN_ID] }
        });
        let error = validate_route_payload(&payload, tenant(), &upstreams).unwrap_err();
        assert_eq!(fields_of(&error), ["plugins.items[0]"]);
        assert_eq!(kinds_of(&error), [ViolationKind::PluginShape]);

        let upstream_payload = serde_json::json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com" }] },
            "protocol": PROTOCOL_HTTP,
            "plugins": { "items": [PLUGIN_ID, GUARD_PLUGIN] }
        });
        let upstream = validate_upstream_payload(&upstream_payload, tenant()).unwrap();
        assert!(validate_upstream(&upstream).is_ok(), "{}", UPSTREAM_TYPE_ID);
    }

    #[test]
    fn an_unrecognised_plugin_reference_is_rejected() {
        let payload = serde_json::json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com" }] },
            "protocol": PROTOCOL_HTTP,
            "plugins": { "items": ["not-a-plugin"] }
        });
        let error = validate_upstream_payload(&payload, tenant()).unwrap_err();
        assert_eq!(fields_of(&error), ["plugins.items[0]"]);
        assert_eq!(kinds_of(&error), [ViolationKind::PluginShape]);
    }

    #[test]
    fn a_plugin_binding_carries_plugin_ref_and_derives_plugin_uuid() {
        let mut violations = Violations::new();
        let plugins: PluginsConfig = serde_json::from_value(serde_json::json!({
            "items": [GUARD_PLUGIN, PLUGIN_ID]
        }))
        .unwrap();
        let bindings = plugins.bindings();
        assert_eq!(bindings.len(), 2);
        assert!(
            bindings[0].plugin_uuid.is_none(),
            "a named plugin keeps plugin_uuid absent"
        );
        assert!(bindings[1].plugin_uuid.is_some());
        validate_bindings(&bindings, &mut violations);
        assert!(violations.is_empty(), "{}", violations);
        assert_eq!(bindings[0].plugin_ref, GUARD_PLUGIN);
        assert_eq!(
            bindings[1].plugin_uuid,
            Some(Uuid::parse_str(PLUGIN_ID).unwrap())
        );
    }

    #[test]
    fn a_derived_plugin_uuid_that_disagrees_with_its_reference_is_a_violation() {
        let mut violations = Violations::new();
        let plugins: PluginsConfig =
            serde_json::from_value(serde_json::json!({ "items": [GUARD_PLUGIN] })).unwrap();
        let mut bindings = plugins.bindings();
        bindings[0].plugin_uuid = Some(Uuid::parse_str(PLUGIN_ID).unwrap());
        validate_bindings(&bindings, &mut violations);
        assert_eq!(
            violations.as_slice()[0].field,
            "plugins.bindings[0].plugin_uuid"
        );

        let mut violations = Violations::new();
        let mut binding = PluginBinding {
            position: 2,
            plugin_ref: GUARD_PLUGIN.to_owned(),
            plugin_uuid: None,
            config: None,
        };
        validate_bindings(std::slice::from_ref(&binding), &mut violations);
        assert!(
            violations
                .as_slice()
                .iter()
                .any(|violation| violation.message.contains("contiguous")),
            "a non-contiguous position is reported: {}",
            violations
        );

        binding.position = 0;
        binding.plugin_ref = String::new();
        let mut violations = Violations::new();
        validate_bindings(std::slice::from_ref(&binding), &mut violations);
        assert!(
            violations
                .as_slice()
                .iter()
                .any(|violation| violation.message.contains("plugin_ref is always present")),
            "{}",
            violations
        );
    }

    // The Plugin aggregate (inst-sv-09).

    #[test]
    fn a_plugin_aggregate_is_validated_per_its_own_model() {
        let plugin = validate_plugin_payload(&valid_plugin_json(), tenant()).unwrap();
        assert!(validate_plugin_aggregate(&plugin).is_ok());
        assert_eq!(
            plugin.gts_ref().as_deref(),
            Some("gts.cf.core.oagw.guard_plugin.v1~3f0a1b2c-3d4e-4f50-8617-8899aabbccdd")
        );

        let mut violations = Violations::new();
        let plugin = Plugin {
            plugin_type: Some(crate::domain::model::UPSTREAM_TYPE_ID.to_owned()),
            name: Some("  ".to_owned()),
            config_schema: Some(serde_json::json!([])),
            source_code: Some("   ".to_owned()),
            ..Plugin::default()
        };
        validate_plugin(&plugin, &mut violations);
        let fields: Vec<String> = violations
            .as_slice()
            .iter()
            .map(|violation| violation.field.clone())
            .collect();
        assert_eq!(
            fields,
            ["plugin_type", "name", "config_schema", "source_code"],
            "{}",
            violations
        );
    }

    #[test]
    fn a_plugin_payload_cannot_carry_an_unknown_field() {
        let mut payload = valid_plugin_json();
        payload
            .as_object_mut()
            .unwrap()
            .insert("alias".to_owned(), Value::String("x".to_owned()));
        let error = validate_plugin_payload(&payload, tenant()).unwrap_err();
        assert_eq!(fields_of(&error), ["alias"]);
    }

    // §6: a payload violating several rules at once reports every rule, in
    // field order.

    #[test]
    fn a_payload_violating_several_rules_reports_every_rule_in_field_order() {
        let payload = serde_json::json!({
            "nope": 1,
            "server": {
                "endpoints": [{ "scheme": "https", "host": "api.vendor.com", "port": 0 }]
            },
            "protocol": PROTOCOL_HTTP,
            "tags": ["LLM"],
            "cors": {
                "enabled": true,
                "allow_credentials": true,
                "allowed_origins": ["*"]
            }
        });
        let error = validate_upstream_payload(&payload, tenant()).unwrap_err();
        let violations = error.to_violations();
        assert_eq!(
            fields_of(&error),
            [
                "server.endpoints[0].port",
                "tags[0]",
                "cors.allowed_origins[0]",
                "nope"
            ],
            "every violated rule in field order, unknown fields last: {error}"
        );
        assert_eq!(violations.len(), 4, "{error}");
        assert_eq!(
            error.to_string(),
            violations
                .iter()
                .map(std::string::ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        );
        let mapped = OagwError::from(error);
        assert_eq!(mapped.status(), 400);
        assert_eq!(mapped.mapping().variant, "ValidationError");
    }

    #[test]
    fn the_route_collector_reports_every_violated_rule_in_field_order() {
        let payload = serde_json::json!({
            "nope": 1,
            "upstream_id": "not-a-uuid",
            "match": { "http": { "methods": ["TRACE"], "path": "" } },
            "tags": ["LLM"]
        });
        let error = validate_route_payload(&payload, tenant(), &NoUpstreams).unwrap_err();
        assert_eq!(
            fields_of(&error),
            [
                "tags[0]",
                // The empty `path` is a required-scalar emptiness, which the
                // value pass reports in its own block order — after the method
                // entries — and not by a shape-pass placeholder.
                "match.http.methods[0]",
                "match.http.path",
                "upstream_id",
                "nope"
            ],
            "the shape pass runs before the value pass, unknown fields last: {error}"
        );
        assert_eq!(kinds_of(&error).len(), 5);
    }

    #[test]
    fn a_valid_payload_yields_the_aggregate_and_no_violation() {
        let upstream = validate_upstream_payload(&valid_upstream_json(), tenant())
            .unwrap_or_else(|error| panic!("the documented happy path must validate: {error}"));
        assert!(validate_upstream(&upstream).is_ok());
        let upstreams = KnownUpstreams {
            tenant_id: tenant(),
            upstream_id: Uuid::parse_str(UPSTREAM_ID).unwrap(),
            protocol: PROTOCOL_HTTP,
        };
        let route = validate_route_payload(&valid_route_json(), tenant(), &upstreams)
            .unwrap_or_else(|error| panic!("the documented happy path must validate: {error}"));
        assert!(validate_route(&route, &upstreams).is_ok());
        let plugin = validate_plugin_payload(&valid_plugin_json(), tenant())
            .unwrap_or_else(|error| panic!("the documented happy path must validate: {error}"));
        assert!(validate_plugin_aggregate(&plugin).is_ok());
    }

    #[test]
    fn the_identifier_families_are_the_provisioned_ones() {
        assert_eq!(PLUGIN_TYPE_IDS.len(), 3);
        assert!(UPSTREAM_TYPE_ID.starts_with("gts.cf.core.oagw."));
        assert!(ROUTE_TYPE_ID.starts_with("gts.cf.core.oagw."));
    }

    // The submitted secret material a mistyped `secret_ref` carries never
    // reaches a problem detail or a log entry.

    #[test]
    fn a_mistyped_secret_reference_is_reported_without_its_value() {
        let secret = "sk-proj-SECRET";
        let mut violations = Violations::new();
        let auth: AuthConfig = serde_json::from_value(serde_json::json!({
            "type": AUTH_PLUGIN,
            "config": { "secret_ref": secret }
        }))
        .unwrap();
        validate_auth(&auth, &mut violations);
        assert_eq!(violations.as_slice()[0].field, "auth.config.secret_ref");
        assert_eq!(violations.as_slice()[0].kind, ViolationKind::AuthShape);
        assert!(
            !violations.as_slice()[0].message.contains(secret),
            "the submitted value is not echoed: {}",
            violations.as_slice()[0].message
        );

        let payload = serde_json::json!({
            "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com" }] },
            "protocol": PROTOCOL_HTTP,
            "auth": { "type": AUTH_PLUGIN, "config": { "secret_ref": secret } }
        });
        let error = validate_upstream_payload(&payload, tenant()).unwrap_err();
        let detail = error.to_string();
        assert!(
            !detail.contains(secret),
            "the detail carries no value: {detail}"
        );
    }

    // An undeclared key that merely begins with the name of an array field is
    // not a declared entry of it, so the strip pass removes and names it.

    #[test]
    fn an_undeclared_key_that_merely_prefixes_an_array_field_is_named_at_its_path() {
        let mut payload = valid_upstream_json();
        payload["tagsfoo"] = serde_json::json!(1);
        let error = validate_upstream_payload(&payload, tenant()).unwrap_err();
        assert_eq!(fields_of(&error), ["tagsfoo"], "{error}");
        assert_eq!(kinds_of(&error), [ViolationKind::UnknownField], "{error}");

        let mut nested = valid_upstream_json();
        nested["plugins"]["itemsfoo"] = serde_json::json!(1);
        let error = validate_upstream_payload(&nested, tenant()).unwrap_err();
        assert_eq!(fields_of(&error), ["plugins.itemsfoo"], "{error}");
        assert_eq!(kinds_of(&error), [ViolationKind::UnknownField], "{error}");

        // A declared entry of an array field still matches its wildcard.
        let mut declared = valid_upstream_json();
        declared["tags"][0] = serde_json::json!("LLM");
        let error = validate_upstream_payload(&declared, tenant()).unwrap_err();
        assert_eq!(fields_of(&error), ["tags[0]"], "{error}");
        assert_eq!(kinds_of(&error), [ViolationKind::MalformedTag], "{error}");
    }

    // A bare family prefix carries no instance, so it names no plugin.

    #[test]
    fn a_bare_plugin_family_prefix_names_no_plugin() {
        let mut violations = Violations::new();
        let plugins: PluginsConfig = serde_json::from_value(serde_json::json!({
            "items": ["gts.cf.core.oagw.guard_plugin.v1~"]
        }))
        .unwrap();
        validate_plugins(&plugins, true, &mut violations);
        assert_eq!(violations.len(), 1, "{}", violations);
        assert_eq!(violations.as_slice()[0].field, "plugins.items[0]");
        assert_eq!(violations.as_slice()[0].kind, ViolationKind::PluginShape);

        let upstreams = KnownUpstreams {
            tenant_id: tenant(),
            upstream_id: Uuid::parse_str(UPSTREAM_ID).unwrap(),
            protocol: PROTOCOL_HTTP,
        };
        let payload = serde_json::json!({
            "upstream_id": UPSTREAM_ID,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } },
            "plugins": { "items": ["gts.cf.core.oagw.guard_plugin.v1~"] }
        });
        let error = validate_route_payload(&payload, tenant(), &upstreams).unwrap_err();
        assert_eq!(fields_of(&error), ["plugins.items[0]"], "{error}");
        assert_eq!(kinds_of(&error), [ViolationKind::PluginShape], "{error}");
    }

    // An empty required string keeps its dedicated "must not be empty" message
    // and is not replaced by the placeholder.

    #[test]
    fn an_empty_required_path_is_reported_by_the_value_pass() {
        let upstreams = KnownUpstreams {
            tenant_id: tenant(),
            upstream_id: Uuid::parse_str(UPSTREAM_ID).unwrap(),
            protocol: PROTOCOL_HTTP,
        };
        let payload = serde_json::json!({
            "upstream_id": UPSTREAM_ID,
            "match": { "http": { "methods": ["GET"], "path": "" } }
        });
        let error = validate_route_payload(&payload, tenant(), &upstreams).unwrap_err();
        let violations = error.to_violations();
        assert_eq!(violations.len(), 1, "{error}");
        assert_eq!(violations[0].field, "match.http.path");
        assert_eq!(violations[0].kind, ViolationKind::RouteMatchShape);
        assert!(
            violations[0].message.contains("must not be empty"),
            "the empty string is not replaced by a placeholder: {error}"
        );
    }

    // The declared `enabled` and `priority` fields of a route are reported as
    // route-shape violations, never as unknown fields.

    #[test]
    fn a_mistyped_route_extension_field_is_not_an_unknown_field() {
        let upstreams = KnownUpstreams {
            tenant_id: tenant(),
            upstream_id: Uuid::parse_str(UPSTREAM_ID).unwrap(),
            protocol: PROTOCOL_HTTP,
        };
        let payload = serde_json::json!({
            "upstream_id": UPSTREAM_ID,
            "match": { "http": { "methods": ["GET"], "path": "/v1" } },
            "enabled": "yes",
            "priority": "high"
        });
        let error = validate_route_payload(&payload, tenant(), &upstreams).unwrap_err();
        let fields = fields_of(&error);
        assert!(fields.contains(&"enabled".to_owned()), "{error}");
        assert!(fields.contains(&"priority".to_owned()), "{error}");
        assert_eq!(
            kinds_of(&error),
            [
                ViolationKind::RouteMatchShape,
                ViolationKind::RouteMatchShape
            ],
            "both fields are declared members: {error}"
        );
    }

    // A declared container the payload mistypes is reported once, by the shape
    // rule, and its keys are not named as unknown fields.

    #[test]
    fn a_mistyped_container_is_reported_once() {
        let payload = serde_json::json!({
            "tags": { "a": 1 },
            "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com" }] },
            "protocol": PROTOCOL_HTTP
        });
        let error = validate_upstream_payload(&payload, tenant()).unwrap_err();
        assert_eq!(fields_of(&error), ["tags"], "{error}");
        assert_eq!(kinds_of(&error), [ViolationKind::MalformedTag], "{error}");
    }

    // A payload value a violation message echoes is bounded, so an over-long
    // field cannot inflate a problem detail or a log entry.

    #[test]
    fn an_echoed_value_is_truncated_to_a_bounded_prefix() {
        assert_eq!(echoed("payments.vendor.com"), "payments.vendor.com");
        assert_eq!(
            echoed(&"a".repeat(40)),
            format!("{}...(len=40)", "a".repeat(32))
        );

        let long_alias = "a".repeat(1024 * 1024);
        let payload = serde_json::json!({
            "alias": long_alias,
            "server": { "endpoints": [{ "scheme": "https", "host": "api.vendor.com" }] },
            "protocol": PROTOCOL_HTTP
        });
        let error = validate_upstream_payload(&payload, tenant()).unwrap_err();
        assert_eq!(fields_of(&error), ["alias"], "{error}");
        let detail = error.to_string();
        assert!(detail.len() < 256, "the detail is bounded: {detail}");
        assert!(detail.contains("(len=1048576)"), "{detail}");
        let unbounded_run = "a".repeat(64);
        assert!(!detail.contains(unbounded_run.as_str()), "{detail}");
    }

    // The typed default of `parse_shaped` is not a latent panic: every
    // aggregate deserialises from an empty object, so the fallback the drift
    // between a shape table and the model takes is a value, never a panic.

    #[test]
    fn the_aggregates_deserialise_from_an_empty_object() {
        assert!(serde_json::from_value::<Upstream>(serde_json::json!({})).is_ok());
        assert!(serde_json::from_value::<Route>(serde_json::json!({})).is_ok());
        assert!(serde_json::from_value::<Plugin>(serde_json::json!({})).is_ok());
    }
}
