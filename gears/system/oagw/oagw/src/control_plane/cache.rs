//! Control Plane cache and deletion seam — ADR 0006.
//!
//! [`ControlPlaneCache`] is the L1 configuration cache ADR 0006 assigns to this
//! feature: a generation counter the write path advances after a successful
//! write and before the response is produced. The data-plane proxy feature
//! consumes the generation to decide when its in-memory configuration snapshot
//! is stale; this feature only ever moves it forward.
//!
//! [`RateLimitCleanup`] is the in-process seam `cpt-cf-oagw-feature-rate-limiting`
//! registers its cleanup with: a successful upstream or route deletion notifies
//! the registered observer after the transaction commits and before the
//! response is produced, and a failed deletion notifies nothing.
//!
//! [`DataPlaneFlush`] is the same kind of seam for
//! `cpt-cf-oagw-algo-dp-cache`: a successful write of any kind notifies the
//! registered flush in the same process and before the write's response is
//! produced, so a read that follows the write never sees the stale entry.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::RwLock;
use uuid::Uuid;

/// The L1 configuration cache of the management half.
///
/// A generation counter is enough for this feature: the data plane only needs
/// to know that *something* changed, never what.
#[derive(Debug, Clone)]
pub struct ControlPlaneCache {
    generation: Arc<AtomicU64>,
    /// The configuration-write seam the Data Plane flush registers with.
    writes: Arc<WriteObservers>,
}

/// The flush `cpt-cf-oagw-algo-dp-cache` registers for configuration writes.
pub trait DataPlaneFlush: Send + Sync {
    /// Drops the Data Plane entries the write of one tenant affects, in the
    /// same process and before the write's response is produced.
    fn configuration_written(&self, tenant_id: Uuid);
}

/// The registration slot of the configuration-write seam.
#[derive(Default)]
pub struct WriteObservers {
    registered: RwLock<Option<Arc<dyn DataPlaneFlush>>>,
}

impl std::fmt::Debug for WriteObservers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let filled = self.registered.read().is_some();
        formatter
            .debug_struct("WriteObservers")
            .field("registered", &filled)
            .finish()
    }
}

impl WriteObservers {
    /// Creates the empty slot.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers the flush the data-plane proxy feature owns.
    pub fn register(&self, observer: Arc<dyn DataPlaneFlush>) {
        *self.registered.write() = Some(observer);
    }

    /// Notifies the registered flush that one tenant's configuration changed.
    ///
    /// With no flush registered the notification is logged and nothing else
    /// happens, which is the posture of a deployment whose data plane holds no
    /// cache to invalidate.
    pub fn configuration_written(&self, tenant_id: Uuid) {
        match self.registered.read().clone() {
            Some(observer) => observer.configuration_written(tenant_id),
            None => tracing::debug!(
                "no data-plane flush registered; the configuration write is not notified"
            ),
        }
    }
}

impl Default for ControlPlaneCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ControlPlaneCache {
    /// Creates the cache with the generation at zero.
    #[must_use]
    pub fn new() -> Self {
        Self {
            generation: Arc::new(AtomicU64::new(0)),
            writes: Arc::new(WriteObservers::new()),
        }
    }

    /// Advances the generation after a successful write and before the
    /// response is produced.
    pub fn flush(&self) {
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Registers the Data Plane flush a successful write notifies.
    pub fn register_dp_flush(&self, observer: Arc<dyn DataPlaneFlush>) {
        self.writes.register(observer);
    }

    /// Advances the generation and notifies the Data Plane flush that the
    /// configuration of one tenant changed, in that order: a read that races
    /// the write either misses the cache and resolves the new chain, or hits
    /// the entry the flush dropped and re-resolves it.
    pub fn flush_for(&self, tenant_id: Uuid) {
        self.flush();
        self.writes.configuration_written(tenant_id);
    }

    /// Reads the current generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

/// The cleanup the rate-limiting feature registers for configuration
/// deletions.
pub trait RateLimitCleanup: Send + Sync {
    /// Notifies the cleanup that one upstream row was deleted.
    fn upstream_deleted(&self, tenant_id: Uuid, upstream_id: Uuid);
    /// Notifies the cleanup that one route row was deleted.
    fn route_deleted(&self, tenant_id: Uuid, route_id: Uuid);
}

/// The registration slot of the deletion seam.
#[derive(Default)]
pub struct DeletionObservers {
    registered: RwLock<Option<Arc<dyn RateLimitCleanup>>>,
}

impl std::fmt::Debug for DeletionObservers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let filled = self.registered.read().is_some();
        formatter
            .debug_struct("DeletionObservers")
            .field("registered", &filled)
            .finish()
    }
}

impl DeletionObservers {
    /// Creates the empty slot.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers the observer the rate-limiting feature owns.
    pub fn register(&self, observer: Arc<dyn RateLimitCleanup>) {
        *self.registered.write() = Some(observer);
    }

    /// Notifies the registered observer of a successful upstream deletion.
    ///
    /// With no observer registered the notification is logged and nothing else
    /// happens.
    pub fn upstream_deleted(&self, tenant_id: Uuid, upstream_id: Uuid) {
        match self.registered.read().clone() {
            Some(observer) => observer.upstream_deleted(tenant_id, upstream_id),
            None => tracing::debug!(
                "no rate-limit cleanup registered; the upstream deletion is not notified"
            ),
        }
    }

    /// Notifies the registered observer of a successful route deletion.
    ///
    /// With no observer registered the notification is logged and nothing else
    /// happens.
    pub fn route_deleted(&self, tenant_id: Uuid, route_id: Uuid) {
        match self.registered.read().clone() {
            Some(observer) => observer.route_deleted(tenant_id, route_id),
            None => tracing::debug!(
                "no rate-limit cleanup registered; the route deletion is not notified"
            ),
        }
    }
}
