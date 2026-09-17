// Created: 2026-09-03 by Constructor Tech
//! # Outbound API Gateway (`oagw`)
//!
//! A single-process gateway that lets a tenant call third-party services
//! through a stable, policy-checked route. Two planes share one store:
//!
//! * **Control plane** — CRUD for upstreams (endpoint pools), routes (match
//!   rules that bind an upstream) and plugin definitions, all scoped to the
//!   calling tenant.
//! * **Data plane** — `/oagw/v1/proxy/{alias}/...` reverse-proxies an
//!   outbound request: it matches a route, resolves credentials, applies
//!   guard and transform plugins, rate limits, streams the body and relays
//!   protocol upgrades.
//!
//! Everything OAGW emits itself is an RFC 9457 problem document whose `type`
//! member is a GTS identifier (see [`error::ErrorKind`]); upstream responses
//! are relayed untouched and only tagged with
//! `X-OAGW-Error-Source: upstream` when they carry a 4xx/5xx status.
//!
//! ## Layout
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`model`] | Domain types and create-time validation |
//! | [`config`] | `gears.oagw.config` |
//! | [`store`] | In-memory tenant-scoped store |
//! | [`alias`] | Alias derivation and immutability |
//! | [`plugins`] | Plugin catalogue, resolution and configuration readers |
//! | [`oauth`] | OAuth2 client-credential grants with a bounded cache |
//! | [`rate_limit`] | Token-bucket and sliding-window counters |
//! | [`proxy`] | The data-plane pipeline |
//! | [`control`] | The control-plane services |
//! | [`api`] | Axum handlers and OpenAPI registration |
//! | [`gear`] | Gear declaration and REST wiring |

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]

// === MODULES ===
pub mod alias;
pub mod body;
pub mod config;
pub mod control;
pub mod error;
pub mod gear;
pub mod gts;
pub mod model;
pub mod oauth;
pub mod odata;
pub mod plugins;
pub mod proxy;
pub mod rate_limit;
pub mod state;
pub mod store;
pub mod upstream_client;

#[doc(hidden)]
pub mod api;

// === GEAR ===
pub use gear::OagwGear;
