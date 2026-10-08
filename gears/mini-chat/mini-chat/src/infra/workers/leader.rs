//! Leader election of the periodic workers (D "Watchdog Single-Actor
//! Guarantee", B.9.1, ADR-0010).
//!
//! The workers ask [`LeaderElector::is_leader`] before every scan with their
//! role ([`ORPHAN_WATCHDOG_ROLE`], [`UPLOAD_REAPER_ROLE`]). Leadership only
//! decides who scans: double finalization is prevented by the CAS guards of
//! the workers in every mode.

use std::sync::Arc;

/// Role (Lease suffix) of the orphan watchdog.
pub const ORPHAN_WATCHDOG_ROLE: &str = "orphan-watchdog";
/// Role (Lease suffix) of the upload reaper.
pub const UPLOAD_REAPER_ROLE: &str = "upload-reaper";

/// Decides whether this process runs the worker of `role` right now.
pub trait LeaderElector: Send + Sync {
    /// `true` when this process is the current leader of `role`.
    fn is_leader(&self, role: &str) -> bool;
}

/// Single-process mode (built without the `k8s` feature): always leader.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopElector;

impl LeaderElector for NoopElector {
    fn is_leader(&self, _role: &str) -> bool {
        true
    }
}

/// Kubernetes elector (`k8s` feature).
///
/// The DESIGN's Lease elector (`mini-chat-{role}` Leases in
/// `POD_NAMESPACE`, 15 s lease, 2 s renew) is not implemented: this
/// placeholder always reports leadership, so every pod scans, exactly like
/// [`NoopElector`]. That stays correct because the orphan CAS and the
/// reaper CAS re-check their predicates in the terminal update, so a turn
/// or upload is finalized at most once; the cost is duplicate scan work
/// across pods.
#[cfg(feature = "k8s")]
#[derive(Debug, Clone, Copy, Default)]
pub struct K8sLeaseElector;

#[cfg(feature = "k8s")]
impl LeaderElector for K8sLeaseElector {
    fn is_leader(&self, _role: &str) -> bool {
        true
    }
}

/// The elector of this build: [`K8sLeaseElector`] with the `k8s` feature,
/// [`NoopElector`] otherwise.
#[must_use]
pub fn default_elector() -> Arc<dyn LeaderElector> {
    #[cfg(feature = "k8s")]
    {
        Arc::new(K8sLeaseElector)
    }
    #[cfg(not(feature = "k8s"))]
    {
        Arc::new(NoopElector)
    }
}

#[cfg(test)]
#[path = "leader_tests.rs"]
mod tests;
