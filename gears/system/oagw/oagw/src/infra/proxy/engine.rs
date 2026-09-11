// Updated: 2026-09-01 by Constructor Tech
//! The proxy engine: everything that touches the upstream socket.
//!
//! [`Engine`] is deliberately dumb about tenancy, authorization and the store.
//! It is handed a fully resolved [`ResolvedRequest`] — the effective
//! configuration, the headers to send, the body to send — and it does four
//! things: screen the endpoint, dial it, write the request, and read the
//! response back as either a buffered body or a live stream.
//!
//! DNS is resolved *before* a [`HttpPeer`] is built, because that constructor
//! unwraps its address lookup and an unresolvable host would panic the server
//! rather than answer the caller.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::Method;
use pingora_core::connectors::http::v1::Connector as HttpConnector;
use pingora_core::protocols::http::v1::client::HttpSession as PingoraSession;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_http::RequestHeader;

use crate::config::OagwConfig;
use crate::domain::dto::{Endpoint, Scheme};
use crate::infra::proxy::error::GatewayError;
use crate::infra::proxy::ssrf;

/// A dialable endpoint, already resolved to an address and screened.
#[derive(Debug, Clone)]
pub struct ResolvedEndpoint {
    pub scheme: Scheme,
    pub host: String,
    pub port: u16,
    pub addr: std::net::SocketAddr,
    /// `Host` header value the upstream expects.
    pub host_header: String,
}

impl ResolvedEndpoint {
    /// The first endpoint in a resolved list, or the reason there is none.
    #[must_use]
    pub fn choose(endpoints: &[ResolvedEndpoint]) -> Option<&ResolvedEndpoint> {
        endpoints.first()
    }
}

/// Everything the engine needs, already decided.
#[derive(Debug)]
pub struct ResolvedRequest {
    pub method: Method,
    /// Path and query to send upstream — the route's suffix handling is
    /// already applied.
    pub uri: String,
    pub headers: http::HeaderMap,
    pub body: Bytes,
    pub endpoint: ResolvedEndpoint,
    pub read_timeout: Duration,
    pub write_timeout: Duration,
    /// A protocol upgrade the caller asked for. `None` for a plain exchange.
    pub upgrade: Option<UpgradeHandshake>,
}

/// A protocol upgrade the gateway must negotiate on the caller's behalf.
#[derive(Debug, Clone)]
pub struct UpgradeHandshake {
    /// The value of the outgoing `Upgrade` header — `websocket`.
    pub protocol: String,
    /// The headers the negotiation cannot complete without: the WebSocket key
    /// and version, and any subprotocol or extension the caller offered.
    pub headers: http::HeaderMap,
}

/// What came back from the upstream.
pub enum UpstreamResponse {
    /// A complete response head plus a stream of body chunks. Streaming is
    /// what makes server-sent events possible: the chunks are handed to the
    /// caller as the upstream produces them.
    Response {
        status: http::StatusCode,
        headers: http::HeaderMap,
        body: futures_util::stream::BoxStream<'static, Result<Bytes, GatewayError>>,
    },
    /// The upstream accepted an upgrade. The raw connection is handed back so
    /// the caller can pipe bytes both ways.
    Upgraded {
        status: http::StatusCode,
        headers: http::HeaderMap,
        stream: pingora_core::protocols::Stream,
    },
}

impl std::fmt::Debug for UpstreamResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Response { status, .. } => f.debug_tuple("Response").field(status).finish(),
            Self::Upgraded { status, .. } => f.debug_tuple("Upgraded").field(status).finish(),
        }
    }
}

/// The engine.
pub struct Engine {
    connector: Arc<HttpConnector>,
    config: OagwConfig,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine").finish_non_exhaustive()
    }
}

impl Engine {
    #[must_use]
    pub fn new(config: OagwConfig) -> Self {
        Self {
            connector: Arc::new(HttpConnector::new(None)),
            config,
        }
    }

    #[must_use]
    pub fn config(&self) -> &OagwConfig {
        &self.config
    }

    /// Resolve every endpoint of an upstream to a socket address, applying the
    /// SSRF posture to each.
    ///
    /// # Errors
    ///
    /// [`GatewayError`] when the SSRF policy denies a host, or no endpoint
    /// resolves.
    pub async fn resolve_endpoints(
        &self,
        endpoints: &[Endpoint],
    ) -> Result<Vec<ResolvedEndpoint>, GatewayError> {
        let policy = &self.config.ssrf_policy;
        let mut out = Vec::with_capacity(endpoints.len());
        for ep in endpoints {
            let host = ep.normalized_host();
            let port = ep.effective_port();
            let Some(addrs) = lookup(&host, port).await else {
                return Err(unresolvable(&host));
            };
            if let Err(rej) = ssrf::check(&host, &addrs, policy) {
                return Err(screen_error(&host, &rej));
            }
            let Some(addr) = addrs.into_iter().next() else {
                return Err(unresolvable(&host));
            };
            out.push(ResolvedEndpoint {
                scheme: ep.scheme,
                host,
                port,
                addr: std::net::SocketAddr::new(addr, port),
                host_header: ep.host_header(),
            });
        }
        Ok(out)
    }

    /// Send a request upstream and read the response head.
    ///
    /// The body is returned as a stream, so a server-sent-event source is
    /// forwarded chunk by chunk rather than buffered whole.
    ///
    /// # Errors
    ///
    /// [`GatewayError`] on a refused plaintext connection, a dial failure or a
    /// protocol error.
    pub async fn send(&self, req: ResolvedRequest) -> Result<UpstreamResponse, GatewayError> {
        if !req.endpoint.scheme.is_tls() && !self.config.allow_http_upstream {
            return Err(GatewayError::forbidden(
                "plaintext http upstreams are disabled by allow_http_upstream".to_string(),
            )
            .with("host", serde_json::json!(req.endpoint.host)));
        }

        let peer = HttpPeer::new(
            req.endpoint.addr,
            req.endpoint.scheme.is_tls(),
            req.endpoint.host.clone(),
        );
        // `get_http_session` also reports whether the connection came from the
        // pool; a reused connection is indistinguishable for this exchange.
        let (mut session, _reused) = self
            .connector
            .get_http_session(&peer)
            .await
            .map_err(|e| dial_error(&req.endpoint, e))?;

        session.read_timeout = Some(req.read_timeout);
        session.write_timeout = Some(req.write_timeout);

        let mut header = RequestHeader::build(req.method.as_str(), req.uri.as_bytes(), None)
            .map_err(|e| GatewayError::internal(format!("building the upstream request: {e}")))?;
        header.set_uri(
            req.uri
                .parse()
                .map_err(|e| GatewayError::internal(format!("invalid upstream URI: {e}")))?,
        );
        // The inbound map already stores `HeaderName`s, which pingora's
        // `IntoCaseHeaderName` accepts by reference.
        for (name, value) in &req.headers {
            header
                .append_header(name, value.clone())
                .map_err(|e| GatewayError::internal(format!("invalid upstream header: {e}")))?;
        }
        // The body that follows is written as a fixed-length payload, so the
        // head has to say how long it is. Pingora adds nothing of its own, and
        // an upstream that trusts `Content-Length` then reads an empty body
        // from a request that carried one. An upgrade carries no body at all.
        if req.upgrade.is_none()
            && let Some(len) = body_length_header(&req.headers, &req.body)
        {
            header
                .insert_header(http::header::CONTENT_LENGTH, len)
                .map_err(|e| GatewayError::internal(format!("sizing the request body: {e}")))?;
        }

        // The caller asked for an upgrade, so the gateway asks for one too: it
        // is the end of the caller's connection and the start of its own, and
        // the negotiation has to be made to the upstream in its own name.
        if let Some(handshake) = &req.upgrade {
            header
                .insert_header(http::header::CONNECTION, "Upgrade")
                .map_err(|e| GatewayError::internal(format!("framing the upgrade: {e}")))?;
            header
                .insert_header(http::header::UPGRADE, handshake.protocol.as_str())
                .map_err(|e| GatewayError::internal(format!("framing the upgrade: {e}")))?;
            for (name, value) in &handshake.headers {
                header
                    .append_header(name, value.clone())
                    .map_err(|e| GatewayError::internal(format!("invalid upgrade header: {e}")))?;
            }
        }

        session
            .write_request_header(Box::new(header))
            .await
            .map_err(|e| dial_error(&req.endpoint, e))?;

        if !req.body.is_empty() {
            session
                .write_body(&req.body)
                .await
                .map_err(|e| dial_error(&req.endpoint, e))?;
        }
        session
            .finish_body()
            .await
            .map_err(|e| dial_error(&req.endpoint, e))?;

        let resp = session
            .read_resp_header_parts()
            .await
            .map_err(|e| dial_error(&req.endpoint, e))?;

        let status = resp.status;
        // `resp.headers` is already the standard `http::HeaderMap`.
        let headers = clone_headers(&resp.headers);

        if status == http::StatusCode::SWITCHING_PROTOCOLS {
            // Hand the raw connection back: an upgraded exchange is bytes in
            // both directions and is no longer HTTP. A compliant peer sends
            // nothing after the 101 head until the client speaks, so nothing
            // buffered inside the session is lost.
            return Ok(UpstreamResponse::Upgraded {
                status,
                headers,
                stream: session.into_inner(),
            });
        }

        Ok(UpstreamResponse::Response {
            status,
            headers,
            body: Box::pin(body_stream(session)),
        })
    }
}

/// Read the rest of an upstream response, chunk by chunk.
///
/// The session is moved into the stream, so the connection lives exactly as
/// long as the caller keeps pulling.
fn body_stream(
    session: PingoraSession,
) -> impl futures_util::Stream<Item = Result<Bytes, GatewayError>> + Send {
    futures_util::stream::unfold(session, |mut session| async move {
        match session.read_body_bytes().await {
            Ok(Some(chunk)) if !chunk.is_empty() => Some((Ok(chunk), session)),
            Ok(_) => None,
            Err(e) => Some((
                Err(GatewayError::bad_gateway(format!(
                    "reading the upstream response body failed: {e}"
                ))),
                session,
            )),
        }
    })
}

fn clone_headers(map: &http::HeaderMap) -> http::HeaderMap {
    let mut out = http::HeaderMap::with_capacity(map.len());
    for (name, value) in map {
        out.append(name.clone(), value.clone());
    }
    out
}

/// Resolve a host to its addresses. An IP literal needs no lookup and must not
/// be sent to the resolver.
async fn lookup(host: &str, port: u16) -> Option<Vec<IpAddr>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(vec![ip]);
    }
    match tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((host, port)),
    )
    .await
    {
        Ok(Ok(addrs)) => Some(addrs.map(|a| a.ip()).collect()),
        Ok(Err(_)) | Err(_) => None,
    }
}

fn unresolvable(host: &str) -> GatewayError {
    GatewayError::link_unavailable(format!("endpoint '{host}' could not be resolved"))
        .with("host", serde_json::json!(host))
}

fn screen_error(host: &str, rej: &ssrf::SsrfRejection) -> GatewayError {
    match rej {
        ssrf::SsrfRejection::BlockedRange { .. } => GatewayError::forbidden(format!(
            "endpoint '{host}' falls in a network range the SSRF policy denies"
        ))
        .with("host", serde_json::json!(host)),
        ssrf::SsrfRejection::Unresolvable { .. } => unresolvable(host),
    }
}

fn dial_error(endpoint: &ResolvedEndpoint, e: pingora_core::BError) -> GatewayError {
    let detail = format!("dialling {}:{} failed: {e}", endpoint.host, endpoint.port);
    GatewayError::bad_gateway(detail).with("host", serde_json::json!(endpoint.host))
}

/// The `Content-Length` the forwarded head must carry, if any.
///
/// Pingora writes the body it is handed and invents no framing of its own, so
/// the length has to come from here: an upstream that trusts `Content-Length`
/// would otherwise read an empty body from a request that carried one. A head
/// that already declares its own length — or hands the body over chunked — is
/// left exactly as the caller made it.
#[must_use]
fn body_length_header(headers: &http::HeaderMap, body: &[u8]) -> Option<http::HeaderValue> {
    if body.is_empty()
        || headers.contains_key(http::header::CONTENT_LENGTH)
        || headers.contains_key(http::header::TRANSFER_ENCODING)
    {
        return None;
    }
    http::HeaderValue::from_str(&body.len().to_string()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SsrfPolicy;

    fn ep(host: &str, port: u16, scheme: Scheme) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port: Some(port),
        }
    }

    #[tokio::test]
    async fn resolves_an_ip_literal_without_a_lookup() {
        let engine = Engine::new(OagwConfig::default());
        let out = engine
            .resolve_endpoints(&[ep("93.184.216.34", 8080, Scheme::Http)])
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].addr.to_string(), "93.184.216.34:8080");
        // A non-standard port belongs in the Host header the upstream sees.
        assert_eq!(out[0].host_header, "93.184.216.34:8080");
    }

    #[tokio::test]
    async fn a_standard_port_is_omitted_from_the_host_header() {
        let engine = Engine::new(OagwConfig::default());
        let out = engine
            .resolve_endpoints(&[ep("example.com", 443, Scheme::Https)])
            .await
            .unwrap();
        assert_eq!(out[0].host_header, "example.com");
    }

    #[tokio::test]
    async fn an_enabled_policy_refuses_loopback() {
        let engine = Engine::new(OagwConfig::default());
        let err = engine
            .resolve_endpoints(&[ep("127.0.0.1", 8080, Scheme::Http)])
            .await
            .unwrap_err();
        assert_eq!(err.status, http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_disabled_policy_admits_loopback() {
        let mut cfg = OagwConfig::default();
        cfg.ssrf_policy.enabled = false;
        let engine = Engine::new(cfg);
        let out = engine
            .resolve_endpoints(&[ep("127.0.0.1", 8080, Scheme::Http)])
            .await
            .unwrap();
        assert_eq!(out[0].addr.to_string(), "127.0.0.1:8080");
    }

    #[tokio::test]
    async fn an_unresolvable_host_is_reported_not_panicked() {
        let mut cfg = OagwConfig::default();
        cfg.ssrf_policy.enabled = false;
        let engine = Engine::new(cfg);
        let err = engine
            .resolve_endpoints(&[ep("no-such-host.invalid", 80, Scheme::Http)])
            .await
            .unwrap_err();
        assert_eq!(err.status, http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.type_id, crate::gts::ERR_LINK_UNAVAILABLE);
    }

    #[test]
    fn default_policy_denies_the_private_ranges() {
        let policy = SsrfPolicy::default();
        assert!(policy.enabled);
        assert!(crate::infra::proxy::ssrf::is_denied_host("127.0.0.1"));
        assert!(crate::infra::proxy::ssrf::is_denied_host("10.1.2.3"));
        assert!(!crate::infra::proxy::ssrf::is_denied_host("example.com"));
    }

    #[test]
    fn a_body_declares_its_own_length() {
        let out = body_length_header(&http::HeaderMap::new(), Bytes::from_static(b"{}").as_ref());
        assert_eq!(out.unwrap(), "2");
    }

    #[test]
    fn an_empty_body_declares_nothing() {
        let out = body_length_header(&http::HeaderMap::new(), Bytes::new().as_ref());
        assert!(out.is_none());
    }

    #[test]
    fn a_head_that_already_frames_the_body_is_left_alone() {
        let mut headers = http::HeaderMap::new();
        headers.insert(http::header::CONTENT_LENGTH, "7".parse().unwrap());
        assert!(body_length_header(&headers, b"payload").is_none());

        let mut chunked = http::HeaderMap::new();
        chunked.insert(http::header::TRANSFER_ENCODING, "chunked".parse().unwrap());
        assert!(body_length_header(&chunked, b"payload").is_none());
    }
}
