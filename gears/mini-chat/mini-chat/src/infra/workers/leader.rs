//! Leader election for the leader-only workers (DESIGN B.9.1, section 4
//! "Watchdog Single-Actor Guarantee", ADR-0010).
//!
//! Built with the `k8s` feature, each role is led through a Kubernetes Lease
//! `mini-chat-{role}` in the pod's namespace (`POD_NAMESPACE`, identity
//! `POD_NAME`), lease duration 15 s, renewed every 2 s; a missing Lease is
//! created at runtime. Without the feature the [`NoopElector`] makes every
//! process the leader. In both cases the workers' CAS guards prevent double
//! processing.

use std::sync::Arc;

use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Role of the orphan watchdog (Lease `mini-chat-orphan-watchdog`).
pub const ROLE_ORPHAN_WATCHDOG: &str = "orphan-watchdog";
/// Role of the upload reaper (Lease `mini-chat-upload-reaper`).
pub const ROLE_UPLOAD_REAPER: &str = "upload-reaper";

/// Whether this process currently leads `role`.
pub trait LeaderElector: Send + Sync {
    fn is_leader(&self, role: &str) -> bool;
}

/// Single-process mode: always the leader of every role.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopElector;

impl LeaderElector for NoopElector {
    fn is_leader(&self, _role: &str) -> bool {
        true
    }
}

/// The elector for `roles` (the enabled leader-only workers). Without the
/// `k8s` feature, or with no role, this is the [`NoopElector`].
///
/// # Errors
/// `k8s` builds: `POD_NAMESPACE` / `POD_NAME` missing or the in-cluster
/// client cannot be created.
#[cfg_attr(not(feature = "k8s"), allow(clippy::unused_async))]
pub async fn build_elector(
    roles: &[&'static str],
    workers: &mut JoinSet<()>,
    cancel: &CancellationToken,
) -> anyhow::Result<Arc<dyn LeaderElector>> {
    #[cfg(feature = "k8s")]
    if !roles.is_empty() {
        let elector = k8s::LeaseElector::start(roles, workers, cancel).await?;
        return Ok(Arc::new(elector));
    }
    let _ = (roles, workers, cancel);
    Ok(Arc::new(NoopElector))
}

#[cfg(feature = "k8s")]
pub(crate) mod k8s {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
    use k8s_openapi::jiff::{SignedDuration, Timestamp};
    use kube::Api;
    use kube::api::PostParams;
    use tokio::task::JoinSet;
    use tokio::time::MissedTickBehavior;
    use tokio_util::sync::CancellationToken;

    use super::LeaderElector;

    /// Hardcoded Lease name prefix (the Helm value only names pre-created
    /// Leases).
    const LEASE_PREFIX: &str = "mini-chat";
    const LEASE_DURATION_SECS: i32 = 15;
    const RENEW_PERIOD: Duration = Duration::from_secs(2);
    /// Longest one renew tick may take (well below the lease duration).
    const TICK_DEADLINE: Duration = Duration::from_secs(5);

    /// What this pod does with a role's Lease on one renew tick.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum LeaseAction {
        /// No Lease yet: create it, held by this pod.
        Create,
        /// This pod holds it: refresh `renewTime`.
        Renew,
        /// Unheld or expired: take it over.
        Acquire,
        /// Another pod holds a live Lease.
        Follow,
    }

    /// The decision for `lease` as read at `now`. A Lease without a holder,
    /// or whose `renewTime + leaseDurationSeconds` has passed (or that never
    /// recorded a renewal) is free.
    pub fn decide(lease: Option<&Lease>, me: &str, now: Timestamp) -> LeaseAction {
        let Some(lease) = lease else {
            return LeaseAction::Create;
        };
        let spec = lease.spec.as_ref();
        let holder = spec
            .and_then(|s| s.holder_identity.as_deref())
            .filter(|h| !h.is_empty());
        match holder {
            None => LeaseAction::Acquire,
            Some(h) if h == me => LeaseAction::Renew,
            Some(_) => {
                let duration = spec
                    .and_then(|s| s.lease_duration_seconds)
                    .unwrap_or(LEASE_DURATION_SECS);
                let expired = spec
                    .and_then(|s| s.renew_time.as_ref())
                    .is_none_or(|t| t.0 + SignedDuration::from_secs(i64::from(duration)) < now);
                if expired {
                    LeaseAction::Acquire
                } else {
                    LeaseAction::Follow
                }
            }
        }
    }

    pub fn lease_name(role: &str) -> String {
        format!("{LEASE_PREFIX}-{role}")
    }

    /// Kubernetes Lease elector: one renew loop per role keeps a flag that
    /// [`LeaderElector::is_leader`] reads.
    pub struct LeaseElector {
        flags: HashMap<String, Arc<AtomicBool>>,
    }

    impl LeaderElector for LeaseElector {
        fn is_leader(&self, role: &str) -> bool {
            self.flags
                .get(role)
                .is_some_and(|f| f.load(Ordering::SeqCst))
        }
    }

    impl LeaseElector {
        /// Creates the in-cluster client and spawns one renew loop per role
        /// into `workers` (they stop with `cancel`).
        pub async fn start(
            roles: &[&'static str],
            workers: &mut JoinSet<()>,
            cancel: &CancellationToken,
        ) -> anyhow::Result<Self> {
            let namespace = std::env::var("POD_NAMESPACE").map_err(|_| {
                anyhow::anyhow!("mini-chat leader election: POD_NAMESPACE is not set")
            })?;
            let me = std::env::var("POD_NAME")
                .map_err(|_| anyhow::anyhow!("mini-chat leader election: POD_NAME is not set"))?;
            let client = kube::Client::try_default()
                .await
                .map_err(|e| anyhow::anyhow!("mini-chat leader election: kube client: {e}"))?;
            let api: Api<Lease> = Api::namespaced(client, &namespace);
            let mut flags = HashMap::new();
            for role in roles {
                let flag = Arc::new(AtomicBool::new(false));
                flags.insert((*role).to_owned(), Arc::clone(&flag));
                workers.spawn(renew_loop(
                    api.clone(),
                    lease_name(role),
                    me.clone(),
                    flag,
                    cancel.clone(),
                ));
            }
            tracing::info!(namespace = %namespace, identity = %me, ?roles, "mini-chat leader election started");
            Ok(Self { flags })
        }
    }

    async fn renew_loop(
        api: Api<Lease>,
        name: String,
        me: String,
        flag: Arc<AtomicBool>,
        cancel: CancellationToken,
    ) {
        let mut ticker = tokio::time::interval(RENEW_PERIOD);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                _ = ticker.tick() => {}
            }
            let leads = tick(&api, &name, &me).await;
            if flag.swap(leads, Ordering::SeqCst) != leads {
                tracing::info!(lease = %name, leader = leads, "leadership changed");
            }
        }
        flag.store(false, Ordering::SeqCst);
    }

    /// One bounded renew tick; failures drop leadership. A tick that cannot
    /// finish well inside the lease duration drops it too, so a hung API call
    /// never outlives the Lease.
    async fn tick(api: &Api<Lease>, name: &str, me: &str) -> bool {
        match tokio::time::timeout(TICK_DEADLINE, try_lead(api, name, me)).await {
            Ok(Ok(leads)) => leads,
            Ok(Err(e)) => {
                tracing::warn!(lease = %name, error = %e, "lease update failed");
                false
            }
            Err(_) => {
                tracing::warn!(lease = %name, "lease update timed out");
                false
            }
        }
    }

    /// One renew tick; `true` when this pod holds the Lease afterwards. A
    /// concurrent writer makes the replace fail on `resourceVersion`.
    async fn try_lead(api: &Api<Lease>, name: &str, me: &str) -> Result<bool, kube::Error> {
        let now = Timestamp::now();
        let current = api.get_opt(name).await?;
        let pp = PostParams::default();
        match decide(current.as_ref(), me, now) {
            LeaseAction::Follow => Ok(false),
            LeaseAction::Create => {
                let lease = Lease {
                    metadata: ObjectMeta {
                        name: Some(name.to_owned()),
                        ..ObjectMeta::default()
                    },
                    spec: Some(held_spec(me, now, 0)),
                };
                api.create(&pp, &lease).await?;
                Ok(true)
            }
            LeaseAction::Renew => {
                let mut lease = current.unwrap_or_default();
                let spec = lease.spec.get_or_insert_with(|| held_spec(me, now, 0));
                spec.renew_time = Some(MicroTime(now));
                spec.lease_duration_seconds = Some(LEASE_DURATION_SECS);
                api.replace(name, &pp, &lease).await?;
                Ok(true)
            }
            LeaseAction::Acquire => {
                let mut lease = current.unwrap_or_default();
                let transitions = lease
                    .spec
                    .as_ref()
                    .and_then(|s| s.lease_transitions)
                    .unwrap_or(0);
                lease.spec = Some(held_spec(me, now, transitions.saturating_add(1)));
                api.replace(name, &pp, &lease).await?;
                Ok(true)
            }
        }
    }

    fn held_spec(me: &str, now: Timestamp, transitions: i32) -> LeaseSpec {
        LeaseSpec {
            holder_identity: Some(me.to_owned()),
            lease_duration_seconds: Some(LEASE_DURATION_SECS),
            acquire_time: Some(MicroTime(now)),
            renew_time: Some(MicroTime(now)),
            lease_transitions: Some(transitions),
            ..LeaseSpec::default()
        }
    }
}
