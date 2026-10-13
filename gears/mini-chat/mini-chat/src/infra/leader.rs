//! Leader election for the leader-only workers (DESIGN B.9.1, ADR-0010).
//!
//! Without the `k8s` feature every process is leader ([`crate::domain::workers::NoopElector`]).
//! With it, a gear-local Kubernetes Lease elector holds one Lease per role in the pod's
//! namespace (`POD_NAMESPACE`, holder `POD_NAME`), 15 s lease duration, 2 s renew period.

use std::time::Duration;

/// Hardcoded Lease name prefix.
pub const LEASE_PREFIX: &str = "mini-chat";
/// Role of the orphan watchdog.
pub const ROLE_ORPHAN_WATCHDOG: &str = "orphan-watchdog";
/// Role of the upload reaper.
pub const ROLE_UPLOAD_REAPER: &str = "upload-reaper";
/// Lease duration.
pub const LEASE_DURATION: Duration = Duration::from_secs(15);
/// Renew period.
pub const RENEW_PERIOD: Duration = Duration::from_secs(2);

/// Lease name of a role.
#[must_use]
pub fn lease_name(role: &str) -> String {
    format!("{LEASE_PREFIX}-{role}")
}

/// What to do with a Lease observed at `now` (seconds since the epoch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseAction {
    /// Take the Lease (free or expired).
    Acquire,
    /// Renew our own Lease.
    Renew,
    /// Another holder's Lease is valid.
    Follow,
}

/// Pure leadership decision.
#[must_use]
pub fn decide(holder: Option<&str>, renew_time: Option<f64>, duration: Duration, me: &str, now: f64) -> LeaseAction {
    match holder {
        Some(h) if h == me => LeaseAction::Renew,
        Some(h) if !h.is_empty() => match renew_time {
            Some(t) if t + duration.as_secs_f64() >= now => LeaseAction::Follow,
            _ => LeaseAction::Acquire,
        },
        _ => LeaseAction::Acquire,
    }
}

#[cfg(feature = "k8s")]
pub use k8s::LeaseElector;

#[cfg(feature = "k8s")]
mod k8s {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
    use kube::api::{Api, PostParams};
    use tokio_util::sync::CancellationToken;

    use super::{LEASE_DURATION, LeaseAction, RENEW_PERIOD, decide, lease_name};
    use crate::domain::workers::LeaderElector;

    /// Kubernetes Lease elector (one Lease per role).
    pub struct LeaseElector {
        flags: HashMap<&'static str, Arc<AtomicBool>>,
    }

    impl LeaderElector for LeaseElector {
        fn is_leader(&self, role: &str) -> bool {
            self.flags.get(role).is_some_and(|f| f.load(Ordering::SeqCst))
        }
    }

    fn now_ts() -> k8s_openapi::jiff::Timestamp {
        k8s_openapi::jiff::Timestamp::now()
    }

    impl LeaseElector {
        /// Starts renew loops for `roles`.
        ///
        /// # Errors
        /// Missing `POD_NAMESPACE` / `POD_NAME` or Kubernetes client initialization failure.
        pub async fn start(roles: &[&'static str], cancel: CancellationToken) -> anyhow::Result<Arc<Self>> {
            let ns = std::env::var("POD_NAMESPACE").map_err(|_| anyhow::anyhow!("POD_NAMESPACE is not set"))?;
            let me = std::env::var("POD_NAME").map_err(|_| anyhow::anyhow!("POD_NAME is not set"))?;
            let client = kube::Client::try_default().await?;
            let mut flags = HashMap::new();
            for role in roles {
                let flag = Arc::new(AtomicBool::new(false));
                flags.insert(*role, flag.clone());
                let api: Api<Lease> = Api::namespaced(client.clone(), &ns);
                let name = lease_name(role);
                let me = me.clone();
                let cancel = cancel.clone();
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(RENEW_PERIOD);
                    loop {
                        tokio::select! {
                            () = cancel.cancelled() => break,
                            _ = tick.tick() => {}
                        }
                        let leader = match tick_once(&api, &name, &me).await {
                            Ok(l) => l,
                            Err(e) => {
                                tracing::warn!(lease = %name, error = %e, "lease renew failed");
                                false
                            }
                        };
                        if flag.swap(leader, Ordering::SeqCst) != leader {
                            tracing::info!(lease = %name, leader, "leadership changed");
                        }
                    }
                    flag.store(false, Ordering::SeqCst);
                });
            }
            Ok(Arc::new(Self { flags }))
        }
    }

    async fn tick_once(api: &Api<Lease>, name: &str, me: &str) -> Result<bool, kube::Error> {
        let duration = i32::try_from(LEASE_DURATION.as_secs()).unwrap_or(15);
        let now = now_ts();
        let Some(mut lease) = api.get_opt(name).await? else {
            let lease = Lease {
                metadata: ObjectMeta { name: Some(name.to_owned()), ..ObjectMeta::default() },
                spec: Some(LeaseSpec {
                    holder_identity: Some(me.to_owned()),
                    lease_duration_seconds: Some(duration),
                    acquire_time: Some(MicroTime(now)),
                    renew_time: Some(MicroTime(now)),
                    lease_transitions: Some(0),
                    ..LeaseSpec::default()
                }),
            };
            return Ok(api.create(&PostParams::default(), &lease).await.is_ok());
        };
        let spec = lease.spec.clone().unwrap_or_default();
        #[allow(clippy::cast_precision_loss)]
        let renew = spec.renew_time.as_ref().map(|t| t.0.as_millisecond() as f64 / 1000.0);
        let dur = std::time::Duration::from_secs(u64::try_from(spec.lease_duration_seconds.unwrap_or(duration)).unwrap_or(15));
        #[allow(clippy::cast_precision_loss)]
        let now_s = now.as_millisecond() as f64 / 1000.0;
        let action = decide(spec.holder_identity.as_deref(), renew, dur, me, now_s);
        let mut new_spec = spec.clone();
        match action {
            LeaseAction::Follow => return Ok(false),
            LeaseAction::Renew => new_spec.renew_time = Some(MicroTime(now)),
            LeaseAction::Acquire => {
                new_spec.holder_identity = Some(me.to_owned());
                new_spec.lease_duration_seconds = Some(duration);
                new_spec.acquire_time = Some(MicroTime(now));
                new_spec.renew_time = Some(MicroTime(now));
                new_spec.lease_transitions = Some(spec.lease_transitions.unwrap_or(0) + 1);
            }
        }
        lease.spec = Some(new_spec);
        // `replace` carries the fetched resourceVersion: a concurrent writer makes it fail (409).
        Ok(api.replace(name, &PostParams::default(), &lease).await.is_ok())
    }
}

#[cfg(test)]
#[path = "leader_tests.rs"]
mod leader_tests;
