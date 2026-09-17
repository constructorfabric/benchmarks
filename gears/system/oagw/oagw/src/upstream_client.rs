// Created: 2026-09-03 by Constructor Tech
//! Upstream transport: dial, optional TLS and HTTP/1.1 dispatch.
//!
//! The client is deliberately thin: one connection per request, no retry and
//! no buffering, so that streaming and upgrades pass through untouched
//! (`DESIGN.md` "Non-functional requirements": no automatic retries).

use std::time::Duration;

use hyper::body::Incoming;
use hyper_util::rt::TokioIo;
use pingora_core::connectors::TransportConnector;
use pingora_core::protocols::Stream;
use pingora_core::upstreams::peer::HttpPeer;

use crate::error::{ErrorKind, OagwError};
use crate::model::EndpointScheme;

/// Reusable upstream connection factory.
///
/// One connector is shared by the whole gear: it carries the TLS
/// configuration and the connection pool.
pub struct UpstreamConnector {
    inner: TransportConnector,
}

impl UpstreamConnector {
    /// Builds a connector with the pingora defaults.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: TransportConnector::new(None),
        }
    }
}

impl Default for UpstreamConnector {
    fn default() -> Self {
        Self::new()
    }
}

/// Establishes an upstream connection, honouring `connect_timeout`.
///
/// # Errors
/// Returns 503 `LinkUnavailable` when the TCP or TLS handshake fails and 504
/// `ConnectionTimeout` when the budget elapses.
pub async fn dial(
    connector: &UpstreamConnector,
    scheme: EndpointScheme,
    host: &str,
    port: u16,
    connect_timeout: Duration,
) -> Result<Stream, OagwError> {
    let address = format!("{host}:{port}");
    let sni = host.to_owned();
    let resolved = tokio::net::lookup_host(&address)
        .await
        .ok()
        .and_then(|mut candidates| candidates.next())
        .ok_or_else(|| {
            OagwError::new(
                ErrorKind::LinkUnavailable,
                format!("upstream address {address} could not be resolved"),
            )
        })?;
    let peer = HttpPeer::new(resolved, scheme.is_tls(), sni);
    let dialled = tokio::time::timeout(connect_timeout, connector.inner.new_stream(&peer))
        .await
        .map_err(|_| {
            OagwError::new(
                ErrorKind::ConnectionTimeout,
                format!("connecting to {address} exceeded the connection budget"),
            )
        })?
        .map_err(|error| {
            OagwError::new(
                ErrorKind::LinkUnavailable,
                format!("could not connect to {address}: {error}"),
            )
        })?;
    Ok(dialled)
}

/// Dispatches an HTTP/1.1 request on an established connection.
///
/// # Errors
/// Returns 504 `RequestTimeout` when the overall budget elapses, 504
/// `IdleTimeout` while streaming after the headers and 502 `ProtocolError`
/// for malformed upstream exchanges.
pub async fn dispatch(
    io: Stream,
    request: http::Request<crate::body::ProxyBody>,
    request_timeout: Duration,
) -> Result<http::Response<Incoming>, OagwError> {
    let io = TokioIo::new(io);
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake(io).await.map_err(|error| {
            OagwError::new(
                ErrorKind::ProtocolError,
                format!("upstream connection could not be established: {error}"),
            )
        })?;
    tokio::spawn(async move {
        if let Err(error) = connection.with_upgrades().await {
            tracing::debug!(%error, "upstream connection closed");
        }
    });
    let response = tokio::time::timeout(request_timeout, sender.send_request(request))
        .await
        .map_err(|_| {
            OagwError::new(
                ErrorKind::RequestTimeout,
                "upstream did not produce response headers within the request budget",
            )
        })?
        .map_err(|error| {
            if error.is_timeout() {
                OagwError::new(
                    ErrorKind::IdleTimeout,
                    format!("upstream connection went idle: {error}"),
                )
            } else if error.is_parse() || error.is_user() {
                OagwError::new(
                    ErrorKind::ProtocolError,
                    format!("upstream spoke an unusable HTTP exchange: {error}"),
                )
            } else {
                OagwError::new(
                    ErrorKind::DownstreamError,
                    format!("upstream exchange failed: {error}"),
                )
            }
        })?;
    Ok(response)
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn error_mapping_uses_gateway_statuses() {
        assert_eq!(ErrorKind::ConnectionTimeout.status(), 504);
        assert_eq!(ErrorKind::LinkUnavailable.status(), 503);
        assert_eq!(ErrorKind::ProtocolError.status(), 502);
    }
}
