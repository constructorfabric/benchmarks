//! REST handlers, DTOs, SSE writer and routes.

pub mod attachments;
pub mod dto;
pub mod handlers;
pub mod routes;
pub mod sse;

#[cfg(test)]
#[path = "chats_tests.rs"]
mod chats_tests;

#[cfg(test)]
#[path = "stream_tests.rs"]
mod stream_tests;

#[cfg(test)]
#[path = "turns_tests.rs"]
mod turns_tests;

#[cfg(test)]
#[path = "mutations_tests.rs"]
mod mutations_tests;

#[cfg(test)]
#[path = "attachments_tests.rs"]
mod attachments_tests;

#[cfg(test)]
#[path = "quota_tests.rs"]
mod quota_tests;

#[cfg(test)]
#[path = "models_reactions_tests.rs"]
mod models_reactions_tests;

#[cfg(test)]
#[path = "authz_tests.rs"]
mod authz_tests;

#[cfg(test)]
#[path = "summary_tests.rs"]
mod summary_tests;

#[cfg(test)]
#[path = "knowledge_search_tests.rs"]
mod knowledge_search_tests;
