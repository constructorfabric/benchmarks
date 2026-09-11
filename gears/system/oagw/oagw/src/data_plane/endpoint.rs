//! Endpoint selection over the resolved upstream's pool.
//!
//! Realizes `cpt-cf-oagw-algo-endpoint-select` and the six-row behaviour matrix
//! of ADR 0001's Appendix A: the required header for a common-suffix alias,
//! the optional but validated header otherwise, round-robin when no header is
//! supplied, and no load balancing at all for a single-endpoint pool.
//!
//! The per-upstream round-robin counter is a per-instance state of the kind
//! ADR 0006 assigns to the Data Plane, and is the only load-balancing behaviour
//! this feature delivers: no weighting, no health exclusion, no stickiness.

use std::sync::Arc;

use parking_lot::Mutex;
use uuid::Uuid;

use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::proxy::{EndpointChoice, ResolvedUpstream, SelectedEndpoint};

/// The routing header the selection reads and strips.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// The round-robin counters of the Data Plane, per upstream.
#[derive(Clone, Default)]
pub struct RoundRobin {
    counters: Arc<Mutex<std::collections::HashMap<Uuid, usize>>>,
}

impl RoundRobin {
    /// Creates an empty counter set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads and advances the counter of one upstream.
    fn next(&self, upstream_id: Uuid, modulo: usize) -> usize {
        let mut counters = self.counters.lock();
        let entry = counters.entry(upstream_id).or_default();
        let value = *entry;
        *entry = (value + 1) % modulo;
        value
    }
}

// @cpt-dod:cpt-cf-oagw-dod-endpoint-selection:p1

/// Selects the endpoint a request is sent to.
///
/// `supplied` is the `X-OAGW-Target-Host` value as received, before the header
/// is stripped from the outbound map; the selection is the only reader of it.
///
/// # Errors
///
#[allow(clippy::result_large_err)]
/// Returns the three 400 variants the matrix produces: `MissingTargetHost` for
/// a multi-endpoint pool whose alias is a common suffix and whose request named
/// no endpoint, `InvalidTargetHost` for a malformed value, and
/// `UnknownTargetHost` for a value that matches no configured host.
pub fn select_endpoint(
    resolved: &ResolvedUpstream,
    supplied: Option<&str>,
    round_robin: &RoundRobin,
) -> Result<SelectedEndpoint, DomainError> {
    // @cpt-begin:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-present-if
    if let Some(value) = supplied {
        // @cpt-begin:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-format
        // The value is validated as a hostname or an IP address with no port,
        // no path, and no special character, which is what DESIGN §3.3's
        // `InvalidTargetHost` row requires of the header.
        let parsed = parse_target_host(value);
        // @cpt-end:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-format
        // @cpt-begin:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-format-if
        let Some(host) = parsed else {
            // @cpt-begin:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-format-return
            return Err(DomainError::gateway(
                ErrorKind::InvalidTargetHost,
                "the X-OAGW-Target-Host value is not a bare host name or IP literal",
            ));
            // @cpt-end:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-format-return
        };
        // @cpt-end:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-format-if

        // @cpt-begin:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-format-else
        // The ELSE of the format check: the value parses as a bare host, so it
        // is matched against the pool.
        // @cpt-end:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-format-else

        // @cpt-begin:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-match
        // The validated value is matched case-insensitively against the
        // endpoint hosts of the resolved upstream.
        let matched = resolved
            .endpoints
            .iter()
            .find(|endpoint| endpoint.host.as_str().eq_ignore_ascii_case(&host))
            .cloned();
        // @cpt-end:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-match

        // @cpt-begin:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-unknown-if
        let Some(endpoint) = matched else {
            // @cpt-begin:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-unknown-return
            let configured: Vec<&str> = resolved
                .endpoints
                .iter()
                .map(|endpoint| endpoint.host.as_str())
                .collect();
            let mut unknown = DomainError::gateway(
                ErrorKind::UnknownTargetHost,
                "the X-OAGW-Target-Host value names no configured endpoint host",
            );
            unknown.detail =
                format!("the value {host:?} names none of the configured hosts {configured:?}");
            return Err(unknown);
            // @cpt-end:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-unknown-return
        };
        // @cpt-end:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-unknown-if

        // @cpt-begin:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-unknown-else
        // The ELSE of the host check: the value named one of the pool's hosts,
        // so that endpoint answers the request.
        // @cpt-end:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-unknown-else

        // @cpt-begin:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-unknown-else-return
        return Ok(SelectedEndpoint {
            endpoint,
            choice: EndpointChoice::Header,
        });
        // @cpt-end:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-unknown-else-return
    }
    // @cpt-end:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-present-if

    // @cpt-begin:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-absent-else
    // The ELSE of the header check: no target host was supplied, so the
    // endpoint count and the alias derivation kind decide.
    // @cpt-end:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-absent-else

    // @cpt-begin:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-absent
    // The pool is never empty through the API: the schema states `minItems: 1`
    // and the store refuses the row. A pool that nevertheless arrives empty is
    // a routing configuration defect, and the selection fails closed on it
    // rather than dividing by zero at the round-robin counter.
    if resolved.endpoints.is_empty() {
        return Err(DomainError::gateway(
            ErrorKind::RouteError,
            "the resolved upstream declares no endpoint the request could be sent to",
        ));
    }
    // @cpt-end:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-absent

    // @cpt-begin:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-single-if
    if let [endpoint] = resolved.endpoints.as_slice() {
        // @cpt-begin:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-single
        return Ok(SelectedEndpoint {
            endpoint: endpoint.clone(),
            choice: EndpointChoice::Only,
        });
        // @cpt-end:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-single
    }
    // @cpt-end:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-single-if

    // @cpt-begin:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-suffix-if
    if resolved.alias_derivation == crate::domain::proxy::AliasDerivation::Derived {
        // @cpt-begin:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-suffix-return
        let configured: Vec<&str> = resolved
            .endpoints
            .iter()
            .map(|endpoint| endpoint.host.as_str())
            .collect();
        let mut missing = DomainError::gateway(
            ErrorKind::MissingTargetHost,
            "a multi-endpoint upstream whose alias is a common suffix requires X-OAGW-Target-Host",
        );
        missing.detail = format!("the valid values are the configured hosts {configured:?}");
        return Err(missing);
        // @cpt-end:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-suffix-return
    }
    // @cpt-end:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-suffix-if

    // @cpt-begin:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-rr-else
    // The ELSE of the derivation check: a multi-endpoint pool whose alias is
    // the explicit kind needs no header, and load balancing selects.
    // @cpt-end:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-rr-else

    // @cpt-begin:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-rr
    let index = round_robin.next(resolved.upstream_id, resolved.endpoints.len());
    // @cpt-end:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-rr

    // @cpt-begin:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-rr-return
    Ok(SelectedEndpoint {
        endpoint: resolved.endpoints[index % resolved.endpoints.len()].clone(),
        choice: EndpointChoice::LoadBalanced,
    })
    // @cpt-end:cpt-cf-oagw-algo-endpoint-select:p1:inst-ep-rr-return
}

/// Parses a supplied target host: a bare host name or IP literal, no port, no
/// path, no scheme, no special character.
///
/// The value is matched case-insensitively against the configured hosts, so the
/// normalized form is what the comparison sees.
#[must_use]
fn parse_target_host(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    // The only colon a bare target may carry is an IPv6 literal's, which the
    // endpoint-host parser admits; any other punctuation names a port, a path,
    // a scheme, or an authority, and is malformed for this header.
    let lowered = trimmed.to_ascii_lowercase();
    if crate::domain::EndpointHost::parse(&lowered).is_ok() {
        return Some(lowered);
    }
    let forbidden = ['/', ':', '@', '?', '#', '\\', ' '];
    if trimmed.chars().any(|character| forbidden.contains(&character)) {
        return None;
    }
    crate::domain::EndpointHost::parse(&lowered).is_ok().then_some(lowered)
}

/// The round-robin counter of one upstream, exposed for the diagnostics of the
/// observability feature.
#[must_use]
pub fn counter_of(round_robin: &RoundRobin, upstream_id: Uuid) -> Option<usize> {
    round_robin.counters.lock().get(&upstream_id).copied()
}

/// The shared round-robin state the Data Plane holds.
///
/// The type is `Clone` over an `Arc`, so one handle reaches every handler.
#[must_use]
pub fn shared() -> Arc<RoundRobin> {
    Arc::new(RoundRobin::new())
}
