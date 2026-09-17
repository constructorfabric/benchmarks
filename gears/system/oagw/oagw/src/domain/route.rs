// Created: 2026-09-04 by Constructor Tech
//! Route aggregate and its matching value objects.
//!
//! Mirrors `docs/schemas/route.v1.schema.json` and the `Route` /
//! `RouteMatch` / `HttpMatch` classes of `docs/DESIGN.md` §3.1. A route binds
//! an inbound prefix (or gRPC method) to an upstream and carries the
//! route-level slice of the hierarchical configuration.

use toolkit_gts::gts_id;

use crate::domain::upstream::HttpMethod;
use crate::error::OagwError;

/// GTS type of the route resource.
pub const ROUTE_GTS_TYPE: &str = gts_id!("cf.core.oagw.route.v1~");

/// How the inbound path suffix is forwarded to the upstream
/// (`docs/schemas/route.v1.schema.json` `path_suffix_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PathSuffixMode {
    /// Append everything following the matched prefix (default).
    #[default]
    Append,
    /// Never forward a suffix.
    Disabled,
}

impl PathSuffixMode {
    /// `true` when a suffix following the matched prefix is forwarded.
    #[must_use]
    pub const fn accepts_suffix(self) -> bool {
        matches!(self, Self::Append)
    }

    /// Parses a mode token.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] for an unknown token.
    pub fn parse(raw: &str) -> Result<Self, OagwError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "append" => Ok(Self::Append),
            "disabled" => Ok(Self::Disabled),
            _ => Err(OagwError::Validation {
                detail: format!(
                    "unknown path_suffix_mode '{raw}' (expected 'append' or 'disabled')"
                ),
            }),
        }
    }
}

/// gRPC match keys (`docs/schemas/route.v1.schema.json` `grpc_match`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrpcMatch {
    /// Fully qualified protobuf service name (non-empty).
    pub service: String,
    /// RPC method name (non-empty).
    pub method: String,
}

impl GrpcMatch {
    /// Builds a gRPC match.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when `service` or `method` is empty.
    pub fn new(service: String, method: String) -> Result<Self, OagwError> {
        if service.trim().is_empty() || method.trim().is_empty() {
            return Err(OagwError::Validation {
                detail: String::from("grpc_match requires both a service and a method"),
            });
        }
        Ok(Self { service, method })
    }
}

/// HTTP match keys (`docs/schemas/route.v1.schema.json` `http_match`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpMatch {
    /// Methods the route accepts (minimum one).
    pub methods: Vec<HttpMethod>,
    /// Inbound path prefix (minimum one character).
    pub path: String,
    /// Query parameters forwarded to the upstream; empty allows none.
    pub query_allowlist: Vec<String>,
    /// How the path suffix is forwarded.
    pub path_suffix_mode: PathSuffixMode,
}

impl HttpMatch {
    /// Builds an HTTP match.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when `methods` is empty or holds a
    /// method outside the schema enum, when `path` is empty, or when the path
    /// does not start with `/`.
    pub fn new(
        methods: Vec<HttpMethod>,
        path: String,
        query_allowlist: Vec<String>,
        path_suffix_mode: PathSuffixMode,
    ) -> Result<Self, OagwError> {
        if methods.is_empty() {
            return Err(OagwError::Validation {
                detail: String::from(
                    "at least one HTTP method is required (http_match.methods.minItems = 1)",
                ),
            });
        }
        for method in &methods {
            if !method.is_route_match_method() {
                return Err(OagwError::Validation {
                    detail: format!(
                        "method '{method}' cannot be matched (use GET, POST, PUT, PATCH or DELETE)"
                    ),
                });
            }
        }
        if path.is_empty() {
            return Err(OagwError::Validation {
                detail: String::from("http_match.path must not be empty"),
            });
        }
        if !path.starts_with('/') {
            return Err(OagwError::Validation {
                detail: format!("http_match.path '{path}' must start with '/'"),
            });
        }
        Ok(Self {
            methods,
            path,
            query_allowlist,
            path_suffix_mode,
        })
    }

    /// `true` when `method` is listed in the match.
    #[must_use]
    pub fn allows_method(&self, method: HttpMethod) -> bool {
        self.methods.contains(&method)
    }

    /// `true` when `name` may be forwarded to the upstream. An empty
    /// allowlist allows no query parameter at all
    /// (`docs/PRD.md` §5.5 `query_allowlist`).
    #[must_use]
    pub fn allows_query_param(&self, name: &str) -> bool {
        self.query_allowlist.iter().any(|allowed| allowed == name)
    }

    /// Splits an inbound path into the matched prefix and the forwarded
    /// suffix, honouring `path_suffix_mode`.
    ///
    /// Returns `None` when the path does not start with the prefix.
    #[must_use]
    pub fn split_path<'a>(&'a self, path: &'a str) -> Option<(&'a str, &'a str)> {
        let suffix = strip_prefix(&self.path, path)?;
        if self.path_suffix_mode.accepts_suffix() {
            Some((self.path.as_str(), suffix))
        } else {
            Some((self.path.as_str(), ""))
        }
    }

    /// Path forwarded to the upstream for `path`.
    ///
    /// * `path_suffix_mode: append` — the suffix following the matched
    ///   prefix, or `/` when there is none;
    /// * `path_suffix_mode: disabled` — always `/`.
    #[must_use]
    pub fn proxy_path(&self, path: &str) -> Option<String> {
        let suffix = strip_prefix(&self.path, path)?;
        Some(if self.path_suffix_mode.accepts_suffix() {
            String::from(suffix)
        } else {
            String::from("/")
        })
    }

    /// `true` when `path` starts with the matched prefix at a segment
    /// boundary and `method` is listed.
    #[must_use]
    pub fn matches(&self, path: &str, method: HttpMethod) -> bool {
        strip_prefix(&self.path, path).is_some() && self.allows_method(method)
    }
}

/// Splits `path` at a segment boundary after `prefix`.
///
/// A prefix `/v1` matches `/v1` and `/v1/users`, but not `/v10`. The returned
/// suffix keeps its leading slash and is `/` when the path equals the prefix.
#[must_use]
fn strip_prefix<'a>(prefix: &'a str, path: &'a str) -> Option<&'a str> {
    if path.len() < prefix.len() || !path.starts_with(prefix) {
        return None;
    }
    let boundary = path.as_bytes().get(prefix.len());
    match boundary {
        None => Some("/"),
        Some(b'/') => Some(&path[prefix.len()..]),
        Some(_) => None,
    }
}

/// Match keys of a route (`docs/DESIGN.md` §3.1 `RouteMatch`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteMatch {
    /// HTTP path prefix match.
    Http(HttpMatch),
    /// gRPC service/method match.
    Grpc(GrpcMatch),
}

impl RouteMatch {
    /// Match specificity: the longer the matched prefix (or the more
    /// qualified the gRPC key), the higher the weight
    /// (`docs/PRD.md` §5.5: the most specific route wins).
    #[must_use]
    pub fn specificity(&self) -> usize {
        match self {
            Self::Http(http) => http.path.len(),
            Self::Grpc(grpc) => grpc.service.len() + grpc.method.len(),
        }
    }

    /// `true` for an HTTP (as opposed to gRPC) match.
    #[must_use]
    pub const fn is_http(&self) -> bool {
        matches!(self, Self::Http(_))
    }
}

/// Inputs of a new [`Route`], validated by [`Route::new`].
#[derive(Debug, Clone, PartialEq)]
pub struct RouteSpec {
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Upstream the route forwards to.
    pub upstream_id: uuid::Uuid,
    /// Match keys.
    pub r#match: RouteMatch,
    /// Plugins executed before the upstream's chain.
    pub plugins: Option<crate::domain::upstream::PluginChain>,
    /// Route-level rate limit (overriding or tightening the upstream's).
    pub rate_limit: Option<crate::domain::upstream::RateLimitConfig>,
    /// Route-level CORS configuration.
    pub cors: Option<crate::domain::upstream::CorsConfig>,
    /// Whether the route participates in matching
    /// (`docs/PRD.md` `cpt-cf-oagw-fr-enable-disable`).
    pub enabled: bool,
    /// Flat discovery tags.
    pub tags: Vec<String>,
}

/// An inbound routing rule bound to exactly one upstream
/// (`docs/DESIGN.md` §3.1 `Route`, GTS type `gts.cf.core.oagw.route.v1~`).
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    /// System-generated identifier.
    pub id: uuid::Uuid,
    /// Owning tenant.
    pub tenant_id: uuid::Uuid,
    /// Upstream the route forwards to.
    pub upstream_id: uuid::Uuid,
    /// Match keys.
    pub r#match: RouteMatch,
    /// Route-level plugin chain.
    pub plugins: Option<crate::domain::upstream::PluginChain>,
    /// Route-level rate limit configuration.
    pub rate_limit: Option<crate::domain::upstream::RateLimitConfig>,
    /// Route-level CORS configuration.
    pub cors: Option<crate::domain::upstream::CorsConfig>,
    /// Whether the route participates in matching. A disabled route keeps its
    /// whole configuration and is simply skipped by the matcher
    /// (`docs/PRD.md` `cpt-cf-oagw-fr-enable-disable`).
    pub enabled: bool,
    /// Flat discovery tags.
    pub tags: Vec<String>,
}

impl Route {
    /// Assembles a route, enforcing the match and tag invariants.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::Validation`] when the match keys are incomplete
    /// or a tag does not match `^[a-z0-9_-]+$`.
    pub fn new(id: uuid::Uuid, spec: &RouteSpec) -> Result<Self, OagwError> {
        crate::domain::upstream::validate_tags(&spec.tags)?;
        if let Some(cors) = &spec.cors {
            cors.validate()?;
        }
        Ok(Self {
            id,
            tenant_id: spec.tenant_id,
            upstream_id: spec.upstream_id,
            r#match: spec.r#match.clone(),
            plugins: spec.plugins.clone(),
            rate_limit: spec.rate_limit.clone(),
            cors: spec.cors.clone(),
            enabled: spec.enabled,
            tags: spec.tags.clone(),
        })
    }

    /// `true` when the route participates in matching.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// HTTP match keys, when the route is an HTTP route.
    #[must_use]
    pub const fn http_match(&self) -> Option<&HttpMatch> {
        match &self.r#match {
            RouteMatch::Http(http) => Some(http),
            RouteMatch::Grpc(_) => None,
        }
    }

    /// `true` when the route accepts `path` for `method`
    /// (gRPC routes never match an HTTP request).
    #[must_use]
    pub fn matches(&self, path: &str, method: HttpMethod) -> bool {
        match &self.r#match {
            RouteMatch::Http(http) => http.matches(path, method),
            RouteMatch::Grpc(_) => false,
        }
    }

    /// `true` when `name` is in the route's `query_allowlist`.
    #[must_use]
    pub fn allows_query_param(&self, name: &str) -> bool {
        match &self.r#match {
            RouteMatch::Http(http) => http.allows_query_param(name),
            RouteMatch::Grpc(_) => false,
        }
    }

    /// Path forwarded to the upstream for `path`.
    #[must_use]
    pub fn proxy_path(&self, path: &str) -> Option<String> {
        match &self.r#match {
            RouteMatch::Http(http) => http.proxy_path(path),
            RouteMatch::Grpc(_) => None,
        }
    }

    /// Match specificity used to pick the winning route.
    #[must_use]
    pub fn specificity(&self) -> usize {
        self.r#match.specificity()
    }
}

/// Picks the most specific route matching `path` and `method`
/// (`docs/PRD.md` §5.5): the longest path prefix wins, and the first
/// registered route wins a tie.
#[must_use]
pub fn best_route_match<'a>(
    routes: &'a [Route],
    path: &str,
    method: HttpMethod,
) -> Option<&'a Route> {
    let mut best: Option<&Route> = None;
    for route in routes {
        if !route.matches(path, method) {
            continue;
        }
        if best.is_none_or(|current| route.specificity() > current.specificity()) {
            best = Some(route);
        }
    }
    best
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
#[path = "route_tests.rs"]
mod route_tests;
