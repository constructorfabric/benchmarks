//! Leader election of the leader-only workers (DESIGN B.9.1, ADR-0010).
//!
//! Built with the `k8s` feature the elector is a Kubernetes Lease named `mini-chat-{role}` in the
//! pod's namespace; without it the no-op elector reports this process as the leader.

use std::sync::Arc;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::domain::orphan::LeaderElector;

#[cfg(feature = "k8s")]
mod k8s;

/// The Lease name prefix, hardcoded in the binary (the Helm value only names pre-created Leases).
#[cfg(feature = "k8s")]
const LEASE_PREFIX: &str = "mini-chat";

/// A prepared elector and the task that keeps its leadership state fresh, when it needs one.
pub struct Election {
    /// The elector the worker consults before every scan.
    pub elector: Arc<dyn LeaderElector>,
    /// The re-election task; it ends when its token is cancelled.
    pub task: Option<JoinHandle<()>>,
}

/// Prepares the elector of the worker `role` (for example `orphan-watchdog`).
///
/// # Errors
/// An error when the Kubernetes elector is built in and `POD_NAMESPACE` / `POD_NAME` are not set
/// or the cluster client cannot be created.
#[cfg(feature = "k8s")]
pub async fn prepare(role: &str, cancel: CancellationToken) -> anyhow::Result<Election> {
    let namespace = std::env::var("POD_NAMESPACE")
        .map_err(|_| anyhow::anyhow!("POD_NAMESPACE must be set for the leader elector"))?;
    let identity = std::env::var("POD_NAME")
        .map_err(|_| anyhow::anyhow!("POD_NAME must be set for the leader elector"))?;
    let (elector, task) = k8s::LeaseElector::start(
        format!("{LEASE_PREFIX}-{role}"),
        &namespace,
        identity,
        cancel,
    )
    .await?;
    Ok(Election {
        elector,
        task: Some(task),
    })
}

/// Prepares the elector of the worker `role`: without the `k8s` feature this process is always
/// the leader and double finalization is prevented by the CAS guard alone.
///
/// # Errors
/// Never fails without the `k8s` feature.
#[cfg(not(feature = "k8s"))]
#[allow(
    clippy::unused_async,
    reason = "the Kubernetes elector of the same signature is async"
)]
pub async fn prepare(_role: &str, _cancel: CancellationToken) -> anyhow::Result<Election> {
    Ok(Election {
        elector: Arc::new(crate::domain::orphan::AlwaysLeader),
        task: None,
    })
}
