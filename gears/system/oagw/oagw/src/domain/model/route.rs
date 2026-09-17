//! Route domain model (`gts.cf.core.oagw.route.v1~`).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{PluginChain, RateLimitConfig};

/// HTTP method accepted by an HTTP route match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum HttpMethod {
    /// `GET`
    #[serde(rename = "GET")]
    Get,
    /// `POST`
    #[serde(rename = "POST")]
    Post,
    /// `PUT`
    #[serde(rename = "PUT")]
    Put,
    /// `DELETE`
    #[serde(rename = "DELETE")]
    Delete,
    /// `PATCH`
    #[serde(rename = "PATCH")]
    Patch,
}

impl HttpMethod {
    /// Uppercase wire representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Delete => "DELETE",
            Self::Patch => "PATCH",
        }
    }

    /// Parse an upper-case HTTP method name.
    #[must_use]
    pub fn from_str_ci(value: &str) -> Option<Self> {
        match value.to_ascii_uppercase().as_str() {
            "GET" => Some(Self::Get),
            "POST" => Some(Self::Post),
            "PUT" => Some(Self::Put),
            "DELETE" => Some(Self::Delete),
            "PATCH" => Some(Self::Patch),
            _ => None,
        }
    }
}

impl std::fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How the `{path_suffix}` captured from the proxy URL is treated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathSuffixMode {
    /// Reject any request that carries a path suffix.
    Disabled,
    /// Append the suffix to `match.http.path`.
    #[default]
    Append,
}

/// HTTP matching rules (`match.http`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct HttpMatch {
    /// Methods this route serves; at least one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<HttpMethod>,
    /// Path pattern.
    #[serde(default)]
    pub path: String,
    /// Allowed query parameters; an empty list allows none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub query_allowlist: Vec<String>,
    /// How the captured `{path_suffix}` is handled.
    #[serde(skip_serializing_if = "path_suffix_mode_is_default")]
    pub path_suffix_mode: PathSuffixMode,
}

impl PathSuffixMode {
    /// True for [`PathSuffixMode::Append`] (the default).
    #[must_use]
    pub fn is_default(self) -> bool {
        self == Self::default()
    }
}

/// `skip_serializing_if` helper for [`PathSuffixMode`].
fn path_suffix_mode_is_default(mode: &PathSuffixMode) -> bool {
    mode.is_default()
}

/// gRPC matching rules (`match.grpc`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct GrpcMatch {
    /// Fully qualified gRPC service name (e.g. `foo.v1.UserService`).
    #[serde(default)]
    pub service: String,
    /// RPC method name (e.g. `GetUser`).
    #[serde(default)]
    pub method: String,
}

/// Protocol-scoped inbound matching rules. Exactly one of `http`/`grpc`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteMatch {
    /// HTTP request matching.
    Http(HttpMatch),
    /// gRPC request matching.
    Grpc(GrpcMatch),
}

impl Default for RouteMatch {
    fn default() -> Self {
        Self::Http(HttpMatch::default())
    }
}

impl RouteMatch {
    /// True when this match is an HTTP match.
    #[must_use]
    pub fn is_http(&self) -> bool {
        matches!(self, Self::Http(_))
    }
}

/// Route (`gts.cf.core.oagw.route.v1~`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Route {
    /// Server-generated identifier (UUID v4).
    pub id: Uuid,
    /// Owning tenant (internal; never serialised to the management API).
    #[serde(skip)]
    pub tenant_id: Uuid,
    /// Whether this route participates in request matching.
    pub enabled: bool,
    /// Flat tags for categorisation and discovery.
    pub tags: Vec<String>,
    /// Referenced upstream; immutable after creation.
    pub upstream_id: Uuid,
    /// Protocol-scoped matching rules.
    #[serde(rename = "match")]
    pub match_config: RouteMatch,
    /// Route-level plugin chain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginChain>,
    /// Route-level rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
}

impl Default for Route {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            enabled: true,
            tags: Vec::new(),
            upstream_id: Uuid::nil(),
            match_config: RouteMatch::Http(HttpMatch::default()),
            plugins: None,
            rate_limit: None,
        }
    }
}

impl Route {
    /// GTS instance id of this route.
    #[must_use]
    pub fn gts_id(&self) -> String {
        crate::domain::gts_helpers::gts_instance_id(
            crate::domain::gts_helpers::OAGW_ROUTE_TYPE_ID,
            &self.id,
        )
    }

    /// Canonical, human-readable description of this route's match, used for
    /// conflict diagnostics.
    #[must_use]
    pub fn match_key(&self) -> String {
        match &self.match_config {
            RouteMatch::Http(http) => format!(
                "http {} {}",
                http.methods
                    .iter()
                    .map(|m| m.as_str())
                    .collect::<Vec<_>>()
                    .join(","),
                http.path
            ),
            RouteMatch::Grpc(grpc) => format!("grpc {}/{}", grpc.service, grpc.method),
        }
    }
}
