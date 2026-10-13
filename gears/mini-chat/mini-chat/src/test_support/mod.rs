//! Test scaffold of the gear (`#[doc(hidden)]`, used by the integration tests under `tests/`).
//!
//! The harness builds a real [`MiniChatGear`](crate::MiniChatGear) through its production `init`,
//! `start` and REST registration, on top of an in-memory `SQLite` database with every gear migration
//! applied, a fake AuthZ resolver (PDP), a fake types registry, the bundled static policy and audit
//! plugins and a fake OAGW. Requests go through the production router (same handlers, extractors
//! and canonical error layer) with the `SecurityContext` of a [`TestUser`] injected.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::missing_errors_doc
)]

mod authn;
mod authz;
mod env;
mod oagw;
mod recording;
mod registry;
mod response;

pub use crate::domain::attachment::IndexingTimings;
pub use authz::AuthzMode;
pub use env::{TestEnv, TestEnvBuilder, TestUser};
pub use oagw::{
    FakeOagw, FakeProvider, RecordedRequest, ScriptedResponse, responses_failed,
    responses_text_stream,
};
pub use registry::FakeTypesRegistry;
pub use response::{SseEvent, StreamingResponse, TestResponse};
