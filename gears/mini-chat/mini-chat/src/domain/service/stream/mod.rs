//! Streaming turns (`cpt-cf-mini-chat-feature-streaming-turns`): the send
//! pipeline, replay, context assembly, tool decision, the provider task, the
//! relay, finalization and the turn status.
//!
//! The domain depends on the ports of [`ports`] only: the turn store, the
//! provider client and its event stream, the provider directory and the
//! metrics sink.

// @cpt-dod:cpt-cf-mini-chat-dod-streaming-turns-quality-gates:p1
pub mod citations;
pub mod context;
pub mod finalize;
pub mod idempotency;
pub mod inputs;
pub mod knowledge;
pub mod pipeline;
pub mod ports;
pub mod relay;
pub mod replay;
pub mod request;
pub mod sanitize;
pub mod status;
pub mod task;
pub mod tools;
pub mod types;
pub mod validate;

#[cfg(test)]
pub(crate) mod test_support;
