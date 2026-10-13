//! Acceptance tests: one module per area of `gears/mini-chat/docs/acceptance-criteria.md`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

#[path = "../common/mod.rs"]
mod common;

mod attachments;
mod authorization;
mod chats;
mod cleanup_recovery;
mod context;
mod errors;
mod idempotency;
mod messages;
mod models_reactions;
mod principles;
mod quota;
mod streaming;
mod turns;
