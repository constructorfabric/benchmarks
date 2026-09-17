//! Control-plane and data-plane services.
//!
//! * [`management`] — the control plane (`ControlPlaneService` in DESIGN.md):
//!   CRUD over upstreams/routes/plugins plus alias resolution. Implemented
//!   here, synchronous, storage-backed.
//! * [`alias`] — pure alias derivation / validation / shadowing functions.
//!
//! The data plane (`DataPlaneService`, `infra/proxy/`) is added in part 2 and
//! consumes both.

pub mod alias;
pub mod management;

pub use management::{
    ControlPlaneStore, ListResult, ManagementService, PluginDraft, RouteDraft, UpstreamDraft,
};
