//! The `Route` entity: a matching rule bound to an upstream.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{ErrorKind, OagwError};

/// Identifier prefix of the route GTS type.
pub const ROUTE_TYPE: &str = "gts.cf.core.oagw.route.v1";

/// HTTP methods a route may declare.
pub const METHODS: [&str; 5] = ["GET", "POST", "PUT", "DELETE", "PATCH"];

/// Builds a full GTS identifier for a route.
#[must_use]
pub fn route_id(id: Uuid) -> String {
    format!("{ROUTE_TYPE}~{id}")
}

/// Whether the trailing path suffix from the proxy URL is appended or rejected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// The suffix after the matched path is appended to the forwarded path.
    #[default]
    Append,
    /// Any suffix is a validation error.
    Disabled,
}

/// HTTP matching rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// Methods the route answers; at least one.
    #[serde(default)]
    pub methods: Vec<String>,
    /// Path prefix the route matches.
    #[serde(default)]
    pub path: String,
    /// Query parameters the caller may send; empty means unrestricted.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// Whether the trailing path suffix is appended.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

/// gRPC matching rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully-qualified gRPC service name.
    pub service: String,
    /// gRPC method name.
    pub method: String,
}

/// A route's match rule: HTTP or gRPC, never both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields, rename_all = "lowercase")]
pub enum RouteMatch {
    /// HTTP method/path matching.
    Http(HttpMatch),
    /// gRPC service/method matching.
    Grpc(GrpcMatch),
}

impl RouteMatch {
    /// The HTTP match, when this is an HTTP match.
    #[must_use]
    pub const fn as_http(&self) -> Option<&HttpMatch> {
        match self {
            Self::Http(m) => Some(m),
            Self::Grpc(_) => None,
        }
    }

    /// The gRPC match, when this is a gRPC match.
    #[must_use]
    pub const fn as_grpc(&self) -> Option<&GrpcMatch> {
        match self {
            Self::Grpc(m) => Some(m),
            Self::Http(_) => None,
        }
    }

}

impl RouteMatch {
    /// The path the HTTP match binds.
    #[must_use]
    pub fn http_path(&self) -> Option<&str> {
        self.as_http().map(|m| m.path.as_str())
    }
}

/// The `Route` entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// Server-generated GTS identifier.
    #[serde(default)]
    pub id: String,
    /// Owning tenant.
    #[serde(default)]
    pub tenant_id: Uuid,
    /// The upstream this route belongs to; immutable across replacement.
    #[serde(default)]
    pub upstream_id: String,
    /// The match rule.
    #[serde(default)]
    pub r#match: Option<RouteMatch>,
    /// Ordering key participating in the conflict rule.
    #[serde(default)]
    pub priority: i64,
    /// Whether the route participates in matching.
    #[serde(default = "crate::domain::default_enabled")]
    pub enabled: bool,
    /// Rate-limit override.
    #[serde(default)]
    pub rate_limit: Option<crate::domain::upstream::RateLimit>,
    /// Ordered plugin bindings.
    #[serde(default)]
    pub plugins: Vec<crate::domain::plugin::PluginBinding>,
    /// Tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

impl Default for Route {
    fn default() -> Self {
        Self {
            id: String::new(),
            tenant_id: Uuid::nil(),
            upstream_id: String::new(),
            r#match: None,
            priority: 0,
            enabled: true,
            rate_limit: None,
            plugins: Vec::new(),
            tags: Vec::new(),
        }
    }
}

impl Route {
    /// The HTTP match of this route, when it has one.
    #[must_use]
    pub fn http_match(&self) -> Option<&HttpMatch> {
        self.r#match.as_ref().and_then(RouteMatch::as_http)
    }

    /// The first method this route accepts, used for the conflict key.
    #[must_use]
    pub fn primary_method(&self) -> Option<&str> {
        self.http_match()
            .and_then(|m| m.methods.first())
            .map(String::as_str)
    }

    /// Validates the route.
    ///
    /// # Errors
    ///
    /// Returns an [`OagwError`] describing the first violation.
    pub fn validate(&self) -> Result<(), OagwError> {
        let Some(m) = &self.r#match else {
            return Err(OagwError::new(
                ErrorKind::ValidationError,
                "route.match is required",
            ));
        };
        match m {
            RouteMatch::Http(http) => {
                if http.methods.is_empty() {
                    return Err(OagwError::new(
                        ErrorKind::ValidationError,
                        "route.match.http.methods must contain at least one method",
                    ));
                }
                for method in &http.methods {
                    if !METHODS.contains(&method.as_str()) {
                        return Err(OagwError::new(
                            ErrorKind::ValidationError,
                            format!("route method `{method}` is not supported"),
                        ));
                    }
                }
                if http.path.is_empty() {
                    return Err(OagwError::new(
                        ErrorKind::ValidationError,
                        "route.match.http.path must not be empty",
                    ));
                }
                if !http.path.starts_with('/') {
                    return Err(OagwError::new(
                        ErrorKind::ValidationError,
                        "route.match.http.path must start with `/`",
                    ));
                }
            }
            RouteMatch::Grpc(grpc) => {
                if grpc.service.is_empty() || grpc.method.is_empty() {
                    return Err(OagwError::new(
                        ErrorKind::ValidationError,
                        "route.match.grpc requires both service and method",
                    ));
                }
            }
        }
        for tag in &self.tags {
            crate::domain::alias::validate_tag(tag)?;
        }
        crate::domain::plugin::validate_bindings(
            &self.plugins,
            crate::domain::plugin::Stage::Route,
        )
    }
}

#[cfg(test)]
#[path = "route_tests.rs"]
mod tests;
