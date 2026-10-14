//! Leader election for the leader-only workers (orphan watchdog and upload reaper).
//!
//! A gear-local Kubernetes Lease elector when the crate is built with the `k8s` feature,
//! otherwise a no-op elector that always reports leadership. Every leader-only worker has its
//! own role and, under `k8s`, its own Lease.

use std::sync::Arc;

use async_trait::async_trait;

use crate::config::MiniChatConfig;
use crate::infra::tasks::BackgroundTasks;

/// A leader-only worker; each role is elected on its own Lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeaderRole {
    OrphanWatchdog,
    UploadReaper,
}

impl LeaderRole {
    /// Name of the role's Lease object in the pod's namespace (DESIGN, Watchdog Single-Actor
    /// Guarantee).
    #[must_use]
    pub const fn lease_name(self) -> &'static str {
        match self {
            Self::OrphanWatchdog => "mini-chat-orphan-watchdog",
            Self::UploadReaper => "mini-chat-upload-reaper",
        }
    }
}

/// Reports whether this process currently leads a leader-only worker.
#[async_trait]
pub trait LeaderElector: Send + Sync {
    /// `true` while this process is the leader of `role`. Never waits on the coordination
    /// backend: leadership is kept up to date by a background task.
    async fn is_leader(&self, role: LeaderRole) -> bool;
}

/// The elector without a coordination backend: always the leader.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopLeaderElector;

#[async_trait]
impl LeaderElector for NoopLeaderElector {
    async fn is_leader(&self, _role: LeaderRole) -> bool {
        true
    }
}

/// `true` when a leader-only worker (`orphan_watchdog` or `upload_reaper`) is enabled.
#[must_use]
pub fn needs_leader_election(config: &MiniChatConfig) -> bool {
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-leader-needed
    config.orphan_watchdog.enabled || config.upload_reaper.enabled
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-leader-needed
}

/// The roles of the enabled leader-only workers.
#[cfg(any(feature = "k8s", test))]
fn enabled_roles(config: &MiniChatConfig) -> Vec<LeaderRole> {
    let mut roles = Vec::new();
    if config.orphan_watchdog.enabled {
        roles.push(LeaderRole::OrphanWatchdog);
    }
    if config.upload_reaper.enabled {
        roles.push(LeaderRole::UploadReaper);
    }
    roles
}

/// Prepare the elector: `None` when no leader-only worker is enabled; otherwise the Lease
/// elector (feature `k8s`), whose renew task runs on `tasks` and ends when they are
/// cancelled, or the no-op elector.
///
/// # Errors
///
/// A failure to build the Kubernetes client (feature `k8s` only).
pub async fn prepare_elector(
    config: &MiniChatConfig,
    tasks: &BackgroundTasks,
) -> anyhow::Result<Option<Arc<dyn LeaderElector>>> {
    if !needs_leader_election(config) {
        return Ok(None);
    }
    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-leader-elector
    #[cfg(feature = "k8s")]
    {
        let elector = lease::K8sLeaseElector::connect(enabled_roles(config)).await?;
        let renewer = Arc::clone(&elector);
        tasks.spawn("leader-lease-renewal", move |token| renewer.run(token));
        Ok(Some(elector))
    }
    #[cfg(not(feature = "k8s"))]
    {
        let _ = tasks;
        let elector: Arc<dyn LeaderElector> = Arc::new(NoopLeaderElector);
        Ok(Some(elector))
    }
    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-gear-start:p1:inst-start-leader-elector
}

/// The Kubernetes Lease elector. A background task renews the Lease of every enabled role
/// each [`RENEW_PERIOD`]; `is_leader` only reads the outcome, so it never waits on the
/// Kubernetes API, and a leader that cannot renew stops leading once its lease would expire.
#[cfg(feature = "k8s")]
mod lease {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
    use k8s_openapi::jiff::Timestamp;
    use kube::api::PostParams;
    use kube::{Api, Client};
    use tokio_util::sync::CancellationToken;

    use super::{LeaderElector, LeaderRole};

    /// How long a lease is valid without a renewal (DESIGN: 15 s, hardcoded).
    const LEASE_DURATION: Duration = Duration::from_secs(15);
    /// Time between two acquire or renew rounds (DESIGN: 2 s, hardcoded).
    const RENEW_PERIOD: Duration = Duration::from_secs(2);
    /// Bound of one round, so a hanging Kubernetes API cannot stall the renewal.
    const ROUND_TIMEOUT: Duration = Duration::from_secs(5);
    /// HTTP status of an optimistic-concurrency or already-exists conflict.
    const CONFLICT: u16 = 409;

    #[derive(Debug, Default)]
    struct RoleState {
        /// Start of the last round that held the lease; leadership expires
        /// [`LEASE_DURATION`] after it.
        held_since_round: Option<Instant>,
        leader: bool,
    }

    pub struct K8sLeaseElector {
        api: Api<Lease>,
        identity: String,
        roles: Vec<LeaderRole>,
        state: Mutex<HashMap<LeaderRole, RoleState>>,
    }

    impl K8sLeaseElector {
        /// Build the Kubernetes client of the pod and the elector identity.
        pub async fn connect(roles: Vec<LeaderRole>) -> anyhow::Result<Arc<Self>> {
            let client = Client::try_default()
                .await
                .map_err(|e| anyhow::anyhow!("cannot build the Kubernetes client: {e}"))?;
            let namespace = client.default_namespace().to_owned();
            let pod = std::env::var("POD_NAME")
                .or_else(|_| std::env::var("HOSTNAME"))
                .unwrap_or_else(|_| "mini-chat".to_owned());
            let identity = format!("{pod}-{}", uuid::Uuid::new_v4().simple());
            let state = roles
                .iter()
                .map(|role| (*role, RoleState::default()))
                .collect();
            Ok(Arc::new(Self {
                api: Api::namespaced(client, &namespace),
                identity,
                roles,
                state: Mutex::new(state),
            }))
        }

        fn state(&self) -> std::sync::MutexGuard<'_, HashMap<LeaderRole, RoleState>> {
            match self.state.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            }
        }

        /// Renew the Lease of every role every [`RENEW_PERIOD`] until `token` is cancelled.
        pub async fn run(self: Arc<Self>, token: CancellationToken) {
            let mut ticker = tokio::time::interval(RENEW_PERIOD);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    () = token.cancelled() => break,
                    _ = ticker.tick() => {}
                }
                let rounds = self.roles.iter().map(|role| self.round(*role));
                futures::future::join_all(rounds).await;
            }
        }

        /// One bounded round of `role`, recorded in the state.
        async fn round(&self, role: LeaderRole) {
            let started = Instant::now();
            let lease = role.lease_name();
            match tokio::time::timeout(ROUND_TIMEOUT, self.acquire_or_renew(role)).await {
                Ok(Ok(held)) => self.record(role, held, started),
                Ok(Err(error)) => {
                    tracing::warn!(%error, lease, "leader lease round failed");
                }
                Err(_elapsed) => tracing::warn!(lease, "leader lease round timed out"),
            }
        }

        fn record(&self, role: LeaderRole, held: bool, started: Instant) {
            let mut state = self.state();
            let entry = state.entry(role).or_default();
            if held != entry.leader {
                let (identity, lease) = (&self.identity, role.lease_name());
                if held {
                    tracing::info!(%identity, lease, "acquired the leader lease");
                } else {
                    tracing::warn!(%identity, lease, "lost the leader lease");
                }
            }
            entry.leader = held;
            if held {
                entry.held_since_round = Some(started);
            }
        }

        fn lease_spec(&self, now: Timestamp, transitions: i32) -> LeaseSpec {
            LeaseSpec {
                holder_identity: Some(self.identity.clone()),
                lease_duration_seconds: i32::try_from(LEASE_DURATION.as_secs()).ok(),
                acquire_time: Some(MicroTime(now)),
                renew_time: Some(MicroTime(now)),
                lease_transitions: Some(transitions),
                ..LeaseSpec::default()
            }
        }

        /// One acquire or renew round; `Ok(true)` when this process holds the lease.
        async fn acquire_or_renew(&self, role: LeaderRole) -> Result<bool, kube::Error> {
            let name = role.lease_name();
            let now = Timestamp::now();
            let params = PostParams::default();
            let outcome = match self.api.get_opt(name).await? {
                None => {
                    let lease = Lease {
                        metadata: ObjectMeta {
                            name: Some(name.to_owned()),
                            ..ObjectMeta::default()
                        },
                        spec: Some(self.lease_spec(now, 0)),
                    };
                    self.api.create(&params, &lease).await
                }
                Some(mut lease) => {
                    let spec = lease.spec.take().unwrap_or_default();
                    let mine = spec.holder_identity.as_deref() == Some(self.identity.as_str());
                    let valid_for = i64::from(
                        spec.lease_duration_seconds
                            .unwrap_or(i32::try_from(LEASE_DURATION.as_secs()).unwrap_or(15)),
                    );
                    let expired = spec.renew_time.as_ref().is_none_or(|renewed| {
                        now.as_second().saturating_sub(renewed.0.as_second()) > valid_for
                    });
                    if !mine && !expired {
                        return Ok(false);
                    }
                    let renewed = if mine {
                        LeaseSpec {
                            renew_time: Some(MicroTime(now)),
                            ..spec
                        }
                    } else {
                        let transitions = spec.lease_transitions.unwrap_or(0).saturating_add(1);
                        self.lease_spec(now, transitions)
                    };
                    lease.spec = Some(renewed);
                    // the resource version in the metadata makes the replace conditional
                    self.api.replace(name, &params, &lease).await
                }
            };
            match outcome {
                Ok(_) => Ok(true),
                Err(kube::Error::Api(status)) if status.code == CONFLICT => Ok(false),
                Err(error) => Err(error),
            }
        }
    }

    #[async_trait]
    impl LeaderElector for K8sLeaseElector {
        async fn is_leader(&self, role: LeaderRole) -> bool {
            // a leader that could not renew stops leading once its lease would have expired
            self.state().get(&role).is_some_and(|entry| {
                entry.leader
                    && entry
                        .held_since_round
                        .is_some_and(|at| at.elapsed() < LEASE_DURATION)
            })
        }
    }
}

#[cfg(test)]
#[path = "leader_tests.rs"]
mod leader_tests;
