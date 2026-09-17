//! Infrastructure adapters implementing the domain ports.
//!
//! [`memory`] implements the management plane's `ConfigStore` port;
//! [`plugin`] holds the built-in plugins and [`proxy`] the data plane.
pub mod memory;
pub mod plugin;
pub mod proxy;

pub use memory::MemoryStore;
