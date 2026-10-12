//! The leader elector abstraction of the leader-only workers (DESIGN B.9.1).

/// Tells whether this process is the active leader of a worker role.
///
/// The worker re-checks leadership before every scan and stops claiming candidates once it is
/// lost. Implementations keep their own cadence of re-election.
pub trait LeaderElector: Send + Sync {
    /// `true` while this process holds leadership.
    fn is_leader(&self) -> bool;
}

/// The elector of single-process deployments: this process is always the leader. Double
/// finalization is prevented by the CAS guard of the finalization, not by the elector.
#[derive(Debug, Clone, Copy, Default)]
pub struct AlwaysLeader;

impl LeaderElector for AlwaysLeader {
    fn is_leader(&self) -> bool {
        true
    }
}
