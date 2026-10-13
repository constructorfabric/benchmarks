//! Leader election for the leader-only background workers
//! (`cpt-cf-mini-chat-flow-gear-foundation-gear-start`, steps `inst-prepare-elector`;
//! `cpt-cf-mini-chat-algo-background-workers-elect-leader`): leadership is held
//! per role. Builds without the cargo feature `k8s` use the no-op elector,
//! which makes every process the leader of every role.

use std::sync::Arc;

use crate::config::MiniChatConfig;
use crate::infra::background_tasks::BackgroundTasks;

/// The leader-only workers; each holds one Lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeaderRole {
    /// The orphan watchdog.
    OrphanWatchdog,
    /// The abandoned-upload reaper.
    UploadReaper,
}

impl LeaderRole {
    /// Every role.
    pub const ALL: [Self; 2] = [Self::OrphanWatchdog, Self::UploadReaper];

    /// The role name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OrphanWatchdog => "orphan-watchdog",
            Self::UploadReaper => "upload-reaper",
        }
    }

    /// The name of the Lease object: `mini-chat-{role}`.
    #[must_use]
    pub const fn lease_name(self) -> &'static str {
        match self {
            Self::OrphanWatchdog => "mini-chat-orphan-watchdog",
            Self::UploadReaper => "mini-chat-upload-reaper",
        }
    }

    /// The name of the renew task on the background registry: `lease-{role}`.
    #[must_use]
    pub const fn task_name(self) -> &'static str {
        match self {
            Self::OrphanWatchdog => "lease-orphan-watchdog",
            Self::UploadReaper => "lease-upload-reaper",
        }
    }

    /// Whether the worker of this role is enabled in `config`.
    #[must_use]
    pub fn enabled(self, config: &MiniChatConfig) -> bool {
        match self {
            Self::OrphanWatchdog => config.orphan_watchdog.enabled,
            Self::UploadReaper => config.upload_reaper.enabled,
        }
    }
}

/// Decides whether this replica runs the worker of a role.
pub trait LeaderElector: Send + Sync {
    /// Whether this replica currently holds the leadership of `role`.
    fn is_leader(&self, role: LeaderRole) -> bool;

    /// Short name of the elector, for logs.
    fn name(&self) -> &'static str;
}

/// Elector of a single-replica deployment: this process is always the leader.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopLeaderElector;

impl LeaderElector for NoopLeaderElector {
    fn is_leader(&self, _role: LeaderRole) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "noop"
    }
}

/// Whether any leader-only worker is enabled.
#[must_use]
pub fn leader_workers_enabled(config: &MiniChatConfig) -> bool {
    config.orphan_watchdog.enabled || config.upload_reaper.enabled
}

/// The enabled leader-only roles.
#[must_use]
pub fn enabled_roles(config: &MiniChatConfig) -> Vec<LeaderRole> {
    LeaderRole::ALL
        .into_iter()
        .filter(|role| role.enabled(config))
        .collect()
}

/// Prepares the no-op elector of the leader-only workers: `None` when none of
/// them is enabled.
#[must_use]
pub fn prepare_elector(config: &MiniChatConfig) -> Option<Arc<dyn LeaderElector>> {
    // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-if-leader-workers
    if !leader_workers_enabled(config) {
        return None;
    }
    // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-gear-start:p1:inst-if-leader-workers
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-noop-elector
    let elector: Arc<dyn LeaderElector> = Arc::new(NoopLeaderElector);
    Some(elector)
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-noop-elector
}

/// Why the elector cannot start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ElectorError {
    /// `POD_NAMESPACE` or `POD_NAME` is missing or empty.
    #[error("environment variable {0} is missing or empty")]
    MissingEnv(&'static str),
    /// The in-cluster Kubernetes client could not be built.
    #[error("kubernetes client could not be built: {0}")]
    Client(String),
}

/// Starts the elector of the leader-only workers
/// (`cpt-cf-mini-chat-flow-gear-foundation-gear-start`, `inst-prepare-elector`):
/// `None` when no leader-only worker is enabled; the no-op elector without the
/// cargo feature `k8s`; with it the Kubernetes Lease elector, whose renew tasks
/// run on `tasks`.
///
/// # Errors
///
/// [`ElectorError`] in a `k8s` build when `POD_NAMESPACE` or `POD_NAME` is
/// missing or empty, or the in-cluster client cannot be built.
pub async fn start_elector(
    config: &MiniChatConfig,
    tasks: &BackgroundTasks,
) -> Result<Option<Arc<dyn LeaderElector>>, ElectorError> {
    #[cfg(feature = "k8s")]
    {
        if !leader_workers_enabled(config) {
            return Ok(None);
        }
        super::kube_lease::start_kube_elector(&enabled_roles(config), tasks).map(Some)
    }
    #[cfg(not(feature = "k8s"))]
    {
        let _ = tasks;
        Ok(prepare_elector(config))
    }
}

#[cfg(test)]
#[path = "leader_tests.rs"]
mod tests;
