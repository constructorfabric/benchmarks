//! Leader election abstraction of the leader-only background workers (DESIGN
//! B.9.1 "Execution model"): the orphan watchdog asks it before every scan and
//! does not claim candidates while the answer is no.
//!
//! The deployment-specific elector (a Kubernetes Lease, built with the `k8s`
//! feature) lives in the infrastructure layer. [`SingleProcessElector`] is the
//! no-op elector of single-instance deployments: this process is always the
//! leader, and double finalization is prevented by the CAS of the finalizer
//! (ADR-0010).

use async_trait::async_trait;

/// Decides whether this process is the active leader of a background worker.
#[async_trait]
pub trait LeaderElector: Send + Sync {
    /// `true` while this process may claim and process the worker's work.
    async fn is_leader(&self) -> bool;
}

/// The elector of a single process: it is always the leader.
#[derive(Debug, Clone, Copy, Default)]
pub struct SingleProcessElector;

#[async_trait]
impl LeaderElector for SingleProcessElector {
    async fn is_leader(&self) -> bool {
        true
    }
}
