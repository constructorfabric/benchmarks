// Created: 2026-09-01 by Constructor Tech
//! Business logic, free of infrastructure dependencies.
//!
//! * [`model`] — the wire-facing resource shapes.
//! * [`alias`] — alias derivation and the immutability rules.
//! * [`store`] — the in-memory Control Plane store.
//! * [`hierarchy`] — tenant-chain walking and hierarchical merging.
//! * [`validate`] — create/replace validation.
//! * [`ratelimit`] — the token bucket / sliding window counters.
//! * [`breaker`] — the per-endpoint circuit breaker.
//! * [`headers`] — request/response header transformation.

pub mod alias;
pub mod breaker;
pub mod errors;
pub mod headers;
pub mod hierarchy;
pub mod model;
pub mod ratelimit;
pub mod store;

use std::time::Duration;

/// A fresh random UUID.
#[must_use]
pub fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// How long opening a socket to an upstream, or waiting for its response
/// head, may take. Both are pre-streaming events, so this bounds the
/// time-to-first-byte without ever cutting off an established stream.
pub const TIMEOUT_CONNECT: Duration = Duration::from_secs(10);

use errors::OagwError;
use model::{
    AuthConfig, Endpoint, HttpMatch, PluginSet, RateLimit, Route, RouteMatch, SharingMode, Upstream,
};

/// Legal endpoint schemes. `http` is accepted at create time even when
/// `allow_http_upstream` is off — that flag governs whether a plaintext
/// connection is actually established, not which schemes the field takes.
const SCHEMES: [&str; 5] = ["http", "https", "wss", "wt", "grpc"];

/// Validate a create/replace payload for an upstream.
///
/// # Errors
/// Returns the first violated rule.
pub fn validate_upstream(u: &Upstream) -> Result<(), OagwError> {
    if u.server.endpoints.is_empty() {
        return Err(OagwError::validation_field_error(
            "server.endpoints",
            "at least one endpoint is required",
        ));
    }
    let first = &u.server.endpoints[0];
    for ep in &u.server.endpoints {
        if !SCHEMES.contains(&ep.scheme.as_str()) {
            return Err(OagwError::validation_field_error(
                "server.endpoints[].scheme",
                format!(
                    "scheme '{}' is not one of http, https, wss, wt, grpc",
                    ep.scheme
                ),
            ));
        }
        if ep.port == 0 {
            return Err(OagwError::validation_field_error(
                "server.endpoints[].port",
                "port must be between 1 and 65535",
            ));
        }
        if ep.is_ip() {
            // IP literals are validated by the parser itself; only the
            // scheme/port homogeneity check below applies.
        } else if let Err(reason) = model::validate_hostname(&ep.host) {
            return Err(OagwError::validation_field_error(
                "server.endpoints[].host",
                reason,
            ));
        }
    }
    // Pool homogeneity: identical protocol, scheme and port.
    for ep in &u.server.endpoints {
        if ep.scheme != first.scheme {
            return Err(OagwError::validation_field_error(
                "server.endpoints[].scheme",
                "all endpoints in a pool must use the same scheme",
            ));
        }
        if ep.port != first.port {
            return Err(OagwError::validation_field_error(
                "server.endpoints[].port",
                "all endpoints in a pool must use the same port",
            ));
        }
    }
    if u.protocol != model::protocol::HTTP && u.protocol != model::protocol::GRPC {
        return Err(OagwError::validation_field_error(
            "protocol",
            format!("'{0}' is not a known oagw protocol identifier", u.protocol),
        ));
    }
    if let Some(auth) = &u.auth {
        validate_auth(auth)?;
    }
    if let Some(rl) = &u.rate_limit {
        validate_rate_limit(rl)?;
    }
    if let Some(cors) = &u.cors
        && let Err(reason) = cors.validate()
    {
        return Err(OagwError::validation_field_error("cors", reason));
    }
    validate_plugin_set(&u.plugins)?;
    Ok(())
}

fn validate_auth(auth: &AuthConfig) -> Result<(), OagwError> {
    let Some(id) = auth.auth_type.as_deref() else {
        return Err(OagwError::validation_field_error(
            "auth.type",
            "an auth plugin identifier is required when auth is configured",
        ));
    };
    if model::builtin_plugins::is_resolvable_auth(id) {
        return Ok(());
    }
    Err(OagwError::validation_field_error(
        "auth.type",
        format!("unknown auth plugin '{id}'"),
    ))
}

/// Validate a rate-limit block.
///
/// # Errors
/// Returns the first violated rule.
pub fn validate_rate_limit(rl: &RateLimit) -> Result<(), OagwError> {
    if rl.sustained.rate == 0 {
        return Err(OagwError::validation_field_error(
            "rate_limit.sustained.rate",
            "rate must be at least 1",
        ));
    }
    if let Some(burst) = &rl.burst
        && burst.capacity == 0
    {
        return Err(OagwError::validation_field_error(
            "rate_limit.burst.capacity",
            "capacity must be at least 1",
        ));
    }
    if rl.cost == 0 {
        return Err(OagwError::validation_field_error(
            "rate_limit.cost",
            "cost must be at least 1",
        ));
    }
    Ok(())
}

fn validate_plugin_set(plugins: &PluginSet) -> Result<(), OagwError> {
    for binding in &plugins.items {
        let id = binding.id();
        let (kind, _) = split_plugin_kind(id).ok_or_else(|| {
            OagwError::validation_field_error(
                "plugins.items",
                format!("'{id}' is not an oagw plugin identifier"),
            )
        })?;
        match kind {
            "auth" => {}
            "guard" => {
                if model::builtin_plugins::is_catalog_only(id) {
                    return Err(OagwError::validation_field_error(
                        "plugins.items",
                        format!(
                            "guard plugin '{id}' is core Data Plane functionality and cannot be \
                             bound via plugins.items"
                        ),
                    ));
                }
            }
            "transform" => {
                if model::builtin_plugins::is_catalog_only(id) {
                    return Err(OagwError::validation_field_error(
                        "plugins.items",
                        format!(
                            "transform plugin '{id}' is core Data Plane instrumentation and \
                             cannot be bound via plugins.items"
                        ),
                    ));
                }
            }
            other => {
                return Err(OagwError::validation_field_error(
                    "plugins.items",
                    format!("'{id}' has unknown plugin type '{other}'"),
                ));
            }
        }
    }
    Ok(())
}

/// Split `gts.cf.core.oagw.<kind>_plugin.v1~<instance>` into its parts.
#[must_use]
pub fn split_plugin_kind(id: &str) -> Option<(&str, &str)> {
    let body = id.strip_prefix("gts.cf.core.oagw.")?;
    let (head, instance) = body.split_once('~')?;
    let kind = head.strip_suffix("_plugin.v1")?;
    Some((kind, instance))
}

/// Validate a create/replace payload for a route.
///
/// # Errors
/// Returns the first violated rule.
pub fn validate_route(r: &Route) -> Result<(), OagwError> {
    match &r.matcher {
        RouteMatch {
            http: Some(http),
            grpc: None,
        } => validate_http_match(http),
        RouteMatch {
            grpc: Some(grpc),
            http: None,
        } => {
            if grpc.service.is_empty() || grpc.method.is_empty() {
                return Err(OagwError::validation_field_error(
                    "match.grpc",
                    "both service and method are required for a gRPC route",
                ));
            }
            Ok(())
        }
        _ => Err(OagwError::validation_field_error(
            "match",
            "exactly one of match.http or match.grpc must be present",
        )),
    }
}

fn validate_http_match(http: &HttpMatch) -> Result<(), OagwError> {
    if http.methods.is_empty() {
        return Err(OagwError::validation_field_error(
            "match.http.methods",
            "at least one method is required",
        ));
    }
    const METHODS: [&str; 5] = ["GET", "POST", "PUT", "DELETE", "PATCH"];
    for m in &http.methods {
        if !METHODS
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(m))
        {
            return Err(OagwError::validation_field_error(
                "match.http.methods",
                format!("method '{m}' is not one of GET, POST, PUT, DELETE, PATCH"),
            ));
        }
    }
    if http.path.is_empty() {
        return Err(OagwError::validation_field_error(
            "match.http.path",
            "a path pattern is required",
        ));
    }
    if !http.path.starts_with('/') {
        return Err(OagwError::validation_field_error(
            "match.http.path",
            format!("path '{}' must start with '/'", http.path),
        ));
    }
    Ok(())
}

/// Union tags across the ancestor chain, descendant last.
#[must_use]
pub fn union_tags(chains: &[Vec<String>]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for tags in chains {
        for tag in tags {
            if !out.contains(tag) {
                out.push(tag.clone());
            }
        }
    }
    out
}

/// Merge two plugin chains: ancestor items first, descendant appended.
#[must_use]
pub fn merge_plugins(ancestor: &PluginSet, descendant: &PluginSet) -> PluginSet {
    let mut items = ancestor.items.clone();
    items.extend(descendant.items.clone());
    PluginSet {
        sharing: SharingMode::Private,
        items,
    }
}

/// Merge two rate limits: the stricter (lower) sustained rate wins.
#[must_use]
pub fn merge_rate_limits(ancestor: &RateLimit, descendant: &RateLimit) -> RateLimit {
    let mut merged = descendant.clone();
    if ancestor.sustained.rate < descendant.sustained.rate {
        merged.sustained = ancestor.sustained.clone();
    }
    let anc_cap = ancestor.capacity();
    if anc_cap < merged.capacity() {
        merged.burst = Some(model::Burst { capacity: anc_cap });
    }
    merged
}

/// The alias a `GET /proxy/{alias}/…` request names, normalized.
#[must_use]
pub fn proxy_alias(alias: &str) -> String {
    model::normalize_alias(alias)
}

/// Validate an endpoint pool shared by create and replace.
///
/// # Errors
/// Returns the first violated rule.
pub fn validate_endpoints(endpoints: &[Endpoint]) -> Result<(), OagwError> {
    let probe = Upstream {
        server: model::ServerConfig {
            endpoints: endpoints.to_vec(),
        },
        ..Upstream::default()
    };
    validate_upstream(&probe)
}

/// `true` when `sharing` lets a descendant see the configuration.
#[must_use]
pub fn is_visible(sharing: SharingMode) -> bool {
    matches!(sharing, SharingMode::Inherit | SharingMode::Enforce)
}

/// `true` when `sharing` prevents a descendant from overriding.
#[must_use]
pub fn is_enforced(sharing: SharingMode) -> bool {
    sharing == SharingMode::Enforce
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn endpoint(scheme: &str, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme: scheme.to_owned(),
            host: host.to_owned(),
            port,
        }
    }

    fn upstream(endpoints: Vec<Endpoint>) -> Upstream {
        Upstream {
            server: model::ServerConfig { endpoints },
            ..Upstream::default()
        }
    }

    #[test]
    fn an_empty_endpoint_pool_is_rejected() {
        assert!(validate_upstream(&upstream(vec![])).is_err());
    }

    #[test]
    fn http_is_a_legal_scheme_at_create_time() {
        let u = upstream(vec![endpoint("http", "mock.internal", 80)]);
        assert!(validate_upstream(&u).is_ok());
    }

    #[test]
    fn an_unknown_scheme_is_rejected() {
        let u = upstream(vec![endpoint("ftp", "mock.internal", 21)]);
        let err = validate_upstream(&u).unwrap_err();
        assert_eq!(err.status_value(), 400);
        assert!(err.detail().contains("scheme"), "{}", err.detail());
    }

    #[test]
    fn heterogeneous_pools_are_rejected() {
        let u = upstream(vec![
            endpoint("https", "a.vendor.com", 443),
            endpoint("https", "b.vendor.com", 8443),
        ]);
        assert!(validate_upstream(&u).is_err());
        let u = upstream(vec![
            endpoint("https", "a.vendor.com", 443),
            endpoint("wss", "b.vendor.com", 443),
        ]);
        assert!(validate_upstream(&u).is_err());
    }

    #[test]
    fn a_zero_port_is_rejected() {
        let body = serde_json::json!({
            "server": { "endpoints": [ { "scheme": "https", "host": "h", "port": 0 } ] },
            "protocol": model::protocol::HTTP,
        });
        let u: Upstream = serde_json::from_value(body).expect("parses");
        assert!(validate_upstream(&u).is_err());
    }

    #[test]
    fn an_unknown_protocol_is_rejected() {
        let mut u = upstream(vec![endpoint("https", "h", 443)]);
        u.protocol = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.ftp.v1".to_owned();
        assert!(validate_upstream(&u).is_err());
    }

    #[test]
    fn catalog_only_auth_plugins_are_rejected() {
        let mut u = upstream(vec![endpoint("https", "h", 443)]);
        u.auth = Some(model::AuthConfig {
            auth_type: Some(model::builtin_plugins::AUTH_BEARER.to_owned()),
            ..model::AuthConfig::default()
        });
        let err = validate_upstream(&u).unwrap_err();
        assert_eq!(err.status_value(), 400);
        assert!(
            err.detail().contains("unknown auth plugin"),
            "{}",
            err.detail()
        );
    }

    #[test]
    fn core_guard_plugins_are_not_bindable() {
        let mut u = upstream(vec![endpoint("https", "h", 443)]);
        u.plugins.items = vec![model::PluginBinding::Ref(
            model::builtin_plugins::GUARD_CORS.to_owned(),
        )];
        let err = validate_upstream(&u).unwrap_err();
        assert_eq!(err.status_value(), 400);
        assert!(err.detail().contains("core Data Plane"), "{}", err.detail());
    }

    #[test]
    fn route_validation_enforces_the_oneof() {
        let mut r = Route::default();
        assert!(validate_route(&r).is_err());
        r.matcher.http = Some(HttpMatch::default());
        r.matcher.grpc = Some(model::GrpcMatch {
            service: "svc.v1.S".to_owned(),
            method: "Get".to_owned(),
        });
        assert!(validate_route(&r).is_err());
        r.matcher.grpc = None;
        assert!(validate_route(&r).is_err(), "empty methods");
        r.matcher.http = Some(HttpMatch {
            methods: vec!["GET".to_owned()],
            path: "/v1".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: model::PathSuffixMode::Append,
        });
        assert!(validate_route(&r).is_ok());
    }

    #[test]
    fn route_methods_are_constrained() {
        let mut http = HttpMatch {
            methods: vec!["TRACE".to_owned()],
            path: "/v1".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: model::PathSuffixMode::Append,
        };
        let r = Route {
            matcher: RouteMatch {
                http: Some(http.clone()),
                grpc: None,
            },
            ..Route::default()
        };
        assert!(validate_route(&r).is_err());
        http.methods = vec!["PATCH".to_owned()];
        let r = Route {
            matcher: RouteMatch {
                http: Some(http),
                grpc: None,
            },
            ..Route::default()
        };
        assert!(validate_route(&r).is_ok());
    }

    #[test]
    fn plugin_identifiers_split_into_kind_and_instance() {
        assert_eq!(
            split_plugin_kind("gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"),
            Some(("guard", "cf.core.oagw.required_headers.v1"))
        );
        assert_eq!(
            split_plugin_kind("gts.cf.core.oagw.auth_plugin.v1~7f3c"),
            Some(("auth", "7f3c"))
        );
        assert_eq!(split_plugin_kind("gts.cf.core.upstream.v1~x"), None);
    }

    #[test]
    fn rate_limits_merge_to_the_stricter_side() {
        let ancestor = RateLimit {
            sustained: model::SustainedRate {
                rate: 10_000,
                window: model::RateWindow::Minute,
            },
            burst: Some(model::Burst { capacity: 10_000 }),
            ..RateLimit::default()
        };
        let descendant = RateLimit {
            sustained: model::SustainedRate {
                rate: 100,
                window: model::RateWindow::Minute,
            },
            ..RateLimit::default()
        };
        let merged = merge_rate_limits(&ancestor, &descendant);
        assert_eq!(merged.sustained.rate, 100);
        assert_eq!(merged.capacity(), 100);
    }

    #[test]
    fn tags_union_additively() {
        let merged = union_tags(&[
            vec!["openai".to_owned(), "llm".to_owned()],
            vec!["llm".to_owned(), "beta".to_owned()],
        ]);
        assert_eq!(merged, vec!["openai", "llm", "beta"]);
    }

    #[test]
    fn sharing_modes_classify_visibility() {
        assert!(!is_visible(SharingMode::Private));
        assert!(is_visible(SharingMode::Inherit));
        assert!(is_visible(SharingMode::Enforce));
        assert!(!is_enforced(SharingMode::Inherit));
        assert!(is_enforced(SharingMode::Enforce));
    }
}
