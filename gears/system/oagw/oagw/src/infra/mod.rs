//! Infrastructure implementations for the `oagw` gear.
//!
//! * [`storage`] — the in-memory control-plane store.
//! * [`plugin`] — plugin registries and the built-in plugins.
//! * [`type_provisioning`] — publishes the OAGW type schemas to the
//!   types-registry.
//! * [`proxy`] — the data plane: alias/route resolution, the request pipeline,
//!   the upstream transport and the plugin runtime.

pub mod plugin;
pub mod proxy;
pub mod storage;
pub mod type_provisioning;
