//! Domain services: control plane (management) and data plane (proxy).
//!
//! - [`management`] — `ControlPlaneService`: alias derivation/enforcement,
//!   resource CRUD, tenant-chain resolution and effective-config merging.
//! - [`data_plane`] — `DataPlaneService`: the proxy execution orchestrator.
//! - [`alias`] — pure alias-derivation helpers (PRD §5.5).

pub mod alias;
pub mod data_plane;
pub mod management;
