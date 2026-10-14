//! Background maintenance (feature `cpt-cf-mini-chat-feature-background-maintenance`): the
//! thread-summary handler of the queue `mini-chat.thread_summary` (frozen range, prompt, the
//! non-streaming summary call per adapter kind, the compare-and-set commit, the system-task usage
//! event) and the chat-cleanup handler of the queue `mini-chat.chat_cleanup` (provider files, the
//! Anthropic secondary copy and the vector store of a soft-deleted chat, within the 5 s call
//! deadline and the 20 s delivery budget).
//!
//! The pure rules live in the small modules (`summary_payload`, `summary_prompt`,
//! `summary_request`, `summary_response`, `summary_parse`, `chat_payload`, `secondary`,
//! `settings`); `summary_worker` and `chat_cleanup` hold the handler algorithms over the ports of
//! `summary_store`, `summary_call` and `chat_store`, which `infra` implements.

pub mod chat_cleanup;
pub mod chat_payload;
pub mod chat_store;
pub mod metrics;
pub mod secondary;
pub mod settings;
pub mod summary_call;
pub mod summary_parse;
pub mod summary_payload;
pub mod summary_prompt;
pub mod summary_request;
pub mod summary_response;
pub mod summary_store;
pub mod summary_worker;

use toolkit_macros::domain_model;

/// The result of one delivery of either handler, mapped by the infrastructure adapters to the
/// shared outbox result (`Ok`, `Retry`, `Reject(reason)`). A reason never carries message
/// content, summary text or a provider identifier.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandlerOutcome {
    Ok,
    Retry,
    Reject(String),
}
