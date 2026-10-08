//! The service graph shared by every handler (`Extension<Arc<AppServices>>`).

use std::sync::Arc;

use oagw_sdk::ServiceGatewayClientV1;
use toolkit_db::DBProvider;

use crate::config::MiniChatConfig;
use crate::domain::attachment::AttachmentService;
use crate::domain::authz::Authz;
use crate::domain::chat_service::ChatService;
use crate::domain::error::DomainError;
use crate::domain::message_service::MessageService;
use crate::domain::model_service::ModelService;
use crate::domain::quota::QuotaService;
use crate::domain::reaction_service::ReactionService;
use crate::domain::stream::StreamService;
use crate::domain::turn_service::TurnService;
use crate::infra::gateways::audit::AuditGateway;
use crate::infra::gateways::policy::PolicyGateway;
use crate::infra::llm::{LlmClient, ProviderResolver, S2sContext};
use crate::infra::outbox::OutboxEnqueuer;
use crate::infra::storage::{AnthropicFiles, FileStorage, VectorStores};
use crate::metrics::Metrics;

/// `Arc` holder of every service. Built only by [`crate::wiring::build_services`]; later tasks add
/// their services here.
pub struct AppServices {
    pub cfg: Arc<MiniChatConfig>,
    pub db: Arc<DBProvider<DomainError>>,
    pub authz: Arc<Authz>,
    pub models: Arc<ModelService>,
    pub chats: Arc<ChatService>,
    pub metrics: Arc<Metrics>,
    pub policy: Arc<dyn PolicyGateway>,
    pub audit: Arc<dyn AuditGateway>,
    pub gateway: Arc<dyn ServiceGatewayClientV1>,
    /// S2S security context for every OAGW call; set by `serve` once the exchange succeeded.
    pub s2s: S2sContext,
    /// Maps provider ids (and tenants) to OAGW aliases and request paths.
    pub providers: Arc<ProviderResolver>,
    /// Chat adapters over the OAGW client.
    pub llm: Arc<LlmClient>,
    /// Provider file storage (`OpenAI` / Azure) over OAGW.
    pub files: Arc<dyn FileStorage>,
    /// Provider vector stores (`OpenAI` / Azure) over OAGW.
    pub vector_stores: Arc<dyn VectorStores>,
    /// Anthropic Files client (secondary image copies); only when an `anthropic_messages`
    /// provider is configured.
    pub anthropic_files: Option<Arc<AnthropicFiles>>,
    /// Enqueues outbox messages; usable once the pipeline has been started.
    pub outbox: Arc<OutboxEnqueuer>,
    /// Credit quotas: preflight, reserve, settlement and status.
    pub quota: Arc<QuotaService>,
    /// The send pipeline (`messages:stream`).
    pub stream: Arc<StreamService>,
    /// Message list.
    pub messages: Arc<MessageService>,
    /// Message reactions.
    pub reactions: Arc<ReactionService>,
    /// Turn status and the mutations of the latest turn (retry, edit, delete).
    pub turns: Arc<TurnService>,
    /// Attachment upload (indexing, thumbnails), metadata and deletion.
    pub attachments: Arc<AttachmentService>,
}
