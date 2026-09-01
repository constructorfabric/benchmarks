// Created: 2026-08-31 by Constructor Tech
//! Write-path validation (DESIGN §2.2 constraints, §3.2 "Guard Rules" and
//! §4.4 "Security Considerations").
//!
//! Pure checks: no store access, no HTTP types beyond the crate error surface,
//! so the data plane (slice 2) can reuse [`validate_framing`] unchanged.

use crate::config::SsrfPolicy;
use crate::domain::alias::{classify_host, normalize, validate_alias};
use crate::domain::model::{CorsConfig, Endpoint, RateLimitConfig, RouteMatch};
use crate::error::{OagwError, OagwErrorKind, OagwResult};

/// Inputs the write path needs from `gears.oagw.config`.
///
/// Materialised by [`crate::config::OagwConfig::validation_policy`] so the
/// validation rules cannot drift from the configuration keys.
#[derive(Debug, Clone)]
pub struct ValidationPolicy {
    /// Whether plaintext (`http` / `ws`) endpoints may be dialled
    /// (DESIGN §2.2 `constraint-https-only`).
    pub allow_http_upstream: bool,
    /// Hard request-body limit in bytes.
    pub max_body_bytes: u64,
    /// Server-side request forgery guards.
    pub ssrf: SsrfPolicy,
}

/// Endpoints per upstream the management API accepts.
///
/// Multi-endpoint pools are load-balancing groups (DESIGN §3.2); a pool large
/// enough to need pagination belongs in a dedicated load balancer, not in a
/// single upstream record.
pub const MAX_ENDPOINTS: usize = 64;

/// Discovery tags per record.
///
/// Tags feed the slice-2 discovery index; 32 is well above any realistic
/// naming scheme (`team`, `env`, `region`, ` pii` …) and keeps a tag list from
/// becoming an unindexed search field.
pub const MAX_TAGS: usize = 32;

/// Maximum bytes of a custom plugin's Starlark `source_code`.
///
/// Plugins are stored and (in slice 2) compiled verbatim; 256 KiB covers every
/// realistic policy script while bounding the record size.
pub const MAX_SOURCE_BYTES: usize = 256 * 1024;

/// Maximum bytes of a custom plugin's `config` object.
///
/// `config` is replayed to the plugin on every request; 16 KiB bounds the
/// record and keeps configuration snippets from turning into a data store.
pub const MAX_CONFIG_BYTES: usize = 16 * 1024;

/// Validate the discovery tags of a record.
///
/// A tag is `[a-z0-9_-]+` — a short lowercase label a discovery query can
/// filter on; at most [`MAX_TAGS`] per record.
///
/// # Errors
/// 400 when a tag is empty, carries characters outside the label set, or the
/// list exceeds [`MAX_TAGS`].
pub fn validate_tags(tags: &[String]) -> OagwResult<()> {
    if tags.len() > MAX_TAGS {
        return Err(OagwError::validation(format!(
            "a record carries at most {MAX_TAGS} tags"
        )));
    }
    for tag in tags {
        let valid = !tag.is_empty()
            && tag.len() <= 64
            && tag.chars().all(|character| {
                character.is_ascii_lowercase()
                    || character.is_ascii_digit()
                    || character == '_'
                    || character == '-'
            });
        if !valid {
            return Err(OagwError::validation(
                "tags must be lowercase '[a-z0-9_-]+' labels of at most 64 characters",
            )
            .with_extension(|ext| ext.invalid_value = Some(tag.clone())));
        }
    }
    Ok(())
}

/// Validate the size of a custom plugin's `config` object.
///
/// # Errors
/// 400 when the rendered configuration exceeds [`MAX_CONFIG_BYTES`].
pub fn validate_config_bytes(config: &serde_json::Value) -> OagwResult<()> {
    let size = serde_json::to_vec(config)
        .map_err(|_| OagwError::validation("plugin config must be valid JSON"))?
        .len();
    if size > MAX_CONFIG_BYTES {
        return Err(OagwError::validation(format!(
            "plugin config exceeds {MAX_CONFIG_BYTES} bytes"
        ))
        .with_extension(|ext| ext.invalid_value = Some(size.to_string())));
    }
    Ok(())
}

/// Validate an endpoint pool.
///
/// Enforces: at least one endpoint and at most [`MAX_ENDPOINTS`], resolvable
/// hosts, in-range ports, scheme homogeneity, the plaintext-scheme switch and
/// the SSRF host lists.
///
/// # Errors
/// 400 on every violated rule; 503-style kinds are reserved for the data plane.
pub fn validate_endpoints(policy: &ValidationPolicy, endpoints: &[Endpoint]) -> OagwResult<()> {
    if endpoints.is_empty() {
        return Err(OagwError::validation("at least one endpoint is required"));
    }
    if endpoints.len() > MAX_ENDPOINTS {
        return Err(OagwError::validation(format!(
            "an upstream accepts at most {MAX_ENDPOINTS} endpoints"
        )));
    }
    for endpoint in endpoints {
        validate_host(&endpoint.host)?;
        if endpoint.port == 0 {
            return Err(
                OagwError::validation("endpoint port must be between 1 and 65535")
                    .with_extension(|ext| ext.invalid_value = Some(endpoint.port.to_string())),
            );
        }
        if endpoint.scheme.is_plaintext() && !policy.allow_http_upstream {
            return Err(OagwError::validation(
                "plaintext upstream schemes require oagw.config.allow_http_upstream",
            )
            .with_extension(|ext| ext.invalid_value = Some(endpoint_scheme(endpoint))));
        }
    }
    validate_pool_homogeneity(endpoints)?;
    for endpoint in endpoints {
        check_ssrf(&policy.ssrf, &endpoint.host)?;
    }
    Ok(())
}

fn endpoint_scheme(endpoint: &Endpoint) -> String {
    format!("{:?}", endpoint.scheme).to_ascii_lowercase()
}

/// Host syntax check (RFC 1123 hostname or IP literal).
fn validate_host(host: &str) -> OagwResult<()> {
    if classify_host(host) == crate::domain::alias::HostKind::Invalid {
        return Err(OagwError::validation(
            "endpoint host must be an RFC 1123 hostname or an IP address",
        )
        .with_extension(|ext| {
            ext.host = Some(host.to_owned());
            ext.invalid_value = Some(host.to_owned());
        }));
    }
    Ok(())
}

/// All endpoints of a pool share scheme and port (DESIGN §3.2 "Multi-Endpoint
/// Load Balancing").
fn validate_pool_homogeneity(endpoints: &[Endpoint]) -> OagwResult<()> {
    let first = &endpoints[0];
    let homogeneous = endpoints
        .iter()
        .all(|endpoint| endpoint.scheme == first.scheme && endpoint.port == first.port);
    if homogeneous {
        Ok(())
    } else {
        Err(OagwError::validation(
            "all endpoints of an upstream must share the same scheme and port",
        ))
    }
}

/// Server-side request forgery guard of the write path (DESIGN §4.4).
///
/// Only the configured host lists are consulted: `denied_hosts` always wins,
/// and a non-empty `allowed_hosts` turns the policy into an allowlist. DNS
/// resolution and IP pinning are a separate concern (DESIGN §4.5).
fn check_ssrf(ssrf: &SsrfPolicy, host: &str) -> OagwResult<()> {
    check_host_lists(ssrf, host, OagwErrorKind::Validation)
}

/// Server-side request forgery guard of the data plane (DESIGN §4.4).
///
/// Re-applies the host lists to the endpoint that is about to be dialled and,
/// when the policy is enabled, refuses the IP literals that point at the
/// gateway's own neighbourhood: loopback, link-local (which covers the cloud
/// metadata address), unique-local and unspecified addresses. No DNS: only a
/// literal host is inspected, so the check costs nothing per request.
///
/// # Errors
/// 503 when the policy refuses the host; the write path already reported the
/// same host as 400, so a record that survived it is reported as an
/// unavailable link rather than as a client mistake.
pub fn check_egress(ssrf: &SsrfPolicy, host: &str) -> OagwResult<()> {
    if !ssrf.enabled {
        return Ok(());
    }
    check_host_lists(ssrf, host, OagwErrorKind::LinkUnavailable)?;
    let candidate = normalize(host);
    if candidate
        .parse::<std::net::IpAddr>()
        .is_ok_and(is_local_network)
    {
        return Err(OagwError::new(
            OagwErrorKind::LinkUnavailable,
            "upstream host is a local-network address denied by the SSRF policy",
        )
        .with_extension(|ext| ext.host = Some(candidate)));
    }
    Ok(())
}

/// Whether an IP address is one the gateway must never dial for a client.
fn is_local_network(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast()
        }
        // Unique-local (`fc00::/7`) and link-local (`fe80::/10`) are the IPv6
        // counterparts; the loopback and unspecified addresses are already
        // covered by the standard predicates.
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Host-list guard, parameterised by the error kind of the caller.
///
/// The write path reports a refused host as a 400 validation error, the data
/// plane as an unavailable link.
fn check_host_lists(ssrf: &SsrfPolicy, host: &str, kind: OagwErrorKind) -> OagwResult<()> {
    if !ssrf.enabled {
        return Ok(());
    }
    let candidate = normalize(host);
    let denied = ssrf
        .denied_hosts
        .iter()
        .any(|blocked| normalize(blocked) == candidate);
    if denied {
        return Err(
            OagwError::new(kind, "upstream host is denied by the SSRF policy")
                .with_extension(|ext| ext.host = Some(candidate)),
        );
    }
    let allow_listed = ssrf.allowed_hosts.is_empty()
        || ssrf
            .allowed_hosts
            .iter()
            .any(|allowed| normalize(allowed) == candidate);
    if !allow_listed {
        return Err(
            OagwError::new(kind, "upstream host is not in the SSRF allowlist")
                .with_extension(|ext| ext.host = Some(candidate)),
        );
    }
    Ok(())
}

/// Validate a route match rule (route schema: exactly one of `http`/`grpc`).
///
/// # Errors
/// 400 when neither or both of `http` and `grpc` are set, when the `http`
/// branch declares no method or a relative path, or when the `grpc` branch
/// omits its service or method.
pub fn validate_route_match(match_rule: &RouteMatch) -> OagwResult<()> {
    match match_rule {
        RouteMatch {
            http: Some(http),
            grpc: None,
        } => validate_http_match(http),
        RouteMatch {
            http: None,
            grpc: Some(grpc),
        } => {
            if grpc.service.trim().is_empty() || grpc.method.trim().is_empty() {
                return Err(OagwError::validation(
                    "grpc match requires both a service and a method",
                ));
            }
            Ok(())
        }
        _ => Err(OagwError::validation(
            "route match must define exactly one of 'http' or 'grpc'",
        )),
    }
}

fn validate_http_match(http: &crate::domain::model::HttpMatch) -> OagwResult<()> {
    if http.methods.is_empty() {
        return Err(OagwError::validation(
            "http match requires at least one method",
        ));
    }
    if !http.path.starts_with('/') {
        return Err(OagwError::validation("http match path must start with '/'")
            .with_extension(|ext| ext.path = Some(http.path.clone())));
    }
    Ok(())
}

/// Validate request framing headers (DESIGN §3.2 body validation).
///
/// * at most one `Content-Length` value, and it must parse
/// * `Transfer-Encoding`, when present, must be exactly `chunked`
/// * the declared length must not exceed `max_body_bytes`
///
/// Returns the accepted body length, `None` when no length was declared.
///
/// # Errors
/// 400 for an unparseable or conflicting `Content-Length` and for a
/// `Transfer-Encoding` other than `chunked`, 413 for a declared length above
/// `max_body_bytes`.
pub fn validate_framing(
    content_length_headers: &[&str],
    transfer_encoding: Option<&str>,
    max_body_bytes: u64,
) -> OagwResult<Option<u64>> {
    let mut declared: Option<u64> = None;
    for raw in content_length_headers {
        for value in raw.split(',') {
            let value = value.trim();
            let parsed = value.parse::<u64>().map_err(|_| {
                OagwError::validation("Content-Length is not a valid number")
                    .with_extension(|ext| ext.invalid_value = Some(value.to_owned()))
            })?;
            if let Some(previous) = declared
                && previous != parsed
            {
                return Err(OagwError::validation("conflicting Content-Length values"));
            }
            declared = Some(parsed);
        }
    }
    if let Some(encoding) = transfer_encoding {
        let encodings: Vec<String> = encoding
            .split(',')
            .map(|value| value.trim().to_ascii_lowercase())
            .filter(|value| !value.is_empty())
            .collect();
        if encodings.as_slice() != ["chunked"] {
            return Err(
                OagwError::validation("only chunked Transfer-Encoding is supported")
                    .with_extension(|ext| ext.invalid_value = Some(encoding.to_owned())),
            );
        }
    }
    if let Some(length) = declared
        && length > max_body_bytes
    {
        return Err(OagwError::payload_too_large(max_body_bytes, length));
    }
    Ok(declared)
}

/// Validate the header transformation rules of an upstream (upstream schema
/// `headers`).
///
/// The data plane applies these rules to every request, so a name `http` could
/// never parse, or a `passthrough` mode the schema does not define, is
/// rejected when the record is written instead of silently ignored (or worse,
/// silently widened) per request.
///
/// # Errors
/// 400 when a configured header name is not a valid HTTP header name or the
/// `passthrough` mode is not one of `none`, `allowlist` or `all`.
pub fn validate_headers(config: Option<&crate::domain::model::HeadersConfig>) -> OagwResult<()> {
    let Some(config) = config else {
        return Ok(());
    };
    if let Some(request) = config.request.as_ref() {
        validate_header_rules(&request.set, &request.add, &request.remove)?;
        validate_passthrough(request.passthrough.as_deref())?;
    }
    if let Some(response) = config.response.as_ref() {
        validate_header_rules(&response.set, &response.add, &response.remove)?;
    }
    Ok(())
}

/// Validate the names of one rule block.
fn validate_header_rules(
    set: &std::collections::HashMap<String, String>,
    add: &std::collections::HashMap<String, String>,
    remove: &[String],
) -> OagwResult<()> {
    let names = set
        .keys()
        .chain(add.keys())
        .map(String::as_str)
        .chain(remove.iter().map(String::as_str));
    for name in names {
        if http::header::HeaderName::try_from(name).is_err() {
            return Err(
                OagwError::validation(format!("'{name}' is not a valid HTTP header name"))
                    .with_extension(|ext| ext.invalid_value = Some(name.to_owned())),
            );
        }
    }
    Ok(())
}

/// The `passthrough` modes the upstream schema defines.
const PASSTHROUGH_MODES: [&str; 3] = ["none", "allowlist", "all"];

/// Validate the `passthrough` mode.
fn validate_passthrough(mode: Option<&str>) -> OagwResult<()> {
    let Some(mode) = mode else {
        return Ok(());
    };
    if PASSTHROUGH_MODES.contains(&mode) {
        return Ok(());
    }
    Err(
        OagwError::validation("passthrough must be one of 'none', 'allowlist' or 'all'")
            .with_extension(|ext| ext.invalid_value = Some(mode.to_owned())),
    )
}

/// Validate an explicit alias handed to the write path.
///
/// # Errors
/// 400 with the alias rule that rejected the value.
pub fn validate_explicit_alias(alias: &str) -> OagwResult<()> {
    validate_alias(alias).map_err(|detail| {
        OagwError::new(OagwErrorKind::Validation, detail)
            .with_extension(|ext| ext.invalid_value = Some(alias.to_owned()))
    })
}

/// The algorithms the deployment enforces.
///
/// ADR-0003 "Configuration" lists `sliding_window` as the second value of the
/// `algorithm` enum but marks it *optional*: this deployment ships the token
/// bucket only, and an unknown algorithm must fail closed rather than be
/// enforced as something it is not.
const RATE_LIMIT_ALGORITHM: &str = "token_bucket";

/// Window units of `sustained.window` (ADR-0003 "Configuration").
const RATE_LIMIT_WINDOWS: [&str; 4] = ["second", "minute", "hour", "day"];

/// Counter scopes of `scope` (ADR-0003 "Configuration").
const RATE_LIMIT_SCOPES: [&str; 5] = ["global", "tenant", "user", "ip", "route"];

/// Behaviours of `strategy` (ADR-0003 "Configuration").
const RATE_LIMIT_STRATEGIES: [&str; 3] = ["reject", "queue", "degrade"];

/// Validate a rate-limit policy (ADR-0003 "Configuration").
///
/// The data plane cannot enforce a limit it does not understand, so every
/// member is checked here rather than ignored per request.
///
/// # Errors
/// 400 when the algorithm is not [`RATE_LIMIT_ALGORITHM`], the sustained rate
/// or burst capacity is below 1, the window is not one of the four units, or
/// `scope` / `strategy` / `cost` are outside the ADR's enums.
pub fn validate_rate_limit(config: Option<&RateLimitConfig>) -> OagwResult<()> {
    let Some(config) = config else {
        return Ok(());
    };
    if config.algorithm != RATE_LIMIT_ALGORITHM {
        return Err(OagwError::validation(format!(
            "rate limit algorithm '{algorithm}' is not supported by this deployment; \
             only '{RATE_LIMIT_ALGORITHM}' is enforced",
            algorithm = config.algorithm
        ))
        .with_extension(|ext| ext.invalid_value = Some(config.algorithm.clone())));
    }
    let invalid = |detail: String, value: String| {
        OagwError::validation(detail).with_extension(|ext| ext.invalid_value = Some(value))
    };
    if config.sustained.rate == 0 {
        return Err(invalid(
            "rate limit sustained.rate must be at least 1".to_owned(),
            config.sustained.rate.to_string(),
        ));
    }
    // The model materialises the schema default as an empty string; a blank
    // window *is* the schema default, so it is accepted as `second`.
    let window = config.sustained.window.as_str();
    if !window.is_empty() && !RATE_LIMIT_WINDOWS.contains(&window) {
        return Err(invalid(
            "rate limit sustained.window must be one of 'second', 'minute', 'hour' or 'day'"
                .to_owned(),
            window.to_owned(),
        ));
    }
    if let Some(burst) = config.burst.as_ref()
        && burst.capacity == 0
    {
        return Err(invalid(
            "rate limit burst.capacity must be at least 1".to_owned(),
            burst.capacity.to_string(),
        ));
    }
    if !RATE_LIMIT_SCOPES.contains(&config.scope.as_str()) {
        return Err(invalid(
            "rate limit scope must be one of 'global', 'tenant', 'user', 'ip' or 'route'"
                .to_owned(),
            config.scope.clone(),
        ));
    }
    if !RATE_LIMIT_STRATEGIES.contains(&config.strategy.as_str()) {
        return Err(invalid(
            "rate limit strategy must be one of 'reject', 'queue' or 'degrade'".to_owned(),
            config.strategy.clone(),
        ));
    }
    if config.cost == 0 {
        return Err(invalid(
            "rate limit cost must be at least 1".to_owned(),
            config.cost.to_string(),
        ));
    }
    Ok(())
}

/// Validate a CORS policy (ADR-0004 "Credentials restriction").
///
/// # Errors
/// 400 when credentials are allowed while the origin list carries the
/// wildcard, which ADR-0004 forbids: a credentialed response to `*` would hand
/// a cross-origin reader every credential the browser holds.
pub fn validate_cors(config: &CorsConfig) -> OagwResult<()> {
    if config.allow_credentials && config.allowed_origins.iter().any(|origin| origin == "*") {
        return Err(OagwError::validation(
            "cannot use allow_credentials with a wildcard origin (ADR-0004: \"Cannot use \
             allow_credentials with wildcard origin\")",
        )
        .with_extension(|ext| ext.invalid_value = Some("*".to_owned())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::config::SsrfPolicy;
    use crate::domain::model::{Endpoint, GrpcMatch, HttpMatch, HttpMethod, RouteMatch};
    use crate::domain::validation::{
        MAX_CONFIG_BYTES, MAX_ENDPOINTS, MAX_TAGS, ValidationPolicy, validate_config_bytes,
        validate_cors, validate_endpoints, validate_framing, validate_rate_limit,
        validate_route_match, validate_tags,
    };
    use crate::error::OagwErrorKind;

    fn policy() -> ValidationPolicy {
        ValidationPolicy {
            allow_http_upstream: true,
            max_body_bytes: 100,
            ssrf: SsrfPolicy {
                enabled: false,
                allowed_hosts: Vec::new(),
                denied_hosts: Vec::new(),
            },
        }
    }

    fn endpoint(scheme: crate::domain::model::Scheme, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    /// A policy over the schema defaults, which the write path materialises.
    fn rate_limit() -> crate::domain::model::RateLimitConfig {
        crate::domain::model::RateLimitConfig {
            sharing: crate::domain::model::SharingMode::Private,
            algorithm: "token_bucket".to_owned(),
            sustained: crate::domain::model::SustainedRate {
                rate: 5,
                window: String::new(),
            },
            burst: Some(crate::domain::model::BurstConfig { capacity: 5 }),
            scope: "tenant".to_owned(),
            strategy: "reject".to_owned(),
            cost: 1,
            response_headers: true,
        }
    }

    /// A policy over the wildcard origin, the permissive default.
    fn cors_config() -> crate::domain::model::CorsConfig {
        crate::domain::model::CorsConfig {
            sharing: crate::domain::model::SharingMode::Private,
            enabled: true,
            allowed_origins: Vec::from(["*".to_owned()]),
            allowed_methods: Vec::new(),
            expose_headers: Vec::new(),
            allow_credentials: false,
        }
    }

    #[test]
    fn requires_at_least_one_endpoint() {
        let error = validate_endpoints(&policy(), &[]).unwrap_err();
        assert_eq!(error.kind(), &OagwErrorKind::Validation);
    }

    #[test]
    fn rejects_invalid_hosts() {
        let endpoints = [endpoint(
            crate::domain::model::Scheme::Https,
            "bad host",
            443,
        )];
        let error = validate_endpoints(&policy(), &endpoints).unwrap_err();
        assert_eq!(error.extensions().host.as_deref(), Some("bad host"));
    }

    #[test]
    fn rejects_port_zero() {
        let endpoints = [endpoint(
            crate::domain::model::Scheme::Https,
            "api.vendor.com",
            0,
        )];
        assert!(validate_endpoints(&policy(), &endpoints).is_err());
    }

    #[test]
    fn rejects_heterogeneous_pools() {
        let endpoints = [
            endpoint(crate::domain::model::Scheme::Https, "a.vendor.com", 443),
            endpoint(crate::domain::model::Scheme::Https, "b.vendor.com", 8443),
        ];
        assert!(validate_endpoints(&policy(), &endpoints).is_err());
    }

    #[test]
    fn plaintext_schemes_need_the_config_switch() {
        let strict = ValidationPolicy {
            allow_http_upstream: false,
            ..policy()
        };
        let endpoints = [endpoint(
            crate::domain::model::Scheme::Http,
            "api.vendor.com",
            80,
        )];
        assert!(validate_endpoints(&strict, &endpoints).is_err());
        assert!(validate_endpoints(&policy(), &endpoints).is_ok());
    }

    #[test]
    fn ssrf_denied_hosts_win_over_the_allowlist() {
        let policy = ValidationPolicy {
            ssrf: SsrfPolicy {
                enabled: true,
                allowed_hosts: vec!["metadata.internal".to_owned()],
                denied_hosts: vec!["metadata.internal".to_owned()],
            },
            ..policy()
        };
        let endpoints = [endpoint(
            crate::domain::model::Scheme::Https,
            "metadata.internal",
            443,
        )];
        assert!(validate_endpoints(&policy, &endpoints).is_err());
    }

    #[test]
    fn ssrf_allowlist_rejects_unknown_hosts() {
        let policy = ValidationPolicy {
            ssrf: SsrfPolicy {
                enabled: true,
                allowed_hosts: vec!["api.vendor.com".to_owned()],
                denied_hosts: Vec::new(),
            },
            ..policy()
        };
        let allowed = [endpoint(
            crate::domain::model::Scheme::Https,
            "API.Vendor.com",
            443,
        )];
        assert!(validate_endpoints(&policy, &allowed).is_ok());
        let blocked = [endpoint(
            crate::domain::model::Scheme::Https,
            "other.vendor.com",
            443,
        )];
        assert!(validate_endpoints(&policy, &blocked).is_err());
    }

    #[test]
    fn route_match_requires_exactly_one_branch() {
        let http = RouteMatch {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Get],
                path: "/v1/chat".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
            }),
            grpc: None,
        };
        assert!(validate_route_match(&http).is_ok());
        assert!(
            validate_route_match(&RouteMatch {
                http: None,
                grpc: None
            })
            .is_err()
        );
        assert!(
            validate_route_match(&RouteMatch {
                http: Some(HttpMatch {
                    methods: Vec::new(),
                    path: "/v1".to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
                }),
                grpc: None,
            })
            .is_err()
        );
        assert!(
            validate_route_match(&RouteMatch {
                http: None,
                grpc: Some(GrpcMatch {
                    service: "pkg.Svc".to_owned(),
                    method: "Get".to_owned(),
                }),
            })
            .is_ok()
        );
    }

    #[test]
    fn http_match_paths_must_be_absolute() {
        let rule = RouteMatch {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Post],
                path: "v1/chat".to_owned(),
                query_allowlist: Vec::new(),
                path_suffix_mode: crate::domain::model::PathSuffixMode::Append,
            }),
            grpc: None,
        };
        assert!(validate_route_match(&rule).is_err());
    }

    #[test]
    fn framing_accepts_a_single_content_length() {
        assert_eq!(
            validate_framing(&["42"], None, 100).ok().flatten(),
            Some(42)
        );
        assert_eq!(validate_framing(&[], None, 100).ok().flatten(), None);
    }

    #[test]
    fn framing_rejects_conflicting_or_unparseable_lengths() {
        assert!(validate_framing(&["42", "43"], None, 100).is_err());
        assert!(validate_framing(&["abc"], None, 100).is_err());
    }

    #[test]
    fn framing_only_allows_chunked_transfer_encoding() {
        assert!(validate_framing(&["10"], Some("chunked"), 100).is_ok());
        assert!(validate_framing(&["10"], Some("gzip, chunked"), 100).is_err());
    }

    #[test]
    fn framing_enforces_the_body_limit() {
        let error = validate_framing(&["101"], None, 100).unwrap_err();
        assert_eq!(error.kind(), &OagwErrorKind::PayloadTooLarge);
    }

    #[test]
    fn accepts_the_documented_tag_vocabulary() {
        assert!(
            validate_tags(&["team".to_owned(), "eu-west-1".to_owned(), "p_1".to_owned()]).is_ok()
        );
    }

    #[test]
    fn rejects_a_tag_outside_the_label_set() {
        for tag in [
            "",
            "Team",
            "eu west",
            "pii/handler",
            "\u{43a}\u{43b}\u{44e}\u{447}",
        ] {
            let error = validate_tags(&[tag.to_owned()]).unwrap_err();
            assert_eq!(error.kind(), &OagwErrorKind::Validation);
            assert_eq!(error.extensions().invalid_value.as_deref(), Some(tag));
        }
    }

    #[test]
    fn rejects_more_tags_than_the_cap() {
        let tags: Vec<String> = (0..=MAX_TAGS).map(|index| format!("tag{index}")).collect();
        assert!(validate_tags(&tags).is_err());
        assert!(validate_tags(&tags[..MAX_TAGS]).is_ok());
    }

    #[test]
    fn rejects_more_endpoints_than_the_cap() {
        let endpoints: Vec<Endpoint> = (0..=MAX_ENDPOINTS)
            .map(|index| {
                endpoint(
                    crate::domain::model::Scheme::Https,
                    &format!("host{index}.vendor.com"),
                    443,
                )
            })
            .collect();
        assert!(validate_endpoints(&policy(), &endpoints).is_err());
        assert!(validate_endpoints(&policy(), &endpoints[..MAX_ENDPOINTS]).is_ok());
    }

    #[test]
    fn rejects_a_config_beyond_the_cap() {
        let small = serde_json::json!({ "level": "debug" });
        assert!(validate_config_bytes(&small).is_ok());
        let large = serde_json::json!({ "blob": "x".repeat(MAX_CONFIG_BYTES + 1) });
        assert!(validate_config_bytes(&large).is_err());
    }

    #[test]
    fn a_rate_limit_without_a_policy_is_accepted() {
        assert!(validate_rate_limit(None).is_ok());
    }

    #[test]
    fn the_schema_default_policy_is_accepted() {
        // A policy the write path materialised from the schema defaults: the
        // window is the empty string and it stands for `second`.
        assert!(validate_rate_limit(Some(&rate_limit())).is_ok());
    }

    #[test]
    fn only_the_token_bucket_algorithm_is_enforced() {
        let mut config = rate_limit();
        config.algorithm = "sliding_window".to_owned();
        let error = validate_rate_limit(Some(&config)).unwrap_err();
        assert_eq!(error.kind(), &OagwErrorKind::Validation);
        assert_eq!(error.status(), 400);
        assert_eq!(
            error.extensions().invalid_value.as_deref(),
            Some("sliding_window")
        );
        assert!(
            error
                .to_string()
                .contains("not supported by this deployment"),
            "the detail names the deployment's limitation: {error}"
        );
    }

    #[test]
    fn a_policy_below_one_request_is_rejected() {
        let mut config = rate_limit();
        config.sustained.rate = 0;
        assert_eq!(
            validate_rate_limit(Some(&config)).unwrap_err().status(),
            400
        );
        let mut config = rate_limit();
        config.burst = Some(crate::domain::model::BurstConfig { capacity: 0 });
        assert!(validate_rate_limit(Some(&config)).is_err());
        let mut config = rate_limit();
        config.cost = 0;
        assert!(validate_rate_limit(Some(&config)).is_err());
    }

    #[test]
    fn a_window_outside_the_four_units_is_rejected() {
        for window in ["week", "Second", "seconds"] {
            let mut config = rate_limit();
            config.sustained.window = window.to_owned();
            let error = validate_rate_limit(Some(&config)).unwrap_err();
            assert_eq!(error.extensions().invalid_value.as_deref(), Some(window));
        }
        // The schema default is the empty string, and it stands for `second`.
        for window in ["", "second", "minute", "hour", "day"] {
            let mut config = rate_limit();
            config.sustained.window = window.to_owned();
            assert!(validate_rate_limit(Some(&config)).is_ok(), "{window}");
        }
    }

    #[test]
    fn a_scope_or_strategy_outside_the_enums_is_rejected() {
        let mut config = rate_limit();
        config.scope = "cluster".to_owned();
        assert!(validate_rate_limit(Some(&config)).is_err());
        let mut config = rate_limit();
        config.strategy = "shed".to_owned();
        assert!(validate_rate_limit(Some(&config)).is_err());
        for scope in ["global", "tenant", "user", "ip", "route"] {
            let mut config = rate_limit();
            config.scope = scope.to_owned();
            assert!(validate_rate_limit(Some(&config)).is_ok(), "{scope}");
        }
        for strategy in ["reject", "queue", "degrade"] {
            let mut config = rate_limit();
            config.strategy = strategy.to_owned();
            assert!(validate_rate_limit(Some(&config)).is_ok(), "{strategy}");
        }
    }

    #[test]
    fn a_wildcard_origin_behind_credentials_is_rejected() {
        let mut config = cors_config();
        config.allow_credentials = true;
        let error = validate_cors(&config).unwrap_err();
        assert_eq!(error.kind(), &OagwErrorKind::Validation);
        assert_eq!(error.status(), 400);
        assert!(
            error.to_string().contains("wildcard origin"),
            "the detail quotes the ADR rule: {error}"
        );
    }

    #[test]
    fn credentials_behind_a_named_origin_are_accepted() {
        let mut config = cors_config();
        config.allowed_origins = Vec::from(["https://app.example.com".to_owned()]);
        config.allow_credentials = true;
        assert!(validate_cors(&config).is_ok());
        // A wildcard without credentials is the ordinary permissive policy.
        assert!(validate_cors(&cors_config()).is_ok());
    }
}
