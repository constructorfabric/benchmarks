//! Leader-only background workers: orphan watchdog and upload reaper (B.9.1, B.9.5).

use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::domain::service::MiniChatService;

/// Decides whether this process runs a leader-only worker role.
#[async_trait::async_trait]
pub trait LeaderElector: Send + Sync {
    async fn is_leader(&self, role: &str) -> bool;
}

/// Single-process elector: always leader (used without the `k8s` feature).
pub struct NoopElector;

#[async_trait::async_trait]
impl LeaderElector for NoopElector {
    async fn is_leader(&self, _role: &str) -> bool {
        true
    }
}

/// Kubernetes Lease elector (`mini-chat-{role}` leases), built with `k8s`.
#[cfg(feature = "k8s")]
pub mod k8s {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use k8s_openapi::api::coordination::v1::Lease;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
    use kube::api::{Api, ObjectMeta, PostParams};
    use parking_lot::Mutex;

    const LEASE_SECS: i32 = 15;
    const RENEW: Duration = Duration::from_secs(2);

    pub struct LeaseElector {
        client: kube::Client,
        namespace: String,
        identity: String,
        last: Mutex<HashMap<String, (Instant, bool)>>,
    }

    impl LeaseElector {
        /// # Errors
        /// Missing `POD_NAMESPACE` / `POD_NAME` or no in-cluster client.
        pub async fn from_env() -> anyhow::Result<Self> {
            let namespace = std::env::var("POD_NAMESPACE")?;
            let identity = std::env::var("POD_NAME")?;
            let client = kube::Client::try_default().await?;
            Ok(Self { client, namespace, identity, last: Mutex::new(HashMap::new()) })
        }

        async fn try_acquire(&self, name: &str) -> anyhow::Result<bool> {
            let api: Api<Lease> = Api::namespaced(self.client.clone(), &self.namespace);
            let now = k8s_openapi::jiff::Timestamp::now();
            match api.get_opt(name).await? {
                None => {
                    let lease = Lease {
                        metadata: ObjectMeta { name: Some(name.to_owned()), ..Default::default() },
                        spec: Some(k8s_openapi::api::coordination::v1::LeaseSpec {
                            holder_identity: Some(self.identity.clone()),
                            lease_duration_seconds: Some(LEASE_SECS),
                            acquire_time: Some(MicroTime(now)),
                            renew_time: Some(MicroTime(now)),
                            ..Default::default()
                        }),
                    };
                    Ok(api.create(&PostParams::default(), &lease).await.is_ok())
                }
                Some(mut lease) => {
                    let spec = lease.spec.clone().unwrap_or_default();
                    let held_by_me = spec.holder_identity.as_deref() == Some(self.identity.as_str());
                    let expired = spec.renew_time.as_ref().is_none_or(|t| {
                        now.as_second() - t.0.as_second() > i64::from(spec.lease_duration_seconds.unwrap_or(LEASE_SECS))
                    });
                    if !held_by_me && !expired {
                        return Ok(false);
                    }
                    let mut new_spec = spec;
                    new_spec.holder_identity = Some(self.identity.clone());
                    new_spec.lease_duration_seconds = Some(LEASE_SECS);
                    new_spec.renew_time = Some(MicroTime(now));
                    if !held_by_me {
                        new_spec.acquire_time = Some(MicroTime(now));
                    }
                    lease.spec = Some(new_spec);
                    Ok(api.replace(name, &PostParams::default(), &lease).await.is_ok())
                }
            }
        }
    }

    #[async_trait::async_trait]
    impl super::LeaderElector for LeaseElector {
        async fn is_leader(&self, role: &str) -> bool {
            let name = format!("mini-chat-{role}");
            let cached = self.last.lock().get(&name).copied();
            if let Some((at, leader)) = cached
                && at.elapsed() < RENEW
            {
                return leader;
            }
            let leader = self.try_acquire(&name).await.unwrap_or(false);
            self.last.lock().insert(name, (Instant::now(), leader));
            leader
        }
    }
}

/// Spawn the orphan watchdog loop.
#[must_use]
pub fn spawn_watchdog(svc: Arc<MiniChatService>, elector: Arc<dyn LeaderElector>, cancel: CancellationToken) -> JoinHandle<()> {
    let every = Duration::from_secs(svc.cfg.orphan_watchdog.scan_interval_secs.max(1));
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                _ = tick.tick() => {
                    if !elector.is_leader("orphan-watchdog").await {
                        continue;
                    }
                    match svc.orphan_scan().await {
                        Ok(n) if n > 0 => tracing::info!(finalized = n, "orphan watchdog finalized turns"),
                        Ok(_) => {}
                        Err(e) => tracing::warn!(error = %e, "orphan watchdog scan failed"),
                    }
                }
            }
        }
    })
}

/// Spawn the upload reaper loop.
#[must_use]
pub fn spawn_reaper(svc: Arc<MiniChatService>, elector: Arc<dyn LeaderElector>, cancel: CancellationToken) -> JoinHandle<()> {
    let every = Duration::from_secs(svc.cfg.upload_reaper.scan_interval_secs.max(1));
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                _ = tick.tick() => {
                    if !elector.is_leader("upload-reaper").await {
                        continue;
                    }
                    match svc.reaper_scan().await {
                        Ok(n) if n > 0 => tracing::info!(reaped = n, "upload reaper failed abandoned uploads"),
                        Ok(_) => {}
                        Err(e) => tracing::warn!(error = %e, "upload reaper scan failed"),
                    }
                }
            }
        }
    })
}
