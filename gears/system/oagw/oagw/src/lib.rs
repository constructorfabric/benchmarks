//! `oagw` — Constructor Fabric outbound API gateway gear.
//!
//! Feature 1 (Gear Foundation) delivers the gear declaration with its gear
//! dependencies, the `gears.oagw.config` binding, the DDD-Light module layout,
//! the gear-relative REST mount root `/oagw/v1`, the canonical
//! `application/problem+json` error mapping and the `X-OAGW-Error-Source`
//! response-header layer. Later decomposition entries (2.2 to 2.7) populate the
//! modules that are declared — but intentionally empty — here.
//!
//! # Layering (`cpt-cf-oagw-design-layers`)
//!
//! ```text
//! api/rest   transport: routes, handlers, DTOs, error response mapping
//! domain     services, models, repository contracts, domain errors
//! infra      proxy engine, storage, plugin registries, type provisioning
//! ```
//!
//! The dependency direction is one-way: the `domain` layer depends on neither
//! `infra` nor HTTP types, `infra` implements the `domain` repository
//! contracts, and `api` is the only layer that maps between HTTP and domain
//! types.

#![forbid(unsafe_code)]

// @cpt-begin:cpt-cf-oagw-dod-module-layout:p1:inst-full
/// Transport layer: REST routes, handlers, DTOs and the error-response mapping.
#[doc(hidden)]
pub mod api;

/// Typed binding of the `gears.oagw.config` configuration section.
pub mod config;

/// Domain layer: entities, domain errors and repository contracts.
#[doc(hidden)]
pub mod domain;

/// Gear declaration, configuration load and readiness reporting.
pub mod gear;

/// Infrastructure layer: proxy engine, storage, plugin registries, type provisioning.
#[doc(hidden)]
pub mod infra;
// @cpt-end:cpt-cf-oagw-dod-module-layout:p1:inst-full

pub use config::OagwConfig;
pub use gear::OagwGear;
