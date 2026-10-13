//! Leader election for the background workers (DESIGN B.9.1, ADR-0010).
//!
//! A worker asks its [`LeaderElector`] before every scan. [`NoopElector`] always says yes
//! (single-process mode); `LeaseElector` (cargo feature `k8s`) holds one Kubernetes Lease per
//! role. Double processing is prevented by the workers' CAS guards in either case.

use async_trait::async_trait;

/// Role of the orphan turn watchdog (Lease `mini-chat-orphan-watchdog`).
pub const ROLE_ORPHAN_WATCHDOG: &str = "orphan-watchdog";
/// Role of the upload reaper (Lease `mini-chat-upload-reaper`).
pub const ROLE_UPLOAD_REAPER: &str = "upload-reaper";

/// Decides whether this process currently runs the background job of `role`.
#[async_trait]
pub trait LeaderElector: Send + Sync {
    /// `true` while this process holds the leadership of `role`.
    async fn is_leader(&self, role: &str) -> bool;
}

/// Elector of single-process deployments: always the leader.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopElector;

#[async_trait]
impl LeaderElector for NoopElector {
    async fn is_leader(&self, _role: &str) -> bool {
        true
    }
}

#[cfg(feature = "k8s")]
pub use lease::LeaseElector;

#[cfg(feature = "k8s")]
mod lease {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    use async_trait::async_trait;
    use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
    use k8s_openapi::jiff::Timestamp;
    use kube::api::{Api, PostParams};
    use tokio_util::sync::CancellationToken;

    use super::LeaderElector;

    /// Prefix of the Lease names (`mini-chat-{role}`); hardcoded in the binary.
    const LEASE_PREFIX: &str = "mini-chat";
    /// Seconds a lease stays valid without a renewal.
    const LEASE_DURATION_SECS: i32 = 15;
    /// Time between two renewals.
    const RENEW_PERIOD: Duration = Duration::from_secs(2);
    /// HTTP status of a conflicting write (stale `resourceVersion` or an existing object).
    const CONFLICT: u16 = 409;

    /// Kubernetes Lease elector: `Lease` `mini-chat-{role}` in `POD_NAMESPACE`, held by
    /// `POD_NAME`, 15 s duration, renewed every 2 s. A missing Lease is created.
    ///
    /// The first `is_leader(role)` makes one acquire attempt and starts a task that renews (or
    /// tries to acquire) the Lease every 2 s; later calls read the task's latest verdict. The
    /// tasks end when the elector is dropped.
    pub struct LeaseElector {
        api: Api<Lease>,
        holder: String,
        roles: Mutex<HashMap<String, Arc<AtomicBool>>>,
        stop: CancellationToken,
    }

    impl LeaseElector {
        /// Elector over the ambient Kubernetes configuration, reading `POD_NAMESPACE` and
        /// `POD_NAME` from the environment.
        ///
        /// # Errors
        /// A missing environment variable or no usable Kubernetes configuration.
        pub async fn from_env() -> anyhow::Result<Self> {
            let namespace = std::env::var("POD_NAMESPACE")
                .map_err(|_| anyhow::anyhow!("POD_NAMESPACE is required for leader election"))?;
            let holder = std::env::var("POD_NAME")
                .map_err(|_| anyhow::anyhow!("POD_NAME is required for leader election"))?;
            let client = kube::Client::try_default().await?;
            Ok(Self::new(client, &namespace, holder))
        }

        /// Elector over `client`, with leases in `namespace` held as `holder`.
        #[must_use]
        pub fn new(client: kube::Client, namespace: &str, holder: String) -> Self {
            Self {
                api: Api::namespaced(client, namespace),
                holder,
                roles: Mutex::new(HashMap::new()),
                stop: CancellationToken::new(),
            }
        }

        fn lease_name(role: &str) -> String {
            format!("{LEASE_PREFIX}-{role}")
        }
    }

    impl Drop for LeaseElector {
        fn drop(&mut self) {
            self.stop.cancel();
        }
    }

    /// What one acquire / renew attempt of a role needs; cheap to clone into the renew task.
    #[derive(Clone)]
    struct Attempt {
        api: Api<Lease>,
        name: String,
        holder: String,
    }

    impl Attempt {
        /// One attempt: `true` when this process holds the Lease afterwards. An API error is
        /// logged and counts as "not leader".
        async fn run(&self) -> bool {
            match self.try_hold().await {
                Ok(held) => held,
                Err(err) => {
                    tracing::warn!(lease = %self.name, error = %err, "leader election attempt failed");
                    false
                }
            }
        }

        async fn try_hold(&self) -> Result<bool, kube::Error> {
            let now = Timestamp::now();
            let Some(current) = self.api.get_opt(&self.name).await? else {
                return self.write(self.api_create(now)).await;
            };
            let spec = current.spec.clone().unwrap_or_default();
            let mine = spec.holder_identity.as_deref() == Some(self.holder.as_str());
            if !mine && !expired(&spec, now) {
                return Ok(false);
            }
            let transitions = spec.lease_transitions.unwrap_or(0);
            let next = Lease {
                metadata: ObjectMeta {
                    name: Some(self.name.clone()),
                    // Optimistic concurrency: a concurrent writer makes the replace conflict.
                    resource_version: current.metadata.resource_version.clone(),
                    ..ObjectMeta::default()
                },
                spec: Some(LeaseSpec {
                    holder_identity: Some(self.holder.clone()),
                    lease_duration_seconds: Some(LEASE_DURATION_SECS),
                    acquire_time: if mine {
                        spec.acquire_time.clone()
                    } else {
                        Some(MicroTime(now))
                    },
                    renew_time: Some(MicroTime(now)),
                    lease_transitions: Some(if mine {
                        transitions
                    } else {
                        transitions.saturating_add(1)
                    }),
                    ..LeaseSpec::default()
                }),
            };
            self.write(Write::Replace(next)).await
        }

        fn api_create(&self, now: Timestamp) -> Write {
            Write::Create(Lease {
                metadata: ObjectMeta {
                    name: Some(self.name.clone()),
                    ..ObjectMeta::default()
                },
                spec: Some(LeaseSpec {
                    holder_identity: Some(self.holder.clone()),
                    lease_duration_seconds: Some(LEASE_DURATION_SECS),
                    acquire_time: Some(MicroTime(now)),
                    renew_time: Some(MicroTime(now)),
                    lease_transitions: Some(0),
                    ..LeaseSpec::default()
                }),
            })
        }

        /// Performs the write; a conflict means another pod won the race (not leader).
        async fn write(&self, write: Write) -> Result<bool, kube::Error> {
            let pp = PostParams::default();
            let result = match write {
                Write::Create(lease) => self.api.create(&pp, &lease).await,
                Write::Replace(lease) => self.api.replace(&self.name, &pp, &lease).await,
            };
            match result {
                Ok(_) => Ok(true),
                Err(kube::Error::Api(status)) if status.code == CONFLICT => Ok(false),
                Err(err) => Err(err),
            }
        }
    }

    enum Write {
        Create(Lease),
        Replace(Lease),
    }

    /// The holder did not renew within the lease duration (or there is no holder).
    fn expired(spec: &LeaseSpec, now: Timestamp) -> bool {
        let Some(holder) = spec.holder_identity.as_deref() else {
            return true;
        };
        if holder.is_empty() {
            return true;
        }
        let duration = i64::from(spec.lease_duration_seconds.unwrap_or(LEASE_DURATION_SECS));
        spec.renew_time
            .as_ref()
            .or(spec.acquire_time.as_ref())
            .is_none_or(|t| now.as_second() >= t.0.as_second().saturating_add(duration))
    }

    #[async_trait]
    impl LeaderElector for LeaseElector {
        async fn is_leader(&self, role: &str) -> bool {
            let known = self
                .roles
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(role)
                .cloned();
            if let Some(flag) = known {
                return flag.load(Ordering::SeqCst);
            }
            let attempt = Attempt {
                api: self.api.clone(),
                name: Self::lease_name(role),
                holder: self.holder.clone(),
            };
            // The first verdict is synchronous, so the first scan does not wait for a renewal.
            let flag = Arc::new(AtomicBool::new(attempt.run().await));
            let flag = {
                let mut roles = self.roles.lock().unwrap_or_else(PoisonError::into_inner);
                // Another caller may have registered the role meanwhile; its task owns the flag.
                if let Some(existing) = roles.get(role) {
                    return existing.load(Ordering::SeqCst);
                }
                roles.insert(role.to_owned(), Arc::clone(&flag));
                flag
            };
            let verdict = flag.load(Ordering::SeqCst);
            let stop = self.stop.clone();
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(RENEW_PERIOD);
                // The first tick is immediate; the first verdict was just taken.
                ticker.tick().await;
                loop {
                    tokio::select! {
                        () = stop.cancelled() => break,
                        _ = ticker.tick() => {}
                    }
                    let held = attempt.run().await;
                    if flag.swap(held, Ordering::SeqCst) != held {
                        tracing::info!(lease = %attempt.name, leader = held, "leadership changed");
                    }
                }
            });
            verdict
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn spec(
            holder: Option<&str>,
            renewed_secs: Option<i64>,
            duration: Option<i32>,
        ) -> LeaseSpec {
            LeaseSpec {
                holder_identity: holder.map(str::to_owned),
                renew_time: renewed_secs
                    .map(|s| MicroTime(Timestamp::from_second(s).expect("timestamp"))),
                lease_duration_seconds: duration,
                ..LeaseSpec::default()
            }
        }

        fn at(secs: i64) -> Timestamp {
            Timestamp::from_second(secs).expect("timestamp")
        }

        #[test]
        fn lease_expires_after_its_duration_without_renewal() {
            let held = spec(Some("pod-a"), Some(1_000), Some(15));
            assert!(!expired(&held, at(1_000)));
            assert!(!expired(&held, at(1_014)));
            assert!(expired(&held, at(1_015)));
            assert!(expired(&held, at(2_000)));
        }

        #[test]
        fn lease_without_holder_or_renewal_is_expired() {
            assert!(expired(&spec(None, Some(1_000), Some(15)), at(1_000)));
            assert!(expired(&spec(Some(""), Some(1_000), Some(15)), at(1_000)));
            assert!(expired(&spec(Some("pod-a"), None, Some(15)), at(1_000)));
        }

        #[test]
        fn missing_duration_defaults_to_fifteen_seconds() {
            let held = spec(Some("pod-a"), Some(1_000), None);
            assert!(!expired(&held, at(1_014)));
            assert!(expired(&held, at(1_015)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_elector_is_leader() {
        let elector = NoopElector;
        assert!(elector.is_leader(ROLE_ORPHAN_WATCHDOG).await);
        assert!(elector.is_leader(ROLE_UPLOAD_REAPER).await);
        assert!(elector.is_leader("anything").await);
    }
}
