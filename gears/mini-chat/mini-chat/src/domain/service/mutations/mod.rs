//! Turn mutations (`cpt-cf-mini-chat-feature-turn-mutations`): retry, edit and
//! delete of the latest terminal turn of the requesting user.
//!
//! The domain depends on the ports of [`ports`] and on the ports of the stream
//! pipeline only: the mutation store, the turn store, the quota service and the
//! stream service that opens the replacement stream.

// @cpt-dod:cpt-cf-mini-chat-dod-turn-mutations-quality-gates:p1
pub mod audit;
pub mod history;
pub mod inputs;
pub mod ports;
pub mod preview;
pub mod service;
pub mod setup;
pub mod summary;
pub mod timing;
pub mod types;

#[cfg(test)]
pub(crate) mod test_support;
