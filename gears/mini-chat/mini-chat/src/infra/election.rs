//! Gear-local leader election for the leader-only background workers
//! (orphan watchdog, upload reaper).
//!
//! Without the `k8s` feature a no-op elector is used (single-process mode)
//! and every instance scans. With `k8s` each worker runs under a Kubernetes
//! Lease (`mini-chat-orphan-watchdog` / `mini-chat-upload-reaper`) with a
//! 15 s lease duration and a 2 s renew period (hardcoded per DESIGN); the
//! elector creates a missing Lease at runtime. Double finalization is
//! prevented by the CAS guards in either case (ADR-0010).

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

/// Leadership view consumed by the background workers.
pub trait LeaderElector: Send + Sync {
    /// Whether this instance currently holds the lease.
    fn is_leader(&self) -> bool;
}

/// No-op elector: every instance leads (single-process mode).
struct NoopElector;

impl LeaderElector for NoopElector {
    fn is_leader(&self) -> bool {
        true
    }
}

/// Prepare the elector for the named worker.
///
/// Without the `k8s` feature this is always the no-op elector and never
/// fails. With `k8s`, missing `POD_NAMESPACE`/`POD_NAME` or an unavailable
/// Kubernetes client is a startup error.
#[cfg(not(feature = "k8s"))]
#[allow(clippy::unused_async)] // symmetric with the k8s variant
pub async fn prepare(_lease_name: &str, _cancel: &CancellationToken) -> Result<Arc<dyn LeaderElector>, String> {
    Ok(Arc::new(NoopElector))
}

/// Prepare the elector for the named worker (k8s Lease, see module docs).
#[cfg(feature = "k8s")]
pub async fn prepare(lease_name: &str, cancel: &CancellationToken) -> Result<Arc<dyn LeaderElector>, String> {
    k8s::prepare(lease_name, cancel).await
}

/// Kubernetes Lease elector (`k8s` feature).
#[cfg(feature = "k8s")]
mod k8s {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
    use kube::api::{Api, PostParams};
    use tokio_util::sync::CancellationToken;

    use super::LeaderElector;

    /// Lease duration (hardcoded per DESIGN).
    const LEASE_DURATION_MS: i64 = 15_000;
    /// Renew period (hardcoded per DESIGN).
    const RENEW_PERIOD: std::time::Duration = std::time::Duration::from_secs(2);

    /// A Lease-backed elector. Leadership is inferred from the last
    /// successful acquire/renew: it is held until the lease duration
    /// elapses without a successful renew (e.g. the API server is
    /// unreachable).
    pub struct K8sLeaseElector {
        /// Unix milliseconds of the last successful acquire/renew; `0` when
        /// the lease is not held.
        last_success_ms: AtomicI64,
    }

    impl LeaderElector for K8sLeaseElector {
        fn is_leader(&self) -> bool {
            let last = self.last_success_ms.load(Ordering::Relaxed);
            last > 0 && now_ms() - last < LEASE_DURATION_MS
        }
    }

    /// Current time as unix milliseconds.
    fn now_ms() -> i64 {
        k8s_openapi::jiff::Timestamp::now().as_millisecond()
    }

    /// Current time as a `MicroTime`.
    fn now_micro() -> MicroTime {
        MicroTime(k8s_openapi::jiff::Timestamp::now())
    }

    /// Prepare the elector for `lease_name` and spawn its renew loop.
    pub async fn prepare(lease_name: &str, cancel: &CancellationToken) -> Result<Arc<dyn LeaderElector>, String> {
        let namespace = std::env::var("POD_NAMESPACE")
            .map_err(|_| "POD_NAMESPACE is required for the k8s leader elector".to_owned())?;
        let identity =
            std::env::var("POD_NAME").map_err(|_| "POD_NAME is required for the k8s leader elector".to_owned())?;
        let client = kube::Client::try_default()
            .await
            .map_err(|err| format!("k8s leader elector: no Kubernetes configuration available: {err}"))?;
        let elector = Arc::new(K8sLeaseElector {
            last_success_ms: AtomicI64::new(0),
        });
        let api = Api::<Lease>::namespaced(client, &namespace);
        let name = lease_name.to_owned();
        let task_elector = Arc::clone(&elector);
        let cancel = cancel.clone();
        tokio::spawn(async move { renew_loop(api, name, identity, task_elector, cancel).await });
        Ok(elector)
    }

    /// Renew loop: try to acquire/renew every renew period.
    async fn renew_loop(
        api: Api<Lease>,
        name: String,
        identity: String,
        elector: Arc<K8sLeaseElector>,
        cancel: CancellationToken,
    ) {
        let mut interval = tokio::time::interval(RENEW_PERIOD);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                _ = interval.tick() => {
                    match try_acquire_or_renew(&api, &name, &identity).await {
                        Ok(true) => {
                            elector.last_success_ms.store(now_ms(), Ordering::Relaxed);
                        }
                        Ok(false) => {
                            // Another live pod holds the lease.
                            elector.last_success_ms.store(0, Ordering::Relaxed);
                        }
                        Err(err) => {
                            // Transient API error: keep the previous state;
                            // `is_leader` steps down once the lease duration
                            // has elapsed without a successful renew.
                            tracing::warn!("mini-chat: k8s lease renew failed for {name}: {err}");
                        }
                    }
                }
            }
        }
    }

    /// One acquire/renew round. `Ok(true)` = the lease is held.
    async fn try_acquire_or_renew(api: &Api<Lease>, name: &str, identity: &str) -> Result<bool, kube::Error> {
        match api.get(name).await {
            Err(kube::Error::Api(err)) if err.code == 404 => {
                // Missing Lease: create it and hold it immediately.
                let now = now_micro();
                let lease = Lease {
                    metadata: kube::core::ObjectMeta {
                        name: Some(name.to_owned()),
                        ..Default::default()
                    },
                    spec: Some(LeaseSpec {
                        holder_identity: Some(identity.to_owned()),
                        acquire_time: Some(now.clone()),
                        renew_time: Some(now),
                        lease_duration_seconds: Some(i32::try_from(LEASE_DURATION_MS / 1000).unwrap_or(15)),
                        lease_transitions: Some(0),
                        ..Default::default()
                    }),
                };
                match api.create(&PostParams::default(), &lease).await {
                    Ok(_) => Ok(true),
                    // Lost the create race against another pod.
                    Err(kube::Error::Api(err)) if err.code == 409 => Ok(false),
                    Err(err) => Err(err),
                }
            }
            Err(err) => Err(err),
            Ok(lease) => {
                let mut lease = lease;
                let mut spec = lease.spec.clone().unwrap_or_default();
                let holder = spec.holder_identity.as_deref().unwrap_or("");
                let renew_ms = spec.renew_time.as_ref().map_or(0, |t| t.0.as_millisecond());
                let expired = renew_ms + LEASE_DURATION_MS < now_ms();
                if holder == identity {
                    // Renew our own lease.
                    spec.renew_time = Some(now_micro());
                    lease.spec = Some(spec);
                    match api.replace(name, &PostParams::default(), &lease).await {
                        Ok(_) => Ok(true),
                        Err(kube::Error::Api(err)) if err.code == 409 => Ok(false),
                        Err(err) => Err(err),
                    }
                } else if expired {
                    // Take over an expired lease (optimistic concurrency via
                    // the resource version carried by `replace`).
                    let now = now_micro();
                    spec.holder_identity = Some(identity.to_owned());
                    spec.acquire_time = Some(now.clone());
                    spec.renew_time = Some(now);
                    spec.lease_duration_seconds = Some(i32::try_from(LEASE_DURATION_MS / 1000).unwrap_or(15));
                    spec.lease_transitions = Some(spec.lease_transitions.unwrap_or(0) + 1);
                    lease.spec = Some(spec);
                    match api.replace(name, &PostParams::default(), &lease).await {
                        Ok(_) => Ok(true),
                        Err(kube::Error::Api(err)) if err.code == 409 => Ok(false),
                        Err(err) => Err(err),
                    }
                } else {
                    Ok(false)
                }
            }
        }
    }
}
