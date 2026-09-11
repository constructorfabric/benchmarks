//! Infrastructure layer for the OAGW gear.
//!
//! Implements the domain contracts (`crate::domain::repo`) and hosts the
//! proxy engine, the plugin registries and the type provisioning. Feature 1
//! (Gear Foundation) declares the modules only: the store, the plugin
//! registries and the type provisioning arrive with entries 2.2, 2.3 and 2.5.

pub mod hierarchy;
pub mod obs;
pub mod plugin;
pub mod proxy;
pub mod storage;
pub mod type_provisioning;
