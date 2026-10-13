//! The summary coverage comparison of the summary invalidation.

use crate::domain::service::stream::ports::MessageBoundary;

/// Whether the summary `frontier` is at or after the mutated turn's user
/// message in the order `(created_at ASC, id ASC)`.
#[must_use]
pub fn summary_covers(frontier: MessageBoundary, user_message: MessageBoundary) -> bool {
    (frontier.created_at, frontier.id) >= (user_message.created_at, user_message.id)
}

#[cfg(test)]
#[path = "summary_tests.rs"]
mod tests;
