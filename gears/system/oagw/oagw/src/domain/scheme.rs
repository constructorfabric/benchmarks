//! `Scheme` value object — the write-time endpoint scheme literal.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Endpoint scheme literal carried by an OAGW endpoint.
///
/// The single write-time admission predicate lives on this type
/// ([`Scheme::is_write_admitted`]): `http` is legal only when the operator
/// lifted the posture with `oagw.config.allow_http_upstream`, every other
/// literal is always legal. Dialing is a different check owned by the data
/// plane feature and is deliberately not expressed here.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    /// Plaintext HTTP. Admitted at write time only when
    /// `oagw.config.allow_http_upstream` is `true`.
    Http,
    /// HTTP over TLS. Always admitted.
    Https,
    /// WebSocket over TLS. Always admitted.
    Wss,
    /// WebTransport. Always admitted.
    Wt,
    /// gRPC. Always admitted.
    Grpc,
}

impl Scheme {
    /// Lowercase wire literal for this scheme.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
            Self::Wss => "wss",
            Self::Wt => "wt",
            Self::Grpc => "grpc",
        }
    }

    /// Write-time admission predicate: `true` when this scheme literal may be
    /// stored on a configured endpoint.
    ///
    /// `Scheme::Http` is admitted only when `allow_http_upstream` is `true`;
    /// every other literal is admitted unconditionally. Recording the lifted
    /// posture on the configuration does not by itself authorize a plaintext
    /// dial.
    #[must_use]
    pub const fn is_write_admitted(self, allow_http_upstream: bool) -> bool {
        // @cpt-begin:cpt-cf-oagw-algo-config-load-validate:p1:inst-config-http-if
        match self {
            Self::Http => allow_http_upstream,
            Self::Https | Self::Wss | Self::Wt | Self::Grpc => true,
        }
        // @cpt-end:cpt-cf-oagw-algo-config-load-validate:p1:inst-config-http-if
    }
}

impl fmt::Display for Scheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
