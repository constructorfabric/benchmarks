// Updated: 2026-09-01 by Constructor Tech
//! The infrastructure layer: everything the domain calls out to.
//!
//! `plugin` holds the builtin implementations of the three ADR-0002 contracts;
//! `proxy` is the Data Plane; `storage` is the Control Plane store;
//! `type_provisioning` registers the gear's GTS identifiers with the types
//! registry.

pub mod plugin;
pub mod proxy;
pub mod storage;
pub mod type_provisioning;
