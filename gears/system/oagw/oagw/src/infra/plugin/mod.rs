//! Plugin catalog of the OAGW gear
//! (`cpt-cf-oagw-algo-plugin-catalog-register`).
//!
//! Entry 2.3's infra half: the three builtin registries, the reserved
//! catalog-only identifier set and the registration of the plugin type schemas
//! with `cpt-cf-oagw-actor-types-registry`. The plugin traits live in
//! [`crate::domain::plugin`], so the domain layer holds no registry dependency
//! (`inst-pcrg-09`).
//!
//! Nothing here resolves a plugin into an executable instance or runs one: that
//! is the data plane of entry 2.5 (`inst-pcat-11`). A registry entry is a typed
//! *declaration* — identity, type and declared phases — and the catalog is
//! read-only for the lifetime of the process.

pub mod catalog;
pub mod registry;

pub use catalog::{PluginCatalog, PluginCatalogError};
