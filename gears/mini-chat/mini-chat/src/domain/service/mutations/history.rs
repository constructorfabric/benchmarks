//! Replacement history loading
//! (`cpt-cf-mini-chat-algo-turn-mutations-load-replacement-history`).

use toolkit_macros::domain_model;
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::ports::MutationRepository;
use crate::domain::error::DomainError;
use crate::domain::service::stream::ports::{
    HistoryMessage, MessageBoundary, RecentQuery, ThreadSummaryInfo, TurnRepository,
};

/// The history of a replacement.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplacementHistory {
    /// The snapshot boundary; none when only the new user message is live.
    pub boundary: Option<MessageBoundary>,
    /// The thread summary.
    pub summary: Option<ThreadSummaryInfo>,
    /// The recent messages, chronological.
    pub recent: Vec<HistoryMessage>,
}

/// Loads the history after the replacement commit: the boundary is the latest
/// live message with a request id other than `new_request_id`; the summary and
/// the recent messages are read with that boundary as the send pipeline does.
///
/// # Errors
///
/// The errors of the repositories.
pub async fn load_replacement_history(
    turns: &dyn TurnRepository,
    mutations: &dyn MutationRepository,
    scope: &AccessScope,
    chat_id: Uuid,
    new_request_id: Uuid,
    recent_messages_limit: u32,
) -> Result<ReplacementHistory, DomainError> {
    let boundary = mutations
        .history_boundary(scope, chat_id, new_request_id)
        .await?;
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-load-replacement-history:p1:inst-replacement-recent
    let summary = turns.thread_summary(scope, chat_id).await?;
    let recent = match boundary {
        Some(boundary) if recent_messages_limit > 0 => {
            let query = RecentQuery {
                boundary,
                frontier: summary.as_ref().map(|summary| summary.frontier),
                limit: recent_messages_limit,
            };
            turns.recent_messages(scope, chat_id, &query).await?
        }
        _ => Vec::new(),
    };
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-load-replacement-history:p1:inst-replacement-recent
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-load-replacement-history:p1:inst-return-history
    Ok(ReplacementHistory {
        boundary,
        summary,
        recent,
    })
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-load-replacement-history:p1:inst-return-history
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;
