// Updated: 2026-09-01 by Constructor Tech
//! Control Plane services.
//!
//! [`management::ManagementService`] owns every rule the management API
//! enforces: validation, alias derivation and immutability, uniqueness,
//! enable/disable semantics, plugin immutability and the `PluginInUse` check,
//! and the tenant scoping that keeps one tenant's roster invisible to another.
//! The REST handlers are thin projections of it.

pub mod management;

pub use management::ManagementService;
