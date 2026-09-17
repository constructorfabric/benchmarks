//! Upstream domain model (`gts.cf.core.oagw.upstream.v1~`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{HeaderRules, PluginChain, PluginRef, SharingMode};
use crate::domain::gts_helpers::{PROTOCOL_GRPC_ID, PROTOCOL_HTTP_ID};

/// Standard port for `http` endpoints (PRD alias-derivation rules).
pub const DEFAULT_HTTP_PORT: u16 = 80;
/// Standard port for every non-`http` scheme.
pub const DEFAULT_TLS_PORT: u16 = 443;

/// Endpoint scheme of an upstream endpoint.
///
/// `http` is a legal value **unconditionally**: the `allow_http_upstream`
/// configuration flag governs only whether a plaintext connection is actually
/// dialled at proxy time, never whether the field is accepted.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Scheme {
    /// Plaintext HTTP.
    Http,
    /// HTTP over TLS.
    #[default]
    Https,
    /// WebSocket over TLS.
    Wss,
    /// WebSocket (cleartext).
    Wt,
    /// gRPC over TLS.
    Grpc,
}

impl Scheme {
    /// Wire representation.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Wss => "wss",
            Self::Wt => "wt",
            Self::Grpc => "grpc",
        }
    }

    /// Default port for this scheme.
    #[must_use]
    pub fn default_port(self) -> u16 {
        match self {
            Self::Http => DEFAULT_HTTP_PORT,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => DEFAULT_TLS_PORT,
        }
    }

    /// True when the scheme normally carries TLS.
    #[must_use]
    pub fn is_tls(self) -> bool {
        !matches!(self, Self::Http | Self::Wt)
    }
}

/// Upstream protocol discriminator (`upstream.protocol`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// HTTP/1.1 and HTTP/2.
    #[default]
    Http,
    /// gRPC.
    Grpc,
}

impl Protocol {
    /// GTS instance id for this protocol.
    #[must_use]
    pub fn gts_id(self) -> &'static str {
        match self {
            Self::Http => PROTOCOL_HTTP_ID,
            Self::Grpc => PROTOCOL_GRPC_ID,
        }
    }

    /// Parse a protocol from its GTS instance id.
    #[must_use]
    pub fn from_gts_id(value: &str) -> Option<Self> {
        match value {
            PROTOCOL_HTTP_ID => Some(Self::Http),
            PROTOCOL_GRPC_ID => Some(Self::Grpc),
            _ => None,
        }
    }

    /// True when the protocol uses HTTP-style request matching.
    #[must_use]
    pub fn is_http(self) -> bool {
        self == Self::Http
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.gts_id())
    }
}

impl Serialize for Protocol {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.gts_id())
    }
}

impl<'de> Deserialize<'de> for Protocol {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::from_gts_id(&raw)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown protocol `{raw}`")))
    }
}

/// A single upstream endpoint (`server.endpoints[]`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Endpoint {
    /// Endpoint scheme.
    pub scheme: Scheme,
    /// Hostname or IP address.
    pub host: String,
    /// Port; defaults to the scheme's standard port when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

impl Endpoint {
    /// Effective port: the configured port or the scheme default.
    #[must_use]
    pub fn effective_port(&self) -> u16 {
        self.port.unwrap_or_else(|| self.scheme.default_port())
    }

    /// True when the endpoint is a literal IP address (v4 or v6).
    #[must_use]
    pub fn is_ip_literal(&self) -> bool {
        self.host.parse::<std::net::IpAddr>().is_ok()
    }

    /// Host as it appears in a `Host` header, i.e. including a bracketed IPv6
    /// literal and the port only when it is non-standard.
    #[must_use]
    pub fn authority(&self) -> String {
        let port = self.effective_port();
        let standard = self.scheme.default_port() == port;
        if self.is_ip_literal() && self.host.contains(':') {
            if standard {
                format!("[{}]", self.host)
            } else {
                format!("[{}]:{port}", self.host)
            }
        } else if standard {
            self.host.clone()
        } else {
            format!("{}:{port}", self.host)
        }
    }
}

/// The `server` block of an upstream: one or more endpoints serving the same
/// logical service (pooled by identical protocol/scheme/port).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct ServerConfig {
    /// Endpoints; at least one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<Endpoint>,
}

/// Upstream authentication binding (`auth` block).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct AuthBinding {
    /// Which auth plugin to execute.
    pub plugin: PluginRef,
    /// How the binding participates in tenant-hierarchy merging.
    pub sharing: SharingMode,
    /// Plugin configuration (`ctx.config`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

/// Upstream service (`gts.cf.core.oagw.upstream.v1~`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Upstream {
    /// Server-generated identifier (UUID v4).
    pub id: Uuid,
    /// Owning tenant (internal; never serialised to the management API).
    #[serde(skip)]
    pub tenant_id: Uuid,
    /// Whether this upstream accepts traffic.
    pub enabled: bool,
    /// Human-readable routing identifier; derived or operator-supplied.
    pub alias: String,
    /// Whether the alias was auto-derived from the endpoints (ADR 0003).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub alias_derived: bool,
    /// Flat tags for categorisation and discovery.
    pub tags: Vec<String>,
    /// Endpoints serving this upstream.
    pub server: ServerConfig,
    /// Upstream protocol.
    pub protocol: Protocol,
    /// Authentication binding, when configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthBinding>,
    /// Header transformation rules.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HeaderRules>,
    /// Upstream-level plugin chain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginChain>,
    /// Upstream-level rate limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<super::RateLimitConfig>,
    /// CORS configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cors: Option<super::CorsConfig>,
    /// Arbitrary internal bookkeeping (not part of the wire contract).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

impl Default for Upstream {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            tenant_id: Uuid::nil(),
            enabled: true,
            alias: String::new(),
            alias_derived: false,
            tags: Vec::new(),
            server: ServerConfig::default(),
            protocol: Protocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            annotations: BTreeMap::new(),
        }
    }
}

impl Upstream {
    /// GTS instance id of this upstream.
    #[must_use]
    pub fn gts_id(&self) -> String {
        crate::domain::gts_helpers::gts_instance_id(
            crate::domain::gts_helpers::OAGW_UPSTREAM_TYPE_ID,
            &self.id,
        )
    }

    /// Every configured endpoint.
    #[must_use]
    pub fn endpoints(&self) -> &[Endpoint] {
        &self.server.endpoints
    }

    /// Hosts named by the endpoints (lower-cased, without brackets).
    #[must_use]
    pub fn hosts(&self) -> Vec<String> {
        self.server
            .endpoints
            .iter()
            .map(|e| normalize_host(&e.host))
            .collect()
    }

    /// Distinct hosts in declaration order.
    #[must_use]
    pub fn distinct_hosts(&self) -> Vec<String> {
        let mut seen = std::collections::BTreeSet::new();
        self.hosts()
            .into_iter()
            .filter(|h| seen.insert(h.clone()))
            .collect()
    }

    /// Endpoint matching `alias` exactly (host + optional `:port` suffix).
    #[must_use]
    pub fn endpoint_for_alias(&self, alias: &str) -> Option<&Endpoint> {
        self.server.endpoints.iter().find(|e| {
            let host = normalize_host(&e.host);
            let port = e.effective_port();
            let standard = port == e.scheme.default_port();
            let candidate = if standard {
                host.clone()
            } else {
                format!("{host}:{port}")
            };
            candidate == alias
        })
    }
}

/// Lower-case a host and strip a trailing dot.
#[must_use]
pub fn normalize_host(host: &str) -> String {
    host.trim().trim_end_matches('.').to_ascii_lowercase()
}
