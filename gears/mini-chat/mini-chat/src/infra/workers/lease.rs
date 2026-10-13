//! The Lease-based leader election
//! (`cpt-cf-mini-chat-algo-background-workers-elect-leader`,
//! `cpt-cf-mini-chat-state-background-workers-leadership`): the decision logic
//! behind a Lease API port, so it is tested without a cluster. The Kubernetes
//! adapter of the port compiles only with the cargo feature `k8s`.
//!
//! Expiry is judged by the observed-time rule: a pod takes over only when the
//! Lease record has not changed for `LEASE_DURATION` on its own monotonic
//! clock. Updates carry the Lease `resourceVersion` (optimistic concurrency).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use super::leader::{ElectorError, LeaderElector, LeaderRole};
use crate::domain::service::workers::clock::{WorkerClock, with_timeout};
use crate::infra::background_tasks::BackgroundTasks;

/// `leaseDurationSeconds` of each Lease.
pub const LEASE_DURATION: Duration = Duration::from_secs(15);

/// Renew and acquire cadence.
pub const LEASE_RENEW_PERIOD: Duration = Duration::from_secs(2);

/// Leadership is reported only while the last successful renewal is younger
/// than this.
pub const LEASE_RENEW_DEADLINE: Duration = Duration::from_secs(10);

/// Each Kubernetes API call.
pub const LEASE_API_TIMEOUT: Duration = Duration::from_secs(5);

/// Prefix of the Lease names.
pub const LEASE_PREFIX: &str = "mini-chat";

/// A warning for a role is logged at most this often.
const WARN_EVERY: Duration = Duration::from_secs(60);

/// The fields of a Lease the election reads and writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseRecord {
    /// `holderIdentity`; `None` or empty when released.
    pub holder: Option<String>,
    /// `leaseDurationSeconds`.
    pub lease_duration_secs: i32,
    /// `acquireTime`.
    pub acquire_time: Option<OffsetDateTime>,
    /// `renewTime`.
    pub renew_time: Option<OffsetDateTime>,
    /// `leaseTransitions`.
    pub transitions: i32,
    /// `metadata.resourceVersion` (empty for a Lease to create).
    pub resource_version: String,
}

/// A failed Lease API call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LeaseApiError {
    /// 409: another pod created or changed the Lease first.
    #[error("conflict")]
    Conflict,
    /// Any other failure.
    #[error("{0}")]
    Failed(String),
}

/// The Lease API port: GET, CREATE and REPLACE of `coordination.k8s.io/v1`
/// Leases of the pod's namespace. A REPLACE carries `record.resource_version`.
#[async_trait]
pub trait LeaseApi: Send + Sync {
    /// The Lease `name`; `None` when it does not exist.
    async fn get(&self, name: &str) -> Result<Option<LeaseRecord>, LeaseApiError>;

    /// Creates the Lease `name`.
    async fn create(&self, name: &str, record: &LeaseRecord) -> Result<LeaseRecord, LeaseApiError>;

    /// Replaces the Lease `name` under `record.resource_version`.
    async fn replace(&self, name: &str, record: &LeaseRecord)
    -> Result<LeaseRecord, LeaseApiError>;
}

/// The identity of this pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodIdentity {
    /// `POD_NAMESPACE`.
    pub namespace: String,
    /// `POD_NAME`: the holder identity.
    pub name: String,
}

impl PodIdentity {
    /// Reads `POD_NAMESPACE` and `POD_NAME` through `lookup`.
    ///
    /// # Errors
    ///
    /// [`ElectorError::MissingEnv`] naming the variable that is missing or
    /// empty.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ElectorError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-read-pod-identity
        let read = |name: &'static str| {
            lookup(name)
                .filter(|value| !value.trim().is_empty())
                .ok_or(ElectorError::MissingEnv(name))
        };
        Ok(Self {
            namespace: read("POD_NAMESPACE")?,
            name: read("POD_NAME")?,
        })
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-read-pod-identity
    }

    /// Reads the process environment.
    ///
    /// # Errors
    ///
    /// See [`Self::from_lookup`].
    pub fn from_env() -> Result<Self, ElectorError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }
}

/// What one round did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundOutcome {
    /// The missing Lease was created.
    Created,
    /// The Lease of this pod was renewed.
    Renewed,
    /// An empty or expired Lease was taken over.
    TookOver,
    /// Another pod holds the Lease.
    Follower,
    /// A write lost with 409.
    Conflict,
    /// A call failed or timed out.
    Failed,
}

/// The record as last observed while another pod held it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Observed {
    holder: Option<String>,
    renew_time: Option<OffsetDateTime>,
    resource_version: String,
    first_seen: Duration,
}

#[derive(Default)]
struct ElectionState {
    observed: Option<Observed>,
    last_renewal: Option<Duration>,
    last_warning: Option<Duration>,
}

/// The election of one role.
pub struct LeaseElection {
    role: LeaderRole,
    identity: String,
    api: Arc<dyn LeaseApi>,
    clock: Arc<dyn WorkerClock>,
    state: Mutex<ElectionState>,
}

impl LeaseElection {
    /// Creates the election of `role` for the holder `identity`.
    #[must_use]
    pub fn new(
        role: LeaderRole,
        identity: impl Into<String>,
        api: Arc<dyn LeaseApi>,
        clock: Arc<dyn WorkerClock>,
    ) -> Self {
        Self {
            role,
            identity: identity.into(),
            api,
            clock,
            state: Mutex::new(ElectionState::default()),
        }
    }

    /// Whether this pod reports the leadership: the last successful renewal is
    /// younger than [`LEASE_RENEW_DEADLINE`].
    #[must_use]
    pub fn is_leader(&self) -> bool {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-report-leadership
        let now = self.clock.now();
        self.state.lock().is_ok_and(|state| {
            state
                .last_renewal
                .is_some_and(|renewed| now.saturating_sub(renewed) < LEASE_RENEW_DEADLINE)
        })
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-report-leadership
    }

    fn set_renewed(&self) {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-record-renewal
        let now = self.clock.now();
        if let Ok(mut state) = self.state.lock() {
            state.last_renewal = Some(now);
            state.observed = None;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-record-renewal
    }

    fn set_follower(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.last_renewal = None;
        }
    }

    /// A failed or timed-out call: the round is not renewed; a warning at most
    /// once per minute; the gear never fails.
    fn failed(&self, cause: &str) -> RoundOutcome {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-lease-api-error
        let now = self.clock.now();
        let warn = self.state.lock().is_ok_and(|mut state| {
            let due = state
                .last_warning
                .is_none_or(|at| now.saturating_sub(at) >= WARN_EVERY);
            if due {
                state.last_warning = Some(now);
            }
            due
        });
        if warn {
            tracing::warn!(
                role = self.role.as_str(),
                cause,
                "the lease round did not renew"
            );
        }
        RoundOutcome::Failed
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-lease-api-error
    }

    /// One acquire or renew round.
    pub async fn round(&self) -> RoundOutcome {
        let name = self.role.lease_name();
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-get-lease
        let read = with_timeout(&*self.clock, LEASE_API_TIMEOUT, self.api.get(name)).await;
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-get-lease
        let current = match read {
            Some(Ok(current)) => current,
            Some(Err(error)) => return self.failed(&error.to_string()),
            None => return self.failed("timeout"),
        };
        let now_wall = crate::domain::clock::now_micros();
        let Some(record) = current else {
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-create-lease
            let fresh = LeaseRecord {
                holder: Some(self.identity.clone()),
                lease_duration_secs: i32::try_from(LEASE_DURATION.as_secs()).unwrap_or(15),
                acquire_time: Some(now_wall),
                renew_time: Some(now_wall),
                transitions: 0,
                resource_version: String::new(),
            };
            let created = with_timeout(
                &*self.clock,
                LEASE_API_TIMEOUT,
                self.api.create(name, &fresh),
            )
            .await;
            return self.after_write(created, RoundOutcome::Created);
            // @cpt-end:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-create-lease
        };
        if record.holder.as_deref() == Some(self.identity.as_str()) {
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-renew-lease
            let renewed_record = LeaseRecord {
                renew_time: Some(now_wall),
                ..record
            };
            let replaced = with_timeout(
                &*self.clock,
                LEASE_API_TIMEOUT,
                self.api.replace(name, &renewed_record),
            )
            .await;
            return self.after_write(replaced, RoundOutcome::Renewed);
            // @cpt-end:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-renew-lease
        }
        if self.expired_or_empty(&record) {
            // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-take-over-lease
            let taken = LeaseRecord {
                holder: Some(self.identity.clone()),
                lease_duration_secs: i32::try_from(LEASE_DURATION.as_secs()).unwrap_or(15),
                acquire_time: Some(now_wall),
                renew_time: Some(now_wall),
                transitions: record.transitions.saturating_add(1),
                resource_version: record.resource_version,
            };
            let replaced = with_timeout(
                &*self.clock,
                LEASE_API_TIMEOUT,
                self.api.replace(name, &taken),
            )
            .await;
            return self.after_write(replaced, RoundOutcome::TookOver);
            // @cpt-end:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-take-over-lease
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-observe-other
        // @cpt-begin:cpt-cf-mini-chat-state-background-workers-leadership:p1:inst-stay-follower
        self.set_follower();
        RoundOutcome::Follower
        // @cpt-end:cpt-cf-mini-chat-state-background-workers-leadership:p1:inst-stay-follower
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-observe-other
    }

    /// Updates the observation of `record` and decides whether it is empty or
    /// has not changed for [`LEASE_DURATION`] on the local monotonic clock.
    fn expired_or_empty(&self, record: &LeaseRecord) -> bool {
        let now = self.clock.now();
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        let unchanged = state.observed.as_ref().is_some_and(|seen| {
            seen.holder == record.holder
                && seen.renew_time == record.renew_time
                && seen.resource_version == record.resource_version
        });
        if !unchanged {
            state.observed = Some(Observed {
                holder: record.holder.clone(),
                renew_time: record.renew_time,
                resource_version: record.resource_version.clone(),
                first_seen: now,
            });
        }
        let empty = record.holder.as_deref().is_none_or(str::is_empty);
        let first_seen = state.observed.as_ref().map_or(now, |seen| seen.first_seen);
        empty || now.saturating_sub(first_seen) >= LEASE_DURATION
    }

    /// The result of a CREATE or REPLACE.
    fn after_write(
        &self,
        result: Option<Result<LeaseRecord, LeaseApiError>>,
        success: RoundOutcome,
    ) -> RoundOutcome {
        match result {
            Some(Ok(_)) => {
                // @cpt-begin:cpt-cf-mini-chat-state-background-workers-leadership:p1:inst-to-leader
                // @cpt-begin:cpt-cf-mini-chat-state-background-workers-leadership:p1:inst-stay-leader
                self.set_renewed();
                // @cpt-end:cpt-cf-mini-chat-state-background-workers-leadership:p1:inst-stay-leader
                // @cpt-end:cpt-cf-mini-chat-state-background-workers-leadership:p1:inst-to-leader
                success
            }
            Some(Err(LeaseApiError::Conflict)) => {
                // @cpt-begin:cpt-cf-mini-chat-state-background-workers-leadership:p1:inst-to-follower
                self.set_follower();
                // @cpt-end:cpt-cf-mini-chat-state-background-workers-leadership:p1:inst-to-follower
                RoundOutcome::Conflict
            }
            Some(Err(error)) => self.failed(&error.to_string()),
            None => self.failed("timeout"),
        }
    }

    /// Releases the Lease best effort: an empty `holderIdentity` under the read
    /// `resourceVersion`, so another pod takes over without waiting. This pod
    /// stops reporting the leadership at once.
    pub async fn release(&self) {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-release-lease
        let held = self
            .state
            .lock()
            .is_ok_and(|mut state| state.last_renewal.take().is_some());
        if !held {
            return;
        }
        let name = self.role.lease_name();
        let read = with_timeout(&*self.clock, LEASE_API_TIMEOUT, self.api.get(name)).await;
        let Some(Ok(Some(record))) = read else {
            tracing::warn!(
                role = self.role.as_str(),
                "the lease could not be read for release"
            );
            return;
        };
        if record.holder.as_deref() != Some(self.identity.as_str()) {
            return;
        }
        let released = LeaseRecord {
            holder: None,
            ..record
        };
        let replaced = with_timeout(
            &*self.clock,
            LEASE_API_TIMEOUT,
            self.api.replace(name, &released),
        )
        .await;
        if !matches!(replaced, Some(Ok(_))) {
            tracing::warn!(role = self.role.as_str(), "the lease could not be released");
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-release-lease
    }

    /// Rounds every [`LEASE_RENEW_PERIOD`] until `stop` is cancelled, then
    /// releases the Lease.
    pub async fn run(self: Arc<Self>, stop: CancellationToken) {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-lease-loop
        loop {
            self.round().await;
            tokio::select! {
                () = stop.cancelled() => break,
                () = self.clock.sleep(LEASE_RENEW_PERIOD) => {}
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-lease-loop
        self.release().await;
    }
}

/// The elector of the `k8s` build: one election per enabled role.
pub struct LeaseElector {
    elections: Vec<(LeaderRole, Arc<LeaseElection>)>,
}

impl LeaseElector {
    /// The election of `role`.
    #[must_use]
    pub fn election(&self, role: LeaderRole) -> Option<&Arc<LeaseElection>> {
        self.elections
            .iter()
            .find(|(candidate, _)| *candidate == role)
            .map(|(_, election)| election)
    }
}

impl LeaderElector for LeaseElector {
    fn is_leader(&self, role: LeaderRole) -> bool {
        self.election(role)
            .is_some_and(|election| election.is_leader())
    }

    fn name(&self) -> &'static str {
        "kubernetes-lease"
    }
}

// @cpt-dod:cpt-cf-mini-chat-dod-background-workers-leader-election:p1
/// Builds the elector over `api` and spawns the renew task `lease-{role}` of
/// every enabled role on `tasks`.
#[must_use]
pub fn start_lease_elector(
    roles: &[LeaderRole],
    identity: &PodIdentity,
    api: &Arc<dyn LeaseApi>,
    clock: &Arc<dyn WorkerClock>,
    tasks: &BackgroundTasks,
) -> Arc<LeaseElector> {
    let elections: Vec<(LeaderRole, Arc<LeaseElection>)> = roles
        .iter()
        .map(|role| {
            (
                *role,
                Arc::new(LeaseElection::new(
                    *role,
                    identity.name.clone(),
                    Arc::clone(api),
                    Arc::clone(clock),
                )),
            )
        })
        .collect();
    for (role, election) in &elections {
        // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-spawn-lease-task
        let (election, stop) = (Arc::clone(election), tasks.token());
        tasks.spawn(role.task_name(), async move { election.run(stop).await });
        // @cpt-end:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-spawn-lease-task
    }
    Arc::new(LeaseElector { elections })
}

#[cfg(test)]
#[path = "lease_tests.rs"]
mod tests;
