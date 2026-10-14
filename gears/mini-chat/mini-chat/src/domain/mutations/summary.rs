//! The summary frontier comparison of the summary invalidation
//! (`cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation`).

use time::OffsetDateTime;
use uuid::Uuid;

/// Whether the summary frontier `(summarized_up_to_created_at, summarized_up_to_message_id)` is
/// at or after the target user message `(created_at, id)` in the order `(created_at, id)`.
#[must_use]
pub fn frontier_covers(frontier: (OffsetDateTime, Uuid), target: (OffsetDateTime, Uuid)) -> bool {
    // @cpt-begin:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-covers
    frontier >= target
    // @cpt-end:cpt-cf-mini-chat-algo-turn-mutations-and-reactions-summary-invalidation:p1:inst-si-covers
}
