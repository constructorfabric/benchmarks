//! `Upstream` aggregate and its sub-configurations.
//!
//! Mirrors `schemas/upstream.v1.schema.json` property for property: no
//! missing, extra, or renamed field. `deny_unknown_fields` is applied only
//! where the schema sets `additionalProperties: false`. Layering: no
//! transport or persistence type appears here — statuses are `u16`, headers
//! are strings.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::domain::alias::EndpointHost;
use crate::domain::error::ModelError;
use crate::domain::scheme::Scheme;

/// Hierarchical sharing mode: `private`, `inherit`, or `enforce`.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharingMode {
    /// Not visible to descendants.
    Private,
    /// Descendants can override.
    Inherit,
    /// Descendants cannot override.
    Enforce,
}

/// Which inbound headers are forwarded to the upstream.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Passthrough {
    /// Forward no inbound header.
    None,
    /// Forward only the entries of `passthrough_allowlist`.
    Allowlist,
    /// Forward every inbound header.
    All,
}

/// Time window of a sustained rate.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Window {
    /// One second.
    Second,
    /// One minute.
    Minute,
    /// One hour.
    Hour,
    /// One day.
    Day,
}

/// Rate limiting algorithm.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Algorithm {
    /// Allows bursts.
    TokenBucket,
    /// Prevents boundary bursts.
    SlidingWindow,
}

/// Scope of the rate limit counters.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitScope {
    /// One counter per node.
    Global,
    /// One counter per tenant.
    Tenant,
    /// One counter per user.
    User,
    /// One counter per client IP.
    Ip,
    /// One counter per route.
    Route,
}

/// Behaviour when the limit is exceeded.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strategy {
    /// Reject the request.
    Reject,
    /// Queue the request.
    Queue,
    /// Degrade the response.
    Degrade,
}

/// One configured endpoint of an upstream service.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Endpoint scheme literal; see [`Scheme::is_write_admitted`].
    pub scheme: Scheme,
    /// Host name or IP literal of the upstream service.
    pub host: EndpointHost,
    /// Endpoint port. The shipped schema documents a default of `443`.
    #[serde(default)]
    pub port: Option<u16>,
}

/// The `server` configuration of an upstream: one or more endpoints.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// At least one endpoint.
    pub endpoints: Vec<Endpoint>,
}

impl ServerConfig {
    /// Validates the structural invariant the schema states as `minItems: 1`.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::NoEndpoints`] when no endpoint is configured.
    pub fn validate(&self) -> Result<(), ModelError> {
        if self.endpoints.is_empty() {
            return Err(ModelError::NoEndpoints);
        }
        Ok(())
    }
}

/// Authentication plugin binding of an upstream.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Authentication plugin type, as a GTS identifier.
    #[serde(rename = "type", default)]
    pub r#type: Option<String>,
    /// Sharing mode for hierarchical configuration.
    #[serde(default)]
    pub sharing: Option<SharingMode>,
    /// Authentication plugin configuration.
    #[serde(default)]
    pub config: Option<serde_json::Value>,
}

/// Request header transformation rules.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestHeaderRules {
    /// Headers to set (overwrite if exists).
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Headers to add (append, allow duplicates).
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Header names to remove from the inbound request.
    #[serde(default)]
    pub remove: Vec<String>,
    /// Which inbound headers to forward.
    #[serde(default)]
    pub passthrough: Option<Passthrough>,
    /// Headers to forward when `passthrough` is `allowlist`.
    #[serde(default)]
    pub passthrough_allowlist: Vec<String>,
}

/// Response header transformation rules. The shipped schema gives response
/// rules no passthrough fields.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeaderRules {
    /// Headers to set on the response to the client.
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    /// Headers to add to the response.
    #[serde(default)]
    pub add: BTreeMap<String, String>,
    /// Headers to strip from the upstream response.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Header transformation rules for requests and responses.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadersConfig {
    /// Rules applied to the inbound request.
    #[serde(default)]
    pub request: Option<RequestHeaderRules>,
    /// Rules applied to the response to the client.
    #[serde(default)]
    pub response: Option<ResponseHeaderRules>,
}

/// Tokens replenished per window.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sustained {
    /// Tokens replenished per window. At least 1.
    pub rate: u64,
    /// Time window of the sustained rate.
    #[serde(default)]
    pub window: Option<Window>,
}

/// Maximum burst size.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Burst {
    /// Bucket capacity. Defaults to `sustained.rate` when not specified.
    pub capacity: u64,
}

/// Rate limiting configuration.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Sharing mode for rate limits; `enforce` means descendants cannot
    /// exceed this limit.
    #[serde(default)]
    pub sharing: Option<SharingMode>,
    /// Rate limiting algorithm.
    #[serde(default)]
    pub algorithm: Option<Algorithm>,
    /// Sustained rate.
    #[serde(default)]
    pub sustained: Option<Sustained>,
    /// Burst ceiling.
    #[serde(default)]
    pub burst: Option<Burst>,
    /// Scope for the rate limit counters.
    #[serde(default)]
    pub scope: Option<RateLimitScope>,
    /// Behaviour when the limit is exceeded.
    #[serde(default)]
    pub strategy: Option<Strategy>,
    /// Tokens consumed per request. At least 1.
    #[serde(default)]
    pub cost: Option<u64>,
}

/// CORS configuration.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Sharing mode for the CORS configuration.
    #[serde(default)]
    pub sharing: Option<SharingMode>,
    /// Whether CORS is enabled.
    pub enabled: bool,
    /// Allowed origins; `["*"]` means any origin.
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Allowed HTTP methods.
    #[serde(default)]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond the CORS-safelisted ones.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// Whether credentials (cookies, auth headers) are allowed.
    #[serde(default)]
    pub allow_credentials: bool,
}

/// Plugin chain of an upstream or route. Each item is a built-in plugin GTS
/// identifier or a custom plugin UUID string.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginsConfig {
    /// Sharing mode for the plugin chain.
    #[serde(default)]
    pub sharing: Option<SharingMode>,
    /// Plugins applied to this upstream service or route, as the canonical
    /// identifier of every item the body carried.
    ///
    /// The shipped `upstream.v1` and `route.v1` schemas declare the items as
    /// bare identifier strings, while the binding item this feature validates
    /// and writes is the object that carries a `position`, a `plugin_ref`, an
    /// optional `plugin_uuid`, and a configuration. The object form is
    /// accepted here by reading its `plugin_ref`, and the position, the UUID,
    /// and the configuration travel to the binding rows through the binding
    /// validation of the plugin system, so the chain the domain type carries is
    /// the identifier list the shipped schema names.
    #[serde(default, deserialize_with = "deserialize_plugin_items")]
    pub items: Vec<String>,
}

/// Reads one `plugins.items` array, accepting the bare identifier string the
/// shipped schema declares and the binding object the plugin system writes.
fn deserialize_plugin_items<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let carried = Vec::<Value>::deserialize(deserializer)?;
    let mut items = Vec::with_capacity(carried.len());
    for item in carried {
        let reference = match item {
            // The bare identifier form the schema declares.
            Value::String(identifier) => identifier,
            // The binding object form: the reference is the identifier the
            // item names, and the object's other members are the binding
            // row's.
            Value::Object(fields) => match fields.get("plugin_ref").and_then(Value::as_str) {
                Some(reference) => String::from(reference),
                None => {
                    return Err(serde::de::Error::custom(
                        "the item carries no plugin_ref to identify the plugin by",
                    ))
                }
            },
            other => {
                return Err(serde::de::Error::custom(format!(
                    "the item is neither an identifier nor a plugin binding object: {other}"
                )))
            }
        };
        items.push(reference);
    }
    Ok(items)
}

/// The `Upstream` aggregate: a tenant-scoped configuration object
/// representing an external service.
///
/// Carries exactly the properties `schemas/upstream.v1.schema.json` declares.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upstream {
    /// System-generated unique identifier.
    pub id: Uuid,
    /// Whether this upstream is enabled.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Human-readable routing identifier, un-normalized at this layer.
    #[serde(default)]
    pub alias: Option<String>,
    /// Flat tags for categorization and discovery.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Server endpoints; required by the schema.
    pub server: ServerConfig,
    /// Protocol used to connect, as a GTS identifier; required by the schema.
    pub protocol: String,
    /// Authentication plugin binding.
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default)]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain.
    #[serde(default)]
    pub plugins: Option<PluginsConfig>,
    /// Rate limiting configuration.
    #[serde(default)]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default)]
    pub cors: Option<CorsConfig>,
}

/// Declared default for `Upstream::enabled`.
fn default_enabled() -> bool {
    true
}

impl Upstream {
    /// Builds an enabled upstream with the required `server` and `protocol`
    /// and every optional sub-configuration unset.
    #[must_use]
    pub fn new(id: Uuid, server: ServerConfig, protocol: String) -> Self {
        Self {
            id,
            enabled: default_enabled(),
            alias: None,
            tags: Vec::new(),
            server,
            protocol,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
        }
    }

    /// Validates the structural invariants the shipped schema states.
    ///
    /// # Errors
    ///
    /// Returns [`ModelError::NoEndpoints`] when `server` carries no endpoint.
    pub fn validate(&self) -> Result<(), ModelError> {
        self.server.validate()
    }
}
