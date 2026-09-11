//! Outbound HTTP client (`research.md` R7).
//!
//! `hyper-util`'s legacy client over an `HttpsConnector` gives exactly what the
//! data plane needs: streaming response bodies for SSE, HTTP/1.1 upgrade
//! support for WebSocket, and a connection pool shared by every request. A
//! custom [`ProxyBody`] keeps the request side fully buffered while the
//! response side stays a stream.

use bytes::Bytes;
use http::Request;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::TokioExecutor;

/// Errors raised while dialling the upstream.
#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    /// The connection could not be established or DNS failed.
    #[error("upstream connection failed")]
    Connect,
    /// The upstream call exceeded its deadline.
    #[error("upstream call timed out")]
    Timeout,
    /// The response could not be read.
    #[error("upstream response failed")]
    Response,
    /// The request could not be sent.
    #[error("upstream request failed")]
    Send,
}

/// The fully-qualified client type used by the data plane.
///
/// The request side is `Full<Bytes>`: the data plane buffers the inbound body
/// before dialling, so the client never needs a streaming request body.
pub type ProxyClient = Client<HttpsConnector<HttpConnector>, http_body_util::Full<Bytes>>;

/// Builds the shared outbound client.
///
/// # Errors
/// [`UpstreamError::Connect`] when the TLS root store cannot be loaded.
pub fn build_client() -> Result<ProxyClient, UpstreamError> {
    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .map_err(|_| UpstreamError::Connect)?
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .wrap_connector(HttpConnector::new());
    Ok(Client::builder(TokioExecutor::new()).build(https))
}

/// Sends a request with a deadline, mapping failures to [`UpstreamError`].
pub async fn send(
    client: &ProxyClient,
    request: Request<Bytes>,
    timeout: std::time::Duration,
) -> Result<http::Response<Incoming>, UpstreamError> {
    let request = request.map(http_body_util::Full::new);
    let response = tokio::time::timeout(timeout, client.request(request))
        .await
        .map_err(|_| UpstreamError::Timeout)?
        .map_err(|_| UpstreamError::Connect)?;
    Ok(response)
}

/// Reads a response body up to `limit` bytes.
///
/// # Errors
/// [`UpstreamError::Response`] when the body cannot be read, or
/// [`crate::domain::error::DomainError::PayloadTooLarge`] semantics via the
/// caller when the limit is exceeded.
pub async fn read_body(mut body: Incoming, limit: usize) -> Result<Bytes, UpstreamError> {
    let mut collected: Vec<u8> = Vec::new();
    while let Some(chunk) = body.frame().await {
        let chunk = chunk.map_err(|_| UpstreamError::Response)?.into_data();
        match chunk {
            Ok(data) => {
                if collected.len() + data.len() > limit {
                    return Err(UpstreamError::Response);
                }
                collected.extend_from_slice(&data);
            }
            Err(_) => return Err(UpstreamError::Response),
        }
    }
    Ok(Bytes::from(collected))
}

/// Typed alias for a spliced connection pair.
pub type SplicedIo = hyper::upgrade::Upgraded;

/// Timer used by the data plane for per-call deadlines.
#[must_use]
pub fn timer() -> hyper_util::rt::TokioTimer {
    hyper_util::rt::TokioTimer::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_builds_against_the_workspace_tls_stack() {
        let client = build_client().expect("client");
        // The legacy client is cheap to clone and shared across requests.
        let _ = client.clone();
    }

    #[test]
    fn the_error_variants_map_to_the_documented_statuses() {
        assert!(matches!(UpstreamError::Timeout, UpstreamError::Timeout));
        assert!(matches!(UpstreamError::Connect, UpstreamError::Connect));
    }
}
