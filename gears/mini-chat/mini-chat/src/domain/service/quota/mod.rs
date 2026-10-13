//! Quota and billing service of the gear
//! (`cpt-cf-mini-chat-feature-quota-billing`): integer credit arithmetic,
//! period keys, policy resolution, reserve estimation, the preflight tier
//! cascade, the reserve write contract, billing outcome derivation, quota
//! settlement, usage events, CAS-guarded turn finalization, per-turn tool
//! limits and the quota view behind `GET /v1/quota/status`.
//!
//! Everything here is database-free: the persistence side is the
//! [`ports::QuotaRepository`] port, implemented by
//! `infra::db::repos::quota::SeaQuotaRepository`.

// @cpt-dod:cpt-cf-mini-chat-dod-quota-billing-quality-gates:p1

pub mod arith;
pub mod estimate;
pub mod events;
pub mod finalize;
pub mod periods;
pub mod policy;
pub mod ports;
pub mod preflight;
pub mod reserve;
pub mod service;
pub mod settle;
pub mod status;
pub mod tools;
pub mod types;

#[cfg(test)]
pub(crate) mod test_support;
