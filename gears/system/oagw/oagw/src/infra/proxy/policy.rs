//! Proxy-path policy: the deployment knobs the data plane enforces.
//!
//! One struct carries `oagw.config` into the proxy path so a handler never
//! reads the manifest itself: the per-attempt upstream budget, the body ceiling
//! and the plaintext-upstream stance. Nothing here retries — `DESIGN` §3.3
//! makes every timeout a terminal `504` with a retriable problem body, not a
//! second attempt.

use std::time::Duration;

use crate::domain::error::DomainError;

/// The data-plane policy of one deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProxyPolicy {
    /// `proxy_timeout_secs`: budget of one upstream exchange, including the
    /// connection and the response headers.
    pub proxy_timeout: Duration,
    /// `allow_http_upstream`: whether a cleartext upstream may be selected.
    pub allow_http_upstream: bool,
}

impl ProxyPolicy {
    /// A policy from the deployment configuration.
    #[must_use]
    pub const fn new(proxy_timeout_secs: u64, allow_http_upstream: bool) -> Self {
        let secs = if proxy_timeout_secs == 0 {
            1
        } else {
            proxy_timeout_secs
        };
        Self {
            proxy_timeout: Duration::from_secs(secs),
            allow_http_upstream,
        }
    }

    /// The `504` the proxy emits when the budget expires.
    #[must_use]
    pub fn request_timeout(&self) -> DomainError {
        DomainError::RequestTimeout {
            limit_secs: self.proxy_timeout.as_secs(),
        }
    }
}
