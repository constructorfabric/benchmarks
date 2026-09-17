//! Shared outbound HTTP client used by the forwarding proxy and the
//! OAuth2 token-fetching auth plugin.
//!
//! Wraps a `hyper-util` legacy client with an `axum::body::Body` so the
//! same body type can stream through both the inbound (axum) and outbound
//! (hyper) halves of the proxy in both directions.

use std::sync::Arc;

use axum::body::Body;
use http::Request;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as HyperClient;
use hyper_util::rt::TokioExecutor;

/// Minimal failure type returned by [`OagwHttpClient::send`].
pub type HttpSendError = hyper_util::client::legacy::Error;

/// A cloneable outbound HTTP client.
#[derive(Clone)]
pub struct OagwHttpClient {
    inner: HyperClient<HttpConnector, Body>,
}

impl Default for OagwHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl OagwHttpClient {
    /// Build a client over plain TCP (TLS is provided by the caller when
    /// required; the data plane only ever forwards over `http`/`https`
    /// upstream schemes).
    #[must_use]
    pub fn new() -> Self {
        let connector = HttpConnector::new();
        let inner = HyperClient::builder(TokioExecutor::new()).build(connector);
        Self { inner }
    }

    /// Send a request (full exchange: connect + headers + body).
    ///
    /// # Errors
    ///
    /// Returns the underlying hyper client error on connect/protocol/IO
    /// failure. The total timeout is enforced by the caller (the data
    /// plane applies `proxy_timeout`, the OAuth2 plugin a fixed budget).
    pub async fn send(
        &self,
        req: Request<Body>,
    ) -> Result<http::Response<Body>, HttpSendError> {
        // The hyper-legacy client streams hyper's own `Incoming` body
        // regardless of the request body type; adapt it to `axum::body::Body`.
        let resp = self.inner.request(req).await?;
        Ok(resp.map(Body::new))
    }
}

/// Convenience alias for the pointer type threaded through services.
pub type SharedHttpClient = Arc<OagwHttpClient>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn client_connects_and_roundtrips() {
        use httpmock::prelude::*;
        use http::{Method, StatusCode};

        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/hello");
            then.status(200).header("content-type", "text/plain").body("pong");
        });

        let client = OagwHttpClient::new();
        let url = format!("{}/hello", server.base_url());
        let req = Request::builder()
            .method(Method::GET)
            .uri(url)
            .body(Body::empty())
            .unwrap();
        let resp = client.send(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        mock.assert();
    }
}
