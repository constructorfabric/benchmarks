//! The outbound forwarding of one proxy exchange.
//!
//! Realizes `cpt-cf-oagw-algo-outbound-forward`: the dial-time scheme check,
//! the shared outbound client ADR 0006 assigns the Data Plane, the adaptive
//! per-host HTTP version detection of DESIGN §3.2 Security Considerations, the
//! deadline `proxy_timeout_secs` carries, and the single send. The upstream
//! response is returned as received for `cpt-cf-oagw-algo-response-classify`.
//!
//! The single send is the whole of the retry posture this routine has: the
//! gateway never re-issues the client request (`cpt-cf-oagw-principle-no-retry`),
//! and the connector's own endpoint and connection attempts stay inside it —
//! exactly the clause of `cpt-cf-oagw-fr-request-proxy` that permits an
//! intermediary to retry a connection and not a request. An upstream 401
//! triggers no refresh, no retry, and no re-send: it is answered under the
//! error-source classification, not here.
//!
//! The routine is split at the seam `cpt-cf-oagw-feature-streaming` consumes:
//! [`OutboundClient::begin`] dials the endpoint and writes the request, and the
//! [`LiveExchange`] it returns is the upstream half of the exchange. Reading
//! the response header is the boundary `RequestTimeout` bounds and the last
//! moment at which the exchange can still be answered as a whole; what
//! `cpt-cf-oagw-algo-stream-pump` reads past that boundary is the body or the
//! tunnel, and the idle deadline is the only deadline over it.

use std::collections::HashMap;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use pingora_core::connectors::http::Connector;
use pingora_core::protocols::http::client::HttpSession;
use pingora_core::protocols::tls::ALPN;
use pingora_core::upstreams::peer::{HttpPeer, PeerOptions};
use pingora_http::RequestHeader;

use crate::config::OagwConfig;
use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::proxy::{OutboundRequest, ProxyResponse};
use crate::domain::scheme::Scheme;

/// The lifetime of a per-host HTTP version cache entry, which DESIGN §3.2
/// states as 1 hour.
pub const HTTP_VERSION_TTL: Duration = Duration::from_secs(3600);

/// The scheme of a forwarded request: HTTP over TLS, or plaintext.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    /// HTTP over TLS, where ALPN negotiates the version.
    Tls,
    /// Plaintext HTTP, which has no ALPN to negotiate with and is always
    /// HTTP/1.1 in this run.
    Plain,
}

/// The HTTP version a host is known to answer, as one cache entry of the
/// adaptive detection holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Version {
    /// The host negotiated HTTP/2, and `ALPN::H2H1` is advertised again.
    H2,
    /// The host fell back to HTTP/1.1, and only `ALPN::H1` is advertised.
    H1,
}

#[derive(Debug, Clone, Copy)]
struct VersionEntry {
    version: Version,
    recorded_at: Instant,
}

/// The shared outbound client the Data Plane holds.
///
/// One connector per process, constructed once and reused, with the per-host
/// version cache beside it. A cloned handle shares both, which is what makes a
/// detected version visible to every later request in the process.
#[derive(Clone)]
pub struct OutboundClient {
    connector: Arc<Connector>,
    versions: Arc<Mutex<HashMap<String, VersionEntry>>>,
}

impl Default for OutboundClient {
    fn default() -> Self {
        Self::new()
    }
}

impl OutboundClient {
    /// Constructs the shared client, once per process.
    #[must_use]
    pub fn new() -> Self {
        Self {
            connector: Arc::new(Connector::new(None)),
            versions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The version preference one host is dialled with.
    ///
    /// A host with no entry, or with an entry past its hour, is dialled with
    /// HTTP/2 preferred so the ALPN negotiation can report what it supports; a
    /// host known to fall back is dialled HTTP/1.1 only; a plaintext host has
    /// no ALPN to negotiate and is dialled HTTP/1.1.
    fn preference_of(&self, host: &str, transport: Transport) -> Version {
        if transport == Transport::Plain {
            return Version::H1;
        }
        let mut versions = self.versions.lock();
        match versions.get(host) {
            Some(entry) if entry.recorded_at.elapsed() < HTTP_VERSION_TTL => entry.version,
            _ => {
                versions.remove(host);
                Version::H2
            }
        }
    }

    /// Forwards one request and returns the upstream response as received.
    ///
    /// This is the form the proxy path takes for an exchange whose answer is
    /// classified and assembled whole. [`OutboundClient::begin`] is the same
    /// send up to the response header, which is where
    /// `cpt-cf-oagw-algo-stream-mode-select` selects the transfer mode of the
    /// body and `cpt-cf-oagw-algo-stream-pump` takes the transfer over.
    ///
    /// # Errors
    ///
    /// Returns the `ProtocolError` failure for a scheme that is never dialled
    /// and for the plaintext dial the lifted posture does not authorize, the
    /// `LinkUnavailable` failure for an endpoint that cannot be resolved or
    /// reached, and the two timeout failures for a connection or an exchange
    /// that outlives the deadline.
    #[allow(clippy::result_large_err)]
    pub async fn send(
        &self,
        request: &OutboundRequest,
        config: &OagwConfig,
    ) -> Result<ProxyResponse, DomainError> {
        // @cpt-begin:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-return
        let mut live = self.begin(request, config).await?;
        let head = live.head().await?;
        let body = live.finish().await?;
        Ok(ProxyResponse::upstream(head.status, head.headers, body))
        // @cpt-end:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-return
    }

    /// Opens the upstream half of one exchange and sends the request over it.
    ///
    /// The routine dials the selected endpoint, writes the outbound request
    /// once, and returns the [`LiveExchange`] whose response head the caller
    /// reads next. No response byte is read here, because the mode the body is
    /// transferred in is selected from the response headers and not from the
    /// body.
    ///
    /// # Errors
    ///
    /// Returns the `ProtocolError` failure for a scheme that is never dialled
    /// and for the plaintext dial the lifted posture does not authorize, the
    /// `LinkUnavailable` failure for an endpoint that cannot be resolved or
    /// reached, and the two timeout failures for a connection that outlives the
    /// deadline.
    #[allow(clippy::result_large_err)]
    pub async fn begin(
        &self,
        request: &OutboundRequest,
        config: &OagwConfig,
    ) -> Result<LiveExchange, DomainError> {
        // @cpt-begin:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-scheme
        // The dial-time check is a separate check against the same constraint
        // the write-time acceptance tests: recording a lifted posture never
        // authorizes a plaintext dial, and `wt` and `grpc` are never dialed.
        let transport = match request.scheme {
            // @cpt-begin:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-scheme-wt-if
            Scheme::Wt | Scheme::Grpc => {
                // @cpt-begin:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-scheme-wt-return
                return Err(DomainError::gateway(
                    ErrorKind::ProtocolError,
                    "the selected endpoint's scheme is never dialed by this gateway",
                ));
                // @cpt-end:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-scheme-wt-return
            }
            // @cpt-end:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-scheme-wt-if
            // @cpt-begin:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-scheme-http-if
            Scheme::Http if !config.allow_http_upstream => {
                // @cpt-begin:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-scheme-http-return
                return Err(DomainError::gateway(
                    ErrorKind::ProtocolError,
                    "the plaintext endpoint scheme is not admitted by the runtime posture",
                ));
                // @cpt-end:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-scheme-http-return
            }
            // @cpt-end:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-scheme-http-if
            Scheme::Http => Transport::Plain,
            Scheme::Https | Scheme::Wss => Transport::Tls,
        };
        // @cpt-end:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-scheme

        // @cpt-begin:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-scheme-else
        // The ELSE of the scheme check: the endpoint's scheme is one this
        // posture dials, so the request proceeds over the shared client.
        // @cpt-end:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-scheme-else

        // @cpt-begin:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-client
        // The shared client is constructed once and reused; this call only
        // borrows it.
        let client = self.connector.clone();
        // @cpt-end:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-client

        // @cpt-begin:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-version
        // On the first request to a host, HTTP/2 is attempted through ALPN
        // during the TLS handshake; on success the supported version is cached
        // for that host, on fallback HTTP/1.1 is, and on every subsequent
        // request the cached version is used. An outbound request that carries
        // an `Upgrade` header is dialled HTTP/1.1 in either case, because
        // `Connection` and `Upgrade` are HTTP/1.1 hop-by-hop headers and the
        // extended CONNECT that would carry a tunnel over HTTP/2 is not a
        // mechanism this run delivers, which is the bound DESIGN §3.2 Security
        // Considerations' HTTP version negotiation note places on a tunnel.
        let upgraded = request
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("upgrade"));
        let preference = if upgraded {
            Version::H1
        } else {
            self.preference_of(&request.host, transport)
        };
        // @cpt-end:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-version

        // @cpt-begin:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-deadline
        // The one deadline applies to both the connection-establishment phase
        // and the exchange phase.
        let deadline = Duration::from_secs(config.proxy_timeout_secs.max(1));
        // @cpt-end:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-deadline

        // @cpt-begin:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-link-if
        // The endpoint host cannot be resolved or reached at all: the dial is
        // refused as a link failure rather than attempted against an address
        // the connector would have to unwrap.
        let address = (request.host.as_str(), request.port.unwrap_or(443))
            .to_socket_addrs()
            .map_err(|_| {
                // @cpt-begin:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-link-return
                link_unavailable()
                // @cpt-end:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-link-return
            })?
            .next()
            .ok_or_else(link_unavailable)?;
        // @cpt-end:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-link-if

        let mut options = PeerOptions::new();
        options.connection_timeout = Some(deadline);
        options.total_connection_timeout = Some(deadline);
        options.read_timeout = Some(deadline);
        options.write_timeout = Some(deadline);
        options.verify_cert = true;
        options.verify_hostname = true;
        options.alpn = advertised(preference, transport);
        let mut peer = HttpPeer::new(address, transport == Transport::Tls, request.host.clone());
        peer.options = options;

        // @cpt-begin:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-send
        // The request is sent once. The connector's own endpoint and connection
        // attempts stay inside it; the gateway never re-issues the client
        // request.
        // @cpt-begin:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-send-else
        // The ELSE of the three deadline and link checks: the endpoint
        // resolved and the deadline is set, so the request is sent once.
        let (session, _reused) = client
            .get_http_session(&peer)
            .await
            .map_err(|error| forward_failure(&error))?;
        let mut live = LiveExchange {
            client,
            versions: Arc::clone(&self.versions),
            host: request.host.clone(),
            session,
            peer,
            deadline,
        };
        // The deadline is carried on the session itself from here on, so the
        // read of the answer's head and the write of the request are bounded by
        // it and not only the connection the dial established. The pump lifts
        // the read half when it takes the body over, because the body's only
        // deadline is the idle one.
        live.session.set_read_timeout(Some(deadline));
        live.session.set_write_timeout(Some(deadline));
        live.write(request).await?;
        Ok(live)
        // @cpt-end:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-send-else
        // @cpt-end:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-send
    }
}

/// The upstream half of one live exchange, after the request was written and
/// before the answer is read.
///
/// The response header is read first, because it is the boundary
/// `proxy_timeout_secs` bounds and the last moment at which the exchange can
/// still be answered as a whole. Past it, the body is read one chunk at a time
/// or consumed away into the raw stream an upgrade is carried over, and the
/// session is never returned to the shared client's pool while either is still
/// in flight.
pub struct LiveExchange {
    client: Arc<Connector>,
    versions: Arc<Mutex<HashMap<String, VersionEntry>>>,
    host: String,
    session: HttpSession,
    peer: HttpPeer,
    deadline: Duration,
}

/// The head of the upstream's answer, as received.
#[derive(Debug, Clone)]
pub struct ResponseHead {
    /// The response status as the upstream sent it.
    pub status: u16,
    /// The response headers, lower-cased and in arrival order.
    pub headers: Vec<(String, String)>,
}

impl ResponseHead {
    /// The first value of one header, compared case-insensitively.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

impl LiveExchange {
    /// Writes the outbound request over the session the dial opened.
    ///
    /// # Errors
    ///
    /// Returns the `ProtocolError` failure for a request header that cannot be
    /// built, and the failures the connector reports for a write that outlives
    /// the deadline.
    #[allow(clippy::result_large_err)]
    async fn write(&mut self, request: &OutboundRequest) -> Result<(), DomainError> {
        // The outbound path already carries the query the route admitted.
        let path_and_query = request.path.clone();
        let mut header = RequestHeader::build(
            request.method.as_str(),
            path_and_query.as_bytes(),
            Some(request.headers.len()),
        )
        .map_err(|error| protocol_failure(&error))?;
        for (name, value) in &request.headers {
            header
                .insert_header(name.clone(), value.as_str())
                .map_err(|error| protocol_failure(&error))?;
        }
        // The gateway holds the whole request body, so its length is known and is
        // stated: a body sent without a length has no framing an HTTP/1.1 reader
        // can trust, and the upstream would read it as an empty one. This is the
        // framing half of `inst-fwd-send`, which the single send carries.
        if !request.body.is_empty()
            && !request
                .headers
                .iter()
                .any(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        {
            header
                .insert_header("content-length", request.body.len().to_string())
                .map_err(|error| protocol_failure(&error))?;
        }
        self.session
            .write_request_header(Box::new(header))
            .await
            .map_err(|error| forward_failure(&error))?;
        if request.body.is_empty() {
            self.session
                .finish_request_body()
                .await
                .map_err(|error| forward_failure(&error))?;
        } else {
            self.session
                .write_request_body(
                    bytes::Bytes::from(request.body.clone()),
                    true,
                )
                .await
                .map_err(|error| forward_failure(&error))?;
        }
        Ok(())
    }

    /// Reads the upstream's response header, which is the boundary the
    /// `RequestTimeout` deadline bounds and the last moment at which the
    /// exchange can still be answered as a whole.
    ///
    /// # Errors
    ///
    /// Returns the `RequestTimeout` failure for a header that does not arrive
    /// within the deadline, and the `ProtocolError` failure for an answer that
    /// carries no response header at all.
    #[allow(clippy::result_large_err)]
    pub async fn head(&mut self) -> Result<ResponseHead, DomainError> {
        self.session
            .read_response_header()
            .await
            .map_err(|error| forward_failure(&error))?;
        let Some(answered) = self.session.response_header() else {
            return Err(DomainError::gateway(
                ErrorKind::ProtocolError,
                "the upstream answered no response header",
            ));
        };
        self.record_negotiated();        Ok(ResponseHead {
            status: answered.status.as_u16(),
            headers: answered
                .headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_ascii_lowercase(),
                        String::from_utf8_lossy(value.as_bytes()).into_owned(),
                    )
                })
                .collect(),
        })
    }

    /// Records the HTTP version this dial negotiated, for the hour it stays
    /// valid.
    ///
    /// The detection is the adaptive per-host negotiation DESIGN §3.2 Security
    /// Considerations states, and it is recorded at the response header for the
    /// same reason the preference is read at the dial: a version is a property
    /// of the connection the dial opened and not of the answer it carried.
    fn record_negotiated(&self) {
        let negotiated = match &self.session {
            HttpSession::H2(_) => Some(Version::H2),
            HttpSession::H1(_) => Some(Version::H1),
            HttpSession::Custom(_) => None,
        };
        if let Some(version) = negotiated {
            record_version(&self.versions, &self.host, version);
        }
    }

    /// Reads the next chunk of the body, or none at its end.
    ///
    /// The caller bounds the wait with the idle deadline of the stream, which
    /// is the only deadline over a body once the response headers have arrived,
    /// so the session's own read timeout is lifted here.
    ///
    /// # Errors
    ///
    /// Returns the failure the connector reports for a read that failed.
    #[allow(clippy::result_large_err)]
    pub async fn chunk(&mut self) -> Result<Option<bytes::Bytes>, DomainError> {
        self.session.set_read_timeout(None);
        self.session
            .read_response_body()
            .await
            .map_err(|error| forward_failure(&error))
    }

    /// Reads the body to completion and returns it.
    ///
    /// # Errors
    ///
    /// Returns the failure the connector reports for a read that outlives the
    /// deadline or fails.
    #[allow(clippy::result_large_err)]
    pub async fn finish(mut self) -> Result<Vec<u8>, DomainError> {
        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = self.chunk().await? {
            body.extend_from_slice(&chunk);
        }
        self.release().await;
        Ok(body)
    }

    /// Returns the session to the shared client's pool, which closes it when
    /// its answer is not reusable — a body still in flight is not — and keeps
    /// it when it is.
    ///
    /// The pump calls this at the clean end of a body transfer. A stream that
    /// is still being transferred never reaches it: the exchange is dropped
    /// instead, which closes the connection, because a session whose body the
    /// pump has not drained cannot answer a later exchange.
    pub async fn release(self) {
        self.client
            .release_http_session(self.session, &self.peer, Some(self.deadline))
            .await;
    }

    /// Writes one chunk of the tunnel to the upstream half.
    ///
    /// A taken-up upgrade ends the message the session was carrying, so its
    /// body writer is turned to the close-delimited form that moves the bytes
    /// that belong to no message as they are written and flushes each one, and
    /// the chunk is written with no end signalled: the tunnel ends by tearing
    /// the half down and never by finishing a body.
    ///
    /// # Errors
    ///
    /// Returns the failure the session reports for a write that failed.
    #[allow(clippy::result_large_err)]
    pub async fn write_upstream(&mut self, chunk: &[u8]) -> Result<(), DomainError> {
        if let HttpSession::H1(client) = &mut self.session {
            client.maybe_upgrade_body_writer();
        }
        self.session
            .write_request_body(bytes::Bytes::copy_from_slice(chunk), false)
            .await
            .map_err(|error| forward_failure(&error))
    }

    /// Tears the session down, which closes the connection the tunnel was
    /// carried over in both directions.
    ///
    /// The exchange never reaches the shared client's pool on this path, so
    /// the abrupt giving-up the session offers is the one that applies.
    pub async fn teardown(mut self) {
        self.session.shutdown().await;
    }
}

/// The ALPN one dial advertises, from the cached preference and the transport.
fn advertised(preference: Version, transport: Transport) -> ALPN {
    match (preference, transport) {
        (Version::H2, Transport::Tls) => ALPN::H2H1,
        _ => ALPN::H1,
    }
}

/// Records the version a host negotiated, for the hour it stays valid.
fn record_version(versions: &Mutex<HashMap<String, VersionEntry>>, host: &str, version: Version) {
    versions.lock().insert(
        String::from(host),
        VersionEntry {
            version,
            recorded_at: Instant::now(),
        },
    );
}

/// The failure a connector or an exchange error is answered with, mapped onto
/// the catalogue rows the three phases of the forward name.
#[allow(clippy::result_large_err)]
fn forward_failure(error: &pingora_core::Error) -> DomainError {
    failure_of_type(&error.etype)
}

/// The catalogue failure one connector error type maps onto.
#[allow(clippy::result_large_err)]
fn failure_of_type(etype: &pingora_core::ErrorType) -> DomainError {
    match etype {
        pingora_core::ErrorType::ConnectTimedout | pingora_core::ErrorType::TLSHandshakeTimedout => {
            // @cpt-begin:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-deadline-conn-if
            // @cpt-begin:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-deadline-conn-return
            DomainError::gateway(
                ErrorKind::ConnectionTimeout,
                "the connection to the selected endpoint was not established within the deadline",
            )
            // @cpt-end:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-deadline-conn-return
            // @cpt-end:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-deadline-conn-if
        }
        pingora_core::ErrorType::ReadTimedout | pingora_core::ErrorType::WriteTimedout => {
            // @cpt-begin:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-deadline-req-if
            // @cpt-begin:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-deadline-req-return
            DomainError::gateway(
                ErrorKind::RequestTimeout,
                "the exchange with the selected endpoint exceeded the deadline",
            )
            // @cpt-end:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-deadline-req-return
            // @cpt-end:cpt-cf-oagw-algo-outbound-forward:p1:inst-fwd-deadline-req-if
        }
        pingora_core::ErrorType::ConnectRefused
        | pingora_core::ErrorType::ConnectNoRoute
        | pingora_core::ErrorType::ConnectError
        | pingora_core::ErrorType::SocketError
        | pingora_core::ErrorType::TLSHandshakeFailure
        | pingora_core::ErrorType::InvalidCert
        | pingora_core::ErrorType::HandshakeError
        | pingora_core::ErrorType::ConnectionClosed => link_unavailable(),
        _ => DomainError::gateway(
            ErrorKind::ProtocolError,
            "the exchange with the selected endpoint failed as a protocol error",
        ),
    }
}

/// The failure a response header or a request header cannot be built with.
#[allow(clippy::result_large_err)]
fn protocol_failure(error: &pingora_core::Error) -> DomainError {
    DomainError::gateway(
        ErrorKind::ProtocolError,
        format!("the outbound request or its answer is not valid: {error}"),
    )
}

/// The 503 failure an unreachable endpoint is answered with.
#[allow(clippy::result_large_err)]
fn link_unavailable() -> DomainError {
    DomainError::gateway(
        ErrorKind::LinkUnavailable,
        "the selected endpoint could not be resolved or reached",
    )
}

// @cpt-dod:cpt-cf-oagw-dod-outbound-forwarding:p1
#[cfg(test)]
mod tests {
    use super::{record_version, HTTP_VERSION_TTL, OutboundClient, Transport, Version, VersionEntry};

    #[test]
    fn a_host_with_no_entry_is_dialled_http2_preferred() {
        let client = OutboundClient::new();
        assert_eq!(
            client.preference_of("tls.vendor.com", Transport::Tls),
            Version::H2,
            "the first request advertises both so ALPN can report what it supports"
        );
    }

    #[test]
    fn a_plaintext_host_has_no_version_to_negotiate() {
        let client = OutboundClient::new();
        record_version(&client.versions, "plain.vendor.com", Version::H2);
        assert_eq!(
            client.preference_of("plain.vendor.com", Transport::Plain),
            Version::H1,
            "no ALPN runs on a plaintext dial"
        );
    }

    #[test]
    fn a_negotiated_version_is_preferred_for_the_next_request() {
        let client = OutboundClient::new();
        record_version(&client.versions, "tls.vendor.com", Version::H1);
        assert_eq!(
            client.preference_of("tls.vendor.com", Transport::Tls),
            Version::H1,
            "the second request uses the cached result"
        );
    }

    #[test]
    fn an_entry_past_its_hour_is_no_longer_used() {
        let client = OutboundClient::new();
        client.versions.lock().insert(
            String::from("tls.vendor.com"),
            VersionEntry {
                version: Version::H1,
                recorded_at: std::time::Instant::now() - HTTP_VERSION_TTL,
            },
        );
        assert_eq!(
            client.preference_of("tls.vendor.com", Transport::Tls),
            Version::H2,
            "the cache entry is dropped once its hour has passed"
        );
    }
}
