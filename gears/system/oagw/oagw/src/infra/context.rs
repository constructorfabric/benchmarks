// Created: 2026-09-01 by Constructor Tech
//! The Data Plane's per-request state.
//!
//! `docs/DESIGN.md` §3.1 "Internal domain types (ProxyContext,
//! ProxyResponse, etc.)". This is the object handed to every plugin phase
//! and to the forwarder.

use std::collections::BTreeMap;

use toolkit_http::ResponseBody;
use toolkit_security::SecurityContext;

use crate::domain::errors::OagwError;
use crate::domain::model::{PluginBinding, Target};

/// Everything a plugin phase can read and mutate.
#[derive(Debug, Clone)]
pub struct PluginRequest {
    /// Outbound HTTP method.
    pub method: String,
    /// Outbound path, including the query string.
    pub path: String,
    /// Outbound headers, after the configured transformation.
    pub headers: Vec<(String, String)>,
    /// Request body, buffered (the 100 MiB ceiling keeps this bounded).
    pub body: Vec<u8>,
    /// The endpoint the request is headed for.
    pub target: Target,
    /// The upstream alias the request resolved to.
    pub alias: String,
    /// The upstream resource id.
    pub upstream_id: String,
    /// The route that matched, when one did.
    pub route_id: Option<String>,
    /// The calling tenant.
    pub tenant_id: String,
    /// The authenticated subject, when the caller identified itself.
    pub subject: Option<String>,
    /// The gateway-generated or propagated request id.
    pub request_id: String,
    /// The request's content type, when the client sent one.
    pub content_type: Option<String>,
    /// The upstream `auth.config` block, merged with any route override.
    pub auth_config: BTreeMap<String, serde_json::Value>,
    /// The config the current plugin's binding carries. The executor sets
    /// this before each phase invocation.
    pub plugin_config: BTreeMap<String, serde_json::Value>,
    /// The caller's security context.
    pub security: SecurityContext,
}

impl PluginRequest {
    /// Read a header case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// A plugin config entry rendered as a string, when it holds one.
    #[must_use]
    pub fn config_str(&self, key: &str) -> Option<String> {
        self.plugin_config
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    }

    /// A plugin config entry rendered as a string, falling back to
    /// `default`.
    #[must_use]
    pub fn config_str_or(&self, key: &str, default: &str) -> String {
        self.config_str(key).unwrap_or_else(|| default.to_owned())
    }

    /// Set a header, replacing every existing value.
    pub fn set_header(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        let value = value.into();
        self.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(&name));
        self.headers.push((name, value));
    }

    /// Append a header, keeping existing values.
    pub fn add_header(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.headers.push((name.into(), value.into()));
    }

    /// Remove every value of a header.
    pub fn remove_header(&mut self, name: &str) {
        self.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
    }

    /// `true` when a header is present.
    #[must_use]
    pub fn has_header(&self, name: &str) -> bool {
        self.header(name).is_some()
    }
}

/// The plugin chain a request executes.
#[derive(Debug, Clone)]
pub struct Chain {
    /// Auth plugin, at most one.
    pub auth: Option<PluginBinding>,
    /// Guard plugins in execution order.
    pub guards: Vec<PluginBinding>,
    /// Transform plugins in execution order.
    pub transforms: Vec<PluginBinding>,
}

impl Chain {
    /// An empty chain.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            auth: None,
            guards: Vec::new(),
            transforms: Vec::new(),
        }
    }
}

/// The outcome of running a proxy request.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // the socket only moves out, never copied
pub enum ProxyOutcome {
    /// A response was produced (by the upstream, or by a CORS preflight).
    Response(ProxyResponse),
    /// The upgrade to a WebSocket session was negotiated.
    Upgraded(UpgradeHandle),
}

/// The body of a proxied response.
///
/// Buffered for ordinary payloads, streamed for `text/event-stream` and any
/// other response the upstream kept open. The stream type is toolkit-http's
/// own boxed body so an upstream body can be handed downstream without
/// re-wrapping it.
pub enum ProxyBody {
    /// Fully buffered bytes.
    Full(bytes::Bytes),
    /// An incremental body.
    Stream(ResponseBody),
}

impl std::fmt::Debug for ProxyBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full(b) => f.debug_tuple("Full").field(&b.len()).finish(),
            Self::Stream(_) => f.debug_tuple("Stream").field(&"boxed").finish(),
        }
    }
}

/// A proxied response.
#[derive(Debug)]
pub struct ProxyResponse {
    /// Upstream status code.
    pub status: u16,
    /// Headers to return to the client.
    pub headers: Vec<(String, String)>,
    /// Body.
    pub body: ProxyBody,
}

/// A negotiated WebSocket session.
pub struct UpgradeHandle {
    /// Status the upstream returned (101 on a successful handshake).
    pub status: u16,
    /// Upstream response head, relayed verbatim.
    pub headers: Vec<(String, String)>,
    /// The upstream socket, already past the handshake and ready to be
    /// spliced against the client's upgraded connection.
    pub io: crate::infra::dp::UpstreamIo,
}

impl std::fmt::Debug for UpgradeHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpgradeHandle")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .finish_non_exhaustive()
    }
}

/// The upstream response a guard or transform plugin sees.
#[derive(Debug, Clone)]
pub struct PluginResponse {
    /// Response status returned by the upstream.
    pub status: u16,
    /// Response headers after the configured transformation.
    pub headers: Vec<(String, String)>,
    /// The config the current plugin's binding carries. The executor sets
    /// this before each phase invocation.
    pub plugin_config: BTreeMap<String, serde_json::Value>,
}

impl PluginResponse {
    /// `true` when a header is present, case-insensitively.
    #[must_use]
    pub fn has_header(&self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        self.headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(&lower))
    }

    /// A plugin config entry rendered as a string, when it holds one.
    #[must_use]
    pub fn config_str(&self, key: &str) -> Option<String> {
        self.plugin_config
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    }
}

impl ProxyResponse {
    /// An error rendered as a response.
    #[must_use]
    pub fn from_error(error: &OagwError) -> Self {
        Self {
            status: error.status_value(),
            headers: error.response_headers(),
            body: ProxyBody::Full(error.to_body_bytes().into()),
        }
    }

    /// The response body as fully buffered bytes, draining a stream if need be.
    pub async fn into_bytes(self) -> Result<bytes::Bytes, OagwError> {
        match self.body {
            ProxyBody::Full(bytes) => Ok(bytes),
            ProxyBody::Stream(body) => {
                use http_body_util::BodyExt;
                let mut collected = bytes::BytesMut::new();
                let mut body = body;
                while let Some(frame) = body.frame().await {
                    let frame = frame.map_err(|error| {
                        OagwError::downstream_error(format!("upstream body failed: {error}"))
                    })?;
                    if let Some(data) = frame.data_ref() {
                        collected.extend_from_slice(data);
                    }
                }
                Ok(collected.freeze())
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn request() -> PluginRequest {
        PluginRequest {
            method: "GET".to_owned(),
            path: "/v1/chat".to_owned(),
            headers: vec![("x-a".to_owned(), "1".to_owned())],
            body: Vec::new(),
            target: Target {
                host: "api.openai.com".to_owned(),
                port: 443,
                secure: true,
            },
            alias: "api.openai.com".to_owned(),
            upstream_id: "u1".to_owned(),
            route_id: Some("r1".to_owned()),
            tenant_id: "t1".to_owned(),
            subject: None,
            request_id: "req-1".to_owned(),
            content_type: None,
            auth_config: BTreeMap::new(),
            plugin_config: BTreeMap::new(),
            security: SecurityContext::anonymous(),
        }
    }

    #[test]
    fn headers_are_accessed_case_insensitively() {
        let mut r = request();
        assert_eq!(r.header("X-A"), Some("1"));
        assert!(r.has_header("x-a"));
        r.set_header("X-A", "2");
        assert_eq!(r.header("x-a"), Some("2"));
        r.add_header("x-a", "3");
        assert_eq!(r.headers.len(), 2);
        r.remove_header("X-A");
        assert!(r.headers.is_empty());
    }

    #[test]
    fn plugin_config_reads_strings_with_a_fallback() {
        let mut r = request();
        r.plugin_config
            .insert("name".to_owned(), serde_json::json!("abc"));
        assert_eq!(r.config_str("name").as_deref(), Some("abc"));
        assert_eq!(r.config_str_or("missing", "fallback"), "fallback");
        r.plugin_config
            .insert("num".to_owned(), serde_json::json!(7));
        assert_eq!(r.config_str("num"), None);
    }

    #[test]
    fn an_error_renders_as_a_response() {
        let response = ProxyResponse::from_error(&OagwError::route_not_found("no route"));
        assert_eq!(response.status, 404);
        assert!(
            response
                .headers
                .iter()
                .any(|(n, v)| n == "content-type" && v.starts_with("application/problem+json"))
        );
        let bytes = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(response.into_bytes())
            .expect("bytes");
        let body: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(body["status"], 404);
    }
}
