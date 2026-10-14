//! OAGW provisioning of the provider upstreams and routes.
//!
//! `provisioning` is the synchronous first pass (S2S exchange, upstream and route creation,
//! deferred set); `reconcile` is the background task that retries the deferred entries.

pub mod knowledge_client;
pub mod provisioning;
pub mod reconcile;
pub mod storage_client;
pub mod summary_client;
