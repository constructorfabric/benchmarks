//! Leader election of the leader-only workers (orphan watchdog, upload
//! reaper).
//!
//! Without the `k8s` feature every process is the leader ([`NoopElector`]
//! in the domain); with it a Kubernetes `Lease` named `mini-chat-{role}` in
//! the pod namespace is held per role (15 s lease, renewed every 2 s).

#[cfg(feature = "k8s")]
pub use k8s::LeaseElector;

#[cfg(feature = "k8s")]
mod k8s {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
    use k8s_openapi::jiff::Timestamp;
    use kube::Api;
    use kube::api::PostParams;
    use parking_lot::Mutex;
    use tokio_util::sync::CancellationToken;

    use crate::domain::service::workers::LeaderElector;

    const LEASE_SECONDS: i32 = 15;
    const RENEW_PERIOD: Duration = Duration::from_secs(2);
    const PREFIX: &str = "mini-chat";

    /// Kubernetes `Lease` elector.
    pub struct LeaseElector {
        api: Api<Lease>,
        identity: String,
        leading: Arc<Mutex<HashMap<String, bool>>>,
        renewers: Mutex<HashMap<String, CancellationToken>>,
        cancel: CancellationToken,
    }

    impl LeaseElector {
        /// Build the elector from the in-cluster config (`POD_NAMESPACE`,
        /// `POD_NAME`).
        ///
        /// # Errors
        /// Missing environment or Kubernetes client configuration.
        pub async fn from_env(cancel: CancellationToken) -> anyhow::Result<Self> {
            let namespace = std::env::var("POD_NAMESPACE")
                .map_err(|_| anyhow::anyhow!("POD_NAMESPACE is required for leader election"))?;
            let identity = std::env::var("POD_NAME")
                .map_err(|_| anyhow::anyhow!("POD_NAME is required for leader election"))?;
            let client = kube::Client::try_default().await?;
            Ok(Self {
                api: Api::namespaced(client, &namespace),
                identity,
                leading: Arc::new(Mutex::new(HashMap::new())),
                renewers: Mutex::new(HashMap::new()),
                cancel,
            })
        }

        async fn try_acquire(api: &Api<Lease>, name: &str, identity: &str) -> bool {
            let now = MicroTime(Timestamp::now());
            match api.get_opt(name).await {
                Ok(None) => {
                    let lease = Lease {
                        metadata: ObjectMeta {
                            name: Some(name.to_owned()),
                            ..ObjectMeta::default()
                        },
                        spec: Some(LeaseSpec {
                            holder_identity: Some(identity.to_owned()),
                            lease_duration_seconds: Some(LEASE_SECONDS),
                            acquire_time: Some(now.clone()),
                            renew_time: Some(now),
                            lease_transitions: Some(0),
                            ..LeaseSpec::default()
                        }),
                    };
                    api.create(&PostParams::default(), &lease).await.is_ok()
                }
                Ok(Some(mut lease)) => {
                    let spec = lease.spec.clone().unwrap_or_default();
                    let holder = spec.holder_identity.clone().unwrap_or_default();
                    let duration = i64::from(spec.lease_duration_seconds.unwrap_or(LEASE_SECONDS));
                    let expired = spec
                        .renew_time
                        .as_ref()
                        .is_none_or(|t| Timestamp::now().as_second() - t.0.as_second() > duration);
                    if holder != identity && !expired {
                        return false;
                    }
                    let transitions =
                        spec.lease_transitions.unwrap_or(0) + i32::from(holder != identity);
                    lease.spec = Some(LeaseSpec {
                        holder_identity: Some(identity.to_owned()),
                        lease_duration_seconds: Some(LEASE_SECONDS),
                        acquire_time: if holder == identity {
                            spec.acquire_time
                        } else {
                            Some(now.clone())
                        },
                        renew_time: Some(now),
                        lease_transitions: Some(transitions),
                        ..spec
                    });
                    // resourceVersion in metadata makes the replace a CAS
                    api.replace(name, &PostParams::default(), &lease)
                        .await
                        .is_ok()
                }
                Err(e) => {
                    tracing::warn!(error = %e, lease = name, "mini-chat: lease read failed");
                    false
                }
            }
        }

        fn ensure_renewer(&self, name: &str) {
            let mut renewers = self.renewers.lock();
            if renewers.contains_key(name) {
                return;
            }
            let token = self.cancel.child_token();
            renewers.insert(name.to_owned(), token.clone());
            let api = self.api.clone();
            let identity = self.identity.clone();
            let leading = Arc::clone(&self.leading);
            let name = name.to_owned();
            tokio::spawn(async move {
                loop {
                    let ok = Self::try_acquire(&api, &name, &identity).await;
                    leading.lock().insert(name.clone(), ok);
                    tokio::select! {
                        () = token.cancelled() => return,
                        () = tokio::time::sleep(RENEW_PERIOD) => {}
                    }
                }
            });
        }
    }

    #[async_trait]
    impl LeaderElector for LeaseElector {
        async fn is_leader(&self, role: &str) -> bool {
            let name = format!("{PREFIX}-{role}");
            self.ensure_renewer(&name);
            if let Some(v) = self.leading.lock().get(&name) {
                return *v;
            }
            let ok = Self::try_acquire(&self.api, &name, &self.identity).await;
            self.leading.lock().insert(name, ok);
            ok
        }
    }
}
