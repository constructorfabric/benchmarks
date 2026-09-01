//! The outbound HTTP client (`DESIGN` §3.2, `infra/proxy/transport`).
//!
//! One shared hyper client dials every upstream. A request is sent once: the
//! response *headers* are bounded by the deployment's `proxy_timeout_secs`
//! budget and the body is then forwarded as a stream, so an SSE answer reaches
//! the caller chunk by chunk while the upstream is still writing it. Nothing
//! here retries — `DESIGN` §3.3 makes every timeout a terminal `504`.
//!
//! WebSocket upgrades are tunnelled: the upstream is dialled over HTTP/1.1 with
//! the caller's `Upgrade` handshake, and both upgraded streams are then spliced
//! together with `tokio::io::copy_bidirectional`.

use std::sync::Arc;
use std::time::Duration;

use http::{Request, Response, Uri};
use http_body_util::BodyExt;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

use crate::domain::error::DomainError;
use crate::infra::proxy::policy::ProxyPolicy;

/// The connector this transport dials with: TLS for `https`, plaintext for the
/// `allow_http_upstream` deployments.
type Connector = hyper_rustls::HttpsConnector<HttpConnector>;

/// The hyper client behind every upstream exchange.
type UpstreamClient = Client<Connector, axum::body::Body>;

/// The upgraded raw stream of a WebSocket tunnel.
pub type TunnelStream = hyper::upgrade::Upgraded;

/// The outbound HTTP transport of the data plane.
#[derive(Clone)]
pub struct UpstreamTransport {
    client: Arc<UpstreamClient>,
    proxy_timeout: Duration,
}

impl std::fmt::Debug for UpstreamTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamTransport")
            .field("proxy_timeout", &self.proxy_timeout)
            .finish_non_exhaustive()
    }
}

impl UpstreamTransport {
    /// A transport honouring `policy`.
    ///
    /// # Errors
    /// Returns [`DomainError::Internal`] when the TLS root store cannot be
    /// loaded; the data plane cannot start without it.
    pub fn new(policy: &ProxyPolicy) -> Result<Self, DomainError> {
        let mut http = HttpConnector::new();
        http.set_connect_timeout(Some(policy.proxy_timeout));
        http.set_nodelay(true);

        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()
            .map_err(|error| {
                tracing::error!(diagnostic = %error, "tls root store unavailable");
                DomainError::internal("the TLS root store could not be loaded", error)
            })?
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);

        let client = Client::builder(TokioExecutor::new()).build(https);
        Ok(Self {
            client: Arc::new(client),
            proxy_timeout: policy.proxy_timeout,
        })
    }

    /// Send one request and return the upstream response with its body still
    /// streaming.
    ///
    /// # Errors
    /// Returns [`DomainError::ConnectionTimeout`] when the endpoint cannot even
    /// be reached in the budget, [`DomainError::RequestTimeout`] when the
    /// response headers exceed it, [`DomainError::LinkUnavailable`] when the
    /// endpoint cannot be reached, and [`DomainError::ProtocolError`] when the
    /// upstream speaks something that is not HTTP.
    pub async fn send(
        &self,
        request: Request<axum::body::Body>,
    ) -> Result<Response<axum::body::Body>, DomainError> {
        let authority = authority_of(&request);
        let exchange = tokio::time::timeout(self.proxy_timeout, self.client.request(request));
        let response = match exchange.await {
            Ok(Ok(response)) => response,
            Err(_) => return Err(self.timeout()),
            Ok(Err(error)) => return Err(Self::transport_error(&authority, &error)),
        };
        let (mut parts, body) = response.into_parts();
        // The size the upstream declared is the size the client body delivers:
        // carrying it keeps the answer framed as the upstream framed it instead
        // of re-encoding a `Content-Length` body as chunked.
        if let Some(size) = declared_size(&parts.headers) {
            parts.extensions.insert(ExactBodySize(size));
        }
        let stream = body.into_data_stream();
        Ok(Response::from_parts(
            parts,
            axum::body::Body::from_stream(stream),
        ))
    }

    /// Open a WebSocket tunnel: the upstream `101` headers plus its side of the
    /// upgraded connection.
    ///
    /// # Errors
    /// Returns the same errors as [`Self::send`], plus
    /// [`DomainError::ProtocolError`] when the upstream refuses the upgrade and
    /// [`DomainError::DownstreamError`] when the tunnel cannot be established.
    pub async fn tunnel(
        &self,
        request: Request<axum::body::Body>,
    ) -> Result<(http::response::Parts, TunnelStream), DomainError> {
        let authority = authority_of(&request);
        let exchange = tokio::time::timeout(self.proxy_timeout, self.client.request(request));
        let response = match exchange.await {
            Ok(Ok(response)) => response,
            Err(_) => return Err(self.timeout()),
            Ok(Err(error)) => return Err(Self::transport_error(&authority, &error)),
        };

        let (mut parts, body) = response.into_parts();
        if parts.status != http::StatusCode::SWITCHING_PROTOCOLS {
            return Err(DomainError::ProtocolError {
                detail: format!(
                    "upstream answered {} instead of switching protocols",
                    parts.status.as_u16()
                ),
                trace_id: None,
            });
        }
        let on_upgrade = parts
            .extensions
            .remove::<hyper::upgrade::OnUpgrade>()
            .ok_or_else(|| DomainError::ProtocolError {
                detail: "upstream did not offer an upgraded connection".to_owned(),
                trace_id: None,
            })?;
        // The client connection is driven by the response body: dropping it
        // before the upgrade is handed over cancels the exchange. The body is
        // only released once the upgraded stream has arrived.
        let upgraded = on_upgrade
            .await
            .map_err(|error| DomainError::ProtocolError {
                detail: format!("upstream upgrade failed: {error}"),
                trace_id: None,
            })?;
        drop(body);
        Ok((parts, upgraded))
    }

    /// The `504` the budget produces.
    fn timeout(&self) -> DomainError {
        DomainError::RequestTimeout {
            limit_secs: self.proxy_timeout.as_secs(),
        }
    }

    /// Map a transport failure onto the canonical identity it belongs to.
    fn transport_error(authority: &str, error: &hyper_util::client::legacy::Error) -> DomainError {
        // A dial that never completes is a connection timeout (`DESIGN` §3.3:
        // `timeout.connection.v1`), not a `503`: the endpoint answered nothing
        // at all, so the caller may retry a slower one.
        if error.is_connect() {
            // A dial that ran out of its budget is a connection timeout
            // (`DESIGN` §3.3, `timeout.connection.v1`), not a `503`: the
            // endpoint answered nothing at all, so the caller may retry a
            // slower one.
            if let Some(hyper) = hyper_source(error)
                && hyper.is_timeout()
            {
                return DomainError::connection_timeout(authority, 0);
            }
            return DomainError::LinkUnavailable {
                detail: format!("upstream could not be reached: {error}"),
                retry_after: None,
            };
        }
        if let Some(hyper) = hyper_source(error) {
            // The caller walked away, or the body it was streaming failed: the
            // exchange never completed, which is a downstream abort rather than
            // a protocol error the upstream produced.
            if hyper.is_body_write_aborted() || hyper.is_canceled() {
                return DomainError::downstream_error(format!(
                    "the exchange with the upstream was aborted: {error}"
                ));
            }
            if hyper.is_timeout() {
                return DomainError::request_timeout(0);
            }
        }
        DomainError::ProtocolError {
            detail: format!("upstream exchange failed: {error}"),
            trace_id: None,
        }
    }
}

/// The `hyper` failure a client error carries, if any: the client only
/// classifies connects, so the finer timeouts and aborts are read off its
/// source.
fn hyper_source(error: &hyper_util::client::legacy::Error) -> Option<&hyper::Error> {
    let mut source: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(error);
    while let Some(cause) = source {
        if let Some(hyper) = cause.downcast_ref::<hyper::Error>() {
            return Some(hyper);
        }
        source = cause.source();
    }
    None
}

/// The body size an upstream declared, when it declared one.
fn declared_size(headers: &http::HeaderMap) -> Option<u64> {
    headers
        .get(http::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The exact size an upstream declared for its body, when it declared one.
#[derive(Debug, Clone, Copy)]
pub struct ExactBodySize(pub u64);

/// The endpoint a request is dialled against, for the errors that name it.
fn authority_of(request: &Request<axum::body::Body>) -> String {
    request
        .uri()
        .host()
        .map_or_else(String::new, ToOwned::to_owned)
}

/// The absolute URL of an upstream exchange: endpoint, path and query.
///
/// The authority is what the connector dials and what `Host` becomes; the path
/// and query are the request target the upstream sees, so they travel in one
/// `scheme://authority/path?query` URI.
#[must_use]
pub fn outbound_uri(endpoint_url: &str, path: &str, query: &[(String, String)]) -> Uri {
    let encoded = form_urlencoded::Serializer::new(String::new())
        .extend_pairs(
            query
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        )
        .finish();
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    let target = if encoded.is_empty() {
        format!("{endpoint_url}{path}")
    } else {
        format!("{endpoint_url}{path}?{encoded}")
    };
    target.parse().unwrap_or_else(|_| path_only(&path))
}

/// The best effort when the endpoint URL itself is malformed: the path, alone,
/// so the request target is still the one the route selected.
fn path_only(path: &str) -> Uri {
    path.parse().unwrap_or_else(|_| Uri::from_static("/"))
}
