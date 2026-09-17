//! `Route` entity and its match-rule value objects.
//!
//! Mirrors `oagw_route`, `oagw_route_http_match`, `oagw_route_grpc_match`, and
//! `oagw_route_method` (§3.7).  A route belongs to exactly one upstream
//! (`upstream_id` immutable), defines match rules, priority, and route-level
//! overrides for rate limits, CORS, and plugins.
//!
//! The gRPC match variant is schema-shaped but reserved: gRPC routing is not
//! served yet and any use is rejected at the repository boundary (DoD
//! `cpt-cf-oagw-dod-domain-model-repositories-grpc-reserved`).

use serde::{Deserialize, Serialize};
use std::time::SystemTime;
use uuid::Uuid;

use super::config::{CorsConfig, PluginsConfig, RateLimitConfig};

/// HTTP method allowlist member for a route.
///
/// Mirrors `oagw_route_method` (`(route_id, method)` rows); the upstream
/// schema constrains the allowlist to these five methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RouteMethod {
    #[serde(rename = "GET")]
    Get,
    #[serde(rename = "POST")]
    Post,
    #[serde(rename = "PUT")]
    Put,
    #[serde(rename = "DELETE")]
    Delete,
    #[serde(rename = "PATCH")]
    Patch,
}

impl RouteMethod {
    /// The HTTP method token as it appears on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
        }
    }
}

/// How the `/path_suffix` from the proxy URL is treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PathSuffixMode {
    /// Rejects `path_suffix` usage.
    Disabled,
    /// Appends the suffix to the route path.
    #[default]
    Append,
}

/// HTTP match rules (used when the upstream protocol is HTTP).
///
/// Mirrors `oagw_route_http_match` and `oagw_route_method`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HttpMatch {
    /// HTTP methods supported by this route (non-empty; empty allowlist
    /// rejects all methods).
    pub methods: Vec<RouteMethod>,
    /// Longest path-prefix match (the route's primary matching key).
    pub path_prefix: String,
    /// Allowed query parameter names; empty allows none.
    pub query_allowlist: Vec<String>,
    pub path_suffix_mode: PathSuffixMode,
}

impl Default for HttpMatch {
    fn default() -> Self {
        Self {
            methods: Vec::new(),
            path_prefix: String::new(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        }
    }
}

/// gRPC match rules — reserved for Phase 3.
///
/// The in-memory shape of `oagw_route_grpc_match` (`route_id`, `service`,
/// `method`) is reserved with **no gRPC proxy code path implemented or
/// reachable** (DoD `cpt-cf-oagw-dod-domain-model-repositories-grpc-reserved`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct GrpcMatch {
    /// Fully qualified gRPC service name (e.g. `foo.v1.UserService`).
    pub service: String,
    /// RPC method name (e.g. `GetUser`).
    pub method: String,
}

/// Protocol-scoped inbound matching rules for a route.
///
/// Exactly one of `http` | `grpc` is present (mirrors `oagw_route.match_type`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RouteMatch {
    Http(HttpMatch),
    /// Reserved (Phase 3).
    Grpc(GrpcMatch),
}

/// The `Route` entity — belongs to an upstream; base GTS type
/// `gts.cf.core.oagw.route.v1~*`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Route {
    /// Resource identifier.
    pub id: Uuid,
    /// Owning tenant.
    pub tenant_id: Uuid,
    /// Parent upstream (immutable once set).
    pub upstream_id: Uuid,
    /// `http` or `grpc` (grpc planned/Phase 3).
    pub match_type: MatchType,
    /// Route priority for tie-breaking.
    pub priority: i32,
    /// Route enabled flag.
    pub enabled: bool,
    #[serde(rename = "match")]
    pub match_: RouteMatch,
    pub rate_limit: Option<RateLimitConfig>,
    pub cors: Option<CorsConfig>,
    pub plugins: PluginsConfig,
    /// Discovery tags.
    pub tags: Vec<String>,
    pub created_at: Option<SystemTime>,
    pub updated_at: Option<SystemTime>,
}

/// Route match type mirroring `oagw_route.match_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MatchType {
    #[default]
    Http,
    /// Reserved (Phase 3).
    Grpc,
}

impl Default for Route {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            upstream_id: Uuid::nil(),
            match_type: MatchType::Http,
            priority: 0,
            enabled: true,
            match_: RouteMatch::Http(HttpMatch::default()),
            rate_limit: None,
            cors: None,
            plugins: PluginsConfig::default(),
            tags: Vec::new(),
            created_at: None,
            updated_at: None,
        }
    }
}

impl Route {
    /// Named constructor producing a canonical HTTP route.
    #[must_use]
    pub fn http(
        tenant_id: Uuid,
        upstream_id: Uuid,
        path_prefix: impl Into<String>,
        methods: Vec<RouteMethod>,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            tenant_id,
            upstream_id,
            match_type: MatchType::Http,
            match_: RouteMatch::Http(HttpMatch {
                methods,
                path_prefix: path_prefix.into(),
                ..HttpMatch::default()
            }),
            ..Self::default()
        }
    }

    /// Returns the HTTP method allowlist when this route is an HTTP route.
    #[must_use]
    pub fn methods(&self) -> Option<&[RouteMethod]> {
        match &self.match_ {
            RouteMatch::Http(m) => Some(&m.methods),
            RouteMatch::Grpc(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_method_wire_tokens() {
        assert_eq!(serde_json::to_string(&RouteMethod::Get).unwrap(), "\"GET\"");
        assert_eq!(
            serde_json::to_string(&RouteMethod::Patch).unwrap(),
            "\"PATCH\""
        );
        assert_eq!(RouteMethod::Post.as_str(), "POST");
    }

    #[test]
    fn route_http_constructor_sets_match() {
        let route = Route::http(
            Uuid::from_u128(1),
            Uuid::from_u128(2),
            "/v1/chat",
            vec![RouteMethod::Post],
        );
        assert_eq!(route.match_type, MatchType::Http);
        assert_eq!(route.methods(), Some(&[RouteMethod::Post][..]));
        let RouteMatch::Http(m) = &route.match_ else {
            panic!("expected http match");
        };
        assert_eq!(m.path_prefix, "/v1/chat");
        assert_eq!(m.path_suffix_mode, PathSuffixMode::Append); // default
    }

    #[test]
    fn grpc_match_shape_is_reserved() {
        // `RouteMatch` is `#[serde(untagged)]`: the match object is the bare
        // shape (`service`/`method`); no variant name wrapper is emitted.
        let json = serde_json::json!({
            "id": "11111111-1111-1111-1111-111111111111",
            "tenant_id": "22222222-2222-2222-2222-222222222222",
            "upstream_id": "33333333-3333-3333-3333-333333333333",
            "match_type": "grpc",
            "match": { "service": "foo.v1.UserService", "method": "GetUser" }
        });
        let route: Route = serde_json::from_value(json).unwrap();
        assert_eq!(route.match_type, MatchType::Grpc);
        assert!(matches!(route.match_, RouteMatch::Grpc(_)));
        assert_eq!(route.methods(), None);
    }

    #[test]
    fn route_deserializes_defaults() {
        let json = serde_json::json!({
            "id": "11111111-1111-1111-1111-111111111111",
            "tenant_id": "22222222-2222-2222-2222-222222222222",
            "upstream_id": "33333333-3333-3333-3333-333333333333",
            "match": {
                "methods": ["GET"],
                "path_prefix": "/v1",
                "query_allowlist": ["q"],
                "path_suffix_mode": "disabled"
            }
        });
        let route: Route = serde_json::from_value(json).unwrap();
        assert_eq!(route.match_type, MatchType::Http);
        assert!(route.enabled);
        let RouteMatch::Http(m) = &route.match_ else {
            panic!("expected http match");
        };
        assert_eq!(m.path_suffix_mode, PathSuffixMode::Disabled);
    }
}
