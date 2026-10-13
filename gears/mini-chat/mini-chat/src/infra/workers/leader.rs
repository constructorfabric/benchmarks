//! Leader election of the leader-only workers (DESIGN §4 "Watchdog Single-Actor
//! Guarantee", ADR-0010): [`NoopElector`] (single process: every instance
//! leads) and, with the `k8s` cargo feature, [`K8sLeaseElector`] (one Kubernetes
//! `Lease` per role). Double work is harmless either way: the workers' CAS
//! guards prevent double finalization.

use std::sync::Arc;

use async_trait::async_trait;

/// Role of the orphan turn watchdog (Lease `mini-chat-orphan-watchdog`).
pub const ORPHAN_WATCHDOG_ROLE: &str = "orphan-watchdog";
/// Role of the upload reaper (Lease `mini-chat-upload-reaper`).
pub const UPLOAD_REAPER_ROLE: &str = "upload-reaper";

/// Decides whether this instance runs the scans of a worker `role`.
#[async_trait]
pub trait LeaderElector: Send + Sync {
    /// Whether this instance currently leads `role`. Cheap to call on every scan.
    async fn is_leader(&self, role: &str) -> bool;
}

/// Single-process mode: every instance is the leader of every role.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopElector;

#[async_trait]
impl LeaderElector for NoopElector {
    async fn is_leader(&self, _role: &str) -> bool {
        true
    }
}

/// The elector of this build: the Kubernetes Lease elector with the `k8s`
/// feature (needs `POD_NAMESPACE`, `POD_NAME` and a cluster client), else the
/// no-op elector.
///
/// # Errors
/// With the `k8s` feature: a missing environment variable or no usable cluster
/// configuration.
#[cfg(feature = "k8s")]
pub async fn default_elector() -> anyhow::Result<Arc<dyn LeaderElector>> {
    Ok(Arc::new(K8sLeaseElector::from_env().await?))
}

/// The elector of this build: the no-op elector (the `k8s` feature is off).
///
/// # Errors
/// Never.
#[cfg(not(feature = "k8s"))]
#[allow(clippy::unused_async)] // same signature as the `k8s` build
pub async fn default_elector() -> anyhow::Result<Arc<dyn LeaderElector>> {
    Ok(Arc::new(NoopElector))
}

#[cfg(feature = "k8s")]
pub use k8s::K8sLeaseElector;

#[cfg(feature = "k8s")]
mod k8s {
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
    use kube::api::{Api, PostParams};
    use tokio_util::sync::{CancellationToken, DropGuard};
    use tracing::{debug, info, warn};

    use super::{Arc, LeaderElector};
    use crate::domain::clock::now_utc;

    /// Lease name prefix (hardcoded; the Helm `leaderElection.prefix` only names
    /// the Leases the chart pre-creates).
    const LEASE_PREFIX: &str = "mini-chat";
    /// A Lease whose last renewal is older than this can be taken over.
    const LEASE_DURATION_SECS: i32 = 15;
    /// Renewal (and acquisition attempt) period.
    const RENEW_PERIOD: Duration = Duration::from_secs(2);

    /// Leadership through one Kubernetes `Lease` per role (`mini-chat-{role}` in
    /// the pod's namespace; created when missing).
    ///
    /// The first `is_leader(role)` call starts a task that acquires or renews the
    /// Lease every 2 s; `is_leader` reads its latest outcome, so any API failure
    /// means "not the leader" until the next successful renewal. The tasks end
    /// when the elector is dropped.
    pub struct K8sLeaseElector {
        client: kube::Client,
        namespace: String,
        identity: String,
        roles: Mutex<HashMap<String, Arc<AtomicBool>>>,
        stop: CancellationToken,
        _stop_on_drop: DropGuard,
    }

    impl K8sLeaseElector {
        /// Elector for the pod named by `POD_NAME` in `POD_NAMESPACE`, using the
        /// default cluster client (in-cluster or kubeconfig).
        ///
        /// # Errors
        /// A missing or empty environment variable; no usable cluster configuration.
        pub async fn from_env() -> anyhow::Result<Self> {
            let namespace = required_env("POD_NAMESPACE")?;
            let identity = required_env("POD_NAME")?;
            let client = kube::Client::try_default().await?;
            let stop = CancellationToken::new();
            Ok(Self {
                client,
                namespace,
                identity,
                roles: Mutex::new(HashMap::new()),
                stop: stop.clone(),
                _stop_on_drop: stop.drop_guard(),
            })
        }

        /// The leader flag of `role`, starting its renewal task on first use.
        fn flag(&self, role: &str) -> Arc<AtomicBool> {
            let mut roles = self
                .roles
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(flag) = roles.get(role) {
                return Arc::clone(flag);
            }
            let flag = Arc::new(AtomicBool::new(false));
            roles.insert(role.to_owned(), Arc::clone(&flag));
            let api: Api<Lease> = Api::namespaced(self.client.clone(), &self.namespace);
            tokio::spawn(renew_loop(
                api,
                format!("{LEASE_PREFIX}-{role}"),
                self.identity.clone(),
                Arc::clone(&flag),
                self.stop.clone(),
            ));
            flag
        }
    }

    #[async_trait]
    impl LeaderElector for K8sLeaseElector {
        async fn is_leader(&self, role: &str) -> bool {
            self.flag(role).load(Ordering::Acquire)
        }
    }

    fn required_env(name: &str) -> anyhow::Result<String> {
        std::env::var(name)
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow::anyhow!("{name} must be set for the Kubernetes leader elector"))
    }

    #[allow(clippy::cognitive_complexity)] // tracing macros inflate the score
    async fn renew_loop(
        api: Api<Lease>,
        name: String,
        identity: String,
        leader: Arc<AtomicBool>,
        stop: CancellationToken,
    ) {
        let mut ticker = tokio::time::interval(RENEW_PERIOD);
        loop {
            tokio::select! {
                biased;
                () = stop.cancelled() => break,
                _ = ticker.tick() => {}
            }
            let holds = match try_lead(&api, &name, &identity, now_utc()).await {
                Ok(holds) => holds,
                Err(err) => {
                    warn!(lease = %name, %err, "leader election request failed; not the leader");
                    false
                }
            };
            if leader.swap(holds, Ordering::AcqRel) != holds {
                info!(lease = %name, leader = holds, "leadership changed");
            }
        }
        leader.store(false, Ordering::Release);
    }

    /// One acquire-or-renew attempt: `true` when `identity` holds the Lease after
    /// it. Updates carry the read `resourceVersion`, so a concurrent writer makes
    /// the attempt fail (conflict) instead of overwriting.
    async fn try_lead(
        api: &Api<Lease>,
        name: &str,
        identity: &str,
        now: DateTime<Utc>,
    ) -> Result<bool, kube::Error> {
        let Some(mut lease) = api.get_opt(name).await? else {
            let lease = Lease {
                metadata: ObjectMeta {
                    name: Some(name.to_owned()),
                    ..ObjectMeta::default()
                },
                spec: Some(LeaseSpec {
                    holder_identity: Some(identity.to_owned()),
                    lease_duration_seconds: Some(LEASE_DURATION_SECS),
                    acquire_time: Some(micro_time(now)),
                    renew_time: Some(micro_time(now)),
                    lease_transitions: Some(0),
                    ..LeaseSpec::default()
                }),
            };
            api.create(&PostParams::default(), &lease).await?;
            return Ok(true);
        };
        let mut spec = lease.spec.take().unwrap_or_default();
        let ours = spec.holder_identity.as_deref() == Some(identity);
        if !ours && !expired(&spec, now) {
            debug!(lease = %name, "lease held by another pod");
            return Ok(false);
        }
        if !ours {
            spec.acquire_time = Some(micro_time(now));
            spec.lease_transitions = Some(spec.lease_transitions.unwrap_or(0).saturating_add(1));
            spec.holder_identity = Some(identity.to_owned());
        }
        spec.lease_duration_seconds = Some(LEASE_DURATION_SECS);
        spec.renew_time = Some(micro_time(now));
        lease.spec = Some(spec);
        api.replace(name, &PostParams::default(), &lease).await?;
        Ok(true)
    }

    /// The Lease has no holder or its last renewal is older than its duration.
    fn expired(spec: &LeaseSpec, now: DateTime<Utc>) -> bool {
        let Some(renewed) = spec.renew_time.as_ref().or(spec.acquire_time.as_ref()) else {
            return true;
        };
        let duration_micros =
            i64::from(spec.lease_duration_seconds.unwrap_or(LEASE_DURATION_SECS)) * 1_000_000;
        spec.holder_identity.is_none()
            || renewed.0.as_microsecond().saturating_add(duration_micros) < now.timestamp_micros()
    }

    fn micro_time(t: DateTime<Utc>) -> MicroTime {
        MicroTime(
            k8s_openapi::jiff::Timestamp::from_microsecond(t.timestamp_micros())
                .unwrap_or(k8s_openapi::jiff::Timestamp::UNIX_EPOCH),
        )
    }
}
