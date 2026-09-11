//! OAGW domain model: the wire types of
//! `docs/schemas/upstream.v1.schema.json` / `route.v1.schema.json`, the alias
//! rules of DESIGN ("Alias Enforcement Rules") and the shared resource
//! identifier helpers.
//!
//! The module is transport- and persistence-free: plain data types plus pure
//! business rules (alias derivation, endpoint validation, alias immutability),
//! so both the control plane and the REST layer can depend on it.

use std::collections::HashMap;
use std::net::IpAddr;

use http::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::error::OagwError;

/// GTS prefix minted for an upstream identifier.
pub const UPSTREAM_ID_PREFIX: &str = "gts.cf.core.oagw.upstream.v1~";
/// GTS prefix minted for a route identifier.
pub const ROUTE_ID_PREFIX: &str = "gts.cf.core.oagw.route.v1~";

/// GTS base types a stored (custom) plugin may carry, in [`PluginKind`] order.
pub const PLUGIN_BASE_TYPES: [&str; 3] = [
    "gts.cf.core.oagw.auth_plugin.v1~",
    "gts.cf.core.oagw.guard_plugin.v1~",
    "gts.cf.core.oagw.transform_plugin.v1~",
];

/// Maximum length of an alias (RFC 1123 hostname bound).
pub const MAX_ALIAS_LEN: usize = 253;
/// Maximum length of a single hostname label.
const MAX_LABEL_LEN: usize = 63;
/// Maximum length of a hostname (trailing dot excluded).
const MAX_HOSTNAME_LEN: usize = 253;
/// Maximum length of a stored plugin name (`oagw_plugin.name`).
const MAX_PLUGIN_NAME_LEN: usize = 255;

/// HTTP methods accepted in `cors.allowed_methods` (`upstream.v1.schema.json`).
///
/// The vocabulary is the schema's, not the data plane's: `HEAD` and `OPTIONS`
/// are in it because CORS advertises them to a browser (`OPTIONS` is the
/// preflight itself, and `HEAD` is the method a browser may ask about even
/// though this gateway does not forward it), while the data plane proxies only
/// the five methods of [`ROUTE_METHODS`] — a `HEAD` actual request is refused
/// with a 400 by [`crate::domain::proxy::ensure_supported_method`] and an
/// `OPTIONS` actual request is answered by the preflight handler before
/// anything is resolved. Widening [`ROUTE_METHODS`] is therefore a data-plane
/// decision, never a consequence of the CORS configuration.
const CORS_METHODS: [&str; 7] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

/// Mint the GTS identifier of an upstream from its UUID.
#[must_use]
pub fn upstream_gts_id(id: Uuid) -> String {
    format!("{UPSTREAM_ID_PREFIX}{id}")
}

/// Mint the GTS identifier of a route from its UUID.
#[must_use]
pub fn route_gts_id(id: Uuid) -> String {
    format!("{ROUTE_ID_PREFIX}{id}")
}

/// Parse a resource path segment: a bare UUID or the GTS form `…~{uuid}`.
///
/// # Errors
///
/// Returns a 400 [`OagwError`] when the segment is neither a UUID nor a
/// `prefix`-qualified GTS identifier carrying a UUID.
pub fn parse_resource_id(prefix: &str, raw: &str) -> Result<Uuid, OagwError> {
    let trimmed = raw.trim();
    let bare = if let Some(rest) = trimmed.strip_prefix(prefix) {
        rest
    } else if trimmed.contains('~') {
        return Err(OagwError::validation(format!(
            "identifier `{trimmed}` must be a UUID or a `{prefix}<uuid>` GTS identifier"
        ))
        .with_invalid_value(trimmed.to_owned()));
    } else {
        trimmed
    };
    Uuid::parse_str(bare).map_err(|_| {
        OagwError::validation(format!(
            "identifier `{trimmed}` must be a UUID or a `{prefix}<uuid>` GTS identifier"
        ))
        .with_invalid_value(trimmed.to_owned())
    })
}

/// Mint the GTS identifier of a plugin record.
///
/// A stored plugin is addressed by the `plugin_ref` it was created with: the
/// kind-specific base type followed by the record UUID
/// (`gts.cf.core.oagw.guard_plugin.v1~{uuid}`), which is what ADR-0001 uses in
/// the plugin-deletion examples.
#[must_use]
pub fn plugin_gts_id(kind: PluginKind, id: Uuid) -> String {
    format!("{}{id}", kind.gts_base_type())
}

/// Parse a plugin path segment: a bare UUID or one of the three plugin GTS base
/// types followed by a UUID (`gts.cf.core.oagw.guard_plugin.v1~{uuid}`).
///
/// # Errors
///
/// Returns a 400 [`OagwError`] when the segment is neither a UUID nor a
/// plugin GTS identifier carrying a UUID.
pub fn parse_plugin_id(raw: &str) -> Result<Uuid, OagwError> {
    let trimmed = raw.trim();
    let Some(instance) = trimmed.split_once('~').map(|(_, instance)| instance) else {
        return Uuid::parse_str(trimmed).map_err(|_| plugin_id_error(trimmed));
    };
    if !is_plugin_base_type_of(trimmed) {
        return Err(plugin_id_error(trimmed));
    }
    Uuid::parse_str(instance).map_err(|_| plugin_id_error(trimmed))
}

/// Whether `identifier` starts with one of the plugin GTS base types
/// (`gts.cf.core.oagw.<kind>_plugin.v1~`).
fn is_plugin_base_type_of(identifier: &str) -> bool {
    PLUGIN_BASE_TYPES
        .iter()
        .any(|base_type| identifier.starts_with(base_type))
}

fn plugin_id_error(raw: &str) -> OagwError {
    OagwError::validation(format!(
        "identifier `{raw}` must be a UUID or a `gts.cf.core.oagw.<kind>_plugin.v1~<uuid>` GTS \
         identifier"
    ))
    .with_invalid_value(raw.to_owned())
}

/// Normalize an alias: ASCII lowercase with trailing dots stripped.
///
/// Resolution is case-insensitive (`Api.OpenAI.COM.` and `api.openai.com` are
/// the same routing key), so every alias is stored in this canonical form.
#[must_use]
pub fn normalize_alias(alias: &str) -> String {
    alias
        .trim()
        .to_ascii_lowercase()
        .trim_end_matches('.')
        .to_owned()
}

/// Whether `alias` matches the schema pattern
/// `^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$`.
#[must_use]
pub fn is_valid_alias(alias: &str) -> bool {
    if alias.is_empty() || alias.len() > MAX_ALIAS_LEN {
        return false;
    }
    let bytes = alias.as_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_alphanumeric) {
        return false;
    }
    if bytes.len() == 1 {
        return true;
    }
    if !bytes.last().is_some_and(u8::is_ascii_alphanumeric) {
        return false;
    }
    bytes[1..bytes.len() - 1]
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b':' | b'-'))
}

/// Whether `tag` matches the schema pattern `^[a-z0-9_-]+$`.
#[must_use]
pub fn is_valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
}

/// Whether `host` is an IPv4 or IPv6 address.
#[must_use]
pub fn is_ip_address(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok()
}

/// Whether `host` is a valid RFC 1123 hostname (a trailing FQDN dot is
/// tolerated and ignored): at most 253 characters, labels of 1–63 ASCII
/// alphanumeric or hyphen characters that neither start nor end with a hyphen.
#[must_use]
pub fn is_valid_hostname(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() || host.len() > MAX_HOSTNAME_LEN {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= MAX_LABEL_LEN
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

/// Canonical form of an endpoint host: ASCII lowercase without the trailing
/// FQDN dot.
#[must_use]
pub fn normalize_host(host: &str) -> String {
    let trimmed = host.trim();
    let lowered = trimmed.to_ascii_lowercase();
    lowered.strip_suffix('.').unwrap_or(&lowered).to_owned()
}

/// Outcome of alias derivation for an endpoint set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DerivedAlias {
    /// Derivation succeeded; this is the normalized alias the endpoints imply.
    Derived(String),
    /// Derivation is impossible: an explicit alias is required.
    NotDerivable,
}

/// Compute the alias implied by an endpoint set.
///
/// * a single hostname yields the hostname, or `hostname:port` for a
///   non-standard port (HTTP 80, everything else 443);
/// * several hostnames yield their longest common suffix when it is a
///   registrable domain of at least two labels (PSL-validated — `foo.co.uk` +
///   `bar.co.uk` is *not* derivable because `co.uk` is a public suffix);
/// * any IP-addressed endpoint, endpoints carrying two different non-standard
///   ports, or hostnames without a registrable common suffix are not
///   derivable.
#[must_use]
pub fn derive_alias(endpoints: &[Endpoint]) -> DerivedAlias {
    let mut hostnames: Vec<String> = Vec::with_capacity(endpoints.len());
    let mut non_standard_port: Option<u16> = None;

    for endpoint in endpoints {
        let host = normalize_host(&endpoint.host);
        if is_ip_address(&host) || !is_valid_hostname(&host) {
            return DerivedAlias::NotDerivable;
        }
        if endpoint.port != endpoint.scheme.default_port() {
            match non_standard_port {
                Some(previous) if previous != endpoint.port => return DerivedAlias::NotDerivable,
                Some(_) | None => non_standard_port = Some(endpoint.port),
            }
        }
        hostnames.push(host);
    }

    let port_suffix = non_standard_port
        .map(|port| format!(":{port}"))
        .unwrap_or_default();

    if let [single] = hostnames.as_slice() {
        return DerivedAlias::Derived(format!("{single}{port_suffix}"));
    }

    let Some(suffix) = common_domain_suffix(&hostnames) else {
        return DerivedAlias::NotDerivable;
    };
    DerivedAlias::Derived(format!("{suffix}{port_suffix}"))
}

/// Longest common suffix of `hostnames` that is a registrable domain of at
/// least two labels (`us.vendor.com` + `eu.vendor.com` → `vendor.com`).
fn common_domain_suffix(hostnames: &[String]) -> Option<String> {
    let labels: Vec<Vec<&str>> = hostnames
        .iter()
        .map(|host| host.split('.').collect())
        .collect();
    let shortest = labels.iter().map(Vec::len).min().unwrap_or(0);

    let mut shared: Vec<&str> = Vec::new();
    for offset in 0..shortest {
        let candidate = labels[0][labels[0].len() - 1 - offset];
        if labels
            .iter()
            .all(|set| set[set.len() - 1 - offset] == candidate)
        {
            shared.push(candidate);
        } else {
            break;
        }
    }
    if shared.len() < 2 {
        return None;
    }
    shared.reverse();
    let suffix = shared.join(".");
    // A bare public suffix (`co.uk`) is not a registrable domain, so a pool
    // sharing only it must be named explicitly.
    if psl::domain_str(&suffix) != Some(suffix.as_str()) {
        return None;
    }
    Some(suffix)
}

/// Resolve the alias of a **new** upstream from the user-provided value and the
/// endpoint set.
///
/// # Errors
///
/// Returns a 400 [`OagwError`] when the provided alias is malformed, when it
/// disagrees with the alias derived from hostname-based endpoints, or when the
/// endpoints are not derivable and no alias was provided.
pub fn resolve_alias(provided: Option<&str>, endpoints: &[Endpoint]) -> Result<String, OagwError> {
    let normalized = provided.map(normalize_alias);
    if let Some(value) = &normalized {
        ensure_valid_alias(value)?;
    }
    match (normalized, derive_alias(endpoints)) {
        (Some(value), DerivedAlias::Derived(derived)) => {
            if value == derived {
                // Providing the derived value is tolerated (idempotent no-op).
                Ok(derived)
            } else {
                Err(OagwError::validation(format!(
                    "alias `{value}` does not match the alias `{derived}` derived from the \
                     endpoints: hostname-based endpoints always auto-derive their alias"
                ))
                .with_invalid_value(value)
                .with_alias(derived))
            }
        }
        (None, DerivedAlias::Derived(derived)) => Ok(derived),
        (Some(value), DerivedAlias::NotDerivable) => Ok(value),
        (None, DerivedAlias::NotDerivable) => Err(OagwError::validation(
            "alias is required: IP-based or non-derivable endpoints do not derive an alias",
        )),
    }
}

/// Enforce alias immutability on update.
///
/// The alias is the routing key of `/oagw/v1/proxy/{alias}/…`, so it never
/// changes: a request that names a different alias is rejected, and an endpoint
/// change that would change the derived alias is rejected as well (the operator
/// must delete and re-create the upstream).
///
/// # Errors
///
/// Returns a 400 [`OagwError`] when the request carries a different alias or
/// when the new endpoints would derive a different alias.
pub fn enforce_alias_update(
    existing_alias: &str,
    current_endpoints: &[Endpoint],
    requested: &UpstreamSpec,
) -> Result<String, OagwError> {
    if let Some(provided) = requested.alias.as_deref() {
        let normalized = normalize_alias(provided);
        ensure_valid_alias(&normalized)?;
        if normalized != existing_alias {
            return Err(OagwError::validation(format!(
                "alias `{existing_alias}` is immutable: delete and re-create the upstream to \
                 route under a different alias"
            ))
            .with_alias(existing_alias.to_owned())
            .with_invalid_value(normalized));
        }
    }

    if alias_would_change(
        existing_alias,
        current_endpoints,
        &requested.server.endpoints,
    ) {
        return Err(OagwError::validation(format!(
            "the endpoint change would change the derived alias of `{existing_alias}`; the alias \
             is immutable, so delete and re-create the upstream instead"
        ))
        .with_alias(existing_alias.to_owned()));
    }
    Ok(existing_alias.to_owned())
}

/// `true` when the requested endpoint set would route under a different alias
/// than `existing_alias`.
///
/// Endpoints that derive an alias keep it only when the derivation is
/// unchanged; endpoints that do not derive one (IP-based, or a pool without a
/// registrable common suffix) keep the alias they were created with, but an
/// upstream that *had* a derived alias must never lose it.
fn alias_would_change(
    existing_alias: &str,
    current_endpoints: &[Endpoint],
    requested_endpoints: &[Endpoint],
) -> bool {
    match derive_alias(requested_endpoints) {
        DerivedAlias::Derived(derived) => derived != existing_alias,
        DerivedAlias::NotDerivable => derive_alias(current_endpoints) != DerivedAlias::NotDerivable,
    }
}

/// Validate a normalized alias against the schema pattern.
fn ensure_valid_alias(alias: &str) -> Result<(), OagwError> {
    if alias.is_empty() {
        return Err(OagwError::validation("alias must not be empty"));
    }
    if !is_valid_alias(alias) {
        return Err(OagwError::validation(format!(
            "alias `{alias}` must match ^[a-z0-9]([a-z0-9.:-]*[a-z0-9])?$ (ASCII lowercase, at \
             most {MAX_ALIAS_LEN} characters)"
        ))
        .with_invalid_value(alias.to_owned()));
    }
    Ok(())
}

/// Wire `scheme` of an upstream endpoint.
///
/// `http` is a legal create-time value (a plaintext upstream can be
/// *declared*); whether the data plane may actually dial it is governed by
/// `gears.oagw.config.allow_http_upstream`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum EndpointScheme {
    /// Plaintext HTTP.
    Http,
    /// TLS HTTP/1.1 and HTTP/2.
    #[default]
    Https,
    /// TLS WebSocket.
    Wss,
    /// WebTransport.
    Wt,
    /// TLS gRPC.
    Grpc,
}

impl EndpointScheme {
    /// Standard (default) port of the scheme: HTTP 80, everything else 443.
    #[must_use]
    pub const fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => 443,
        }
    }

    /// Wire name of the scheme, as it appears in a document (`http`, `https`, …).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Wss => "wss",
            Self::Wt => "wt",
            Self::Grpc => "grpc",
        }
    }
}

/// A single upstream endpoint (`server.endpoints[]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Wire scheme; `https` when omitted.
    #[serde(default)]
    pub scheme: EndpointScheme,
    /// Upstream host: an RFC 1123 hostname or an IP address.
    pub host: String,
    /// Port; the scheme default when omitted.
    #[serde(default = "default_endpoint_port")]
    pub port: u16,
}

fn default_endpoint_port() -> u16 {
    443
}

/// `server` member of an upstream: the endpoint pool the data plane may dial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Endpoints (at least one).
    pub endpoints: Vec<Endpoint>,
}

/// Protocol used to reach the upstream service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub enum UpstreamProtocol {
    /// HTTP/1.1 and HTTP/2.
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1")]
    Http,
    /// gRPC.
    #[serde(rename = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.grpc.v1")]
    Grpc,
}

/// Hierarchical sharing mode of a configuration member.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SharingMode {
    /// Not visible to descendant tenants.
    #[default]
    Private,
    /// Descendant tenants may override.
    Inherit,
    /// Descendant tenants may neither override nor exceed.
    Enforce,
}

/// Upstream authentication plugin configuration (`auth` member).
#[derive(Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AuthConfig {
    /// Authentication plugin type as a GTS identifier
    /// (`gts.cf.core.oagw.auth_plugin.v1~…`).
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub auth_type: Option<String>,
    /// Sharing mode; `private` when omitted.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Authentication plugin configuration (plugin-defined).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

impl std::fmt::Debug for AuthConfig {
    // The `config` member carries credential material, so it is never rendered.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthConfig")
            .field("auth_type", &self.auth_type)
            .field("sharing", &self.sharing)
            .field("config", &"[REDACTED]")
            .finish()
    }
}

/// Which inbound headers the data plane forwards to the upstream.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum HeaderPassthrough {
    /// Forward none (default).
    #[default]
    None,
    /// Forward only `passthrough_allowlist`.
    Allowlist,
    /// Forward everything.
    All,
}

/// Request header transformation rules (`headers.request`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RequestHeaders {
    /// Headers to set (overwrite when present).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<HashMap<String, String>>,
    /// Headers to add (append, duplicates allowed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add: Option<HashMap<String, String>>,
    /// Header names to strip from the inbound request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove: Option<Vec<String>>,
    /// Which inbound headers to forward; `none` when omitted.
    #[serde(default)]
    pub passthrough: HeaderPassthrough,
    /// Headers forwarded when `passthrough` is `allowlist`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passthrough_allowlist: Option<Vec<String>>,
}

/// Response header transformation rules (`headers.response`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResponseHeaders {
    /// Headers to set on the response sent to the client.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set: Option<HashMap<String, String>>,
    /// Headers to add to the response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add: Option<HashMap<String, String>>,
    /// Header names to strip from the upstream response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove: Option<Vec<String>>,
}

/// Header transformation rules (`headers` member).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HeadersConfig {
    /// Inbound request header rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestHeaders>,
    /// Upstream response header rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseHeaders>,
}

/// One entry of a plugin chain (`plugins.items[]`).
///
/// A binding appears in exactly two places — `upstream.plugins.items` and
/// `route.plugins.items` (`upstream.v1.schema.json` / `route.v1.schema.json`),
/// with the authentication plugin bound separately through `upstream.auth`. It
/// is written in either of the two shapes the documents use:
///
/// * a bare plugin GTS identifier (`…guard_plugin.v1~cf.core.oagw.required_headers.v1`),
///   the shape both schemas list;
/// * a bound object carrying the plugin's configuration
///   (`{"plugin_ref": "…", "config": {…}}`), the shape ADR-0009 "Upstream
///   Configuration Example" and the `oagw_upstream_plugin` /
///   `oagw_route_plugin` binding rows of DESIGN §3.6 use — a guard such as
///   `required_headers.v1` is configured per binding, not per plugin.
///
/// A binding without configuration serializes back to the bare identifier, so
/// both shapes survive a round trip unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(untagged)]
pub enum PluginBinding {
    /// A plugin referenced by its GTS identifier, without configuration.
    Ref(String),
    /// A plugin reference plus its plugin-defined configuration.
    Bound(BoundPluginBinding),
}

/// A configured plugin binding: `{"plugin_ref": "…", "config": {…}}`.
///
/// Unknown members are refused (`deny_unknown_fields`) rather than ignored: a
/// binding is a security-relevant document and a typo in a member name must not
/// silently drop the configuration it was meant to carry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BoundPluginBinding {
    /// GTS identifier the plugin is resolved by.
    pub plugin_ref: String,
    /// Plugin-defined configuration; absent when the plugin is unconfigured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

impl PluginBinding {
    /// The GTS identifier the plugin is resolved by.
    #[must_use]
    pub fn plugin_ref(&self) -> &str {
        match self {
            Self::Ref(reference)
            | Self::Bound(BoundPluginBinding {
                plugin_ref: reference,
                ..
            }) => reference,
        }
    }

    /// The configuration carried by the binding, when it has one.
    #[must_use]
    pub const fn config(&self) -> Option<&serde_json::Value> {
        match self {
            Self::Ref(_) => None,
            Self::Bound(BoundPluginBinding { config, .. }) => config.as_ref(),
        }
    }
}

impl Serialize for PluginBinding {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Ref(reference) => serializer.serialize_str(reference),
            // An unconfigured binding is the bare identifier: the two wire shapes
            // stay interchangeable.
            Self::Bound(BoundPluginBinding {
                plugin_ref,
                config: None,
            }) => serializer.serialize_str(plugin_ref),
            Self::Bound(BoundPluginBinding {
                plugin_ref,
                config: Some(config),
            }) => {
                let mut map = serde_json::Map::new();
                map.insert(
                    "plugin_ref".to_owned(),
                    serde_json::Value::from((*plugin_ref).clone()),
                );
                map.insert("config".to_owned(), config.clone());
                serde_json::Value::Object(map).serialize(serializer)
            }
        }
    }
}

/// Ordered plugin chain bound to an upstream or a route (`plugins` member).
///
/// Items are GTS identifiers: builtin plugins are named
/// (`gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1`),
/// custom (Starlark) plugins are UUID-backed
/// (`gts.cf.core.oagw.guard_plugin.v1~{uuid}`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PluginChain {
    /// Sharing mode of the chain; `private` when omitted.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Plugin bindings, in execution order.
    #[serde(default)]
    pub items: Vec<PluginBinding>,
}

/// Rate limit window unit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitWindow {
    /// One second.
    #[default]
    Second,
    /// One minute.
    Minute,
    /// One hour.
    Hour,
    /// One day.
    Day,
}

impl RateLimitWindow {
    /// Wire name of the window unit, as it appears in a document (`second`,
    /// `minute`, …) and in a rate limit key (ADR-0003).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Second => "second",
            Self::Minute => "minute",
            Self::Hour => "hour",
            Self::Day => "day",
        }
    }
}

/// Rate limit algorithm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitAlgorithm {
    /// Allows bursts (default).
    #[default]
    TokenBucket,
    /// Prevents boundary bursts.
    SlidingWindow,
}

/// Scope of the rate limit counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitScope {
    /// One counter for the whole gateway.
    Global,
    /// One counter per tenant (default).
    #[default]
    Tenant,
    /// One counter per user.
    User,
    /// One counter per client IP.
    Ip,
    /// One counter per route.
    Route,
}

impl RateLimitScope {
    /// Wire name of the scope, as it appears in a document and in a rate limit
    /// key (ADR-0003).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Tenant => "tenant",
            Self::User => "user",
            Self::Ip => "ip",
            Self::Route => "route",
        }
    }
}

/// Behaviour when the limit is exceeded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum RateLimitStrategy {
    /// Reject the request (default).
    #[default]
    Reject,
    /// Queue the request.
    Queue,
    /// Degrade the response.
    Degrade,
}

/// Sustained rate of a rate limit (`rate_limit.sustained`, required).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SustainedRate {
    /// Tokens replenished per `window` (at least 1).
    pub rate: u64,
    /// Window unit; `second` when omitted.
    #[serde(default)]
    pub window: RateLimitWindow,
}

/// Burst capacity of a token bucket (`rate_limit.burst`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct BurstCapacity {
    /// Bucket capacity; defaults to `sustained.rate` when omitted.
    pub capacity: u64,
}

/// Rate limiting configuration (`rate_limit` member).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Sharing mode; `private` when omitted.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Algorithm; `token_bucket` when omitted.
    #[serde(default)]
    pub algorithm: RateLimitAlgorithm,
    /// Sustained rate (required).
    pub sustained: SustainedRate,
    /// Burst capacity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burst: Option<BurstCapacity>,
    /// Counter scope; `tenant` when omitted.
    #[serde(default)]
    pub scope: RateLimitScope,
    /// Over-limit behaviour; `reject` when omitted.
    #[serde(default)]
    pub strategy: RateLimitStrategy,
    /// Tokens consumed per request; 1 when omitted.
    #[serde(default = "default_rate_limit_cost")]
    pub cost: u64,
}

fn default_rate_limit_cost() -> u64 {
    1
}

/// CORS configuration (`cors` member).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CorsConfig {
    /// Sharing mode; `private` when omitted.
    #[serde(default)]
    pub sharing: SharingMode,
    /// Whether CORS is enabled for this resource (required).
    pub enabled: bool,
    /// Allowed origins; `["*"]` allows any origin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_origins: Option<Vec<String>>,
    /// Allowed methods; `["GET", "POST"]` when omitted.
    #[serde(default = "default_cors_methods")]
    pub allowed_methods: Vec<String>,
    /// Headers exposed to the browser beyond the CORS-safelisted set.
    #[serde(default)]
    pub expose_headers: Vec<String>,
    /// Whether credentials (cookies, auth headers) are allowed.
    #[serde(default)]
    pub allow_credentials: bool,
}

fn default_cors_methods() -> Vec<String> {
    vec!["GET".to_owned(), "POST".to_owned()]
}

/// Wire representation of an upstream service (`upstream.v1`).
///
/// The same type is the create/update request body and the response body: the
/// schema marks `id` read-only, so it is ignored on input and populated with
/// the GTS identifier on output.
#[derive(Clone, PartialEq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct UpstreamSpec {
    /// GTS identifier of the upstream (response only, ignored on input).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Routing alias. Enforced by endpoint type: hostname-based endpoints
    /// auto-derive it (a provided value must match the derivation), while
    /// IP-based or non-derivable endpoints require an explicit alias. The
    /// alias is immutable once set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Flat tags for categorization and discovery.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Endpoint pool the data plane may dial.
    pub server: ServerConfig,
    /// Protocol used to connect to the upstream.
    pub protocol: UpstreamProtocol,
    /// Whether the upstream accepts traffic; `true` when omitted.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Authentication plugin configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
    /// Header transformation rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeadersConfig>,
    /// Plugin chain bound to this upstream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginChain>,
    /// Rate limiting configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
    /// CORS configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cors: Option<CorsConfig>,
}

fn default_enabled() -> bool {
    true
}

impl std::fmt::Debug for UpstreamSpec {
    // Hand-written so the authentication plugin configuration is never rendered
    // by a `{:?}` log line (see [`AuthConfig`]).
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UpstreamSpec")
            .field("id", &self.id)
            .field("alias", &self.alias)
            .field("tags", &self.tags)
            .field("server", &self.server)
            .field("protocol", &self.protocol)
            .field("enabled", &self.enabled)
            .field(
                "auth",
                &self.auth.as_ref().map(|auth| auth.auth_type.clone()),
            )
            .field("auth_config", &"[REDACTED]")
            .field("headers", &self.headers)
            .field("plugins", &self.plugins)
            .field("rate_limit", &self.rate_limit)
            .field("cors", &self.cors)
            .finish()
    }
}

impl UpstreamSpec {
    /// Validate the constraints the JSON schema expresses beyond serde
    /// (minItems, minimum, pattern and cross-field rules).
    ///
    /// # Errors
    ///
    /// Returns a 400 [`OagwError`] describing the first violation found.
    pub fn validate(&self) -> Result<(), OagwError> {
        if self.server.endpoints.is_empty() {
            return Err(OagwError::validation(
                "server.endpoints must contain at least one endpoint",
            ));
        }
        for endpoint in &self.server.endpoints {
            validate_endpoint(endpoint)?;
        }
        validate_endpoint_pool(&self.server.endpoints)?;
        for tag in &self.tags {
            if !is_valid_tag(tag) {
                return Err(
                    OagwError::validation(format!("tag `{tag}` must match ^[a-z0-9_-]+$"))
                        .with_invalid_value(tag.clone()),
                );
            }
        }
        if let Some(rate_limit) = &self.rate_limit {
            validate_rate_limit(rate_limit)?;
        }
        if let Some(cors) = &self.cors {
            validate_cors(cors)?;
        }
        if let Some(headers) = &self.headers {
            validate_headers(headers)?;
        }
        if let Some(plugins) = &self.plugins {
            validate_plugin_chain(plugins)?;
        }
        Ok(())
    }

    /// Validate the alias of a **new** upstream against the endpoints.
    ///
    /// # Errors
    ///
    /// Returns a 400 [`OagwError`] when the alias is malformed, missing for a
    /// non-derivable endpoint set, or inconsistent with the derivation.
    pub fn resolve_alias(&self) -> Result<String, OagwError> {
        resolve_alias(self.alias.as_deref(), &self.server.endpoints)
    }
}

/// Validate a single endpoint (`host` shape, `port` range).
fn validate_endpoint(endpoint: &Endpoint) -> Result<(), OagwError> {
    let host = endpoint.host.trim();
    if host.is_empty() {
        return Err(OagwError::validation(
            "server.endpoints[].host must not be empty",
        ));
    }
    if !is_ip_address(host) && !is_valid_hostname(host) {
        return Err(OagwError::validation(format!(
            "server.endpoints[].host `{host}` is not a valid RFC 1123 hostname or IP address"
        ))
        .with_host(host.to_owned())
        .with_invalid_value(host.to_owned()));
    }
    if endpoint.port == 0 {
        return Err(
            OagwError::validation("server.endpoints[].port must be between 1 and 65535")
                .with_invalid_value("0"),
        );
    }
    Ok(())
}

/// Validate the pool invariant DESIGN "Multi-Endpoint Load Balancing" states:
/// "All endpoints must have the same `protocol`, `scheme`, and `port`."
///
/// `protocol` is a property of the whole upstream, so only `scheme` and `port`
/// are compared here. Without the invariant a pool would be load-balanced across
/// endpoints the data plane can only serve partially — one scheme of a mixed
/// pool is refused by the plaintext-upstream policy, and an endpoint on another
/// port is unreachable through an authority that carries a single port.
///
/// # Errors
///
/// Returns a 400 [`OagwError`] naming the first endpoint that disagrees with the
/// first one.
fn validate_endpoint_pool(endpoints: &[Endpoint]) -> Result<(), OagwError> {
    let Some(first) = endpoints.first() else {
        return Ok(());
    };
    for endpoint in &endpoints[1..] {
        if endpoint.scheme != first.scheme {
            return Err(OagwError::validation(format!(
                "all endpoints of an upstream must have the same scheme: the pool mixes `{}` and \
                 `{}` ({}://{}:{})",
                first.scheme.name(),
                endpoint.scheme.name(),
                endpoint.scheme.name(),
                endpoint.host,
                endpoint.port
            )));
        }
        if endpoint.port != first.port {
            return Err(OagwError::validation(format!(
                "all endpoints of an upstream must have the same port: the pool mixes {} and {}",
                first.port, endpoint.port
            ))
            .with_invalid_value(endpoint.port.to_string()));
        }
    }
    Ok(())
}

/// Validate the header transformation rules of an upstream (name and value
/// shape of every `set`, `add`, `remove` and `passthrough_allowlist` entry).
///
/// The data plane applies these rules verbatim, so a rule that no HTTP layer
/// could express must never be stored.
fn validate_headers(headers: &HeadersConfig) -> Result<(), OagwError> {
    if let Some(request) = &headers.request {
        if let Some(set) = &request.set {
            for (name, value) in set {
                validate_header_name(name, "headers.request.set")?;
                validate_header_value(value, "headers.request.set")?;
            }
        }
        if let Some(add) = &request.add {
            for (name, value) in add {
                validate_header_name(name, "headers.request.add")?;
                validate_header_value(value, "headers.request.add")?;
            }
        }
        if let Some(remove) = &request.remove {
            for name in remove {
                validate_header_name(name, "headers.request.remove")?;
            }
        }
        if let Some(allowlist) = &request.passthrough_allowlist {
            for name in allowlist {
                validate_header_name(name, "headers.request.passthrough_allowlist")?;
            }
        }
    }
    if let Some(response) = &headers.response {
        if let Some(set) = &response.set {
            for (name, value) in set {
                validate_header_name(name, "headers.response.set")?;
                validate_header_value(value, "headers.response.set")?;
            }
        }
        if let Some(add) = &response.add {
            for (name, value) in add {
                validate_header_name(name, "headers.response.add")?;
                validate_header_value(value, "headers.response.add")?;
            }
        }
        if let Some(remove) = &response.remove {
            for name in remove {
                validate_header_name(name, "headers.response.remove")?;
            }
        }
    }
    Ok(())
}

/// Validate a header *name* the way an HTTP parser would.
fn validate_header_name(name: &str, member: &str) -> Result<(), OagwError> {
    if HeaderName::from_bytes(name.as_bytes()).is_ok() {
        return Ok(());
    }
    Err(OagwError::validation(format!(
        "`{member}` entry `{name}` is not a valid HTTP header name"
    ))
    .with_invalid_value(name.to_owned()))
}

/// Validate a header *value* the way an HTTP layer would.
fn validate_header_value(value: &str, member: &str) -> Result<(), OagwError> {
    if HeaderValue::from_str(value).is_ok() {
        return Ok(());
    }
    Err(
        OagwError::validation(format!("`{member}` value is not a valid HTTP header value"))
            .with_invalid_value(value.to_owned()),
    )
}

/// Validate a rate limit configuration (positive counters).
fn validate_rate_limit(rate_limit: &RateLimitConfig) -> Result<(), OagwError> {
    if rate_limit.sustained.rate == 0 {
        return Err(OagwError::validation(
            "rate_limit.sustained.rate must be at least 1",
        ));
    }
    if rate_limit
        .burst
        .as_ref()
        .is_some_and(|burst| burst.capacity == 0)
    {
        return Err(OagwError::validation(
            "rate_limit.burst.capacity must be at least 1",
        ));
    }
    if rate_limit.cost == 0 {
        return Err(OagwError::validation("rate_limit.cost must be at least 1"));
    }
    Ok(())
}

/// Validate a CORS configuration (method vocabulary, credentials/`*` rule).
fn validate_cors(cors: &CorsConfig) -> Result<(), OagwError> {
    for method in &cors.allowed_methods {
        if !CORS_METHODS.contains(&method.as_str()) {
            return Err(OagwError::validation(format!(
                "cors.allowed_methods entry `{method}` is not a supported HTTP method"
            ))
            .with_invalid_value(method.clone()));
        }
    }
    if let Some(origins) = &cors.allowed_origins
        && cors.allow_credentials
        && origins.iter().any(|origin| origin == "*")
    {
        return Err(OagwError::validation(
            "cors.allow_credentials cannot be combined with the `*` origin",
        )
        .with_invalid_value("*"));
    }
    Ok(())
}

/// HTTP method accepted by a route match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethod {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
    /// `PUT`.
    Put,
    /// `DELETE`.
    Delete,
    /// `PATCH`.
    Patch,
}

/// How the proxy path suffix is treated (`http_match.path_suffix_mode`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PathSuffixMode {
    /// Reject a path suffix.
    Disabled,
    /// Append the suffix to the matched path.
    #[default]
    Append,
}

/// HTTP methods accepted in `match.http.methods` (route.v1 schema).
pub const ROUTE_METHODS: [&str; 5] = ["GET", "POST", "PUT", "DELETE", "PATCH"];

/// HTTP matching rules of a route (`match.http`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpMatch {
    /// Methods served by the route (at least one).
    pub methods: Vec<HttpMethod>,
    /// Path pattern of the route.
    pub path: String,
    /// Allowed query parameters; none when empty.
    #[serde(default)]
    pub query_allowlist: Vec<String>,
    /// Suffix handling; `append` when omitted.
    #[serde(default)]
    pub path_suffix_mode: PathSuffixMode,
}

impl HttpMatch {
    /// Whether the route serves `method` (compared ASCII
    /// case-insensitively, as HTTP method names are).
    #[must_use]
    pub fn accepts(&self, method: &str) -> bool {
        self.methods
            .iter()
            .any(|candidate| candidate.as_str().eq_ignore_ascii_case(method))
    }

    /// `true` when `other` serves any of the methods this match serves: two
    /// routes with the same path and an overlapping method set cannot both be
    /// enabled on one upstream.
    #[must_use]
    pub fn methods_overlap(&self, other: &Self) -> bool {
        self.methods
            .iter()
            .any(|candidate| other.accepts(candidate.as_str()))
    }
}

impl HttpMethod {
    /// The HTTP method name as it appears on the wire (`GET`, `POST`, …).
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

/// gRPC matching rules of a route (`match.grpc`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GrpcMatch {
    /// Fully qualified gRPC service name (`foo.v1.UserService`).
    pub service: String,
    /// RPC method name (`GetUser`).
    pub method: String,
}

/// Protocol-scoped inbound matching rules of a route (`match` member): exactly
/// one of `http` / `grpc` is present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteMatch {
    /// HTTP match rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<HttpMatch>,
    /// gRPC match rules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grpc: Option<GrpcMatch>,
}

impl RouteMatch {
    /// The HTTP match rules, when the route matches on HTTP.
    #[must_use]
    pub const fn http(&self) -> Option<&HttpMatch> {
        self.http.as_ref()
    }

    /// The gRPC match rules, when the route matches on gRPC.
    #[must_use]
    pub const fn grpc(&self) -> Option<&GrpcMatch> {
        self.grpc.as_ref()
    }

    /// Deterministic ordering key: the HTTP path, or the gRPC service for a
    /// gRPC route. Routes are listed by `(upstream_id, order_key)` so a list
    /// answer is stable across calls.
    #[must_use]
    pub fn order_path(&self) -> &str {
        match (&self.http, &self.grpc) {
            (Some(http), _) => &http.path,
            (None, Some(grpc)) => &grpc.service,
            (None, None) => "",
        }
    }

    /// Whether `other` would win the same request as `self`: two HTTP routes
    /// of one upstream with the same path and an overlapping method set are a
    /// match-rule conflict.
    #[must_use]
    pub fn conflicts_with(&self, other: &Self) -> bool {
        match (&self.http, other.http()) {
            (Some(left), Some(right)) => left.path == right.path && left.methods_overlap(right),
            _ => false,
        }
    }
}

/// Wire representation of a route (`route.v1`).
///
/// Route CRUD is wired by a later slice; the type is defined here so the
/// control-plane model is complete.
#[derive(Clone, Debug, PartialEq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct RouteSpec {
    /// GTS identifier of the route (response only, ignored on input).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// GTS identifier of the upstream this route belongs to.
    pub upstream_id: String,
    /// Flat tags for categorization and discovery.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Protocol-scoped matching rules.
    #[serde(rename = "match")]
    pub match_rules: RouteMatch,
    /// Plugin chain bound to this route.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginChain>,
    /// Rate limiting configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<RateLimitConfig>,
}

impl RouteSpec {
    /// Validate the constraints the JSON schema expresses beyond serde
    /// (exactly one match, minItems, path shape, tag pattern, positive rate
    /// counters) plus the `upstream_id` reference shape.
    ///
    /// # Errors
    ///
    /// Returns a 400 [`OagwError`] describing the first violation found.
    pub fn validate(&self) -> Result<(), OagwError> {
        parse_resource_id(UPSTREAM_ID_PREFIX, &self.upstream_id)?;
        for tag in &self.tags {
            if !is_valid_tag(tag) {
                return Err(
                    OagwError::validation(format!("tag `{tag}` must match ^[a-z0-9_-]+$"))
                        .with_invalid_value(tag.clone()),
                );
            }
        }
        self.match_rules.validate()?;
        if let Some(rate_limit) = &self.rate_limit {
            validate_rate_limit(rate_limit)?;
        }
        if let Some(plugins) = &self.plugins {
            validate_plugin_chain(plugins)?;
        }
        Ok(())
    }
}

impl RouteMatch {
    /// Validate the `oneOf {http|grpc}` shape and the match-level constraints.
    fn validate(&self) -> Result<(), OagwError> {
        match (&self.http, &self.grpc) {
            (Some(http), None) => validate_http_match(http),
            (None, Some(grpc)) => validate_grpc_match(grpc),
            (Some(_), Some(_)) | (None, None) => Err(OagwError::validation(
                "match must carry exactly one of `http` or `grpc`",
            )),
        }
    }
}

/// Validate an HTTP match (method vocabulary, path shape, query names).
fn validate_http_match(http: &HttpMatch) -> Result<(), OagwError> {
    if http.methods.is_empty() {
        return Err(OagwError::validation(
            "match.http.methods must contain at least one method",
        ));
    }
    for method in &http.methods {
        if !ROUTE_METHODS.contains(&method.as_str()) {
            return Err(OagwError::validation(format!(
                "match.http.methods entry `{}` is not a supported HTTP method",
                method.as_str()
            ))
            .with_invalid_value(method.as_str().to_owned()));
        }
    }
    if http.path.is_empty() {
        return Err(OagwError::validation("match.http.path must not be empty"));
    }
    if !http.path.starts_with('/') {
        return Err(OagwError::validation(format!(
            "match.http.path `{}` must be an absolute path starting with `/`",
            http.path
        ))
        .with_invalid_value(http.path.clone()));
    }
    for name in &http.query_allowlist {
        if name.is_empty() {
            return Err(OagwError::validation(
                "match.http.query_allowlist entries must not be empty",
            ));
        }
    }
    Ok(())
}

/// Validate a gRPC match (non-empty service and method).
fn validate_grpc_match(match_rules: &GrpcMatch) -> Result<(), OagwError> {
    if match_rules.service.is_empty() {
        return Err(OagwError::validation(
            "match.grpc.service must not be empty",
        ));
    }
    if match_rules.method.is_empty() {
        return Err(OagwError::validation("match.grpc.method must not be empty"));
    }
    Ok(())
}

/// Validate the plugin bindings of a chain (`plugins.items[]`).
///
/// The data plane resolves every binding against the in-process registry, so a
/// reference no registry could resolve must never be stored.
fn validate_plugin_chain(plugins: &PluginChain) -> Result<(), OagwError> {
    for item in &plugins.items {
        let reference = item.plugin_ref();
        if parse_plugin_id(reference).is_err() && !is_named_plugin_id(reference) {
            return Err(OagwError::validation(format!(
                "plugins.items entry `{reference}` must be a plugin GTS identifier"
            ))
            .with_invalid_value(reference.to_owned()));
        }
    }
    Ok(())
}

/// Whether `identifier` is a named (builtin) plugin GTS identifier such as
/// `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1`.
#[must_use]
pub fn is_named_plugin_id(identifier: &str) -> bool {
    let Some((_, instance)) = identifier.split_once('~') else {
        return false;
    };
    is_plugin_base_type_of(identifier)
        && !instance.is_empty()
        && instance
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// Starlark plugin kind; fixes the GTS base type
/// `gts.cf.core.oagw.{kind}_plugin.v1~`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PluginKind {
    /// Credential injection (one per upstream).
    #[default]
    Auth,
    /// Validation / policy enforcement (can reject).
    Guard,
    /// Request or response mutation.
    Transform,
}

impl PluginKind {
    /// GTS base type of the plugin kind.
    #[must_use]
    pub const fn gts_base_type(self) -> &'static str {
        match self {
            Self::Auth => "gts.cf.core.oagw.auth_plugin.v1~",
            Self::Guard => "gts.cf.core.oagw.guard_plugin.v1~",
            Self::Transform => "gts.cf.core.oagw.transform_plugin.v1~",
        }
    }

    /// Lowercase name of the kind, as it appears on the wire.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Guard => "guard",
            Self::Transform => "transform",
        }
    }
}

/// Lifecycle phase a plugin participates in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum PluginPhase {
    /// Before the request is sent upstream (`on_request`).
    #[serde(rename = "on_request")]
    Request,
    /// Before the response is returned to the client (`on_response`).
    #[serde(rename = "on_response")]
    Response,
    /// On upstream failure (`on_error`).
    #[serde(rename = "on_error")]
    Error,
}

/// Wire representation of a custom (tenant-defined Starlark) plugin.
///
/// A plugin is immutable once created (no `PUT`): a change is a new plugin plus
/// a re-bind. The `id` and `plugin_ref` members are read-only — both echo the
/// same kind-qualified GTS identifier the server mints at creation
/// (`gts.cf.core.oagw.{kind}_plugin.v1~{uuid}`); `plugin_ref` also carries the
/// stored (custom) plugin's identifier inside upstream/route plugin chains.
///
/// A plugin without `source` is a *named* plugin resolved from the in-process
/// registry (not stored, see DESIGN "Plugin Identification Model"); the CRUD
/// surface only mints identifiers for stored plugins, so `source` is required
/// when `plugin_ref` names a UUID-backed plugin.
#[derive(Clone, Debug, PartialEq)]
#[toolkit_macros::api_dto(request, response)]
#[serde(deny_unknown_fields)]
pub struct PluginSpec {
    /// GTS identifier of the plugin (response only, ignored on input).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// GTS identifier the plugin is referenced by in upstream and route plugin
    /// chains (response only, mirrors `id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_ref: Option<String>,
    /// Tenant-unique plugin name.
    pub name: String,
    /// Plugin kind (`auth`, `guard` or `transform`); fixes the GTS base type.
    #[serde(default)]
    pub kind: PluginKind,
    /// Lifecycle phases the plugin participates in.
    #[serde(default)]
    pub phases: Vec<PluginPhase>,
    /// Plugin configuration (plugin-defined object).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
    /// Starlark source of a stored (custom) plugin; absent for a named plugin
    /// resolved from the in-process registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

impl PluginSpec {
    /// Validate the plugin document: name, config shape and the `plugin_ref`
    /// / `source` pairing.
    ///
    /// # Errors
    ///
    /// Returns a 400 [`OagwError`] describing the first violation found.
    pub fn validate(&self) -> Result<(), OagwError> {
        if self.name.trim().is_empty() {
            return Err(OagwError::validation("name must not be empty"));
        }
        if self.name.len() > MAX_PLUGIN_NAME_LEN {
            return Err(OagwError::validation(format!(
                "name must be at most {MAX_PLUGIN_NAME_LEN} characters"
            ))
            .with_invalid_value(self.name.clone()));
        }
        if let Some(config) = &self.config
            && !config.is_object()
        {
            return Err(OagwError::validation(
                "config must be a JSON object (plugin-defined configuration)",
            ));
        }
        if let Some(plugin_ref) = &self.plugin_ref {
            validate_plugin_ref(plugin_ref, self.kind)?;
        } else if self.source.is_none() {
            return Err(OagwError::validation(
                "source is required: a custom plugin is created from its Starlark source",
            ));
        }
        Ok(())
    }
}

/// Validate a client-provided `plugin_ref` against the plugin `kind`.
///
/// The `plugin_ref` of a stored plugin is minted by the server; a client value
/// is tolerated only when it names the same kind (`guard_plugin` base type with
/// `kind: guard`) — anything else would make the two identifiers disagree.
fn validate_plugin_ref(plugin_ref: &str, kind: PluginKind) -> Result<(), OagwError> {
    let base_type = kind.gts_base_type();
    let Some(instance) = plugin_ref.strip_prefix(base_type) else {
        return Err(OagwError::validation(format!(
            "plugin_ref `{plugin_ref}` must be a `{base_type}<instance>` GTS identifier naming a \
             `{}` plugin",
            kind.name()
        ))
        .with_invalid_value(plugin_ref.to_owned()));
    };
    if instance.is_empty() {
        return Err(OagwError::validation(format!(
            "plugin_ref `{plugin_ref}` carries no instance identifier after `{base_type}`"
        ))
        .with_invalid_value(plugin_ref.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
