//! Protocol-upgrade proxying (WebSocket, and any other HTTP/1.1 upgrade).
//!
//! An upgrade cannot go through the message-oriented client path: after the
//! `101` the connection stops being HTTP and becomes an opaque byte stream in
//! both directions. So the handshake is written and parsed here, and the two
//! sockets are then spliced.

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use pingora_core::protocols::Stream as TransportStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::domain::error::{ErrorKind, OagwError, OagwResult};
use crate::domain::model::Endpoint;
use crate::domain::plugin::{ProxyRequest, ProxyResponseHead};

use super::connector::{UpstreamConnector, upstream_authority};

/// Largest handshake response head accepted from an upstream.
const MAX_HEAD_BYTES: usize = 64 * 1024;
/// Largest body read from a refused upgrade before it is passed back.
const MAX_REJECT_BODY_BYTES: usize = 64 * 1024;

/// What the upstream did with the upgrade offer.
pub enum UpgradeOutcome {
    /// The upstream switched protocols; the stream is now opaque.
    Switching {
        headers: HeaderMap,
        stream: TransportStream,
        /// Bytes already read past the response head — must reach the client
        /// before anything else.
        leftover: Bytes,
    },
    /// The upstream answered with an ordinary response instead.
    Rejected {
        head: ProxyResponseHead,
        body: Bytes,
    },
}

/// Whether `method`/`headers` form an HTTP/1.1 upgrade request.
#[must_use]
pub fn is_upgrade_request(method: &Method, headers: &HeaderMap) -> bool {
    if method != Method::GET {
        return false;
    }
    let connection_upgrade = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| {
            value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("upgrade"))
        });
    connection_upgrade && headers.contains_key(header::UPGRADE)
}

/// The protocol named by the `Upgrade` header, lowercased.
#[must_use]
pub fn upgrade_protocol(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_ascii_lowercase())
}

/// Perform the upgrade handshake against `endpoint`.
///
/// # Errors
///
/// `502` when the upstream speaks something that is not HTTP, `504` when the
/// handshake does not complete within `timeout`.
pub async fn perform_upgrade(
    connector: &UpstreamConnector,
    endpoint: &Endpoint,
    request: &ProxyRequest,
    timeout: std::time::Duration,
) -> OagwResult<UpgradeOutcome> {
    let mut stream = connector.open_stream(endpoint).await?;
    let handshake = handshake(&mut stream, endpoint, request);

    match tokio::time::timeout(timeout, handshake).await {
        Ok(Ok(HandshakeResult::Switching { headers, leftover })) => Ok(UpgradeOutcome::Switching {
            headers,
            stream,
            leftover,
        }),
        Ok(Ok(HandshakeResult::Rejected { head, body })) => {
            Ok(UpgradeOutcome::Rejected { head, body })
        }
        Ok(Err(err)) => Err(err),
        Err(_) => Err(OagwError::new(
            ErrorKind::RequestTimeout,
            format!(
                "upstream '{}' did not complete the protocol upgrade within {}s",
                endpoint.host,
                timeout.as_secs()
            ),
        )
        .with("host", endpoint.host.clone())),
    }
}

enum HandshakeResult {
    Switching {
        headers: HeaderMap,
        leftover: Bytes,
    },
    Rejected {
        head: ProxyResponseHead,
        body: Bytes,
    },
}

async fn handshake(
    stream: &mut TransportStream,
    endpoint: &Endpoint,
    request: &ProxyRequest,
) -> OagwResult<HandshakeResult> {
    let wire = serialize_request(endpoint, request);
    stream
        .write_all(&wire)
        .await
        .map_err(|err| io_failure(endpoint, "write upgrade request", &err))?;
    stream
        .flush()
        .await
        .map_err(|err| io_failure(endpoint, "flush upgrade request", &err))?;

    let (head_len, buffer) = read_head(stream, endpoint).await?;
    let mut header_slots = [httparse::EMPTY_HEADER; 96];
    let mut parsed = httparse::Response::new(&mut header_slots);
    let status_of = |parsed: &httparse::Response<'_, '_>| parsed.code.unwrap_or(0);

    match parsed.parse(&buffer[..head_len]) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) | Err(_) => {
            return Err(OagwError::new(
                ErrorKind::ProtocolError,
                format!(
                    "upstream '{}' returned a malformed HTTP response to the upgrade request",
                    endpoint.host
                ),
            ));
        }
    }

    let code = status_of(&parsed);
    let status = StatusCode::from_u16(code).map_err(|_| {
        OagwError::new(
            ErrorKind::ProtocolError,
            format!("upstream '{}' returned status {code}", endpoint.host),
        )
    })?;

    let mut headers = HeaderMap::with_capacity(parsed.headers.len());
    for parsed_header in parsed.headers.iter() {
        let Ok(name) = HeaderName::from_bytes(parsed_header.name.as_bytes()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_bytes(parsed_header.value) else {
            continue;
        };
        headers.append(name, value);
    }

    if status == StatusCode::SWITCHING_PROTOCOLS {
        return Ok(HandshakeResult::Switching {
            headers,
            leftover: Bytes::copy_from_slice(&buffer[head_len..]),
        });
    }

    // Not an upgrade after all: read what the upstream said so the client sees
    // its answer rather than a synthetic gateway error.
    let mut body = buffer[head_len..].to_vec();
    let content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<usize>().ok());
    if let Some(expected) = content_length {
        let expected = expected.min(MAX_REJECT_BODY_BYTES);
        while body.len() < expected {
            let mut chunk = vec![0_u8; (expected - body.len()).min(8192)];
            match stream.read(&mut chunk).await {
                Ok(0) => break,
                Ok(read) => body.extend_from_slice(&chunk[..read]),
                Err(err) => return Err(io_failure(endpoint, "read upgrade response body", &err)),
            }
        }
    }

    Ok(HandshakeResult::Rejected {
        head: ProxyResponseHead { status, headers },
        body: Bytes::from(body),
    })
}

/// Read until the end of the response head, returning `(head_len, buffer)`.
async fn read_head(
    stream: &mut TransportStream,
    endpoint: &Endpoint,
) -> OagwResult<(usize, Vec<u8>)> {
    let mut buffer: Vec<u8> = Vec::with_capacity(1024);
    let mut scan_from = 0_usize;
    loop {
        let mut chunk = [0_u8; 4096];
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|err| io_failure(endpoint, "read upgrade response", &err))?;
        if read == 0 {
            return Err(OagwError::new(
                ErrorKind::StreamAborted,
                format!(
                    "upstream '{}' closed the connection during the protocol upgrade",
                    endpoint.host
                ),
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);

        if let Some(offset) = find_head_end(&buffer, scan_from) {
            return Ok((offset, buffer));
        }
        scan_from = buffer.len().saturating_sub(3);

        if buffer.len() > MAX_HEAD_BYTES {
            return Err(OagwError::new(
                ErrorKind::ProtocolError,
                format!(
                    "upstream '{}' sent an oversized response head during the upgrade",
                    endpoint.host
                ),
            ));
        }
    }
}

/// Offset just past the first `\r\n\r\n`, searching from `from`.
fn find_head_end(buffer: &[u8], from: usize) -> Option<usize> {
    buffer[from..]
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| from + position + 4)
}

/// Render the upgrade request in HTTP/1.1 wire format.
fn serialize_request(endpoint: &Endpoint, request: &ProxyRequest) -> Vec<u8> {
    let mut wire = format!(
        "{} {} HTTP/1.1\r\n",
        request.method.as_str(),
        request.path_and_query()
    );
    wire.push_str(&format!("Host: {}\r\n", upstream_authority(endpoint)));
    for (name, value) in &request.headers {
        if name == header::HOST {
            continue;
        }
        if let Ok(value) = value.to_str() {
            wire.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    wire.push_str("\r\n");
    wire.into_bytes()
}

fn io_failure(endpoint: &Endpoint, phase: &str, err: &std::io::Error) -> OagwError {
    OagwError::new(
        ErrorKind::StreamAborted,
        format!("upstream '{}' failed during {phase}: {err}", endpoint.host),
    )
    .with("host", endpoint.host.clone())
}

/// Splice a client connection and an upstream stream until either side closes.
///
/// `leftover` is whatever was already read past the upstream's response head;
/// it must be delivered before the copy loop starts or the first WebSocket
/// frame is lost.
pub async fn splice<C>(
    mut client: C,
    mut upstream: TransportStream,
    leftover: Bytes,
) -> std::io::Result<(u64, u64)>
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if !leftover.is_empty() {
        client.write_all(&leftover).await?;
        client.flush().await?;
    }
    tokio::io::copy_bidirectional(&mut client, &mut upstream).await
}

#[cfg(test)]
#[path = "websocket_tests.rs"]
mod tests;
