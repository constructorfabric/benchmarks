//! Domain services. Every service is constructed from the shared [`Deps`].
//!
//! File ownership (work packages):
//! - REST CRUD: `chats`, `messages`, `models`, `reactions`, `turn_status`
//! - streaming core: `quota`, `context`, `stream`, `finalization`, `mutations`, `summary`,
//!   `orphan_watchdog`, `usage_audit`
//! - attachments: `attachments`, `thumbnail`, `cleanup`, `upload_reaper`

pub mod attachments;
pub mod billing;
pub mod chat_access;
pub mod chats;
pub mod cleanup;
pub mod context;
pub mod finalization;
pub mod messages;
pub mod models;
pub mod mutations;
pub mod orphan_watchdog;
pub mod quota;
pub mod reactions;
pub mod stream;
pub mod summary;
#[cfg(test)]
pub mod test_support;
pub mod thumbnail;
pub mod turn_status;
pub mod upload_reaper;
pub mod usage_audit;

use std::sync::Arc;

use authz_resolver_sdk::PolicyEnforcer;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use toolkit_db::DBProvider;

use crate::config::MiniChatConfig;
use crate::domain::error::DomainError;
use crate::infra::llm::{FileStorage, KnowledgeRetriever, LlmClient, ProviderResolver};
use crate::infra::outbox::MiniChatOutbox;
use crate::infra::plugin_gateways::{AuditGateway, PolicyGateway};

/// Shared dependencies of all services.
pub struct Deps {
    pub cfg: Arc<MiniChatConfig>,
    pub db: Arc<DBProvider<DomainError>>,
    pub enforcer: PolicyEnforcer,
    pub policy: Arc<PolicyGateway>,
    pub audit: Arc<AuditGateway>,
    pub outbox: Arc<MiniChatOutbox>,
    pub llm: Arc<dyn LlmClient>,
    pub storage: Arc<dyn FileStorage>,
    /// Knowledge retriever (only when `knowledge_search.enabled`).
    pub knowledge: Option<Arc<dyn KnowledgeRetriever>>,
    pub providers: Arc<ProviderResolver>,
    /// Upload concurrency limit (`rag.max_concurrent_uploads`).
    pub upload_slots: Arc<Semaphore>,
    /// Cancelled on gear stop (background tasks).
    pub shutdown: CancellationToken,
    /// Tracks spawned background tasks.
    pub tasks: TaskTracker,
}

/// All services, shared with the REST layer.
pub struct Services {
    pub deps: Arc<Deps>,
    pub chats: Arc<chats::ChatService>,
    pub messages: Arc<messages::MessageService>,
    pub models: Arc<models::ModelService>,
    pub reactions: Arc<reactions::ReactionService>,
    pub turn_status: Arc<turn_status::TurnStatusService>,
    pub quota: Arc<quota::QuotaService>,
    pub stream: Arc<stream::StreamService>,
    pub mutations: Arc<mutations::MutationService>,
    pub attachments: Arc<attachments::AttachmentService>,
}

impl Services {
    #[must_use]
    pub fn new(deps: Arc<Deps>) -> Self {
        let quota = Arc::new(quota::QuotaService::new(Arc::clone(&deps)));
        let stream = Arc::new(stream::StreamService::new(Arc::clone(&deps), Arc::clone(&quota)));
        Self {
            chats: Arc::new(chats::ChatService::new(Arc::clone(&deps))),
            messages: Arc::new(messages::MessageService::new(Arc::clone(&deps))),
            models: Arc::new(models::ModelService::new(Arc::clone(&deps))),
            reactions: Arc::new(reactions::ReactionService::new(Arc::clone(&deps))),
            turn_status: Arc::new(turn_status::TurnStatusService::new(Arc::clone(&deps))),
            mutations: Arc::new(mutations::MutationService::new(
                Arc::clone(&deps),
                Arc::clone(&quota),
                Arc::clone(&stream),
            )),
            attachments: Arc::new(attachments::AttachmentService::new(Arc::clone(&deps))),
            quota,
            stream,
            deps,
        }
    }
}
