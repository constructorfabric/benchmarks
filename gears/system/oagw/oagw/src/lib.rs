//! OAGW — Outbound API Gateway gear.
//!
//! A per-alias routing, authentication, security and governance layer for
//! outbound API calls, implemented as a Constructor Fabric gear serving
//! tenant-scoped management CRUD and a proxy API under `/oagw/v1/...`.
//!
//! See `docs/DESIGN.md`, the PRD and ADRs 0001-0009 in the same directory
//! for the authoritative behavior contract.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![allow(clippy::module_name_repetitions)]

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;
