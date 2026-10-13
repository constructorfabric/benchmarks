//! The input of a provider request build.

use mini_chat_sdk::ModelCatalogEntry;
use uuid::Uuid;

use super::context::ContextPlan;
use super::ports::ProviderTarget;
use super::tools::ToolsDecision;

/// Everything an adapter needs to build the request of a turn
/// (`cpt-cf-mini-chat-algo-streaming-turns-build-provider-request`).
#[derive(Debug, Clone, Copy)]
pub struct ProviderRequestInput<'a> {
    /// Provider kind, OAGW alias and `api_path`.
    pub target: &'a ProviderTarget,
    /// Catalog entry of the effective model.
    pub entry: &'a ModelCatalogEntry,
    /// The assembled context.
    pub plan: &'a ContextPlan,
    /// The tools and guards of the turn.
    pub tools: &'a ToolsDecision,
    /// `max_output_tokens_applied` of the turn.
    pub max_output_tokens_applied: i64,
    /// Tenant of the chat.
    pub tenant_id: Uuid,
    /// Requesting user.
    pub user_id: Uuid,
    /// Chat.
    pub chat_id: Uuid,
}
