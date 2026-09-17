//! Upstream HTTP transport (hyper-util legacy client).
//!
//! Bodies are **streamed** end to end: the downstream request body is handed
//! to hyper as a stream and the upstream response body is handed back to axum
//! as a stream, so an SSE exchange relays chunk-by-chunk and a 100 MiB upload
//! is never buffered.
//!
//! TLS is provided by `hyper-rustls` with the process-default rustls crypto
//! provider; `http` and `https` are both dialled by the same connector
//! (`https_or_http`), and the *cleartext* choice is a policy decision made
//! here (`allow_http_upstream`), never a schema validation.

use std::time::Duration;

use axum::body::Body;
use http::{Request, Response, Uri};
use hyper::body::Incoming;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioIo};

use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, Scheme};

/// The concrete client type used for every upstream exchange.
pub type ProxyClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Body>;

/// Why an upstream exchange could not be completed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// The configured policy refuses to dial a cleartext upstream.
    PlaintextDisabled {
        /// Host that would have been dialled.
        host: String,
    },
    /// The URI could not be built from the endpoint.
    InvalidUri {
        /// Human-readable explanation.
        detail: String,
    },
    /// No connection could be established.
    Connect {
        /// Human-readable explanation (never includes credentials).
        detail: String,
    },
    /// The exchange exceeded the configured budget.
    Timeout {
        /// Human-readable explanation.
        detail: String,
    },
}

impl From<TransportError> for DomainError {
    fn from(err: TransportError) -> Self {
        match err {
            TransportError::PlaintextDisabled { host } => DomainError::LinkUnavailable {
                detail: format!(
                    "upstream `{host}` uses a cleartext scheme and `allow_http_upstream` is disabled"
                ),
                retry_after_seconds: None,
            },
            TransportError::InvalidUri { detail } => DomainError::ProtocolError { detail },
            TransportError::Connect { detail } => DomainError::DownstreamError {
                detail,
                upstream_id: None,
                host: None,
                path: None,
            },
            TransportError::Timeout { detail } => DomainError::ConnectionTimeout {
                detail,
                retry_after_seconds: None,
            },
        }
    }
}

/// Build the shared upstream client.
///
/// # Errors
/// Returns an error when the platform root store cannot be loaded.
pub fn build_client() -> Result<ProxyClient, String> {
    let connector = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .map_err(|err| format!("unable to load the platform root certificates: {err}"))?
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();
    Ok(Client::builder(TokioExecutor::new()).build(connector))
}

/// The upstream transport: client, timeouts and dial policy.
pub struct ProxyTransport {
    client: ProxyClient,
    timeout: Duration,
    allow_http: bool,
}

impl std::fmt::Debug for ProxyTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyTransport")
            .field("timeout_secs", &self.timeout.as_secs())
            .field("allow_http", &self.allow_http)
            .finish_non_exhaustive()
    }
}

impl ProxyTransport {
    /// A transport over the given client.
    #[must_use]
    pub fn new(client: ProxyClient, timeout: Duration, allow_http: bool) -> Self {
        Self {
            client,
            timeout,
            allow_http,
        }
    }

    /// Build a transport with the default client.
    ///
    /// # Errors
    /// See [`build_client`].
    pub fn with_defaults(timeout: Duration, allow_http: bool) -> Result<Self, String> {
        Ok(Self::new(build_client()?, timeout, allow_http))
    }

    /// The configured total budget for an upstream exchange.
    #[must_use]
    pub const fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Whether cleartext upstreams may be dialled.
    #[must_use]
    pub const fn allows_http(&self) -> bool {
        self.allow_http
    }

    /// Build the absolute upstream URI for `endpoint` and `path`.
    ///
    /// WebSocket schemes are dialled as their HTTP equivalents; the upgrade
    /// machinery below takes over once the handshake is relayed.
    ///
    /// # Errors
    /// [`TransportError::InvalidUri`] when the endpoint does not form a URI.
    pub fn build_uri(endpoint: &Endpoint, path: &str, query: &str) -> Result<Uri, TransportError> {
        let scheme = match endpoint.scheme {
            Scheme::Wss | Scheme::Https => "https",
            Scheme::Grpc => "https",
            Scheme::Http | Scheme::Wt => "http",
        };
        let authority = endpoint.authority();
        let path = if path.starts_with('/') {
            path.to_owned()
        } else {
            format!("/{path}")
        };
        let raw = if query.is_empty() {
            format!("{scheme}://{authority}{path}")
        } else {
            format!("{scheme}://{authority}{path}?{query}")
        };
        Uri::try_from(raw).map_err(|err| TransportError::InvalidUri {
            detail: format!("unable to build the upstream URI: {err}"),
        })
    }

    /// Refuse to dial an endpoint the configuration forbids.
    ///
    /// `http` remains a legal *configuration* value unconditionally; this is
    /// purely a dial-time decision.
    ///
    /// # Errors
    /// [`TransportError::PlaintextDisabled`] when the endpoint is plaintext and
    /// `allow_http_upstream` is false.
    pub fn check_scheme(&self, endpoint: &Endpoint) -> Result<(), TransportError> {
        if endpoint.scheme.is_tls() || self.allow_http {
            return Ok(());
        }
        Err(TransportError::PlaintextDisabled {
            host: endpoint.authority(),
        })
    }

    /// Refuse to dial a URI the dial policy forbids.
    ///
    /// Same rule as [`ProxyTransport::check_scheme`], expressed over an
    /// arbitrary URI so non-endpoint targets (the OAuth2 token endpoint) get the
    /// identical treatment.
    ///
    /// # Errors
    /// [`TransportError::PlaintextDisabled`] for a `http` URI when
    /// `allow_http_upstream` is false, [`TransportError::InvalidUri`] when the
    /// URI has no scheme or host.
    pub fn check_uri(&self, uri: &Uri) -> Result<(), TransportError> {
        let scheme = uri.scheme_str().ok_or_else(|| TransportError::InvalidUri {
            detail: "upstream URI has no scheme".to_owned(),
        })?;
        let host = uri.host().ok_or_else(|| TransportError::InvalidUri {
            detail: "upstream URI has no host".to_owned(),
        })?;
        if matches!(scheme, "https" | "wss" | "grpcs") || self.allow_http {
            return Ok(());
        }
        if matches!(scheme, "http" | "ws" | "grpc") {
            Err(TransportError::PlaintextDisabled {
                host: uri
                    .authority()
                    .map_or_else(|| host.to_owned(), ToString::to_string),
            })
        } else {
            Err(TransportError::InvalidUri {
                detail: format!("unsupported upstream URI scheme `{scheme}`"),
            })
        }
    }

    /// Send a request upstream, bounded by the configured timeout.
    ///
    /// The timeout covers establishing the connection and receiving the
    /// response *headers*; the body is streamed afterwards under its own idle
    /// timeout ([`TimedBody`]).
    ///
    /// # Errors
    /// [`TransportError`] for every transport-level failure.
    pub async fn send(&self, request: Request<Body>) -> Result<Response<Incoming>, TransportError> {
        self.send_within(request, self.timeout).await
    }

    /// Send a request upstream under an explicit budget.
    ///
    /// The budget covers establishing the connection and receiving the response
    /// headers; the body is streamed afterwards under its own idle timeout.
    ///
    /// # Errors
    /// [`TransportError`] for every transport-level failure.
    pub async fn send_within(
        &self,
        request: Request<Body>,
        budget: Duration,
    ) -> Result<Response<Incoming>, TransportError> {
        let future = self.client.request(request);
        match tokio::time::timeout(budget, future).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(err)) => Err(TransportError::Connect {
                detail: format!("upstream connection failed: {err}"),
            }),
            Err(_) => Err(TransportError::Timeout {
                detail: format!("upstream did not respond within {}s", budget.as_secs()),
            }),
        }
    }
}

/// An error produced while relaying a streamed body.
#[derive(Debug)]
pub enum BodyError {
    /// The upstream body failed.
    Inner(Box<dyn std::error::Error + Send + Sync>),
    /// No frame arrived within the idle budget.
    Timeout {
        /// Configured idle budget, for the log line.
        idle_secs: u64,
    },
}

impl std::fmt::Display for BodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BodyError::Inner(err) => std::fmt::Display::fmt(err, f),
            BodyError::Timeout { idle_secs } => {
                write!(
                    f,
                    "no upstream data received for {idle_secs}s (idle timeout)"
                )
            }
        }
    }
}

impl std::error::Error for BodyError {}

/// A body wrapper that aborts the stream when no frame arrives within `idle`.
///
/// Used on the response path so a wedged upstream cannot hold a client
/// connection forever; the exchange has already been forwarded to the client,
/// so the timeout simply terminates the stream.
pub struct TimedBody<B> {
    inner: B,
    idle: Duration,
    deadline: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}

impl<B> TimedBody<B> {
    /// Wrap `inner` with an idle timeout.
    #[must_use]
    pub fn new(inner: B, idle: Duration) -> Self {
        Self {
            inner,
            idle,
            deadline: None,
        }
    }
}

impl<B> hyper::body::Body for TimedBody<B>
where
    B: hyper::body::Body<Data = bytes::Bytes> + Unpin,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Data = bytes::Bytes;
    type Error = BodyError;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        // `B: Unpin` and the boxed sleep is own-address stable, so the wrapper
        // is `Unpin` and the inner body can be polled without pin-projection.
        let this = self.get_mut();
        // Arm (or re-arm) the idle deadline for the next frame.
        if this.deadline.is_none() {
            this.deadline = Some(Box::pin(tokio::time::sleep(this.idle)));
        }
        if let Some(deadline) = this.deadline.as_mut()
            && deadline.as_mut().poll(cx).is_ready()
        {
            return std::task::Poll::Ready(Some(Err(BodyError::Timeout {
                idle_secs: this.idle.as_secs(),
            })));
        }
        match std::pin::Pin::new(&mut this.inner).poll_frame(cx) {
            std::task::Poll::Ready(Some(Ok(frame))) => {
                this.deadline = None;
                std::task::Poll::Ready(Some(Ok(frame)))
            }
            std::task::Poll::Ready(Some(Err(err))) => {
                std::task::Poll::Ready(Some(Err(BodyError::Inner(err.into()))))
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// Adapt a hyper upgraded connection for `tokio::io::copy_bidirectional`.
#[must_use]
pub fn into_tokio_io(upgraded: hyper::upgrade::Upgraded) -> TokioIo<hyper::upgrade::Upgraded> {
    TokioIo::new(upgraded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_absolute_uris() {
        let endpoint = Endpoint {
            scheme: Scheme::Https,
            host: "api.example.com".to_owned(),
            port: None,
        };
        assert_eq!(
            ProxyTransport::build_uri(&endpoint, "/v1/x", "")
                .unwrap()
                .to_string(),
            "https://api.example.com/v1/x"
        );
        assert_eq!(
            ProxyTransport::build_uri(&endpoint, "/v1/x", "a=b")
                .unwrap()
                .to_string(),
            "https://api.example.com/v1/x?a=b"
        );
        let ws = Endpoint {
            scheme: Scheme::Wss,
            host: "ws.example.com".to_owned(),
            port: Some(8443),
        };
        assert_eq!(
            ProxyTransport::build_uri(&ws, "/", "").unwrap().to_string(),
            "https://ws.example.com:8443/"
        );
    }

    #[test]
    fn http_endpoints_map_to_the_http_scheme() {
        let endpoint = Endpoint {
            scheme: Scheme::Http,
            host: "internal.svc".to_owned(),
            port: Some(8080),
        };
        assert_eq!(
            ProxyTransport::build_uri(&endpoint, "/", "")
                .unwrap()
                .to_string(),
            "http://internal.svc:8080/"
        );
    }

    #[tokio::test]
    async fn plaintext_is_refused_when_disallowed() {
        let transport = ProxyTransport::with_defaults(Duration::from_secs(2), false).unwrap();
        let endpoint = Endpoint {
            scheme: Scheme::Http,
            host: "internal.svc".to_owned(),
            port: None,
        };
        assert!(transport.check_scheme(&endpoint).is_err());
        assert_eq!(
            DomainError::from(TransportError::PlaintextDisabled {
                host: "internal.svc".to_owned()
            })
            .status(),
            503
        );
    }

    #[tokio::test]
    async fn plaintext_is_allowed_when_configured() {
        let transport = ProxyTransport::with_defaults(Duration::from_secs(2), true).unwrap();
        let endpoint = Endpoint {
            scheme: Scheme::Http,
            host: "internal.svc".to_owned(),
            port: None,
        };
        assert!(transport.check_scheme(&endpoint).is_ok());
    }
}
