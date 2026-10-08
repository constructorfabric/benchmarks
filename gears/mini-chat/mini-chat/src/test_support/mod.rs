//! Shared test helpers (`#[cfg(test)]` only). Extended by later tasks.
//!
//! - [`app::TestApp`]: the gear's real service graph and router over in-memory fakes.
//! - [`authn::FakeAuthn`]: scripted S2S client-credentials exchange.
//! - [`gateway::FakeGateway`]: scripted in-memory OAGW (`ServiceGatewayClientV1`).
//! - [`catalog`]: model-catalog fixtures (`test_catalog()` and per-kind model builders).
//! - [`plugins`]: recording model-policy / audit plugin fakes behind the `Direct*` gateways.
//! - [`outbox`]: recording wrapper for outbox handlers (`TestApp::outbox_payloads`).
//! - [`stream`]: fixtures of the streaming tests (chats, scripted Responses streams, rows).
//! - [`attachments`]: fixtures of the attachment tests (multipart bodies, scripted storage).
//! - [`workers`]: rows a crashed process leaves for the orphan watchdog and the upload reaper.
//! - [`db::test_db`], [`pdp::FakePdp`], [`registry::RecordingRegistry`], [`fixtures`].

pub mod app;
pub mod attachments;
pub mod authn;
pub mod catalog;
pub mod db;
pub mod fixtures;
pub mod gateway;
pub mod metrics;
pub mod outbox;
pub mod pdp;
pub mod plugins;
pub mod registry;
pub mod stream;
pub mod workers;
