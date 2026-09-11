//! Endpoint selection (`DESIGN.md` § "Upstream Endpoint Selection").
//!
//! A single-endpoint upstream always dials its only endpoint. A pool dials the
//! endpoint `X-OAGW-Target-Host` names; when the header is absent the pool
//! either demands it — its alias is derived from a shared domain suffix, so a
//! caller has to disambiguate — or rotates across the endpoints in order.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::alias::derive_alias;
use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, Upstream};
use crate::domain::services::proxy::select_target;

/// Rotating selection state, one cursor per multi-endpoint upstream.
#[derive(Debug, Default)]
pub struct EndpointSelector {
    cursors: DashMap<Uuid, AtomicU64>,
}

impl EndpointSelector {
    /// Creates an empty selector.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Picks the endpoint to dial for one request.
    ///
    /// # Errors
    /// [`DomainError::MissingTargetHost`] when the pool's alias is derived from
    /// a shared domain suffix and no target header was supplied,
    /// [`DomainError::InvalidTargetHost`] when the header is not a bare host,
    /// [`DomainError::UnknownTargetHost`] when it names no configured endpoint.
    pub fn select(
        &self,
        upstream: &Upstream,
        target_header: Option<&str>,
    ) -> Result<Endpoint, DomainError> {
        let explicit = target_header
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if upstream.server.endpoints.len() > 1
            && explicit.is_none()
            && !alias_from_shared_suffix(upstream)
        {
            let cursor = self
                .cursors
                .entry(upstream.id.unwrap_or_default())
                .or_default();
            let turn = cursor.fetch_add(1, Ordering::Relaxed);
            let index = usize::try_from(
                turn % u64::from(u32::try_from(upstream.server.endpoints.len()).unwrap_or(1)),
            )
            .unwrap_or(0);
            return Ok(upstream.server.endpoints[index].clone());
        }
        select_target(upstream, target_header)
    }
}

/// Whether the upstream's alias is the shared domain suffix of a multi-host
/// pool: the case where a caller must name its endpoint.
#[must_use]
pub fn alias_from_shared_suffix(upstream: &Upstream) -> bool {
    let Some(alias) = upstream.alias.as_ref() else {
        return false;
    };
    let endpoints = &upstream.server.endpoints;
    if endpoints.len() < 2 {
        return false;
    }
    derive_alias(endpoints).as_ref() == Some(alias)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Scheme, ServerConfig};

    fn upstream(alias: Option<&str>, hosts: &[&str], port: u16) -> Upstream {
        Upstream {
            id: Some(Uuid::new_v4()),
            enabled: true,
            alias: alias.map(str::to_owned),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: hosts
                    .iter()
                    .map(|host| Endpoint {
                        scheme: Scheme::Http,
                        host: (*host).to_owned(),
                        port: Some(port),
                    })
                    .collect(),
            },
            protocol: crate::domain::model::Protocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tenant_id: Uuid::new_v4(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn a_suffix_derived_pool_still_demands_a_target_host() {
        let pool = upstream(
            Some("vendor.test:8443"),
            &["a.vendor.test", "b.vendor.test"],
            8443,
        );
        let selector = EndpointSelector::default();
        assert!(matches!(
            selector.select(&pool, None),
            Err(DomainError::MissingTargetHost { .. })
        ));
    }

    #[test]
    fn an_explicit_alias_pool_rotates_across_its_endpoints() {
        let pool = upstream(Some("pool.internal"), &["10.0.0.1", "10.0.0.2"], 8443);
        let selector = EndpointSelector::default();

        let first = selector.select(&pool, None).expect("first turn");
        let second = selector.select(&pool, None).expect("second turn");
        let third = selector.select(&pool, None).expect("third turn");

        assert_eq!(first.host, "10.0.0.1");
        assert_eq!(second.host, "10.0.0.2");
        // The rotation wraps rather than walking off the end.
        assert_eq!(third.host, "10.0.0.1");
    }

    #[test]
    fn an_explicit_header_still_wins_over_the_rotation() {
        let pool = upstream(Some("pool.local"), &["10.0.0.1", "10.0.0.2"], 8443);
        let selector = EndpointSelector::default();
        let _ = selector.select(&pool, None).expect("advance the cursor");

        let picked = selector
            .select(&pool, Some("10.0.0.2"))
            .expect("explicit host");
        assert_eq!(picked.host, "10.0.0.2");
    }

    #[test]
    fn a_single_endpoint_upstream_needs_no_header() {
        let single = upstream(Some("one.local"), &["one.local"], 8443);
        let selector = EndpointSelector::default();
        let picked = selector.select(&single, None).expect("only endpoint");
        assert_eq!(picked.host, "one.local");
    }

    #[test]
    fn an_alias_that_is_not_the_derivation_rotates() {
        // An explicit alias that is not the endpoints' shared suffix leaves the
        // caller nothing to disambiguate, so the pool rotates.
        let pool = upstream(Some("pool.local"), &["pool-a.local", "pool-b.local"], 8199);
        assert!(!alias_from_shared_suffix(&pool));
        let selector = EndpointSelector::default();
        let first = selector.select(&pool, None).expect("first turn");
        assert!(["pool-a.local", "pool-b.local"].contains(&first.host.as_str()));
    }

    #[test]
    fn a_derivation_that_fails_is_not_a_shared_suffix() {
        // A pool whose only common suffix is a bare public suffix derives no
        // alias at all, so it rotates rather than demanding a header.
        let pool = upstream(
            Some("public-pool.local"),
            &["example.co.uk", "other.co.uk"],
            8443,
        );
        assert!(!alias_from_shared_suffix(&pool));
        let selector = EndpointSelector::default();
        let first = selector.select(&pool, None).expect("rotates");
        assert!(["example.co.uk", "other.co.uk"].contains(&first.host.as_str()));
    }
}
