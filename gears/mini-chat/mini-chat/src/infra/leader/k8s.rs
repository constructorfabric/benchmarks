//! Kubernetes Lease elector: one Lease per worker role, 15 s lease duration, 2 s renew period.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use k8s_openapi::jiff::Timestamp;
use kube::api::{Api, PostParams};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::domain::orphan::LeaderElector;

/// How long a holder owns the Lease after its last renewal.
const LEASE_DURATION_SECS: i32 = 15;
/// How often the holder renews and the others try to acquire.
const RENEW_PERIOD: Duration = Duration::from_secs(2);
/// HTTP conflict: another pod updated the Lease first.
const CONFLICT: u16 = 409;

/// The leadership state of this pod for one Lease.
pub struct LeaseElector {
    leader: AtomicBool,
}

impl LeaderElector for LeaseElector {
    fn is_leader(&self) -> bool {
        self.leader.load(Ordering::Acquire)
    }
}

impl LeaseElector {
    /// Connects to the cluster and spawns the re-election task.
    ///
    /// # Errors
    /// An error when the cluster client cannot be created.
    pub async fn start(
        lease_name: String,
        namespace: &str,
        identity: String,
        cancel: CancellationToken,
    ) -> anyhow::Result<(Arc<dyn LeaderElector>, JoinHandle<()>)> {
        let client = kube::Client::try_default()
            .await
            .map_err(|err| anyhow::anyhow!("failed to create the Kubernetes client: {err}"))?;
        let api: Api<Lease> = Api::namespaced(client, namespace);
        let elector = Arc::new(Self {
            leader: AtomicBool::new(false),
        });
        let task = tokio::spawn(Arc::clone(&elector).run(api, lease_name, identity, cancel));
        Ok((elector, task))
    }

    async fn run(
        self: Arc<Self>,
        api: Api<Lease>,
        lease_name: String,
        identity: String,
        cancel: CancellationToken,
    ) {
        let mut last_renewed: Option<Instant> = None;
        loop {
            last_renewed = renew_round(&api, &lease_name, &identity, last_renewed).await;
            let leader = last_renewed.is_some_and(|at| at.elapsed() < validity());
            if self.leader.swap(leader, Ordering::AcqRel) != leader {
                info!(lease = %lease_name, leader, "leadership changed");
            }
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(RENEW_PERIOD) => {}
            }
        }
        if self.leader.swap(false, Ordering::AcqRel) {
            release_logged(&api, &lease_name, &identity).await;
        }
    }
}

/// Leadership is claimed only while the last renewal is safely inside the lease duration.
fn validity() -> Duration {
    Duration::from_secs(u64::from(LEASE_DURATION_SECS.unsigned_abs()))
        .saturating_sub(2 * RENEW_PERIOD)
}

/// One bounded election round; returns the instant of the last successful renewal.
async fn renew_round(
    api: &Api<Lease>,
    lease_name: &str,
    identity: &str,
    last_renewed: Option<Instant>,
) -> Option<Instant> {
    match tokio::time::timeout(RENEW_PERIOD, acquire_or_renew(api, lease_name, identity)).await {
        Ok(Ok(true)) => Some(Instant::now()),
        Ok(Ok(false)) => None,
        Ok(Err(err)) => {
            warn!(lease = %lease_name, error = %err, "the leader lease could not be renewed");
            last_renewed
        }
        Err(_) => {
            warn!(lease = %lease_name, "the leader lease renewal timed out");
            last_renewed
        }
    }
}

async fn release_logged(api: &Api<Lease>, lease_name: &str, identity: &str) {
    if let Err(err) = release(api, lease_name, identity).await {
        warn!(lease = %lease_name, error = %err, "the leader lease could not be released");
    }
}

fn is_conflict(err: &kube::Error) -> bool {
    matches!(err, kube::Error::Api(status) if status.code == CONFLICT)
}

/// Whether another pod holds the Lease and its last renewal is still inside the lease duration.
fn held_by_other(spec: &LeaseSpec, identity: &str, now: Timestamp) -> bool {
    let holder = spec
        .holder_identity
        .as_deref()
        .filter(|holder| !holder.is_empty());
    let Some(holder) = holder else {
        return false;
    };
    if holder == identity {
        return false;
    }
    let duration = i64::from(spec.lease_duration_seconds.unwrap_or(LEASE_DURATION_SECS));
    matches!(
        &spec.renew_time,
        Some(MicroTime(renewed)) if now.as_second() - renewed.as_second() <= duration
    )
}

/// One election round: creates the Lease, renews it when held by `identity`, takes it over when
/// its holder let it expire. `Ok(true)` when `identity` holds the Lease afterwards.
async fn acquire_or_renew(
    api: &Api<Lease>,
    lease_name: &str,
    identity: &str,
) -> Result<bool, kube::Error> {
    let now = Timestamp::now();
    let Some(mut lease) = api.get_opt(lease_name).await? else {
        let lease = Lease {
            metadata: ObjectMeta {
                name: Some(lease_name.to_owned()),
                ..ObjectMeta::default()
            },
            spec: Some(LeaseSpec {
                holder_identity: Some(identity.to_owned()),
                lease_duration_seconds: Some(LEASE_DURATION_SECS),
                acquire_time: Some(MicroTime(now)),
                renew_time: Some(MicroTime(now)),
                lease_transitions: Some(0),
                ..LeaseSpec::default()
            }),
        };
        return match api.create(&PostParams::default(), &lease).await {
            Ok(_) => Ok(true),
            Err(err) if is_conflict(&err) => Ok(false),
            Err(err) => Err(err),
        };
    };
    let spec = lease.spec.get_or_insert_with(LeaseSpec::default);
    if held_by_other(spec, identity, now) {
        return Ok(false);
    }
    if spec.holder_identity.as_deref() == Some(identity) {
        spec.renew_time = Some(MicroTime(now));
    } else {
        spec.holder_identity = Some(identity.to_owned());
        spec.lease_duration_seconds = Some(LEASE_DURATION_SECS);
        spec.acquire_time = Some(MicroTime(now));
        spec.renew_time = Some(MicroTime(now));
        spec.lease_transitions = Some(spec.lease_transitions.unwrap_or(0).saturating_add(1));
    }
    // The update carries the resource version read above: a concurrent update is a conflict.
    match api
        .replace(lease_name, &PostParams::default(), &lease)
        .await
    {
        Ok(_) => Ok(true),
        Err(err) if is_conflict(&err) => Ok(false),
        Err(err) => Err(err),
    }
}

/// Gives the Lease up so another pod can take over without waiting for the expiry.
async fn release(api: &Api<Lease>, lease_name: &str, identity: &str) -> Result<(), kube::Error> {
    let Some(mut lease) = api.get_opt(lease_name).await? else {
        return Ok(());
    };
    let Some(spec) = lease.spec.as_mut() else {
        return Ok(());
    };
    if spec.holder_identity.as_deref() != Some(identity) {
        return Ok(());
    }
    spec.holder_identity = None;
    api.replace(lease_name, &PostParams::default(), &lease)
        .await
        .map(|_| ())
}
