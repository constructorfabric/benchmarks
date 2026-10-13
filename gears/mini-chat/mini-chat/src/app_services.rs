//! Shared application state of the gear (composition root): the services and
//! the infrastructure handles they are built from.
//!
//! Later features add their services (chats, messages, streaming, ...) as
//! fields here; `init` builds the struct once and shares it behind an `Arc`.

use std::sync::Arc;

use oagw_sdk::ServiceGatewayClientV1;
use toolkit_db::DBProvider;

use crate::config::MiniChatConfig;
use crate::domain::authz::AuthzService;
use crate::domain::error::DomainError;
use crate::domain::service::attachments::ports::{AttachmentRepository, StorageClient};
use crate::domain::service::attachments::service::AttachmentsService;
use crate::domain::service::chats::ChatsService;
use crate::domain::service::messages::MessagesService;
use crate::domain::service::models::ModelsService;
use crate::domain::service::mutations::service::MutationService;
use crate::domain::service::quota::service::QuotaService;
use crate::domain::service::stream::pipeline::StreamService;
use crate::infra::audit_gateway::AuditGateway;
use crate::infra::llm::provider_registry::ProviderResolver;
use crate::infra::metrics::MiniChatMetrics;
use crate::infra::outbox::queues::OutboxEnqueuer;
use crate::infra::policy_gateway::ModelPolicyGateway;
use crate::infra::runtime::GearRuntime;

/// Services and shared dependencies of the gear.
#[derive(Clone)]
pub struct AppServices {
    /// Database provider (Secure ORM entry point).
    pub db: Arc<DBProvider<DomainError>>,
    /// Policy enforcement point over the `authz_resolver` client.
    pub authz: Arc<AuthzService>,
    /// Model-policy plugin gateway (lazy plugin resolution).
    pub policy: Arc<ModelPolicyGateway>,
    /// Audit plugin gateway (lazy plugin resolution).
    pub audit: Arc<AuditGateway>,
    /// Maps a catalog `provider_id` and tenant to adapter kind and OAGW alias.
    pub providers: Arc<ProviderResolver>,
    /// Producer side of the shared outbox.
    pub outbox: Arc<OutboxEnqueuer>,
    /// OpenTelemetry instruments.
    pub metrics: Arc<MiniChatMetrics>,
    /// Validated configuration.
    pub config: Arc<MiniChatConfig>,
    /// Outbound gateway (OAGW) client.
    pub oagw: Arc<dyn ServiceGatewayClientV1>,
    /// Models API service.
    pub models: Arc<ModelsService>,
    /// Chat create, get, list, rename and delete service.
    pub chats: Arc<ChatsService>,
    /// Message history and reactions service.
    pub messages: Arc<MessagesService>,
    /// Quota service: preflight, reserve, settlement, status and warnings.
    pub quota: Arc<QuotaService>,
    /// Stream service: the send pipeline, replay, the provider task and the
    /// turn status.
    pub streams: Arc<StreamService>,
    /// Mutation service: retry, edit and delete of the latest terminal turn.
    pub mutations: Arc<MutationService>,
    /// Attachments service: upload, get and delete.
    pub attachments: Arc<AttachmentsService>,
    /// Late-bound background registry and S2S context (bound at `start`).
    pub runtime: Arc<GearRuntime>,
    /// Attachment store (the cleanup handler and the reaper share it).
    pub attachment_repo: Arc<dyn AttachmentRepository>,
    /// Provider Files and Vector Stores client.
    pub storage: Arc<dyn StorageClient>,
}
