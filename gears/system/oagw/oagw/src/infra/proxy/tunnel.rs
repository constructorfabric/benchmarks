//! WebSocket upgrade relay.
//!
//! The upstream is contacted over a raw HTTP/1.1 connection so the tunnel can
//! relay bytes in both directions without re-framing WebSocket messages.

use std::sync::Arc;
use std::time::Duration;

use http::{HeaderMap, StatusCode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, EndpointScheme};
use crate::infra::proxy::headers;

/// Header terminator of an HTTP/1.1 response.
const TERMINATOR: &[u8] = b"\r\n\r\n";
/// Upper bound on the upstream handshake response head.
const MAX_HEAD: usize = 16 * 1024;
/// Header slots handed to the handshake response parser.
const MAX_RESPONSE_HEADERS: usize = 64;

/// The upstream handshake result.
pub struct UpstreamUpgrade {
    /// Status returned by the upstream.
    pub status: u16,
    /// Headers to copy back to the client.
    pub headers: Vec<(String, String)>,
    /// Bytes already read past the response head.
    pub leftover: Vec<u8>,
    /// The connected upstream socket.
    pub stream: tokio::net::TcpStream,
}

/// Serialized request head to write to the upstream socket.
#[must_use]
pub fn encode_request(
    method: &http::Method,
    path: &str,
    host: &str,
    inbound: &HeaderMap,
) -> Vec<u8> {
    let mut out = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\n");
    for (name, value) in inbound {
        let name = name.as_str();
        if name.eq_ignore_ascii_case(headers::TARGET_HOST)
            || name.eq_ignore_ascii_case(http::header::HOST.as_str())
            || headers::is_hop_by_hop(name)
        {
            continue;
        }
        if let Ok(value) = value.to_str() {
            out.push_str(name);
            out.push_str(": ");
            out.push_str(value);
            out.push_str("\r\n");
        }
    }
    out.push_str("Connection: Upgrade\r\nUpgrade: websocket\r\n\r\n");
    out.into_bytes()
}

/// A connected upstream socket handed to the relay by the data plane.
///
/// It travels in the 101 response's extensions because only the handler, which
/// owns the upgraded client socket, can start the relay.
#[derive(Clone, Default)]
pub struct TunnelHandle {
    /// The socket already handshaken with the upstream, taken by the relay.
    pub stream: Arc<std::sync::Mutex<Option<tokio::net::TcpStream>>>,
}

impl TunnelHandle {
    /// Wrap a freshly handshaken socket.
    #[must_use]
    pub fn new(stream: tokio::net::TcpStream) -> Self {
        Self {
            stream: Arc::new(std::sync::Mutex::new(Some(stream))),
        }
    }

    /// Take the socket out, leaving the handle empty.
    #[must_use]
    pub fn take(&self) -> Option<tokio::net::TcpStream> {
        self.stream.lock().ok().and_then(|mut slot| slot.take())
    }
}

/// A request target with the query string re-attached.
#[must_use]
pub fn with_query(path: &str, query: Option<&str>) -> String {
    match query.filter(|value| !value.is_empty()) {
        Some(query) => format!("{path}?{query}"),
        None => path.to_owned(),
    }
}

/// Connect upstream, send the upgrade request and read the response head.
///
/// # Errors
/// Returns [`DomainError::LinkUnavailable`] on a connection failure and
/// [`DomainError::RequestTimeout`] when the upstream does not answer in time.
pub async fn dial(
    endpoint: &Endpoint,
    head: &[u8],
    timeout: Duration,
) -> Result<UpstreamUpgrade, DomainError> {
    let addr = format!("{}:{}", endpoint.host, endpoint.port);
    let connect = tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&addr)).await;
    let Ok(Ok(stream)) = connect else {
        return Err(DomainError::LinkUnavailable(format!(
            "failed to connect to '{addr}'"
        )));
    };
    let mut stream = stream;
    stream
        .write_all(head)
        .await
        .map_err(|err| DomainError::LinkUnavailable(err.to_string()))?;
    stream
        .flush()
        .await
        .map_err(|err| DomainError::LinkUnavailable(err.to_string()))?;

    let mut head_bytes = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let read = tokio::time::timeout(timeout, stream.read(&mut byte)).await;
        match read {
            Ok(Ok(0)) => {
                return Err(DomainError::StreamAborted);
            }
            Ok(Ok(_)) => {
                head_bytes.push(byte[0]);
                if head_bytes.ends_with(TERMINATOR) {
                    break;
                }
                if head_bytes.len() > MAX_HEAD {
                    return Err(DomainError::ProtocolError(
                        "upstream handshake response is too large".to_owned(),
                    ));
                }
            }
            Ok(Err(err)) => return Err(DomainError::LinkUnavailable(err.to_string())),
            Err(_) => return Err(DomainError::RequestTimeout),
        }
    }
    parse_response(&head_bytes, stream)
}

/// Parse the upstream handshake response.
///
/// # Errors
/// Returns [`DomainError::ProtocolError`] for a malformed response.
pub fn parse_response(
    head: &[u8],
    stream: tokio::net::TcpStream,
) -> Result<UpstreamUpgrade, DomainError> {
    let status = parse_status(head)?;
    let headers = parse_headers(head);
    Ok(UpstreamUpgrade {
        status,
        headers,
        leftover: Vec::new(),
        stream,
    })
}

/// The status code of a parsed HTTP/1.1 response head.
fn parse_status(head: &[u8]) -> Result<u16, DomainError> {
    let mut slots = [httparse::EMPTY_HEADER; MAX_RESPONSE_HEADERS];
    let parsed = httparse::Response::new(&mut slots)
        .parse(head)
        .map_err(|err| DomainError::ProtocolError(format!("malformed upstream response: {err}")))?;
    match parsed {
        httparse::Status::Complete(offset) => {
            let head = std::str::from_utf8(&head[..offset]).map_err(|_| {
                DomainError::ProtocolError("upstream response is not UTF-8".to_owned())
            })?;
            let status = head
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .and_then(|code| code.parse::<u16>().ok());
            status.ok_or_else(|| {
                DomainError::ProtocolError("upstream sent no status code".to_owned())
            })
        }
        httparse::Status::Partial => Err(DomainError::ProtocolError(
            "upstream response is incomplete".to_owned(),
        )),
    }
}

/// The header pairs of a parsed HTTP/1.1 response head.
fn parse_headers(head: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(text) = std::str::from_utf8(head) else {
        return out;
    };
    let mut lines = text.split("\r\n").peekable();
    while let Some(line) = lines.next() {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if lines.peek().is_none() {
            break;
        }
        out.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
    }
    out
}

/// Headers copied from the upstream handshake to the client response.
#[must_use]
pub fn relay_headers(upstream_headers: &[(String, String)]) -> Vec<(String, String)> {
    upstream_headers
        .iter()
        .filter(|(name, _)| {
            name.eq_ignore_ascii_case("upgrade")
                || name.eq_ignore_ascii_case("connection")
                || name.eq_ignore_ascii_case("sec-websocket-accept")
                || name.eq_ignore_ascii_case("sec-websocket-protocol")
                || name.eq_ignore_ascii_case("sec-websocket-extensions")
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

/// Whether a request asks for a protocol upgrade.
#[must_use]
pub fn is_upgrade_request(method: &http::Method, headers: &HeaderMap) -> bool {
    method == http::Method::GET
        && headers
            .get(http::header::CONNECTION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"))
        && headers
            .get(http::header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.to_ascii_lowercase().contains("websocket"))
}

/// Whether a scheme can be reached over a plaintext socket.
#[must_use]
pub const fn is_plaintext(scheme: EndpointScheme) -> bool {
    matches!(scheme, EndpointScheme::Http)
}

/// Status of a successful upgrade.
#[must_use]
pub const fn switching_protocols() -> StatusCode {
    StatusCode::SWITCHING_PROTOCOLS
}

/// Relay bytes between the client and the upstream until either side closes.
pub async fn relay(client: hyper::upgrade::Upgraded, upstream: tokio::net::TcpStream) {
    let mut client = crate::infra::proxy::compat::HyperToTokio::new(client);
    let mut upstream = upstream;
    drop(tokio::io::copy_bidirectional(&mut client, &mut upstream).await);
}
