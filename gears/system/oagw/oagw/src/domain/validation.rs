//! Structural invariant helpers shared by the configuration boundary
//! (`crate::config`), the repository layer and the alias derivation.
//!
//! These are pure functions over the domain DTOs: they validate shapes and
//! patterns and never touch infrastructure. Every rejection names the field
//! and never echoes a rejected credential value.

use std::net::IpAddr;

use crate::domain::dto::{
    Budget, BudgetMode, BurstCapacity, CorsConfig, Endpoint, HeaderPassthrough, HeadersConfig,
    HttpMatch, MatchConfig, Plugin, RateLimitConfig, Route, RouteMatchType, ServerConfig,
    SharingMode, Upstream,
};
use crate::domain::error::DomainError;
use crate::domain::gts_helpers::{PROTOCOL_GRPC, PROTOCOL_HTTP};

/// Longest `X-OAGW-Target-Host` echo (`invalid_value`) the error contract
/// permits, and the only request value ever echoed.
pub const MAX_INVALID_VALUE_LEN: usize = 128;

/// RFC 1123 maximum hostname length.
pub const MAX_HOSTNAME_LEN: usize = 253;

/// Maximum RFC 1123 label length.
pub const MAX_LABEL_LEN: usize = 63;

/// Truncate an `invalid_value` echo to `<= 128` characters with no control
/// character. Only the `X-OAGW-Target-Host` request header value is ever
/// echoed, only through the JSON serializer, and never into `detail` or a
/// header value.
#[must_use]
pub fn bound_invalid_value(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_INVALID_VALUE_LEN)
        .collect();
    cleaned
}

/// Validate the alias pattern `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$` without a
/// regex engine.
#[must_use]
pub fn alias_is_valid(alias: &str) -> bool {
    if alias.is_empty() || alias.len() > 253 {
        return false;
    }
    let bytes = alias.as_bytes();
    let head_ok = bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit();
    let last = bytes[bytes.len() - 1];
    let tail_ok = last.is_ascii_lowercase() || last.is_ascii_digit();
    if !head_ok || !tail_ok {
        return false;
    }
    if bytes.len() == 1 {
        return true;
    }
    bytes[1..bytes.len() - 1]
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b':' | b'-'))
}

/// Validate a tag against `^[a-z0-9_-]+$`.
#[must_use]
pub fn tag_is_valid(tag: &str) -> bool {
    !tag.is_empty()
        && tag
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Whether `host` is an IPv4 or IPv6 address.
#[must_use]
pub fn host_is_ip(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok()
}

/// Validate `host` as an RFC 1123 hostname (<= 253 chars, labels 1..=63 ASCII
/// alphanumeric or hyphen, no leading/trailing hyphen) or an IPv4/IPv6
/// address. A single trailing dot is tolerated as FQDN notation and reported
/// as stripped.
///
/// # Errors
///
/// Returns a validation error naming `field` when the host is neither.
pub fn validate_host(field: &str, host: &str) -> Result<String, DomainError> {
    let trimmed = host.strip_suffix('.').unwrap_or(host);
    if trimmed.is_empty() {
        return Err(DomainError::field_rejection(field, "host is empty"));
    }
    if host_is_ip(trimmed) {
        return Ok(trimmed.to_ascii_lowercase());
    }
    if trimmed.len() > MAX_HOSTNAME_LEN {
        return Err(DomainError::field_rejection(field, "host exceeds 253 characters"));
    }
    if !trimmed.is_ascii() {
        return Err(DomainError::field_rejection(field, "host must be ASCII"));
    }
    for label in trimmed.split('.') {
        if label.is_empty() || label.len() > MAX_LABEL_LEN {
            return Err(DomainError::field_rejection(
                field,
                "host label must be 1-63 characters",
            ));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(DomainError::field_rejection(
                field,
                "host label must be alphanumeric or hyphen",
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(DomainError::field_rejection(
                field,
                "host label must not start or end with a hyphen",
            ));
        }
    }
    Ok(trimmed.to_ascii_lowercase())
}

/// Validate a single endpoint: scheme in the legal set, RFC 1123 host or IP,
/// port `1..=65535`.
///
/// The `http` scheme is admitted only while `allow_http_upstream` is true
/// (graded deviation 2).
///
/// # Errors
///
/// Returns a validation error naming the offending field.
pub fn validate_endpoint(endpoint: &Endpoint, allow_http_upstream: bool) -> Result<Endpoint, DomainError> {
    if endpoint.port == 0 {
        return Err(DomainError::field_rejection("server.endpoints[].port", "port must be 1-65535"));
    }
    if !endpoint.scheme.is_allowed(allow_http_upstream) {
        return Err(DomainError::field_rejection(
            "server.endpoints[].scheme",
            "the `http` scheme is admitted only while `allow_http_upstream` is true",
        ));
    }
    let host = validate_host("server.endpoints[].host", &endpoint.host)?;
    Ok(Endpoint {
        scheme: endpoint.scheme,
        host,
        port: endpoint.port,
    })
}

/// Validate the endpoint pool of a `ServerConfig`: at least one endpoint,
/// pool uniform in scheme and port.
///
/// # Errors
///
/// Returns a validation error naming the offending field.
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-endpoint-validation:p1:inst-um-ev-1
// `inst-um-ev-1` .. `-8`: the non-empty pool, the required `scheme`/`host` with
// the `https` default, the scheme set with `http` behind `allow_http_upstream`,
// the RFC 1123 host or IP form, the 1..=65535 port, the pool uniformity and
// the two legal protocol identifiers (gRPC is configuration surface only).
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-endpoint-validation:p1:inst-um-ev-2
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-endpoint-validation:p1:inst-um-ev-3
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-endpoint-validation:p1:inst-um-ev-4
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-endpoint-validation:p1:inst-um-ev-5
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-endpoint-validation:p1:inst-um-ev-6
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-endpoint-validation:p1:inst-um-ev-7
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-endpoint-validation:p1:inst-um-ev-8
pub fn validate_server(server: &ServerConfig, allow_http_upstream: bool) -> Result<ServerConfig, DomainError> {
    if server.endpoints.is_empty() {
        return Err(DomainError::field_rejection(
            "server.endpoints",
            "at least one endpoint is required",
        ));
    }
    let mut normalized = Vec::with_capacity(server.endpoints.len());
    for endpoint in &server.endpoints {
        normalized.push(validate_endpoint(endpoint, allow_http_upstream)?);
    }
    let first = &normalized[0];
    if let Some(other) = normalized.iter().find(|e| e.scheme != first.scheme) {
        let _ = other;
        return Err(DomainError::field_rejection(
            "server.endpoints[].scheme",
            "the endpoint pool must be uniform in scheme",
        ));
    }
    if let Some(other) = normalized.iter().find(|e| e.port != first.port) {
        let _ = other;
        return Err(DomainError::field_rejection(
            "server.endpoints[].port",
            "the endpoint pool must be uniform in port",
        ));
    }
    Ok(ServerConfig { endpoints: normalized })
}
//
// @cpt-end:cpt-cf-oagw-algo-upstream-management-endpoint-validation:p1:inst-um-ev-8
// @cpt-end:cpt-cf-oagw-algo-upstream-management-endpoint-validation:p1:inst-um-ev-7
// @cpt-end:cpt-cf-oagw-algo-upstream-management-endpoint-validation:p1:inst-um-ev-6
// @cpt-end:cpt-cf-oagw-algo-upstream-management-endpoint-validation:p1:inst-um-ev-5
// @cpt-end:cpt-cf-oagw-algo-upstream-management-endpoint-validation:p1:inst-um-ev-4
// @cpt-end:cpt-cf-oagw-algo-upstream-management-endpoint-validation:p1:inst-um-ev-3
// @cpt-end:cpt-cf-oagw-algo-upstream-management-endpoint-validation:p1:inst-um-ev-2
//
// @cpt-end:cpt-cf-oagw-algo-upstream-management-endpoint-validation:p1:inst-um-ev-1

/// Validate `protocol` against the two legal GTS identifiers.
///
/// # Errors
///
/// Returns a validation error naming the field.
pub fn validate_protocol(protocol: &str) -> Result<(), DomainError> {
    if matches!(protocol, PROTOCOL_HTTP | PROTOCOL_GRPC) {
        Ok(())
    } else {
        Err(DomainError::field_rejection(
            "protocol",
            "protocol must be one of the two OAGW protocol GTS identifiers",
        ))
    }
}

/// Validate every tag of a record.
///
/// # Errors
///
/// Returns a validation error naming the offending field.
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-6
// `inst-um-cv-6`: every tag matches `^[a-z0-9_-]+$` and is stored as a row of
// its parent record.
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-10
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-11
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-13
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-2
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-3
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-4
pub fn validate_tags(field: &str, tags: &[String]) -> Result<(), DomainError> {
    for tag in tags {
        if !tag_is_valid(tag) {
            return Err(DomainError::field_rejection(
                field,
                "tags match `^[a-z0-9_-]+$`",
            ));
        }
    }
    Ok(())
}
//
// @cpt-end:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-4
// @cpt-end:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-3
// @cpt-end:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-2
// @cpt-end:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-13
// @cpt-end:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-11
// @cpt-end:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-10
//
// @cpt-end:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-6

/// Validate the sharing modes a record may declare.
///
/// # Errors
///
/// Returns a validation error naming the offending field.
/// The documented CORS method set of `docs/schemas/upstream.v1.schema.json`,
/// read from the one source the preflight reflection and the actual-request
/// method check also read.
const CORS_METHODS: [&str; 7] = crate::domain::cors::CORS_METHODS;

/// Validate a `cors` block (`inst-um-cv-8`): origins are each the literal `*`
/// or a well-formed origin URI, methods stay within the documented set, and
/// `allow_credentials: true` with a wildcard origin is rejected.
///
/// The `enabled` flag itself is required by the transport body, not here: the
/// schema default `cors.enabled: false` belongs to the merge engine, so the
/// stored block always carries the caller's decision.
///
/// # Errors
///
/// A validation error naming `cors.allowed_origins` or `cors.allowed_methods`.
// @cpt-begin:cpt-cf-oagw-flow-cors-config-validation:p1:inst-cors-cfg-1
// `inst-cors-cfg-1` .. `-9`, `inst-cors-alg-cv-1` .. `-9`: the `cors` block is
// validated on the management write path for an upstream or a route create or
// full replacement — each origin is the literal `*` or a well-formed origin
// URI, each method is in the documented set, `sharing` is one of the three
// sharing modes by deserialization, and `allow_credentials: true` with `*` is
// rejected and stored nowhere.
// @cpt-begin:cpt-cf-oagw-flow-cors-config-validation:p1:inst-cors-cfg-2
// @cpt-begin:cpt-cf-oagw-flow-cors-config-validation:p1:inst-cors-cfg-3
// @cpt-begin:cpt-cf-oagw-flow-cors-config-validation:p1:inst-cors-cfg-4
// @cpt-begin:cpt-cf-oagw-flow-cors-config-validation:p1:inst-cors-cfg-5
// @cpt-begin:cpt-cf-oagw-flow-cors-config-validation:p1:inst-cors-cfg-6
// @cpt-begin:cpt-cf-oagw-flow-cors-config-validation:p1:inst-cors-cfg-7
// @cpt-begin:cpt-cf-oagw-flow-cors-config-validation:p1:inst-cors-cfg-8
// @cpt-begin:cpt-cf-oagw-flow-cors-config-validation:p1:inst-cors-cfg-9
pub fn validate_cors(cors: &CorsConfig) -> Result<(), DomainError> {
    for origin in cors.allowed_origins.iter().flatten() {
        if origin == "*" {
            continue;
        }
        // No pattern form of origin is accepted (`inst-cors-cfg-3`,
        // `inst-cors-alg-cv-3`): the only relaxation the comparison knows is
        // the literal `*` entry, so a wildcard host is not a well-formed origin
        // even where a URL parser tolerates it.
        if origin.contains('*') || origin.contains('?') {
            return Err(DomainError::field_rejection(
                "cors.allowed_origins",
                "each origin is the literal `*` or a well-formed origin URI",
            ));
        }
        if !origin_is_well_formed(origin) {
            return Err(DomainError::field_rejection(
                "cors.allowed_origins",
                "each origin is the literal `*` or a well-formed origin URI",
            ));
        }
    }
    for method in &cors.allowed_methods {
        if !CORS_METHODS.contains(&method.as_str()) {
            return Err(DomainError::field_rejection(
                "cors.allowed_methods",
                "each method is one of GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS",
            ));
        }
    }
    // `allow_credentials: true` with a wildcard origin is rejected here, so the
    // combination can never reach request time (`inst-cors-cfg-6`,
    // `inst-cors-alg-cv-7`).
    cors.allows_wildcard()?;
    Ok(())
}
//
// @cpt-end:cpt-cf-oagw-flow-cors-config-validation:p1:inst-cors-cfg-9
// @cpt-end:cpt-cf-oagw-flow-cors-config-validation:p1:inst-cors-cfg-8
// @cpt-end:cpt-cf-oagw-flow-cors-config-validation:p1:inst-cors-cfg-7
// @cpt-end:cpt-cf-oagw-flow-cors-config-validation:p1:inst-cors-cfg-6
// @cpt-end:cpt-cf-oagw-flow-cors-config-validation:p1:inst-cors-cfg-5
// @cpt-end:cpt-cf-oagw-flow-cors-config-validation:p1:inst-cors-cfg-4
// @cpt-end:cpt-cf-oagw-flow-cors-config-validation:p1:inst-cors-cfg-3
// @cpt-end:cpt-cf-oagw-flow-cors-config-validation:p1:inst-cors-cfg-2
//
// @cpt-end:cpt-cf-oagw-flow-cors-config-validation:p1:inst-cors-cfg-1

/// Store every accepted `allowed_origins` entry in the canonical origin form
/// (`inst-cors-alg-cv-3`), so a configured entry and a request origin serialize
/// to the same string at request time. Called after [`validate_cors`] on the
/// management write path.
pub fn canonicalize_cors(cors: &mut CorsConfig) {
    crate::domain::cors::canonicalize(cors);
}

/// Whether `origin` is a scheme/host origin URI with no path, query or
/// fragment. `url::Url::parse` plus the `path == "/"` check is the RFC 3986
/// origin shape the schema's `format: uri` names.
fn origin_is_well_formed(origin: &str) -> bool {
    if origin.len() > MAX_HOSTNAME_LEN + 16 {
        return false;
    }
    url::Url::parse(origin)
        .ok()
        .is_some_and(|parsed| {
            parsed.path() == "/"
                && parsed.query().is_none()
                && parsed.fragment().is_none()
                && !parsed.host_str().is_some_and(str::is_empty)
                && parsed.username().is_empty()
                && parsed.password().is_none()
        })
}

/// Validate a `rate_limit` block (`inst-um-cv-7`, `inst-rl-cfg-4` .. `-7`):
/// the enumerations are enforced by the deserialization itself, so only the
/// `minimum: 1` bounds of `sustained.rate`, `burst.capacity` and `cost`, and
/// the budget-allocation bounds this feature owns, are checked here.
///
/// # Errors
///
/// A validation error naming the offending field.
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-7
// `inst-um-cv-7`: the sharing, algorithm, window, scope and strategy sets and
// the `sustained.rate`, `burst.capacity` and `cost` lower bounds.
// `inst-rl-cfg-4` .. `-7`: `budget.total` is an integer of at least 1,
// `budget.overcommit_ratio` is in the range 1.0 to 2.0 inclusive, and
// `budget.total` is required whenever `budget.mode` is `allocated` or
// `shared`. The budget surface is parsed, validated and stored but not
// executed (graded deviation 8).
pub fn validate_rate_limit(rate_limit: &RateLimitConfig) -> Result<(), DomainError> {
    if rate_limit.sustained.rate < 1 {
        return Err(DomainError::field_rejection(
            "rate_limit.sustained.rate",
            "the sustained rate is at least 1",
        ));
    }
    if let Some(BurstCapacity { capacity }) = rate_limit.burst {
        if capacity < 1 {
            return Err(DomainError::field_rejection(
                "rate_limit.burst.capacity",
                "the burst capacity is at least 1",
            ));
        }
    }
    if rate_limit.cost < 1 {
        return Err(DomainError::field_rejection(
            "rate_limit.cost",
            "the cost is at least 1",
        ));
    }
    if let Some(budget) = &rate_limit.budget {
        validate_budget(budget)?;
    }
    Ok(())
}
// @cpt-end:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-7

/// The overcommit ratio the budget surface admits, inclusive on both ends.
const MIN_OVERCOMMIT_RATIO: f64 = 1.0;
/// The overcommit ratio the budget surface admits, inclusive on both ends.
const MAX_OVERCOMMIT_RATIO: f64 = 2.0;

// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-config-surface:p1:inst-rl-cfg-4
// `inst-rl-cfg-4` .. `-7`: the budget-allocation surface this feature owns.
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-config-surface:p1:inst-rl-cfg-1
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-config-surface:p1:inst-rl-cfg-2
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-config-surface:p1:inst-rl-cfg-3
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-config-surface:p1:inst-rl-cfg-5
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-config-surface:p1:inst-rl-cfg-6
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-config-surface:p1:inst-rl-cfg-7
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-config-surface:p1:inst-rl-cfg-8
fn validate_budget(budget: &Budget) -> Result<(), DomainError> {
    if matches!(budget.mode, BudgetMode::Allocated | BudgetMode::Shared) && budget.total.is_none() {
        return Err(DomainError::field_rejection(
            "rate_limit.budget.total",
            "the budget total is required when the budget mode allocates or shares a budget",
        ));
    }
    if let Some(total) = budget.total {
        if total < 1 {
            return Err(DomainError::field_rejection(
                "rate_limit.budget.total",
                "the budget total is at least 1",
            ));
        }
    }
    if let Some(ratio) = budget.overcommit_ratio {
        if !(MIN_OVERCOMMIT_RATIO..=MAX_OVERCOMMIT_RATIO).contains(&ratio) {
            return Err(DomainError::field_rejection(
                "rate_limit.budget.overcommit_ratio",
                "the budget overcommit ratio is between 1.0 and 2.0 inclusive",
            ));
        }
    }
    Ok(())
}
//
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-config-surface:p1:inst-rl-cfg-8
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-config-surface:p1:inst-rl-cfg-7
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-config-surface:p1:inst-rl-cfg-6
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-config-surface:p1:inst-rl-cfg-5
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-config-surface:p1:inst-rl-cfg-3
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-config-surface:p1:inst-rl-cfg-2
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-config-surface:p1:inst-rl-cfg-1
//
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-config-surface:p1:inst-rl-cfg-4

pub const fn validate_sharing_mode(field: &str, mode: SharingMode) -> Result<(), DomainError> {
    let _ = field;
    let _ = mode;
    Ok(())
}

/// Validate the ordered `plugins.items` list of a management write
/// (`inst-um-cv-9`): an entry is a builtin or catalog plugin GTS identifier or
/// a custom plugin UUID reference, and no entry appears twice. The positions
/// are the list indices, so they are contiguous from zero by construction and
/// a gap or a duplicate cannot be expressed as a list.
///
/// # Errors
///
/// Returns a validation error naming `plugins.items`.
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-9
// `inst-um-cv-9`: the ordered chain holds builtin GTS identifiers and custom
// UUID references in contiguous positions from zero.
pub fn validate_plugin_items(items: &[String]) -> Result<(), DomainError> {
    let known = |reference: &str| {
        crate::domain::gts_helpers::BUILTIN_PLUGIN_IDS.contains(&reference)
            || crate::domain::gts_helpers::CATALOG_ONLY_PLUGIN_IDS.contains(&reference)
    };
    for (position, reference) in items.iter().enumerate() {
        if !known(reference) && uuid::Uuid::parse_str(reference).is_err() {
            return Err(DomainError::field_rejection(
                "plugins.items",
                "an entry is a plugin GTS identifier or a custom plugin UUID reference",
            ));
        }
        if items[..position].contains(reference) {
            return Err(DomainError::field_rejection(
                "plugins.items",
                "an entry may appear only once in the ordered chain",
            ));
        }
    }
    Ok(())
}
// @cpt-end:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-9

// @cpt-begin:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-12
// `inst-um-cv-12`: every header rule is validated before it is persisted, so a
// rule that could corrupt or forge an outbound request is never stored. The
// rejection names the block (`headers.request` / `headers.response`) and the
// offending key, and is a 400 validation error.
/// The largest header rule name or value the write path accepts, in bytes.
pub const MAX_HEADER_RULE_BYTES: usize = 4096;

/// Whether `name` satisfies the RFC 7230 `field-name` grammar: one or more
/// `tchar` characters, which excludes every separator and every control
/// character.
#[must_use]
pub fn header_name_is_valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_HEADER_RULE_BYTES
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.'
                        | b'^' | b'_' | b'`' | b'|' | b'~'
                )
        })
}

/// Whether `value` carries none of the three bytes that would corrupt an
/// outbound header line and stays within the 4096-byte bound.
#[must_use]
pub fn header_value_is_valid(value: &str) -> bool {
    value.len() <= MAX_HEADER_RULE_BYTES
        && !value.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0))
}

/// Validate one header rule: the name against the RFC 7230 grammar and both
/// the name and the value against the control-character and length bounds.
fn validate_header_rule(block: &str, key: &str, name: &str, value: &str) -> Result<(), DomainError> {
    let field = format!("{block}.{key}.{name}");
    if !header_name_is_valid(name) {
        return Err(DomainError::field_rejection(
            &field,
            "a header name must satisfy RFC 7230 field-name grammar",
        ));
    }
    if !header_value_is_valid(value) {
        return Err(DomainError::field_rejection(
            &field,
            "a header name and value carry no CR, LF or NUL byte and are at most 4096 bytes",
        ));
    }
    Ok(())
}

fn validate_header_names(block: &str, key: &str, names: &[String]) -> Result<(), DomainError> {
    for name in names {
        validate_header_rule(block, key, name, "")?;
    }
    Ok(())
}

fn validate_header_map(
    block: &str,
    key: &str,
    rules: &Option<std::collections::BTreeMap<String, String>>,
) -> Result<(), DomainError> {
    if let Some(rules) = rules {
        for (name, value) in rules {
            validate_header_rule(block, key, name, value)?;
        }
    }
    Ok(())
}

/// Validate a `headers` sub-configuration (`inst-um-cv-5`/`-12`): only the
/// documented keys, `passthrough` restricted to its three values (the DTO
/// enumeration), `passthrough_allowlist` meaningful only with
/// `passthrough: allowlist`, and every name/value inside the RFC 7230 and
/// 4096-byte bounds.
///
/// # Errors
///
/// Returns a validation error naming the offending block and key.
// @cpt-end:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-12
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-5
// `inst-um-cv-5`: the request and response blocks hold only `set`, `add` and
// `remove`, plus the request-side `passthrough` values with the allowlist rule.
pub fn validate_headers(headers: &HeadersConfig) -> Result<(), DomainError> {
    if let Some(request) = &headers.request {
        validate_header_map("headers.request", "set", &request.set)?;
        validate_header_map("headers.request", "add", &request.add)?;
        validate_header_names(
            "headers.request",
            "remove",
            request.remove.as_deref().unwrap_or(&[]),
        )?;
        validate_header_names(
            "headers.request",
            "passthrough_allowlist",
            request.passthrough_allowlist.as_deref().unwrap_or(&[]),
        )?;
        // `passthrough_allowlist` is meaningful only with `allowlist`.
        if !request.passthrough_allowlist.as_deref().unwrap_or(&[]).is_empty()
            && !matches!(request.passthrough, Some(HeaderPassthrough::Allowlist))
        {
            return Err(DomainError::field_rejection(
                "headers.request.passthrough_allowlist",
                "passthrough_allowlist requires passthrough: allowlist",
            ));
        }
    }
    if let Some(response) = &headers.response {
        validate_header_map("headers.response", "set", &response.set)?;
        validate_header_map("headers.response", "add", &response.add)?;
        validate_header_names(
            "headers.response",
            "remove",
            response.remove.as_deref().unwrap_or(&[]),
        )?;
    }
    Ok(())
}
// @cpt-end:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-5

/// Validate a route `match` block: exactly one of `http`/`grpc`, non-empty
/// method list, non-empty path.
///
/// # Errors
///
/// Returns a validation error naming the offending field.
pub fn validate_match(match_: &MatchConfig) -> Result<RouteMatchKind, DomainError> {
    match (&match_.http, &match_.grpc) {
        (Some(http), None) => {
            validate_http_match(http)?;
            Ok(RouteMatchKind::Http)
        }
        (None, Some(grpc)) => {
            if grpc.service.is_empty() || grpc.method.is_empty() {
                return Err(DomainError::field_rejection(
                    "match.grpc",
                    "service and method are both required and non-empty",
                ));
            }
            Ok(RouteMatchKind::Grpc)
        }
        (Some(_), Some(_)) => Err(DomainError::field_rejection(
            "match",
            "exactly one of {http|grpc} must be present",
        )),
        (None, None) => Err(DomainError::field_rejection(
            "match",
            "exactly one of {http|grpc} must be present",
        )),
    }
}

/// The protocol a `match` block selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteMatchKind {
    Http,
    Grpc,
}

fn validate_http_match(http: &HttpMatch) -> Result<(), DomainError> {
    if http.methods.is_empty() {
        return Err(DomainError::field_rejection(
            "match.http.methods",
            "at least one method is required",
        ));
    }
    if http.path.is_empty() {
        return Err(DomainError::field_rejection("match.http.path", "path is required"));
    }
    for name in &http.query_allowlist {
        if name.contains('\r') || name.contains('\n') {
            return Err(DomainError::field_rejection(
                "match.http.query_allowlist",
                "a query parameter name must not carry a control character",
            ));
        }
    }
    Ok(())
}

/// Validate a [`Route`] in full: the match block, the derived match type, the
/// tags, and the route-level `rate_limit`, `cors` and `plugins` overrides.
///
/// The match type is derived from the selected block and is never accepted on
/// write; the caller's value is overwritten here. The route-level overrides
/// are validated against the same schema shapes the upstream carries, because
/// the route payload contract admits them as one of the named schema-external
/// API fields (`cpt-cf-oagw-dod-route-management-schema-conformance`).
///
/// # Errors
///
/// Returns a validation error naming the offending field.
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-config-load:p1:inst-gf-cfg-4
// `inst-gf-cfg-4`: the same validation the configuration-load path applies to
// an upstream applies to a route layer, so a stored route is never malformed.
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-config-load:p1:inst-gf-cfg-4
// @cpt-begin:cpt-cf-oagw-algo-route-management-match-validation:p1:inst-rm-mv-7b
// `inst-rm-mv-7b`: the schema-external `priority` is validated as an integer
// before it is stored; the declared default `0` is materialized by the
// transport DTO for a body that omits it. The route-level `rate_limit`,
// `cors`, `tags` and `plugins` blocks are validated against the same shapes the
// upstream carries, so the merge engine receives a well-formed route layer.
// @cpt-begin:cpt-cf-oagw-algo-route-management-match-validation:p1:inst-rm-mv-1
// @cpt-begin:cpt-cf-oagw-algo-route-management-match-validation:p1:inst-rm-mv-2
// @cpt-begin:cpt-cf-oagw-algo-route-management-match-validation:p1:inst-rm-mv-3
// @cpt-begin:cpt-cf-oagw-algo-route-management-match-validation:p1:inst-rm-mv-4
// @cpt-begin:cpt-cf-oagw-algo-route-management-match-validation:p1:inst-rm-mv-4b
// @cpt-begin:cpt-cf-oagw-algo-route-management-match-validation:p1:inst-rm-mv-5
// @cpt-begin:cpt-cf-oagw-algo-route-management-match-validation:p1:inst-rm-mv-6
// @cpt-begin:cpt-cf-oagw-algo-route-management-match-validation:p1:inst-rm-mv-7
pub fn validate_route(route: &Route) -> Result<Route, DomainError> {
    let kind = validate_match(&route.match_)?;
    validate_tags("tags", &route.tags)?;
    if let Some(cors) = &route.cors {
        validate_cors(cors)?;
    }
    if let Some(rate_limit) = &route.rate_limit {
        validate_rate_limit(rate_limit)?;
    }
    if let Some(plugins) = &route.plugins {
        validate_plugin_items(&plugins.items)?;
    }
    let mut validated = route.clone();
    // The accepted origin entries are stored in the canonical form the
    // request-time comparison serializes to (`inst-cors-alg-cv-3`).
    if let Some(cors) = &mut validated.cors {
        canonicalize_cors(cors);
    }
    validated.match_type = match kind {
        RouteMatchKind::Http => RouteMatchType::Http,
        RouteMatchKind::Grpc => RouteMatchType::Grpc,
    };
    Ok(validated)
    // @cpt-end:cpt-cf-oagw-algo-route-management-match-validation:p1:inst-rm-mv-7b
}
//
// @cpt-end:cpt-cf-oagw-algo-route-management-match-validation:p1:inst-rm-mv-7
// @cpt-end:cpt-cf-oagw-algo-route-management-match-validation:p1:inst-rm-mv-6
// @cpt-end:cpt-cf-oagw-algo-route-management-match-validation:p1:inst-rm-mv-5
// @cpt-end:cpt-cf-oagw-algo-route-management-match-validation:p1:inst-rm-mv-4b
// @cpt-end:cpt-cf-oagw-algo-route-management-match-validation:p1:inst-rm-mv-4
// @cpt-end:cpt-cf-oagw-algo-route-management-match-validation:p1:inst-rm-mv-3
// @cpt-end:cpt-cf-oagw-algo-route-management-match-validation:p1:inst-rm-mv-2
// @cpt-end:cpt-cf-oagw-algo-route-management-match-validation:p1:inst-rm-mv-1
//

/// Validate a custom [`Plugin`] record: a concrete GTS plugin-type identifier,
/// a non-empty unique name, and no garbage-collection timestamp populated by
/// foundation code.
///
/// # Errors
///
/// Returns a validation error naming the offending field.
pub fn validate_plugin(plugin: &Plugin) -> Result<(), DomainError> {
    if plugin.name.is_empty() {
        return Err(DomainError::field_rejection("name", "name is required"));
    }
    if plugin.plugin_type.is_empty() {
        return Err(DomainError::field_rejection(
            "plugin_type",
            "plugin_type must be a concrete OAGW plugin GTS identifier",
        ));
    }
    if crate::domain::gts_helpers::base_type_of(&plugin.plugin_type).is_none()
        && !crate::domain::gts_helpers::BUILTIN_PLUGIN_IDS.contains(&plugin.plugin_type.as_str())
        && !crate::domain::gts_helpers::CATALOG_ONLY_PLUGIN_IDS.contains(&plugin.plugin_type.as_str())
    {
        return Err(DomainError::field_rejection(
            "plugin_type",
            "plugin_type must be a concrete OAGW plugin GTS identifier",
        ));
    }
    Ok(())
}

/// Which alias check an upstream validation applies.
///
/// The management path derives and reconciles the alias *after* the endpoint
/// pool is validated, so it asks for [`AliasMode::Deferred`]; every other
/// caller keeps the pattern check of the stored record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasMode {
    /// The alias must be present and match the alias pattern.
    Require,
    /// The alias is reconciled against the derived value afterwards, so the
    /// pattern check is left to that step.
    Deferred,
}

/// Validate the structural invariants of an [`Upstream`] record that are not
/// configuration-scoped: protocol identifier, alias pattern, endpoint pool,
/// header rules, tags, and the credential-reference boundary over every
/// credential-bearing field. `allow_http_upstream` gates the `http` scheme.
///
/// # Errors
///
/// Returns a validation error naming the offending field.
// @cpt-begin:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-1
// `inst-um-cv-1`: a field outside the upstream schema property set cannot
// reach this function, because every sub-configuration DTO carries
// `deny_unknown_fields` at the deserialization boundary; the management
// service is the caller that asserts the invariant on every write.
pub fn validate_upstream_record(
    upstream: &Upstream,
    allow_http_upstream: bool,
    alias_mode: AliasMode,
) -> Result<Upstream, DomainError> {
    // inst-gf-cred-1/-2: no credential-bearing field may carry anything other
    // than a `cred://` reference, at the boundary before the store sees it.
    crate::domain::credential::reject_non_cred_reference_values(
        "upstream",
        upstream.auth.as_ref(),
        upstream.headers.as_ref(),
    )?;
    validate_protocol(&upstream.protocol)?;
    // @cpt-begin:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-12
    // `inst-um-cv-5`/`-12`: the header rules are validated before persistence.
    if let Some(headers) = &upstream.headers {
        validate_headers(headers)?;
    }
    // @cpt-end:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-12
    if alias_mode == AliasMode::Require && !alias_is_valid(&upstream.alias) {
        return Err(DomainError::field_rejection(
            "alias",
            "alias matches `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`",
        ));
    }
    // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-config-load:p1:inst-gf-cfg-5
    // `inst-gf-cfg-5`/`-6`: `http` is admitted only while
    // `allow_http_upstream` is true, and the rejection names the endpoint.
    let server = validate_server(&upstream.server, allow_http_upstream)?;
    // @cpt-end:cpt-cf-oagw-flow-gear-foundation-config-load:p1:inst-gf-cfg-5
    validate_tags("tags", &upstream.tags)?;
    // @cpt-begin:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-8
    // `inst-um-cv-7`/`-8`: the nested rate-limit and CORS blocks are validated
    // against their schema shapes before anything is stored.
    if let Some(rate_limit) = &upstream.rate_limit {
        validate_rate_limit(rate_limit)?;
    }
    if let Some(cors) = &upstream.cors {
        validate_cors(cors)?;
    }
    // @cpt-end:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-8
    let mut validated = upstream.clone();
    // The accepted origin entries are stored in the canonical form the
    // request-time comparison serializes to (`inst-cors-alg-cv-3`).
    if let Some(cors) = &mut validated.cors {
        canonicalize_cors(cors);
    }
    validated.server = server;
    Ok(validated)
}
// @cpt-end:cpt-cf-oagw-algo-upstream-management-config-validation:p1:inst-um-cv-1

/// Validate an upstream record with the alias pattern enforced, which is the
/// posture of the 2.1 control-plane write path.
///
/// # Errors
///
/// Returns a validation error naming the offending field.
pub fn validate_upstream(upstream: &Upstream, allow_http_upstream: bool) -> Result<Upstream, DomainError> {
    validate_upstream_record(upstream, allow_http_upstream, AliasMode::Require)
}

/// The default endpoint port, re-exported for callers that materialize an
/// endpoint without an explicit port.
pub use crate::domain::dto::EndpointScheme;

#[cfg(test)]
#[path = "validation_tests.rs"]
mod tests;
