//! # `oagw` — Outbound API Gateway
//!
//! Centralized outbound API gateway: every call a gear makes to an external
//! service flows through OAGW, which resolves the target upstream, injects
//! credentials, applies policy and forwards the request.
//!
//! This crate implements the **foundation and the control plane** (slices 1-5
//! of the gear build order):
//!
//! | Module | Contents |
//! |---|---|
//! | [`config`] | deployment configuration (`oagw.config`) |
//! | [`domain`] | model, alias derivation, validation, plugin contracts, repository traits, [`domain::services::ControlPlaneService`] |
//! | [`infra`] | in-memory tenant-scoped repositories behind the domain traits, data-plane placeholder |
//! | [`api`] | management REST transport (`api/rest/`) and the proxy stub route |
//!
//! Deliberately out of scope for these slices (later build order): the data
//! plane (Pingora bridge), streaming, gRPC, rate limiting, CORS, the circuit
//! breaker and Prometheus metrics. The proxy route is registered and answers
//! `503` with a gateway problem until the data plane lands.
//!
//! Route paths are registered gear-relative (`/oagw/v1/...`): the operator's
//! API gateway supplies the `/api` segment through its `prefix_path`, so a
//! gear never writes `/api` into a route path.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod api;
pub mod config;
pub mod domain;
pub mod gear;
pub mod infra;

pub use config::OagwConfig;
pub use gear::OagwGear;
