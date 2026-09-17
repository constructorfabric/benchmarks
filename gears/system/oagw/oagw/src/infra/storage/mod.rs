//! In-memory configuration store (no `toolkit-db` dependency in this crate).
//!
//! The control plane keeps all upstream/route/plugin records in process
//! memory behind `parking_lot` locks; the data plane reads through an L1
//! cache (ADR-0005/0006).

pub mod memory;
