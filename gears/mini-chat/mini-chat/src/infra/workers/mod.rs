//! Leader-only background workers: the leader elector with its Lease logic,
//! the upload reaper and the orphan watchdog.

pub mod leader;
pub mod lease;
pub mod orphan_watchdog;
pub mod upload_reaper;

#[cfg(feature = "k8s")]
pub mod kube_lease;
