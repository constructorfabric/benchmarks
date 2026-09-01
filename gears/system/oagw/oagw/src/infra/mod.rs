//! Adapters that implement the domain ports.
//!
//! The control plane of this slice is backed by an in-process store; the proxy
//! data plane lands in a later slice and keeps its placeholder module here so
//! the layout of `DESIGN` §3.2 does not move.

pub mod plugin;
pub mod proxy;
pub mod storage;
