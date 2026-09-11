//! `Route` aggregate and its match configuration.
//!
//! Mirrors `schemas/route.v1.schema.json` property for property, plus the
//! §1.5 additions the shipped schema omits: the route-level `cors` object and
//! the `priority` and `enabled` attributes of the DESIGN §3.1 Route class.
//! Layering: no transport or persistence type appears here.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::error::ModelError;
use crate::domain::upstream::{CorsConfig, PluginsConfig, RateLimitConfig};

/// How the proxy URL's `/{path_suffix}` is treated.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathSuffixMode {
    /// Rejects path suffix usage.
    Disabled,
    /// Appends it to `path`.
    Append,
}

/// HTTP match rules, used when the upstream protocol is HTTP.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// HTTP methods supported by this route.
    pub methods: Vec<String>,
    /// Path pattern for the route.
    pub path: String,
    /// Allow-listed query parameters. If empty, allow none.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// How to treat `/{path_suffix}` from the proxy URL.
    #[serde(default)]
    pub path_suffix_mode: Option<PathSuffixMode>,
}

/// gRPC match rules, used when the upstream protocol is gRPC.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully qualified gRPC service name, e.g. `foo.v1.UserService`.
    pub service: String,
    /// RPC method name, e.g. `GetUser`.
    pub method: String,
}

/// Protocol-scoped inbound matching rules. Exactly one of `http`/`grpc` must
/// be present.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MatchConfig {
    /// HTTP match rules.
    #[serde(default)]
    pub http: Option<HttpMatch>,
    /// gRPC match rules.
    #[serde(default)]
    pub grpc: Option<GrpcMatch>,
}

impl MatchConfig {
    /// Validates the `oneOf` the schema states over `http` and `grpc`.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::AmbiguousMatch`] when neither or both of `http`
    /// and `grpc` are present.
    pub fn validate(&self) -> Result<(), ModelError> {
        match (self.http.is_some(), self.grpc.is_some()) {
            (true, false) | (false, true) => Ok(()),
            _ => Err(ModelError::AmbiguousMatch),
        }
    }
}

/// The `Route` aggregate: belongs to an upstream and defines match rules plus
/// the route-level overrides.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Route {
    /// System-generated unique identifier.
    pub id: Uuid,
    /// Reference to the upstream service for this route; required by the
    /// schema.
    pub upstream_id: Uuid,
    /// Protocol-scoped inbound matching rules; required by the schema. The
    /// wire key stays `match`.
    #[serde(rename = "match")]
    pub match_config: MatchConfig,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    /// Rate limiting configuration.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    /// Flat tags for categorization and discovery.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Route-level CORS configuration, added per §1.5.
    #[serde(default)]
    pub cors: Option<CorsConfig>,
    /// Match-uniqueness ordering, added per §1.5.
    #[serde(default)]
    pub priority: Option<i64>,
    /// Enable/disable semantics, added per §1.5.
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl Route {
    /// Validates the structural invariants the shipped schema states.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::AmbiguousMatch`] when `match` does not carry
    /// exactly one of `http` or `grpc`.
    pub fn validate(&self) -> Result<(), ModelError> {
        self.match_config.validate()
    }
}
