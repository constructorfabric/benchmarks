// Created: 2026-08-31 by Constructor Tech
//! Upstream lifecycle events shared by the control plane and the data plane.
//!
//! The control plane owns the life of an upstream record; the data plane keeps
//! per-upstream ephemeral state (the round-robin cursor of an endpoint pool).
//! Neither depends on the other, so the control plane publishes the removal
//! through this trait and the data plane subscribes to it in the gear wiring
//! ([`crate::gear`]).

use uuid::Uuid;

/// Notified when an upstream record leaves the control plane.
pub trait UpstreamRemoval: Send + Sync {
    /// The upstream `upstream_id` was deleted, routes and all.
    fn upstream_removed(&self, upstream_id: Uuid);
}
