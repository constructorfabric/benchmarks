//! Storage implementations for the control plane.
//!
//! The gear runs **without a database** (`config/e2e-local.yaml` declares no
//! `database:` block for `oagw`), so the only implementation is the in-memory
//! store. [`memory::MemoryStorage`] is also the reference implementation of
//! [`crate::domain::services::management::ControlPlaneStore`]; a persistent
//! backend would slot in next to it.

pub mod memory;

pub use memory::MemoryStorage;
