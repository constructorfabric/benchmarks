//! The Kubernetes adapter of the Lease API port (cargo feature `k8s` only):
//! GET, CREATE and REPLACE of `coordination.k8s.io/v1` Leases in the namespace
//! of the pod through the `kube` client already declared by the gear.

use std::sync::Arc;

use async_trait::async_trait;
use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{MicroTime, ObjectMeta};
use k8s_openapi::jiff::Timestamp;
use kube::api::PostParams;
use kube::{Api, Client, Config};
use time::OffsetDateTime;

use super::leader::{ElectorError, LeaderElector, LeaderRole};
use super::lease::{LeaseApi, LeaseApiError, LeaseRecord, PodIdentity, start_lease_elector};
use crate::domain::service::workers::clock::{SharedWorkerClock, TokioWorkerClock};
use crate::infra::background_tasks::BackgroundTasks;

/// The Lease API over a `kube` client.
pub struct KubeLeaseApi {
    api: Api<Lease>,
}

impl KubeLeaseApi {
    /// The Leases of `namespace`.
    #[must_use]
    pub fn new(client: Client, namespace: &str) -> Self {
        Self {
            api: Api::namespaced(client, namespace),
        }
    }
}

fn micro_time(at: OffsetDateTime) -> Option<MicroTime> {
    let micros = i64::try_from(at.unix_timestamp_nanos().div_euclid(1_000)).ok()?;
    Timestamp::from_microsecond(micros).ok().map(MicroTime)
}

fn offset_time(at: &MicroTime) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(at.0.as_microsecond()) * 1_000).ok()
}

fn record_of(lease: &Lease) -> LeaseRecord {
    let spec = lease.spec.clone().unwrap_or_default();
    LeaseRecord {
        holder: spec.holder_identity,
        lease_duration_secs: spec.lease_duration_seconds.unwrap_or(0),
        acquire_time: spec.acquire_time.as_ref().and_then(offset_time),
        renew_time: spec.renew_time.as_ref().and_then(offset_time),
        transitions: spec.lease_transitions.unwrap_or(0),
        resource_version: lease.metadata.resource_version.clone().unwrap_or_default(),
    }
}

fn lease_of(name: &str, record: &LeaseRecord) -> Lease {
    Lease {
        metadata: ObjectMeta {
            name: Some(name.to_owned()),
            resource_version: (!record.resource_version.is_empty())
                .then(|| record.resource_version.clone()),
            ..ObjectMeta::default()
        },
        spec: Some(LeaseSpec {
            holder_identity: Some(record.holder.clone().unwrap_or_default()),
            lease_duration_seconds: Some(record.lease_duration_secs),
            acquire_time: record.acquire_time.and_then(micro_time),
            renew_time: record.renew_time.and_then(micro_time),
            lease_transitions: Some(record.transitions),
            ..LeaseSpec::default()
        }),
    }
}

fn api_error(error: &kube::Error) -> LeaseApiError {
    match error {
        kube::Error::Api(status) if status.code == 409 => LeaseApiError::Conflict,
        other => LeaseApiError::Failed(other.to_string()),
    }
}

#[async_trait]
impl LeaseApi for KubeLeaseApi {
    async fn get(&self, name: &str) -> Result<Option<LeaseRecord>, LeaseApiError> {
        self.api
            .get_opt(name)
            .await
            .map(|lease| lease.as_ref().map(record_of))
            .map_err(|error| api_error(&error))
    }

    async fn create(&self, name: &str, record: &LeaseRecord) -> Result<LeaseRecord, LeaseApiError> {
        self.api
            .create(&PostParams::default(), &lease_of(name, record))
            .await
            .map(|lease| record_of(&lease))
            .map_err(|error| api_error(&error))
    }

    async fn replace(
        &self,
        name: &str,
        record: &LeaseRecord,
    ) -> Result<LeaseRecord, LeaseApiError> {
        self.api
            .replace(name, &PostParams::default(), &lease_of(name, record))
            .await
            .map(|lease| record_of(&lease))
            .map_err(|error| api_error(&error))
    }
}

/// Starts the Lease elector of a `k8s` build: reads `POD_NAMESPACE` and
/// `POD_NAME`, builds the in-cluster client and spawns one renew task per role.
///
/// # Errors
///
/// [`ElectorError`] naming the missing variable or the client failure.
pub fn start_kube_elector(
    roles: &[LeaderRole],
    tasks: &BackgroundTasks,
) -> Result<Arc<dyn LeaderElector>, ElectorError> {
    let identity = PodIdentity::from_env()?;
    // @cpt-begin:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-build-kube-client
    let config = Config::incluster().map_err(|error| ElectorError::Client(error.to_string()))?;
    let client =
        Client::try_from(config).map_err(|error| ElectorError::Client(error.to_string()))?;
    // @cpt-end:cpt-cf-mini-chat-algo-background-workers-elect-leader:p1:inst-build-kube-client
    let api: Arc<dyn LeaseApi> = Arc::new(KubeLeaseApi::new(client, &identity.namespace));
    let clock: SharedWorkerClock = Arc::new(TokioWorkerClock::new());
    Ok(start_lease_elector(roles, &identity, &api, &clock, tasks))
}
