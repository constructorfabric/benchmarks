//! Streaming turns (DESIGN section 3.3 "Streaming Contract", section 3.6
//! "Send Message with Streaming Response", "Streaming Cancellation", section
//! 4 "Turn Lifecycle", section 5.7 "Turn Finalization Contract").
//!
//! - [`events`] — the domain stream events the SSE layer serializes and the
//!   streaming error codes;
//! - [`service`] — [`StreamService`]: the setup pipeline of a new turn
//!   (validation, PEP, idempotency and parallel guard, preflight, context
//!   assembly, provider resolution, the reserve transaction) and the
//!   read-only replay of a completed turn;
//! - [`task`] — the provider task of a live turn: incremental relay, tool
//!   limits and counters, progress refresh, citations, cancellation;
//! - [`finalize`] — the CAS finalization transaction (assistant message,
//!   settlement, usage / audit / thread-summary outbox messages).
//!
//! The SSE relay (HTTP layer) sends [`events::StreamStarted`] first, then
//! relays the [`LiveStream`] channel until a terminal event, emits `ping`
//! between `stream_started` and the first content event, cancels the turn
//! token when it is dropped and synthesizes `stream_interrupted` when the
//! channel closes without a terminal event.

pub mod events;
pub mod finalize;
pub mod service;
pub mod task;

pub use events::{StreamEvent, StreamStarted};
pub use service::{
    LiveStream, MutationPreflight, MutationPreflightInput, MutationTurn, SendOptions, SendRequest,
    StreamService, StreamSettings, StreamStart,
};
