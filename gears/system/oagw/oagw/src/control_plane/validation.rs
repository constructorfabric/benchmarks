//! Request validation — `cpt-cf-oagw-algo-request-validate`.
//!
//! The two frozen JSON Schemas are the primary check: one validator per
//! resource kind per write kind (four in all; the upstream create and
//! replacement schemas are the same file, the route replacement schema is the
//! same file with `upstream_id` removed from the root `required` array). The
//! code-level families that follow are the ones the schema cannot express —
//! the §1.5 overlays, the endpoint-pool homogeneity, the scheme admission
//! posture, and the shape checks the schema states only as `format`.
//!
//! Every failing property is accumulated into **one**
//! `DomainError::gateway(ErrorKind::ValidationError, ..)` whose detail names
//! the failing properties, comma-separated, and never carries a request body
//! value: a name only, such as `unknown property 'x' at root` or
//! `server.endpoints[1].port`.

use std::borrow::Cow;

use serde_json::Value;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::alias::EndpointHost;
use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::route::{MatchConfig, Route};
use crate::domain::scheme::Scheme;
use crate::domain::upstream::{
    AuthConfig, CorsConfig, HeadersConfig, PluginsConfig, RateLimitConfig, ServerConfig, Upstream,
};
use crate::gts;

use jsonschema::error::ValidationErrorKind;
use jsonschema::paths::{Location, LocationSegment};

/// The shipped upstream schema, read from the frozen file.
pub const UPSTREAM_SCHEMA: &str = include_str!("../../../docs/schemas/upstream.v1.schema.json");
/// The shipped route schema, read from the frozen file.
pub const ROUTE_SCHEMA: &str = include_str!("../../../docs/schemas/route.v1.schema.json");

/// Declared schema default for `server.endpoints[].port`.
const DEFAULT_ENDPOINT_PORT: u16 = 443;
/// The seven literals `definitions.cors.allowed_methods` admits.
const CORS_METHODS: [&str; 7] = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];
/// The five literals `definitions.http_match.methods` admits.
const HTTP_METHODS: [&str; 5] = ["GET", "POST", "PUT", "DELETE", "PATCH"];
/// Scheme prefix a credential reference must carry; never resolved here.
const CREDENTIAL_SCHEME: &str = "cred://";
/// Route root properties a create may carry: the route schema root is open,
/// so the allowed set is enforced here.
const ROUTE_ROOT_CREATE: [&str; 8] = [
    "tags",
    "upstream_id",
    "match",
    "plugins",
    "rate_limit",
    "cors",
    "priority",
    "enabled",
];
/// Route root properties a replacement may carry: the create set with
/// `upstream_id` replaced by `id`.
const ROUTE_ROOT_REPLACEMENT: [&str; 8] = [
    "id",
    "tags",
    "match",
    "plugins",
    "rate_limit",
    "cors",
    "priority",
    "enabled",
];

/// Which resource kind a request body carries.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResourceKind {
    /// An `oagw_upstream` write.
    Upstream,
    /// An `oagw_route` write.
    Route,
}

impl ResourceKind {
    /// Lowercase singular name of the resource kind.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Upstream => "upstream",
            Self::Route => "route",
        }
    }
}

/// Whether a request body creates a new row or replaces an addressed one.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WriteKind {
    /// `POST`: the full required set applies and `id` is system-generated.
    Create,
    /// `PUT`: the required set is narrowed and `enabled` carries forward.
    Replacement,
}

/// The validated body of an upstream write.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedUpstream {
    /// The identifier the body stated, if any. A create must state none; a
    /// replacement's must equal the addressed row, which
    /// `cpt-cf-oagw-algo-put-replace-diff` confirms.
    pub stated_id: Option<Uuid>,
    /// The validated configuration, with `id` left for the caller to assign.
    pub value: Upstream,
}

/// The validated body of a route write.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedRoute {
    /// The identifier the body stated, if any; see [`ValidatedUpstream`].
    pub stated_id: Option<Uuid>,
    /// The validated configuration, with `id` left for the caller to assign.
    pub value: Route,
}

/// The four compiled validators, one per resource kind per write kind.
pub struct Validator {
    upstream_create: jsonschema::Validator,
    upstream_replacement: jsonschema::Validator,
    route_create: jsonschema::Validator,
    route_replacement: jsonschema::Validator,
    allow_http_upstream: bool,
}

/// Accumulated failing properties of one request body.
#[derive(Debug, Default)]
struct Defects {
    details: Vec<String>,
}

/// One checked endpoint: the raw body values the pool check compares.
struct CheckedEndpoint {
    scheme: Scheme,
    port: u16,
}

impl Defects {
    /// Adds a detail, dropping a repeat of one already recorded.
    fn add(&mut self, detail: String) {
        if !self.details.contains(&detail) {
            self.details.push(detail);
        }
    }

    /// Adds one failing property by name.
    fn add_property(&mut self, property: &str) {
        self.add(property.to_owned());
    }

    /// Whether every family passed.
    fn is_empty(&self) -> bool {
        self.details.is_empty()
    }

    /// The single validation error naming every failing property.
    fn into_error(self) -> DomainError {
        let detail = if self.details.is_empty() {
            String::from("the request body is not valid for the resource kind")
        } else {
            self.details.join(", ")
        };
        DomainError::gateway(ErrorKind::ValidationError, detail)
    }
}

impl Validator {
    #[allow(clippy::result_large_err)]
    /// Compiles the four validators from the two shipped schemas.
    ///
    /// # Errors
    ///
    /// Returns a gateway [`DomainError`] when a shipped schema cannot be
    /// parsed or compiled; the gear surfaces that at init, so startup fails
    /// fast instead of serving an unvalidated write path.
    pub fn compile(config: &OagwConfig) -> Result<Self, DomainError> {
        let mut upstream_schema = parse_schema(UPSTREAM_SCHEMA)?;
        let mut route_schema = parse_schema(ROUTE_SCHEMA)?;

        // @cpt-dod:cpt-cf-oagw-dod-binding-model:p1
        // The feature-local override for `plugins.items`: the two shipped
        // schemas declare the items as bare identifier strings, while the
        // binding item this feature validates and writes is the object that
        // carries a `position`, a `plugin_ref`, an optional `plugin_uuid`, and
        // a configuration. The item shape is therefore not confirmed by the
        // schema pass at all — the `plugins` envelope and its `sharing` enum
        // stay under the shipped schema — and is validated instead by the
        // binding validation, which names every failing item with its position
        // and the reason. The frozen schemas are rewritten in a validation
        // copy only, never on disk.
        override_plugin_items(&mut upstream_schema);
        override_plugin_items(&mut route_schema);

        // The upstream create and replacement schemas are the same file: the
        // upstream schema declares no `required` narrowing.
        let upstream_create =
            compile_validator(&upstream_schema, ResourceKind::Upstream)?;
        let upstream_replacement =
            compile_validator(&upstream_schema, ResourceKind::Upstream)?;

        // The route replacement schema is the same JSON with `upstream_id`
        // removed from the root `required` array.
        let mut narrowed = route_schema.clone();
        narrow_route_replacement(&mut narrowed);
        let route_create = compile_validator(&route_schema, ResourceKind::Route)?;
        let route_replacement = compile_validator(&narrowed, ResourceKind::Route)?;

        Ok(Self {
            upstream_create,
            upstream_replacement,
            route_create,
            route_replacement,
            allow_http_upstream: config.allow_http_upstream,
        })
    }

    /// Validates an upstream body against the schema and the code-level
    /// families.
    ///
    /// # Errors
    ///
    /// Returns one gateway validation error naming every failing property.
#[allow(clippy::result_large_err)]
    pub fn validate_upstream(
        &self,
        write: WriteKind,
        body: &Value,
    ) -> Result<ValidatedUpstream, DomainError> {
        let mut defects = Defects::default();

        // @cpt-begin:cpt-cf-oagw-algo-request-validate:p1:inst-val-parse
        self.schema_pass(ResourceKind::Upstream, write, body, &mut defects);
        // @cpt-end:cpt-cf-oagw-algo-request-validate:p1:inst-val-parse

        if let Some(fields) = body.as_object() {
            self.check_upstream_families(write, fields, &mut defects);
        }

        // @cpt-begin:cpt-cf-oagw-algo-request-validate:p1:inst-val-fail-if
        if !defects.is_empty() {
            // @cpt-begin:cpt-cf-oagw-algo-request-validate:p1:inst-val-fail-return
            return Err(defects.into_error());
            // @cpt-end:cpt-cf-oagw-algo-request-validate:p1:inst-val-fail-return
        }
        // @cpt-end:cpt-cf-oagw-algo-request-validate:p1:inst-val-fail-if

        // @cpt-begin:cpt-cf-oagw-algo-request-validate:p1:inst-val-return
        build_upstream(body)
        // @cpt-end:cpt-cf-oagw-algo-request-validate:p1:inst-val-return
    }

    /// Validates a route body against the schema and the code-level families.
    ///
    /// # Errors
    ///
    /// Returns one gateway validation error naming every failing property.
#[allow(clippy::result_large_err)]
    pub fn validate_route(
        &self,
        write: WriteKind,
        body: &Value,
    ) -> Result<ValidatedRoute, DomainError> {
        let mut defects = Defects::default();

        self.schema_pass(ResourceKind::Route, write, body, &mut defects);

        if let Some(fields) = body.as_object() {
            self.check_route_families(write, fields, &mut defects);
        }

        if !defects.is_empty() {
            return Err(defects.into_error());
        }

        build_route(write, body)
    }

    /// Runs the schema pass and records every failing property.
    ///
    /// When `allow_http_upstream` lifts the posture, the `http` scheme literal
    /// — which the frozen schema enum does not declare — is neutralized to
    /// `https` in a validation copy only, so the two shipped validators stay
    /// the only schema check; the code-level admission check runs on the
    /// original body and rejects `http` when the posture is not lifted.
    fn schema_pass(
        &self,
        kind: ResourceKind,
        write: WriteKind,
        body: &Value,
        defects: &mut Defects,
    ) {
        let validator = match (kind, write) {
            (ResourceKind::Upstream, WriteKind::Create) => &self.upstream_create,
            (ResourceKind::Upstream, WriteKind::Replacement) => &self.upstream_replacement,
            (ResourceKind::Route, WriteKind::Create) => &self.route_create,
            (ResourceKind::Route, WriteKind::Replacement) => &self.route_replacement,
        };

        let subject = neutralized(body, self.allow_http_upstream);
        for error in validator.iter_errors(&subject) {
            defects.add(describe(&error));
        }
    }

    /// The upstream code-level families, in the FEATURE's order.
    fn check_upstream_families(
        &self,
        write: WriteKind,
        fields: &serde_json::Map<String, Value>,
        defects: &mut Defects,
    ) {
        // @cpt-begin:cpt-cf-oagw-algo-request-validate:p1:inst-val-unknown
        check_upstream_root(write, fields, defects);
        // @cpt-end:cpt-cf-oagw-algo-request-validate:p1:inst-val-unknown

        // @cpt-begin:cpt-cf-oagw-algo-request-validate:p1:inst-val-required
        check_required(ResourceKind::Upstream, write, fields, defects);
        // @cpt-end:cpt-cf-oagw-algo-request-validate:p1:inst-val-required

        // @cpt-begin:cpt-cf-oagw-algo-request-validate:p1:inst-val-endpoint-loop
        let endpoints = fields
            .get("server")
            .and_then(|server| server.get("endpoints"))
            .and_then(Value::as_array);
        // @cpt-begin:cpt-cf-oagw-algo-request-validate:p1:inst-val-endpoint
        let checked = check_endpoints(endpoints, self.allow_http_upstream, defects);
        // @cpt-end:cpt-cf-oagw-algo-request-validate:p1:inst-val-endpoint
        // @cpt-end:cpt-cf-oagw-algo-request-validate:p1:inst-val-endpoint-loop

        // @cpt-begin:cpt-cf-oagw-algo-request-validate:p1:inst-val-pool-if
        // @cpt-begin:cpt-cf-oagw-algo-request-validate:p1:inst-val-pool
        check_pool(&checked, defects);
        // @cpt-end:cpt-cf-oagw-algo-request-validate:p1:inst-val-pool
        // @cpt-end:cpt-cf-oagw-algo-request-validate:p1:inst-val-pool-if

        // @cpt-begin:cpt-cf-oagw-algo-request-validate:p1:inst-val-enums
        check_protocol(fields, defects);
        check_tags(fields.get("tags"), "tags", defects);
        // @cpt-end:cpt-cf-oagw-algo-request-validate:p1:inst-val-enums

        // @cpt-begin:cpt-cf-oagw-algo-request-validate:p1:inst-val-subs
        check_sub_configurations(fields, defects);
        // @cpt-end:cpt-cf-oagw-algo-request-validate:p1:inst-val-subs
    }

    /// The route code-level families, in the FEATURE's order.
    fn check_route_families(
        &self,
        write: WriteKind,
        fields: &serde_json::Map<String, Value>,
        defects: &mut Defects,
    ) {
        check_route_root(write, fields, defects);
        check_required(ResourceKind::Route, write, fields, defects);

        // @cpt-begin:cpt-cf-oagw-algo-request-validate:p1:inst-val-route-if
        // @cpt-begin:cpt-cf-oagw-algo-request-validate:p1:inst-val-match
        check_match(fields, defects);
        // @cpt-end:cpt-cf-oagw-algo-request-validate:p1:inst-val-match
        // @cpt-end:cpt-cf-oagw-algo-request-validate:p1:inst-val-route-if

        check_protocol(fields, defects);
        check_tags(fields.get("tags"), "tags", defects);
        check_sub_configurations(fields, defects);
    }
}

/// Parses one shipped schema document.
#[allow(clippy::result_large_err)]
fn parse_schema(text: &str) -> Result<Value, DomainError> {
    serde_json::from_str(text).map_err(|_| {
        DomainError::gateway(ErrorKind::RouteError, "the shipped schema is not valid JSON")
    })
}

/// Compiles one shipped schema document.
#[allow(clippy::result_large_err)]
fn compile_validator(
    schema: &Value,
    kind: ResourceKind,
) -> Result<jsonschema::Validator, DomainError> {
    jsonschema::validator_for(schema).map_err(|_| {
        DomainError::gateway(
            ErrorKind::RouteError,
            format!("the shipped {} schema could not be compiled", kind.as_str()),
        )
    })
}

/// Removes `upstream_id` from the root `required` array of the route schema.
fn narrow_route_replacement(schema: &mut Value) {
    let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) else {
        return;
    };
    required.retain(|entry| entry.as_str() != Some("upstream_id"));
}

/// Rewrites the `plugins.items` constraint of one shipped schema to an
/// unconstrained array, so the item shape the shipped oneOf does not express
/// is left to the binding validation.
///
/// The rewrite is total: an item that is neither an identifier nor a binding
/// object is refused by the binding validation, which names the item and its
/// position, rather than by a schema pass that could only name the path.
fn override_plugin_items(schema: &mut Value) {
    let Some(items) = schema
        .get_mut("properties")
        .and_then(|properties| properties.get_mut("plugins"))
        .and_then(|plugins| plugins.get_mut("properties"))
        .and_then(|properties| properties.get_mut("items"))
    else {
        return;
    };
    *items = serde_json::json!({ "type": "array" });
}

/// Rewrites every `http` endpoint scheme to `https` when the posture is
/// lifted, so the frozen schema enum admits the body.
fn neutralized(body: &Value, allow_http_upstream: bool) -> Cow<'_, Value> {
    if !allow_http_upstream {
        return Cow::Borrowed(body);
    }
    match lift_http_schemes(body) {
        None => Cow::Borrowed(body),
        Some(rewritten) => Cow::Owned(rewritten),
    }
}

/// Clones `body` with every `http` endpoint scheme rewritten to `https`, or
/// answers `None` when no endpoint carries it.
fn lift_http_schemes(body: &Value) -> Option<Value> {
    let endpoints = body
        .get("server")
        .and_then(|server| server.get("endpoints"))
        .and_then(Value::as_array)?;
    if !endpoints
        .iter()
        .any(|endpoint| endpoint.get("scheme").and_then(Value::as_str) == Some("http"))
    {
        return None;
    }

    let mut rewritten = body.clone();
    let slots = rewritten
        .get_mut("server")
        .and_then(|server| server.get_mut("endpoints"))
        .and_then(Value::as_array_mut)?;
    for endpoint in slots {
        if endpoint.get("scheme").and_then(Value::as_str) == Some("http")
            && let Some(scheme) = endpoint.get_mut("scheme")
        {
            *scheme = Value::String(String::from("https"));
        }
    }
    Some(rewritten)
}

/// Renders a schema error as the failing property, naming only.
fn describe(error: &jsonschema::ValidationError<'_>) -> String {
    let path = property_path(error.instance_path());
    match error.kind() {
        ValidationErrorKind::AdditionalProperties { unexpected } => {
            let container = if path.is_empty() {
                String::from("root")
            } else {
                path.clone()
            };
            unexpected
                .iter()
                .map(|name| format!("unknown property '{name}' at {container}"))
                .collect::<Vec<_>>()
                .join(", ")
        }
        ValidationErrorKind::Required { property } => {
            let name = property.as_str().unwrap_or_default();
            if path.is_empty() {
                format!("{name} is required")
            } else {
                format!("{path}.{name} is required")
            }
        }
        _ => {
            if path.is_empty() {
                String::from("root")
            } else {
                path
            }
        }
    }
}

/// Renders a JSON Pointer as a dotted property path with `[n]` array indices.
fn property_path(location: &Location) -> String {
    let mut path = String::new();
    for segment in location {
        match segment {
            LocationSegment::Property(name) => {
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(name.as_ref());
            }
            LocationSegment::Index(index) => {
                path.push('[');
                path.push_str(index.to_string().as_str());
                path.push(']');
            }
        }
    }
    path
}

/// Enforces the route root property set the open schema root cannot express.
fn check_route_root(
    write: WriteKind,
    fields: &serde_json::Map<String, Value>,
    defects: &mut Defects,
) {
    let allowed: [&str; 8] = match write {
        WriteKind::Create => ROUTE_ROOT_CREATE,
        WriteKind::Replacement => ROUTE_ROOT_REPLACEMENT,
    };
    for key in fields.keys() {
        if !allowed.contains(&key.as_str()) {
            defects.add(format!("unknown property '{key}' at root"));
        }
    }
}

/// Enforces the upstream root rule: `id` is system-generated on a create and
/// `tenant_id` is never caller-supplied.
fn check_upstream_root(
    write: WriteKind,
    fields: &serde_json::Map<String, Value>,
    defects: &mut Defects,
) {
    if write == WriteKind::Create && fields.contains_key("id") {
        defects.add_property("id");
    }
    if fields.contains_key("tenant_id") {
        defects.add_property("tenant_id");
    }
}

/// Checks the required properties, branching on create versus replacement.
fn check_required(
    kind: ResourceKind,
    write: WriteKind,
    fields: &serde_json::Map<String, Value>,
    defects: &mut Defects,
) {
    // @cpt-begin:cpt-cf-oagw-dod-request-validation:p1:inst-val-required-branch
    let required: &[&str] = match (kind, write) {
        (ResourceKind::Upstream, _) => &["server", "protocol"],
        (ResourceKind::Route, WriteKind::Create) => &["upstream_id", "match"],
        (ResourceKind::Route, WriteKind::Replacement) => &["match"],
    };
    for property in required {
        if !fields.contains_key(*property) {
            defects.add(format!("{property} is required"));
        }
    }
    // @cpt-end:cpt-cf-oagw-dod-request-validation:p1:inst-val-required-branch
}

/// The per-endpoint shape check the schema states only as `format`.
fn check_endpoints(
    endpoints: Option<&Vec<Value>>,
    allow_http_upstream: bool,
    defects: &mut Defects,
) -> Vec<CheckedEndpoint> {
    let Some(endpoints) = endpoints else {
        return Vec::new();
    };

    let mut checked = Vec::with_capacity(endpoints.len());
    for (index, endpoint) in endpoints.iter().enumerate() {
        let prefix = format!("server.endpoints[{index}]");
        let Some(fields) = endpoint.as_object() else {
            defects.add_property(&prefix);
            continue;
        };

        if let Some(host) = fields.get("host").and_then(Value::as_str)
            && EndpointHost::parse(host).is_err()
        {
            defects.add_property(&format!("{prefix}.host"));
        }

        let Some(scheme) = fields
            .get("scheme")
            .and_then(Value::as_str)
            .and_then(parse_scheme)
        else {
            continue;
        };

        let Some(port) = endpoint_port(fields.get("port")) else {
            defects.add_property(&format!("{prefix}.port"));
            continue;
        };

        // @cpt-begin:cpt-cf-oagw-algo-request-validate:p1:inst-val-http-if
        if !scheme.is_write_admitted(allow_http_upstream) {
            // @cpt-begin:cpt-cf-oagw-algo-request-validate:p1:inst-val-http
            defects.add_property(&format!("{prefix}.scheme"));
            continue;
            // @cpt-end:cpt-cf-oagw-algo-request-validate:p1:inst-val-http
        }
        // @cpt-end:cpt-cf-oagw-algo-request-validate:p1:inst-val-http-if

        checked.push(CheckedEndpoint { scheme, port });
    }
    checked
}

/// Resolves the endpoint port, applying the declared schema default.
fn endpoint_port(port: Option<&Value>) -> Option<u16> {
    let raw = match port {
        None => return Some(DEFAULT_ENDPOINT_PORT),
        Some(port) => port.as_u64()?,
    };
    u16::try_from(raw).ok().filter(|port| *port >= 1)
}

/// Rejects a pool that mixes two schemes or two ports.
fn check_pool(endpoints: &[CheckedEndpoint], defects: &mut Defects) {
    let Some((first, rest)) = endpoints.split_first() else {
        return;
    };
    for (offset, endpoint) in rest.iter().enumerate() {
        let index = offset + 1;
        if endpoint.scheme != first.scheme {
            defects.add_property(&format!("server.endpoints[{index}].scheme"));
        }
        if endpoint.port != first.port {
            defects.add_property(&format!("server.endpoints[{index}].port"));
        }
    }
}

/// Parses an endpoint scheme literal.
fn parse_scheme(text: &str) -> Option<Scheme> {
    match text {
        "http" => Some(Scheme::Http),
        "https" => Some(Scheme::Https),
        "wss" => Some(Scheme::Wss),
        "wt" => Some(Scheme::Wt),
        "grpc" => Some(Scheme::Grpc),
        _ => None,
    }
}

/// Checks the protocol enum.
fn check_protocol(fields: &serde_json::Map<String, Value>, defects: &mut Defects) {
    let Some(protocol) = fields.get("protocol").and_then(Value::as_str) else {
        return;
    };
    if protocol != gts::PROTOCOL_HTTP && protocol != gts::PROTOCOL_GRPC {
        defects.add_property("protocol");
    }
}

/// Checks every tag against the shipped pattern.
fn check_tags(tags: Option<&Value>, prefix: &str, defects: &mut Defects) {
    let Some(tags) = tags.and_then(Value::as_array) else {
        return;
    };
    for (index, tag) in tags.iter().enumerate() {
        if !tag.as_str().is_some_and(tag_matches) {
            defects.add_property(&format!("{prefix}[{index}]"));
        }
    }
}

/// The shipped tag pattern `^[a-z0-9_-]+$`, as a character check.
fn tag_matches(tag: &str) -> bool {
    !tag.is_empty()
        && tag
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-')
}

/// Checks the `match` shape and the §1.5 `priority` requirement.
fn check_match(fields: &serde_json::Map<String, Value>, defects: &mut Defects) {
    let Some(matched) = fields.get("match") else {
        return;
    };
    let Some(branches) = matched.as_object() else {
        defects.add_property("match");
        return;
    };

    match (branches.get("http"), branches.get("grpc")) {
        (Some(http), None) => check_http_match(http, defects),
        (None, Some(grpc)) => check_grpc_match(grpc, defects),
        _ => {
            defects.add_property("match");
            return;
        }
    }

    if fields.get("priority").and_then(Value::as_i64).is_none() {
        defects.add_property("priority");
    }
}

/// Checks the `http` match branch.
fn check_http_match(http: &Value, defects: &mut Defects) {
    let Some(fields) = http.as_object() else {
        defects.add_property("match.http");
        return;
    };
    let methods = fields.get("methods").and_then(Value::as_array);
    let admitted = methods.is_some_and(|methods| {
        !methods.is_empty()
            && methods
                .iter()
                .all(|method| HTTP_METHODS.contains(&method.as_str().unwrap_or_default()))
    });
    if !admitted {
        defects.add_property("match.http.methods");
    }
    if fields
        .get("path")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        defects.add_property("match.http.path");
    }
    if let Some(mode) = fields.get("path_suffix_mode").and_then(Value::as_str)
        && mode != "disabled"
        && mode != "append"
    {
        defects.add_property("match.http.path_suffix_mode");
    }
}

/// Checks the `grpc` match branch.
fn check_grpc_match(grpc: &Value, defects: &mut Defects) {
    let Some(fields) = grpc.as_object() else {
        defects.add_property("match.grpc");
        return;
    };
    for property in ["service", "method"] {
        if fields
            .get(property)
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        {
            defects.add_property(&format!("match.grpc.{property}"));
        }
    }
}

/// Checks the `rate_limit`, `cors`, and credential-reference sub-objects.
fn check_sub_configurations(fields: &serde_json::Map<String, Value>, defects: &mut Defects) {
    if let Some(rate_limit) = fields.get("rate_limit") {
        check_rate_limit(rate_limit, "rate_limit", defects);
    }
    if let Some(cors) = fields.get("cors") {
        check_cors(cors, "cors", defects);
    }
    if let Some(auth) = fields.get("auth") {
        check_credential_reference(auth, defects);
    }
}

/// Checks the `definitions.rate_limit` ranges over the raw body, so an
/// out-of-range integer is named before serde ever sees it.
fn check_rate_limit(rate_limit: &Value, prefix: &str, defects: &mut Defects) {
    let Some(fields) = rate_limit.as_object() else {
        defects.add_property(prefix);
        return;
    };

    let sustained_rate = fields
        .get("sustained")
        .and_then(|sustained| sustained.get("rate"))
        .and_then(Value::as_i64);
    if !sustained_rate.is_some_and(|rate| rate >= 1) {
        defects.add_property(&format!("{prefix}.sustained.rate"));
    }

    if let Some(capacity) = fields.get("burst").and_then(|burst| burst.get("capacity"))
        && capacity.as_i64().is_none_or(|capacity| capacity < 1)
    {
        defects.add_property(&format!("{prefix}.burst.capacity"));
    }

    if let Some(cost) = fields.get("cost")
        && cost.as_i64().is_none_or(|cost| cost < 1)
    {
        defects.add_property(&format!("{prefix}.cost"));
    }
}

/// Checks the `definitions.cors` shape.
fn check_cors(cors: &Value, prefix: &str, defects: &mut Defects) {
    let Some(fields) = cors.as_object() else {
        defects.add_property(prefix);
        return;
    };

    if !fields.get("enabled").is_some_and(Value::is_boolean) {
        defects.add_property(&format!("{prefix}.enabled"));
    }

    let origins = fields.get("allowed_origins").and_then(Value::as_array);
    if let Some(origins) = origins {
        for (index, origin) in origins.iter().enumerate() {
            let property = format!("{prefix}.allowed_origins[{index}]");
            match origin.as_str() {
                None => defects.add_property(&property),
                Some(text) if text != "*" && !carries_scheme(text) => {
                    defects.add_property(&property);
                }
                Some(_) => {}
            }
        }
    }

    let methods = fields
        .get("allowed_methods")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    for (index, method) in methods.enumerate() {
        if !CORS_METHODS.contains(&method.as_str().unwrap_or_default()) {
            defects.add_property(&format!("{prefix}.allowed_methods[{index}]"));
        }
    }

    let wildcard = origins.is_some_and(|origins| {
        origins
            .iter()
            .any(|origin| origin.as_str() == Some("*"))
    });
    if fields.get("allow_credentials").and_then(Value::as_bool) == Some(true) && wildcard {
        defects.add_property(&format!("{prefix}.allowed_origins"));
    }
}

/// Whether an origin literal carries a URI scheme, which is what
/// `format: uri` asks for on an origin.
fn carries_scheme(origin: &str) -> bool {
    let Some(separator) = origin.find("://") else {
        return false;
    };
    let scheme = &origin[..separator];
    !scheme.is_empty()
        && scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'-' || byte == b'.')
}

/// Checks a credential reference for its scheme prefix only; the value is
/// never resolved and never echoed.
fn check_credential_reference(auth: &Value, defects: &mut Defects) {
    if let Some(reference) = auth.get("secret_ref") {
        check_credential_prefix(reference, "auth.secret_ref", defects);
    }
    if let Some(reference) = auth
        .get("config")
        .and_then(|config| config.get("secret_ref"))
    {
        check_credential_prefix(reference, "auth.config.secret_ref", defects);
    }
}

/// Names the property when the reference does not carry the `cred://` prefix.
fn check_credential_prefix(reference: &Value, property: &str, defects: &mut Defects) {
    if let Some(text) = reference.as_str()
        && !text.starts_with(CREDENTIAL_SCHEME)
    {
        defects.add_property(property);
    }
}

/// The upstream body the caller wrote, with the schema defaults applied.
#[derive(Debug, serde::Deserialize)]
struct UpstreamInput {
    /// System-generated identifier; a create must not state one.
    #[serde(default)]
    id: Option<Uuid>,
    /// Whether the upstream is enabled; the schema default is `true`.
    #[serde(default)]
    enabled: Option<bool>,
    /// Caller-supplied alias, normalized by alias derivation.
    #[serde(default)]
    alias: Option<String>,
    /// Flat tags.
    #[serde(default)]
    tags: Vec<String>,
    /// Server endpoints; required.
    server: ServerConfig,
    /// Protocol GTS identifier; required.
    protocol: String,
    /// Authentication plugin binding.
    #[serde(default)]
    auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default)]
    headers: Option<HeadersConfig>,
    /// Plugin chain.
    #[serde(default)]
    plugins: Option<PluginsConfig>,
    /// Rate limiting configuration.
    #[serde(default)]
    rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default)]
    cors: Option<CorsConfig>,
}

/// The route body the caller wrote, with the schema defaults applied.
#[derive(Debug, serde::Deserialize)]
struct RouteInput {
    /// System-generated identifier; a create must not state one.
    #[serde(default)]
    id: Option<Uuid>,
    /// Referenced upstream; required on a create, rejected on a replacement.
    #[serde(default)]
    upstream_id: Option<Uuid>,
    /// Protocol-scoped inbound matching rules; required.
    #[serde(rename = "match")]
    match_config: MatchConfig,
    /// Plugin chain.
    #[serde(default)]
    plugins: Option<PluginsConfig>,
    /// Rate limiting configuration.
    #[serde(default)]
    rate_limit: Option<RateLimitConfig>,
    /// Flat tags.
    #[serde(default)]
    tags: Vec<String>,
    /// Route-level CORS configuration, added per §1.5.
    #[serde(default)]
    cors: Option<CorsConfig>,
    /// Match-uniqueness ordering, added per §1.5 and required.
    #[serde(default)]
    priority: Option<i64>,
    /// Enable/disable semantics, added per §1.5.
    #[serde(default)]
    enabled: Option<bool>,
}

/// Builds the validated upstream once every family passed.
#[allow(clippy::result_large_err)]
fn build_upstream(body: &Value) -> Result<ValidatedUpstream, DomainError> {
    let input: UpstreamInput = serde_json::from_value(body.clone())
        .map_err(|error| DomainError::gateway(ErrorKind::ValidationError, serde_field(&error)))?;

    Ok(ValidatedUpstream {
        stated_id: input.id,
        value: Upstream {
            id: Uuid::nil(),
            enabled: input.enabled.unwrap_or(true),
            alias: input.alias,
            tags: input.tags,
            server: with_default_ports(input.server),
            protocol: input.protocol,
            auth: input.auth,
            headers: input.headers,
            plugins: input.plugins,
            rate_limit: input.rate_limit,
            cors: input.cors,
        },
    })
}

/// Builds the validated route once every family passed.
#[allow(clippy::result_large_err)]
fn build_route(write: WriteKind, body: &Value) -> Result<ValidatedRoute, DomainError> {
    let input: RouteInput = serde_json::from_value(body.clone())
        .map_err(|error| DomainError::gateway(ErrorKind::ValidationError, serde_field(&error)))?;

    // A create defaults `enabled` to `true`; a replacement leaves it absent so
    // the stored value carries forward.
    let enabled = match write {
        WriteKind::Create => Some(input.enabled.unwrap_or(true)),
        WriteKind::Replacement => input.enabled,
    };

    Ok(ValidatedRoute {
        stated_id: input.id,
        value: Route {
            id: Uuid::nil(),
            upstream_id: input.upstream_id.unwrap_or_default(),
            match_config: input.match_config,
            plugins: input.plugins,
            rate_limit: input.rate_limit,
            tags: input.tags,
            cors: input.cors,
            priority: input.priority,
            enabled,
        },
    })
}

/// Applies the schema default port to every endpoint that omits one.
fn with_default_ports(mut server: ServerConfig) -> ServerConfig {
    for endpoint in &mut server.endpoints {
        if endpoint.port.is_none() {
            endpoint.port = Some(DEFAULT_ENDPOINT_PORT);
        }
    }
    server
}

/// The one property name a serde deserialization failure reports.
fn serde_field(error: &serde_json::Error) -> String {
    let message = error.to_string();
    let Some(start) = message.find('`') else {
        return String::from("body");
    };
    let rest = &message[start + 1..];
    match rest.find('`') {
        Some(end) => rest[..end].to_owned(),
        None => String::from("body"),
    }
}
