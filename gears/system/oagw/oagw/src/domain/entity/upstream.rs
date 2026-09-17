//! `Upstream` entity and its associated value objects.
//!
//! Mirrors `oagw_upstream` (§3.7) and the upstream service schema: the
//! tenant-scoped root configuration object representing an external service,
//! unique per `(tenant_id, alias)` within a tenant, with server endpoints,
//! auth config, rate limits, CORS, headers, and plugin bindings.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::time::SystemTime;
use uuid::Uuid;

use super::config::{
    CorsConfig, EndpointScheme, HeadersConfig, PluginsConfig, RateLimitConfig, SharingMode,
    UpstreamProtocol,
};

/// A single endpoint of the upstream server pool.
///
/// All endpoints in a pool share protocol, scheme, and port and are
/// load-balanced round-robin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Endpoint {
    pub scheme: EndpointScheme,
    /// Hostname or IP address of the upstream service.
    pub host: String,
    pub port: u16,
}

impl Default for Endpoint {
    fn default() -> Self {
        Self {
            scheme: EndpointScheme::Https,
            host: String::new(),
            port: 443,
        }
    }
}

/// Upstream server pool: one or more endpoints.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Non-empty by construction (`validate` enforces `minItems: 1`).
    pub endpoints: Vec<Endpoint>,
}

/// Authentication plugin configuration for an upstream.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// Canonical GTS identifier of the auth plugin type, e.g.
    /// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1`.
    pub plugin_type: Option<String>,
    pub sharing: SharingMode,
    /// Plugin configuration; auth secrets are referenced by `cred://` URIs
    /// (vault-aware, resolved through the CredStore at runtime).
    pub config: JsonValue,
}

/// Vault-aware secret reference (`cred://` URI).
///
/// Auth/plugin configurations reference secret material by `cred://` URI;
/// OAGW never stores secrets itself — the CredStore resolves them at runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRef(String);

impl SecretRef {
    /// The `cred://` URI scheme prefix.
    pub const PREFIX: &'static str = "cred://";

    /// Parses a raw reference, rejecting anything that is not a well-formed
    /// `cred://` URI.
    ///
    /// # Errors
    /// Returns a description of the invalid reference.
    pub fn try_new(raw: impl Into<String>) -> Result<Self, String> {
        let raw = raw.into().trim().to_owned();
        if !raw.starts_with(Self::PREFIX) || raw.len() == Self::PREFIX.len() {
            return Err(format!(
                "secret reference must be a '{0}' URI, got '{raw}'",
                Self::PREFIX
            ));
        }
        Ok(Self(raw))
    }

    /// Scans a JSON value for `cred://` secret references (top-level string
    /// values under known keys).
    #[must_use]
    pub fn find_in(config: &JsonValue) -> Vec<Self> {
        let mut found = Vec::new();
        Self::walk(config, &mut found);
        found
    }

    fn walk(value: &JsonValue, acc: &mut Vec<Self>) {
        match value {
            JsonValue::Object(map) => {
                for v in map.values() {
                    Self::walk(v, acc);
                }
            }
            JsonValue::Array(items) => {
                for item in items {
                    Self::walk(item, acc);
                }
            }
            JsonValue::String(s) => {
                if s.starts_with(Self::PREFIX)
                    && let Ok(r) = Self::try_new(s.clone())
                {
                    acc.push(r);
                }
            }
            _ => {}
        }
    }
}

impl std::fmt::Display for SecretRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The `Upstream` entity — the tenant-scoped root configuration object.
///
/// Unique per `(tenant_id, alias)`; base GTS type
/// `gts.cf.core.oagw.upstream.v1~*`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Upstream {
    /// Server-generated resource identifier.
    pub id: Uuid,
    /// Owning tenant (tenant-scoped reads/writes).
    pub tenant_id: Uuid,
    /// Routing key for `/proxy/{alias}/...`; derived or explicit. Immutable.
    pub alias: String,
    pub protocol: UpstreamProtocol,
    /// Enabled flag honored during resolution.
    pub enabled: bool,
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub headers: HeadersConfig,
    pub rate_limit: Option<RateLimitConfig>,
    pub cors: Option<CorsConfig>,
    pub plugins: PluginsConfig,
    /// Discovery tags (add-only union semantics across the hierarchy).
    pub tags: Vec<String>,
    pub created_at: Option<SystemTime>,
    pub updated_at: Option<SystemTime>,
}

impl Default for Upstream {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            alias: String::new(),
            protocol: UpstreamProtocol::Http,
            enabled: true,
            server: ServerConfig::default(),
            auth: AuthConfig::default(),
            headers: HeadersConfig::default(),
            rate_limit: None,
            cors: None,
            plugins: PluginsConfig::default(),
            tags: Vec::new(),
            created_at: None,
            updated_at: None,
        }
    }
}

impl Upstream {
    /// Named constructor producing a canonical tenant-scoped upstream.
    #[must_use]
    pub fn new(tenant_id: Uuid, alias: impl Into<String>, server: ServerConfig) -> Self {
        Self {
            id: Uuid::new_v4(),
            tenant_id,
            alias: alias.into(),
            server,
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(scheme: EndpointScheme, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn endpoint_defaults_match_schema() {
        let e = Endpoint::default();
        assert_eq!(e.scheme, EndpointScheme::Https);
        assert_eq!(e.port, 443);
    }

    #[test]
    fn upstream_new_assigns_tenant_and_alias() {
        let server = ServerConfig {
            endpoints: vec![endpoint(EndpointScheme::Https, "api.vendor.com", 443)],
        };
        let u = Upstream::new(Uuid::from_u128(7), "api.vendor.com", server);
        assert_eq!(u.tenant_id, Uuid::from_u128(7));
        assert_eq!(u.alias, "api.vendor.com");
        assert!(u.enabled);
        assert_eq!(u.server.endpoints.len(), 1);
    }

    #[test]
    fn upstream_deserializes_with_defaults_for_absent_fields() {
        let json = serde_json::json!({
            "id": "11111111-1111-1111-1111-111111111111",
            "tenant_id": "22222222-2222-2222-2222-222222222222",
            "alias": "api.vendor.com",
            "server": { "endpoints": [
                { "scheme": "https", "host": "api.vendor.com", "port": 443 }
            ]}
        });
        let u: Upstream = serde_json::from_value(json).unwrap();
        assert!(u.enabled); // default true
        assert_eq!(u.protocol, UpstreamProtocol::Http); // default
        assert_eq!(u.auth.sharing, SharingMode::Private); // default
    }

    #[test]
    fn secret_ref_rejects_non_cred_uris() {
        assert!(SecretRef::try_new("https://x/y").is_err());
        assert!(SecretRef::try_new("cred://").is_err());
        assert!(SecretRef::try_new("cred://secrets/apikey").is_ok());
    }

    #[test]
    fn secret_ref_find_in_scans_config() {
        let config = serde_json::json!({
            "client_id": "abc",
            "client_secret": "cred://tenants/2/secrets/s1",
            "nested": { "api_key": "cred://tenants/2/secrets/s2" },
            "arr": ["cred://tenants/2/secrets/s3"]
        });
        let refs = SecretRef::find_in(&config);
        assert_eq!(refs.len(), 3, "found: {refs:?}");
        assert!(
            refs.iter()
                .any(|r| r.to_string() == "cred://tenants/2/secrets/s1")
        );
    }
}
