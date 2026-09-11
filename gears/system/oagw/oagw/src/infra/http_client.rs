//! Shared outbound HTTP client for the proxy data plane.

use std::sync::Arc;
use std::time::Duration;

use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioTimer};

/// Body type accepted by the outbound client.
pub type OutboundBody = axum::body::Body;
/// Concrete connector type: TLS-aware, falling back to plaintext.
pub type OutboundConnector = HttpsConnector<HttpConnector>;
/// The pooled outbound client.
pub type OutboundClient = Client<OutboundConnector, OutboundBody>;

/// Build the shared outbound client.
///
/// No automatic retries are configured: the gateway never re-issues a client
/// request (`docs/DESIGN.md` principle `no-retry`).
///
/// # Panics
/// Panics when the native root certificate store cannot be loaded, which in
/// practice means a broken TLS installation on the host.
#[must_use]
pub fn build_client(pool_idle_timeout: Duration) -> Arc<OutboundClient> {
    let mut http = HttpConnector::new();
    http.set_nodelay(true);
    http.set_happy_eyeballs_timeout(Some(Duration::from_millis(300)));
    #[allow(
        clippy::expect_used,
        reason = "a host without root certificates cannot reach TLS upstreams; fail fast at startup"
    )]
    let tls = hyper_rustls::HttpsConnectorBuilder::new()
        .with_native_roots()
        .expect("native root certificates must load")
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .wrap_connector(http);
    let mut builder = Client::builder(TokioExecutor::new());
    builder.timer(TokioTimer::new());
    builder.pool_idle_timeout(Some(pool_idle_timeout));
    builder.pool_timer(TokioTimer::new());
    Arc::new(builder.build(tls))
}
