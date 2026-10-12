//! Leader election of the background workers (DESIGN B.9.1 "Execution
//! model", section 4 "Watchdog Single-Actor Guarantee", ADR-0010).
//!
//! The workers depend on the [`LeaderElector`] abstraction only:
//!
//! - [`NoopElector`] (built without the `k8s` feature) always reports
//!   leadership: single-process mode, every instance scans;
//! - `LeaseElector` (built with `k8s`) holds a Kubernetes Lease named
//!   `mini-chat-{role}` ([`WorkerRole::lease_name`]) in `POD_NAMESPACE`
//!   with the holder identity `POD_NAME`: 15 s lease duration, renewed
//!   every 2 s (hardcoded); a missing Lease is created. Leadership counts
//!   only while the last successful renewal is recent enough, so a pod that
//!   cannot reach the API server stops scanning before another pod can take
//!   the Lease over. On cancellation the held Lease is released.
//!
//! Leader election decides who runs a worker, not how candidates are
//! evaluated: double processing is prevented by the CAS of each worker.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

/// Hardcoded prefix of the Lease names.
pub const LEASE_PREFIX: &str = "mini-chat";

/// Lease duration of the Kubernetes elector.
pub const LEASE_DURATION: std::time::Duration = std::time::Duration::from_secs(15);

/// Renew period of the Kubernetes elector.
pub const RENEW_PERIOD: std::time::Duration = std::time::Duration::from_secs(2);

/// A leader-elected background worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorkerRole {
    OrphanWatchdog,
    UploadReaper,
}

impl WorkerRole {
    /// Role label (`orphan-watchdog`, `upload-reaper`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OrphanWatchdog => "orphan-watchdog",
            Self::UploadReaper => "upload-reaper",
        }
    }

    /// Lease of the role: `{LEASE_PREFIX}-{role}`.
    #[must_use]
    pub fn lease_name(self) -> String {
        format!("{LEASE_PREFIX}-{}", self.as_str())
    }
}

/// Decides whether this process runs a worker role.
pub trait LeaderElector: Send + Sync + std::fmt::Debug {
    /// Whether this process currently holds leadership of its role. Workers
    /// check it before every scan and before every candidate, so a lost
    /// leadership stops claiming new work.
    fn is_leader(&self) -> bool;
}

/// Elector of single-process mode: always the leader.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopElector;

impl LeaderElector for NoopElector {
    fn is_leader(&self) -> bool {
        true
    }
}

/// The elector of `role` for this build: the Kubernetes Lease elector with
/// the `k8s` feature (its renew task stops with `cancel`), else
/// [`NoopElector`].
///
/// # Errors
///
/// With `k8s`: `POD_NAMESPACE` or `POD_NAME` is not set, or no Kubernetes
/// client configuration is available.
#[cfg(not(feature = "k8s"))]
#[allow(
    clippy::unused_async,
    reason = "one signature for both builds; the k8s elector connects to the API server"
)]
pub async fn elector_for(
    _role: WorkerRole,
    _cancel: &CancellationToken,
) -> anyhow::Result<Arc<dyn LeaderElector>> {
    Ok(Arc::new(NoopElector))
}

/// The elector of `role` for this build: the Kubernetes Lease elector with
/// the `k8s` feature (its renew task stops with `cancel`), else
/// [`NoopElector`].
///
/// # Errors
///
/// `POD_NAMESPACE` or `POD_NAME` is not set, or no Kubernetes client
/// configuration is available.
#[cfg(feature = "k8s")]
pub async fn elector_for(
    role: WorkerRole,
    cancel: &CancellationToken,
) -> anyhow::Result<Arc<dyn LeaderElector>> {
    let elector = lease::LeaseElector::start(role, cancel.child_token()).await?;
    Ok(elector)
}

#[cfg(feature = "k8s")]
pub use lease::LeaseElector;

#[cfg(feature = "k8s")]
mod lease {
    use std::sync::{Arc, Mutex, PoisonError};

    use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
    use k8s_openapi::jiff::Timestamp;
    use kube::api::{Api, PostParams};
    use time::OffsetDateTime;
    use tokio::time::{Instant, MissedTickBehavior};
    use tokio_util::sync::CancellationToken;
    use tracing::{debug, info, warn};

    use super::{LEASE_DURATION, LeaderElector, RENEW_PERIOD, WorkerRole};

    const MICROS_PER_SECOND: i64 = 1_000_000;

    /// Kubernetes Lease elector of one role (module docs).
    pub struct LeaseElector {
        lease: String,
        identity: String,
        /// Leadership holds until this instant (last successful renewal plus
        /// the lease duration minus two renew periods).
        valid_until: Mutex<Option<Instant>>,
    }

    impl std::fmt::Debug for LeaseElector {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("LeaseElector")
                .field("lease", &self.lease)
                .field("identity", &self.identity)
                .finish_non_exhaustive()
        }
    }

    impl LeaderElector for LeaseElector {
        fn is_leader(&self) -> bool {
            self.valid_until
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_some_and(|until| until > Instant::now())
        }
    }

    fn env(name: &str) -> anyhow::Result<String> {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("mini-chat leader election: `{name}` is not set"))
    }

    fn now_micro_time() -> Option<MicroTime> {
        Timestamp::from_nanosecond(OffsetDateTime::now_utc().unix_timestamp_nanos())
            .ok()
            .map(MicroTime)
    }

    /// Whether the Lease `spec` lapsed at `now` (no renew time counts as
    /// lapsed).
    pub(super) fn expired(spec: &LeaseSpec, now: &MicroTime) -> bool {
        let duration = i64::from(spec.lease_duration_seconds.unwrap_or(15));
        spec.renew_time.as_ref().is_none_or(|renewed| {
            renewed
                .0
                .as_microsecond()
                .saturating_add(duration.saturating_mul(MICROS_PER_SECOND))
                <= now.0.as_microsecond()
        })
    }

    pub(super) fn lease_seconds() -> i32 {
        i32::try_from(LEASE_DURATION.as_secs()).unwrap_or(i32::MAX)
    }

    /// The instant until which leadership holds after a renewal that
    /// started at `renewed_at`: the lease duration minus two renew periods,
    /// so a pod that stops renewing stops scanning before the Lease lapses
    /// for the other pods.
    pub(super) fn valid_until(renewed_at: Instant) -> Instant {
        renewed_at + LEASE_DURATION.saturating_sub(RENEW_PERIOD * 2)
    }

    impl LeaseElector {
        /// An elector of `role` for `identity` without leadership (no API
        /// connection, no renew task).
        pub(super) fn new(role: WorkerRole, identity: String) -> Self {
            Self {
                lease: role.lease_name(),
                identity,
                valid_until: Mutex::new(None),
            }
        }

        /// Connects to the API server and starts the renew task of `role`,
        /// which runs until `cancel` fires and then releases the Lease.
        ///
        /// # Errors
        ///
        /// `POD_NAMESPACE` / `POD_NAME` missing, or no client configuration.
        pub async fn start(
            role: WorkerRole,
            cancel: CancellationToken,
        ) -> anyhow::Result<Arc<Self>> {
            let namespace = env("POD_NAMESPACE")?;
            let identity = env("POD_NAME")?;
            let client = kube::Client::try_default().await.map_err(|err| {
                anyhow::anyhow!("mini-chat leader election: Kubernetes client unavailable: {err}")
            })?;
            let api: Api<Lease> = Api::namespaced(client, &namespace);
            let elector = Arc::new(Self::new(role, identity));
            info!(
                target: "mini_chat.leader",
                lease = %elector.lease,
                namespace = %namespace,
                identity = %elector.identity,
                "leader election started"
            );
            let task = Arc::clone(&elector);
            tokio::spawn(async move { task.renew_loop(&api, &cancel).await });
            Ok(elector)
        }

        async fn renew_loop(&self, api: &Api<Lease>, cancel: &CancellationToken) {
            let mut ticker = tokio::time::interval(RENEW_PERIOD);
            ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = ticker.tick() => {}
                }
                self.round(api).await;
            }
            self.drop_leadership();
            if let Err(err) = self.release(api).await {
                debug!(target: "mini_chat.leader", lease = %self.lease, error = %err, "lease release failed");
            }
        }

        /// One renew round; an API failure keeps the current validity, which
        /// lapses on its own.
        async fn round(&self, api: &Api<Lease>) {
            let started = Instant::now();
            match self.try_acquire(api).await {
                Ok(true) => self.hold(started),
                Ok(false) => self.drop_leadership(),
                Err(err) => warn!(
                    target: "mini_chat.leader",
                    lease = %self.lease,
                    error = %err,
                    "lease renewal failed"
                ),
            }
        }

        /// Records a successful renewal that started at `renewed_at`.
        pub(super) fn hold(&self, renewed_at: Instant) {
            let mut valid = self
                .valid_until
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if valid.is_none() {
                info!(target: "mini_chat.leader", lease = %self.lease, "leadership acquired");
            }
            *valid = Some(valid_until(renewed_at));
        }

        pub(super) fn drop_leadership(&self) {
            let mut valid = self
                .valid_until
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if valid.take().is_some() {
                info!(target: "mini_chat.leader", lease = %self.lease, "leadership lost");
            }
        }

        /// One acquire / renew round; `true` when this pod holds the Lease.
        async fn try_acquire(&self, api: &Api<Lease>) -> Result<bool, kube::Error> {
            let Some(now) = now_micro_time() else {
                return Ok(false);
            };
            let Some(mut lease) = api.get_opt(&self.lease).await? else {
                return self.create(api, now).await;
            };
            let spec = lease.spec.get_or_insert_with(LeaseSpec::default);
            let mine = spec.holder_identity.as_deref() == Some(self.identity.as_str());
            let vacant = spec.holder_identity.as_deref().is_none_or(str::is_empty);
            if !(mine || vacant || expired(spec, &now)) {
                return Ok(false);
            }
            if !mine {
                spec.holder_identity = Some(self.identity.clone());
                spec.acquire_time = Some(now.clone());
                spec.lease_transitions =
                    Some(spec.lease_transitions.unwrap_or(0).saturating_add(1));
            }
            spec.renew_time = Some(now);
            spec.lease_duration_seconds = Some(lease_seconds());
            conflict_is_false(
                api.replace(&self.lease, &PostParams::default(), &lease)
                    .await,
            )
        }

        async fn create(&self, api: &Api<Lease>, now: MicroTime) -> Result<bool, kube::Error> {
            let lease = Lease {
                metadata: ObjectMeta {
                    name: Some(self.lease.clone()),
                    ..ObjectMeta::default()
                },
                spec: Some(LeaseSpec {
                    holder_identity: Some(self.identity.clone()),
                    acquire_time: Some(now.clone()),
                    renew_time: Some(now),
                    lease_duration_seconds: Some(lease_seconds()),
                    lease_transitions: Some(0),
                    ..LeaseSpec::default()
                }),
            };
            conflict_is_false(api.create(&PostParams::default(), &lease).await)
        }

        /// Clears the holder of a Lease this pod still holds.
        async fn release(&self, api: &Api<Lease>) -> Result<(), kube::Error> {
            let Some(mut lease) = api.get_opt(&self.lease).await? else {
                return Ok(());
            };
            let Some(spec) = lease.spec.as_mut() else {
                return Ok(());
            };
            if spec.holder_identity.as_deref() != Some(self.identity.as_str()) {
                return Ok(());
            }
            spec.holder_identity = None;
            spec.renew_time = None;
            api.replace(&self.lease, &PostParams::default(), &lease)
                .await
                .map(|_| ())
        }
    }

    /// A write lost to a concurrent writer (`409`) is "not the leader".
    pub(super) fn conflict_is_false(
        result: Result<Lease, kube::Error>,
    ) -> Result<bool, kube::Error> {
        match result {
            Ok(_) => Ok(true),
            Err(kube::Error::Api(status)) if status.is_conflict() || status.is_already_exists() => {
                Ok(false)
            }
            Err(err) => Err(err),
        }
    }
}

#[cfg(test)]
#[path = "leader_tests.rs"]
mod tests;
