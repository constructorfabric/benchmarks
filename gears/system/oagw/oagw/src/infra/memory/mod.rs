//! In-memory implementation of [`crate::domain::repo::ConfigStore`].
//!
//! Persistence in this deployment is in-memory: the gear's runtime config
//! carries no database section, so the store is a lock-protected map
//! partitioned per tenant with `dashmap` + `parking_lot`. The whole partition
//! sits behind a single `RwLock` so every cross-collection operation (the
//! upstream → route cascade, the plugin reference scan) is atomic without a
//! second locking protocol.
mod store;

pub use store::MemoryStore;
