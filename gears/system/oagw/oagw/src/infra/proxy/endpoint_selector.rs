//! The Data-Plane round-robin cursor state
//! (`cpt-cf-oagw-algo-request-proxy-endpoint-select`,
//! `inst-rp-target-6`).
//!
//! One cursor per endpoint pool, held beside the shared HTTP client. The key
//! is the pool's *fingerprint*, so a pool whose endpoint set changes gets a
//! fresh cursor — the re-derivation the state note of the algorithm requires —
//! and the advance is a single atomic fetch-add, so concurrent selections
//! never all return the same endpoint.

use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use dashmap::DashMap;
use uuid::Uuid;

use crate::domain::dto::Endpoint;

/// The fingerprint of an endpoint pool: the endpoints in their configured
/// order, as `scheme://host:port`.
#[must_use]
pub fn pool_fingerprint(endpoints: &[Endpoint]) -> String {
    let mut parts: Vec<String> = endpoints
        .iter()
        .map(|endpoint| {
            format!(
                "{}://{}:{}",
                crate::domain::endpoints::scheme_name(endpoint.scheme),
                endpoint.host,
                endpoint.port
            )
        })
        .collect();
    parts.sort();
    parts.join("|")
}

/// The cursor table of the data plane.
#[derive(Default)]
pub struct EndpointSelector {
    cursors: DashMap<String, Arc<AtomicU64>>,
}

impl EndpointSelector {
    /// An empty cursor table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The index to select for `endpoints`, advancing the pool's cursor
    /// atomically.
    ///
    /// The index is always inside the pool; the caller reduces it modulo the
    /// length. Returning the raw counter keeps the cursor meaningful when the
    /// pool shrinks between two calls.
    #[must_use]
    pub fn next(&self, upstream_id: Uuid, endpoints: &[Endpoint]) -> usize {
        let key = format!("{upstream_id}#{}", pool_fingerprint(endpoints));
        let cursor = match self.cursors.get(&key) {
            Some(cursor) => Arc::clone(&cursor),
            None => {
                let cursor = Arc::new(AtomicU64::new(0));
                let entry = self.cursors.entry(key).or_insert(Arc::clone(&cursor));
                Arc::clone(&*entry)
            }
        };
        cursor.fetch_add(1, std::sync::atomic::Ordering::SeqCst) as usize
    }

    /// The number of cursors held, for tests that assert the re-derivation.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cursors.len()
    }

    /// Whether the table holds no cursor.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cursors.is_empty()
    }
}

impl std::fmt::Debug for EndpointSelector {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("EndpointSelector").field("cursors", &self.cursors.len()).finish()
    }
}
