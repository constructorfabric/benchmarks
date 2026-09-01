// Created: 2026-08-29 by Constructor Tech
//! Infrastructure layer: in-memory persistence, the plugin registry, the
//! audit log, the metrics adapter and the data-plane proxy engine.
//!
//! Runtime-owned privileged access lives here only: credential resolution
//! (`credstore_sdk`), outbound TLS (`toolkit_http`) and the WebSocket
//! upstream leg (`tokio-tungstenite`).

pub mod audit;
pub mod metrics;
pub mod plugin;
pub mod proxy;
pub mod storage;
