//! Endpoint selection for the data plane (`DESIGN` §"Multi-Endpoint Load
//! Balancing" and §"Headers Transformation", `X-OAGW-Target-Host`).
//!
//! An upstream owns a pool of endpoints. The alias may or may not name one of
//! them:
//!
//! * a single-endpoint upstream needs no disambiguation;
//! * an alias auto-derived from a hostname *is* that endpoint, so it pins it;
//! * an alias derived from a registrable common suffix (`us.vendor.com` +
//!   `eu.vendor.com` → `vendor.com`) names none of them, so the caller must
//!   send `X-OAGW-Target-Host`;
//! * an explicit (non-derivable) alias over a pool leaves the choice to the
//!   gateway, which rotates the pool round-robin.
//!
//! A caller can always pin an endpoint explicitly with `X-OAGW-Target-Host`.
//! That header is consumed by routing (`DESIGN` §"Headers Transformation") and
//! is never forwarded upstream.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;

use crate::domain::alias::{compute_derived_alias, normalize_alias};
use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, EndpointScheme, Upstream};

/// One endpoint picked for a request, with the transport facts derived from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedEndpoint {
    /// Endpoint scheme; decides the URL scheme and whether TLS is dialled.
    pub scheme: EndpointScheme,
    /// RFC 1123 hostname or IP literal.
    pub host: String,
    /// TCP port.
    pub port: u16,
}

impl SelectedEndpoint {
    /// The HTTP `Host`/`:authority` value to send upstream. The standard port
    /// of the scheme is omitted, as `DESIGN` §"Standard ports" does for
    /// aliases.
    #[must_use]
    pub fn authority(&self) -> String {
        if self.port == self.scheme.standard_port() {
            self.host.clone()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    /// Absolute URL the transport dials, port included.
    #[must_use]
    pub fn url(&self) -> String {
        format!("{}://{}:{}", self.url_scheme(), self.host, self.port)
    }

    /// URL scheme the transport must dial. A `wss` endpoint is reached over TLS
    /// with an HTTP/1.1 upgrade, so it presents as `https`.
    #[must_use]
    pub fn url_scheme(&self) -> &'static str {
        if self.scheme.is_tls() {
            "https"
        } else {
            "http"
        }
    }

    /// `true` when the dialled connection must be TLS.
    #[must_use]
    pub const fn requires_tls(&self) -> bool {
        self.scheme.is_tls()
    }
}

/// Round-robin cursor per upstream id.
type Cursors = Arc<DashMap<uuid::Uuid, AtomicU64>>;

/// Picks the endpoint a request is forwarded to.
///
/// Shared by every request of the process: the round-robin cursors are the
/// only state, and they are keyed by upstream id so two pools never advance
/// each other's turn.
#[derive(Debug, Default, Clone)]
pub struct EndpointSelector {
    cursors: Cursors,
    /// Deployment posture of `DESIGN` §2.2 (HTTPS-only constraint).
    allow_http_upstream: bool,
}

impl EndpointSelector {
    /// A selector enforcing the deployment's plaintext posture.
    #[must_use]
    pub fn new(allow_http_upstream: bool) -> Self {
        Self {
            cursors: Arc::new(DashMap::new()),
            allow_http_upstream,
        }
    }

    /// Alias of [`EndpointSelector::new`], for the permissive loopback posture
    /// tests read better with.
    #[must_use]
    pub fn with_allow_http(allow_http_upstream: bool) -> Self {
        Self::new(allow_http_upstream)
    }

    /// Pick the endpoint for `upstream`.
    ///
    /// # Errors
    /// Returns [`DomainError::InvalidTargetHost`] when the target host is not a
    /// bare hostname, [`DomainError::UnknownTargetHost`] when it names no
    /// endpoint, [`DomainError::MissingTargetHost`] when a common-suffix pool
    /// needs one, and [`DomainError::LinkUnavailable`] for a plaintext
    /// upstream the deployment does not allow.
    pub fn select(
        &self,
        upstream: &Upstream,
        target_host: Option<&str>,
    ) -> Result<SelectedEndpoint, DomainError> {
        let picked = match target_host {
            Some(explicit) => Self::pinned(upstream, explicit)?,
            None => self.autoselect(upstream)?,
        };
        self.guard(upstream, picked)
    }

    /// Selection when the caller pinned an endpoint with the target host
    /// header.
    fn pinned(upstream: &Upstream, explicit: &str) -> Result<SelectedEndpoint, DomainError> {
        let host = parse_target_host(explicit)?;
        let endpoints = &upstream.server.endpoints;
        let Some(endpoint) = endpoints
            .iter()
            .find(|endpoint| endpoint.host.eq_ignore_ascii_case(&host))
        else {
            return Err(DomainError::UnknownTargetHost {
                invalid_value: explicit.to_owned(),
                valid_hosts: endpoint_hosts(endpoints),
            });
        };
        Ok(SelectedEndpoint {
            scheme: endpoint.scheme,
            host: endpoint.host.clone(),
            port: endpoint.port,
        })
    }

    /// Selection when the caller did not pin an endpoint.
    fn autoselect(&self, upstream: &Upstream) -> Result<SelectedEndpoint, DomainError> {
        let endpoints = &upstream.server.endpoints;
        if endpoints.is_empty() {
            return Err(DomainError::validation(
                "upstream has no configured endpoint",
            ));
        }

        let pinned = endpoints
            .iter()
            .position(|endpoint| endpoint.host.eq_ignore_ascii_case(&upstream.alias));
        if let Some(index) = pinned {
            return Ok(select_at(endpoints, index));
        }

        if let [single] = endpoints.as_slice() {
            return Ok(SelectedEndpoint {
                scheme: single.scheme,
                host: single.host.clone(),
                port: single.port,
            });
        }
        if is_suffix_pool(&upstream.alias, endpoints) {
            return Err(DomainError::MissingTargetHost {
                alias: upstream.alias.clone(),
                valid_hosts: endpoint_hosts(endpoints),
            });
        }
        Ok(select_at(
            endpoints,
            self.rotate(upstream.id, endpoints.len()),
        ))
    }

    /// Advance and return the pool cursor for `upstream`.
    fn rotate(&self, upstream: uuid::Uuid, len: usize) -> usize {
        if len <= 1 {
            return 0;
        }
        let cursor = self
            .cursors
            .entry(upstream)
            .or_insert_with(|| AtomicU64::new(0));
        let turn = cursor.fetch_add(1, Ordering::Relaxed);
        let modulo = u64::try_from(len).unwrap_or(u64::MAX);
        usize::try_from(turn % modulo).unwrap_or(0)
    }

    /// Enforce the HTTPS-only posture of the deployment.
    fn guard(
        &self,
        upstream: &Upstream,
        picked: SelectedEndpoint,
    ) -> Result<SelectedEndpoint, DomainError> {
        if picked.requires_tls() || self.allow_http_upstream {
            return Ok(picked);
        }
        Err(DomainError::LinkUnavailable {
            detail: format!(
                "plaintext upstream '{}' is blocked: the deployment requires HTTPS for \
                 upstream connections",
                upstream.alias
            ),
            retry_after: None,
        })
    }
}

/// Endpoint hostnames, in configuration order, for the error extensions.
fn endpoint_hosts(endpoints: &[Endpoint]) -> Vec<String> {
    endpoints
        .iter()
        .map(|endpoint| endpoint.host.clone())
        .collect()
}

/// The endpoint at `index`, as transport facts.
fn select_at(endpoints: &[Endpoint], index: usize) -> SelectedEndpoint {
    SelectedEndpoint {
        scheme: endpoints[index].scheme,
        host: endpoints[index].host.clone(),
        port: endpoints[index].port,
    }
}

/// `true` when `alias` is the registrable common suffix of the pool, i.e. when
/// the alias was derived from the pool instead of naming one of its members.
fn is_suffix_pool(alias: &str, endpoints: &[Endpoint]) -> bool {
    if endpoints.len() < 2 {
        return false;
    }
    if endpoints
        .iter()
        .any(|endpoint| endpoint.host.eq_ignore_ascii_case(alias))
    {
        return false;
    }
    compute_derived_alias(endpoints).is_some_and(|derived| {
        let derived = derived.split(':').next().unwrap_or(&derived);
        derived.eq_ignore_ascii_case(&normalize_alias(alias))
    })
}

/// Strip the decorations `X-OAGW-Target-Host` must not carry.
///
/// # Errors
/// Returns [`DomainError::InvalidTargetHost`] for anything that is not a bare
/// hostname or IP literal: a port belongs to the endpoint configuration, and a
/// path or scheme is not a host.
fn parse_target_host(value: &str) -> Result<String, DomainError> {
    let invalid = || DomainError::InvalidTargetHost {
        invalid_value: value.to_owned(),
    };
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed.contains(['/', '\\', ' ', '\t', '@', ':'])
        || trimmed.starts_with('[')
        || trimmed.starts_with('.')
    {
        return Err(invalid());
    }
    let host = trimmed.trim_end_matches('.');
    if host.is_empty() || host.len() > 253 {
        return Err(invalid());
    }
    if crate::domain::alias::is_ip(host) {
        return Ok(host.to_owned());
    }
    let lowered = host.to_ascii_lowercase();
    let shaped = lowered.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    });
    if !shaped {
        return Err(invalid());
    }
    Ok(lowered)
}
