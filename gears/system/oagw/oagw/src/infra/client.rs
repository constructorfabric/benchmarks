// Created: 2026-09-02 by Constructor Tech
//! Upstream transport and platform adapters.
//!
//! The proxy does its own I/O on `hyper` + `hyper-rustls` rather than through
//! `toolkit_http::HttpClient`: proxying needs a pass-through body, no
//! `Accept-Encoding` injection, no transparent decompression and the ability to
//! take over a connection for a `101 Switching Protocols` upgrade, none of which
//! the toolkit client offers.
//!
//! Also here: the [`SecretResolver`] that adapts the platform credential store
//! to the domain's [`SecretResolver`](crate::domain::plugin::SecretResolver)
//! contract, and the tenant-chain lookup used for hierarchical rate limits.

use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use credstore_sdk::CredStoreClientV1;
use hyper::body::Incoming;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use toolkit_security::SecurityContext;
use tenant_resolver_sdk::TenantResolverClient;
use uuid::Uuid;

use crate::config::SsrfPolicy;
use crate::domain::plugin::SecretResolver as SecretResolverTrait;
use crate::error::GatewayError;

/// The connector stack: TLS when the scheme asks for it, plaintext otherwise.
type UpstreamConnector = hyper_rustls::HttpsConnector<HttpConnector>;

/// An upstream HTTP client: pooled, TLS-capable, and upgrade-capable.
#[derive(Clone)]
pub struct UpstreamClient {
    inner: Client<UpstreamConnector, axum::body::Body>,
    allow_http: bool,
    connect_timeout: Duration,
    ssrf: SsrfPolicy,
}

impl std::fmt::Debug for UpstreamClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpstreamClient")
            .field("allow_http", &self.allow_http)
            .field("connect_timeout", &self.connect_timeout)
            .field("ssrf_enabled", &self.ssrf.enabled)
            .finish_non_exhaustive()
    }
}

impl UpstreamClient {
    /// Builds a client for the given configuration.
    #[must_use]
    pub fn new(allow_http: bool, connect_timeout: Duration, ssrf: SsrfPolicy) -> Self {
        let mut http = HttpConnector::new();
        http.set_connect_timeout(Some(connect_timeout));
        http.set_happy_eyeballs_timeout(Some(Duration::from_millis(300)));
        http.enforce_http(false);
        let builder = hyper_rustls::HttpsConnectorBuilder::new();
        let builder = match builder.with_native_roots() {
            Ok(builder) => builder,
            // No system trust store available: fall back to the compiled-in set.
            Err(_) => hyper_rustls::HttpsConnectorBuilder::new().with_webpki_roots(),
        };
        let tls = builder
            .https_or_http()
            .enable_http1()
            .enable_http2()
            .wrap_connector(http);
        let mut builder = Client::builder(TokioExecutor::new());
        builder.timer(TokioTimer::new());
        builder.pool_idle_timeout(Duration::from_secs(90));
        Self {
            inner: builder.build(tls),
            allow_http,
            connect_timeout,
            ssrf,
        }
    }

    /// Whether a plaintext upstream may be dialled.
    #[must_use]
    pub fn allows_http(&self) -> bool {
        self.allow_http
    }

    /// The configured connect timeout.
    #[must_use]
    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    /// The SSRF policy in force.
    #[must_use]
    pub fn ssrf_policy(&self) -> &SsrfPolicy {
        &self.ssrf
    }

    /// Screens a host against the SSRF policy (`ssrf_policy`).
    ///
    /// Literal addresses are decided here; a hostname is resolved by the
    /// resolver inside the connector, so only literals can be screened cheaply.
    pub fn screen_host(&self, host: &str) -> Result<(), GatewayError> {
        if !self.ssrf.enabled {
            return Ok(());
        }
        let Some(ip) = host.parse::<std::net::IpAddr>().ok() else {
            return Ok(());
        };
        if self.ssrf.allow_private_addresses || self.is_allowed_segment(&ip) {
            return Ok(());
        }
        if is_restricted(&ip) {
            return Err(GatewayError::SsrfBlocked(format!("{ip} is a restricted address")));
        }
        Ok(())
    }

    fn is_allowed_segment(&self, ip: &std::net::IpAddr) -> bool {
        let std::net::IpAddr::V4(addr) = ip else { return false };
        self.ssrf.allowed_segments.iter().any(|segment| {
            let Some((base, prefix)) = segment.split_once('/') else {
                return false;
            };
            let (Ok(base), Ok(prefix)) =
                (base.trim().parse::<std::net::Ipv4Addr>(), prefix.trim().parse::<u8>())
            else {
                return false;
            };
            in_v4_subnet(*addr, base, prefix.min(32))
        })
    }

    /// Sends `request` and returns the upstream response.
    ///
    /// The caller has already applied the `allow_http_upstream` gate and built
    /// the absolute URI, so the pooled client can route the request itself.
    pub async fn send(
        &self,
        request: axum::http::Request<axum::body::Body>,
    ) -> Result<axum::http::Response<Incoming>, GatewayError> {
        let response = self.inner.request(request).await.map_err(map_hyper_error)?;
        Ok(response)
    }
}

fn map_hyper_error(e: hyper_util::client::legacy::Error) -> GatewayError {
    // Response-header timeouts are enforced by the caller with a wall-clock
    // `tokio::time::timeout`, so only connection failures land here.
    if e.is_connect() {
        GatewayError::LinkUnavailable(format!("upstream connection failed: {e}"))
    } else {
        GatewayError::ProtocolError(format!("upstream request failed: {e}"))
    }
}

fn in_v4_subnet(addr: std::net::Ipv4Addr, base: std::net::Ipv4Addr, prefix: u8) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix))
    };
    (u32::from(addr) & mask) == (u32::from(base) & mask)
}

/// Whether `ip` is loopback, private, link-local or otherwise non-public.
#[must_use]
pub fn is_restricted(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// A [`SecretResolver`](crate::domain::plugin::SecretResolver) that reports every
/// reference as unreadable, used when the credential store is not registered in
/// this deployment.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSecretResolver;

#[async_trait::async_trait]
impl SecretResolverTrait for NoopSecretResolver {
    async fn resolve(&self, _tenant_id: Uuid, reference: &str) -> anyhow::Result<Option<String>> {
        tracing::warn!(reference, "credential store unavailable; secret unresolved");
        Ok(None)
    }
}

/// Resolves secrets through the platform credential store.
pub struct CredStoreSecretResolver {
    client: Arc<dyn CredStoreClientV1>,
}

impl std::fmt::Debug for CredStoreSecretResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CredStoreSecretResolver")
    }
}

impl CredStoreSecretResolver {
    /// Wraps a credential-store client.
    #[must_use]
    pub fn new(client: Arc<dyn CredStoreClientV1>) -> Self {
        Self { client }
    }
}

#[async_trait::async_trait]
impl SecretResolverTrait for CredStoreSecretResolver {
    async fn resolve(&self, tenant_id: Uuid, reference: &str) -> anyhow::Result<Option<String>> {
        let key = reference.trim().trim_start_matches("cred://");
        let Ok(secret_ref) = credstore_sdk::SecretRef::new(key) else {
            return Err(anyhow!("invalid secret reference {reference:?}"));
        };
        match self.client.get(&service_context(tenant_id), &secret_ref).await {
            Ok(Some(response)) => {
                let text = std::str::from_utf8(response.value.as_bytes())
                    .map_err(|_| anyhow!("secret {key} is not UTF-8"))?;
                Ok(Some(text.trim_end_matches('\n').to_owned()))
            }
            Ok(None) => Ok(None),
            // A read the tenant is not allowed to make is reported as absent,
            // matching the store's own single-404 posture.
            Err(credstore_sdk::CredStoreError::AccessDenied) => Ok(None),
            Err(credstore_sdk::CredStoreError::NotFound) => Ok(None),
            Err(e) => Err(anyhow!("credential store error: {e}")),
        }
    }
}

/// A service-identity [`SecurityContext`] acting for `tenant_id`.
///
/// The gateway reads secrets and tenant topology on behalf of the calling
/// tenant, not as the end user: the credentials an upstream needs are a
/// property of the upstream, not of whoever happened to send the request.
#[must_use]
pub fn service_context(tenant_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(tenant_id)
        .subject_tenant_id(tenant_id)
        .subject_type("service")
        .build()
        .unwrap_or_else(|_| SecurityContext::anonymous())
}

/// Resolves the ancestor chain of a tenant for hierarchical rate limits.
///
/// Returns `self` first, then each ancestor up to the root. When the resolver
/// is absent, the caller's own tenant is the whole chain.
pub async fn ancestor_chain(
    resolver: Option<&Arc<dyn TenantResolverClient>>,
    tenant_id: Uuid,
) -> Vec<Uuid> {
    let Some(resolver) = resolver else { return vec![tenant_id] };
    let ctx = service_context(tenant_id);
    match resolver
        .get_ancestors(
            &ctx,
            tenant_resolver_sdk::TenantId(tenant_id),
            &tenant_resolver_sdk::GetAncestorsOptions::default(),
        )
        .await
    {
        Ok(response) => {
            let mut chain = vec![response.tenant.id.0];
            chain.extend(response.ancestors.into_iter().map(|a| a.id.0));
            chain
        }
        Err(e) => {
            tracing::debug!(%tenant_id, error = %e, "ancestor lookup failed; treating the tenant as standalone");
            vec![tenant_id]
        }
    }
}
