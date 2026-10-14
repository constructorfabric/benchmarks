//! Domain services built at gear init and shared with the REST layer.

use std::sync::Arc;

use toolkit_macros::domain_model;
use toolkit_security::SecurityContext;

use crate::domain::attachments::AttachmentsService;
use crate::domain::authz::{Operation, PepService};
use crate::domain::chats::ChatsService;
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::messages::MessagesService;
use crate::domain::model_policy::ModelPolicyGateway;
use crate::domain::model_visibility::{VisibleModel, find_visible_model, visible_models};
use crate::domain::mutations::{ReactionService, TurnMutationService};
use crate::domain::quota::QuotaService;
use crate::domain::streaming::MessageStreamService;

/// The Models API service: admission (decision-only authorization), then the
/// policy snapshot through the gateway, then the visibility rules.
#[domain_model]
pub struct ModelsService {
    pep: Arc<PepService>,
    gateway: Arc<dyn ModelPolicyGateway>,
}

impl ModelsService {
    /// Create the service.
    #[must_use]
    pub fn new(pep: Arc<PepService>, gateway: Arc<dyn ModelPolicyGateway>) -> Self {
        Self { pep, gateway }
    }

    /// `GET /v1/models`: the visible models in catalog order.
    ///
    /// # Errors
    ///
    /// An admission refusal (`AuthzDenied`, `AuthzUnavailable`; the catalog is then not
    /// read) or `PolicyUnavailable`.
    pub async fn list(&self, ctx: &SecurityContext) -> Result<Vec<VisibleModel>, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-list-models-admission
        let admission = self.pep.authorize(ctx, Operation::ListModels, None).await;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-list-models-admission
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-list-models-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-list-models-return-refusal
        // a refusal is returned unchanged and the catalog is not read
        admission?;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-list-models-return-refusal
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-list-models-refused

        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-list-models-read-snapshot
        let snapshot = self.gateway.current_snapshot(ctx.subject_id()).await;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-list-models-read-snapshot
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-list-models-snapshot-failed
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-list-models-return-500
        // the cause is logged by the error mapping and the client gets a generic 500
        let snapshot = snapshot?;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-list-models-return-500
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-list-models-snapshot-failed

        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-list-models-project
        let models = visible_models(&snapshot);
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-list-models-project
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-list-models-return
        Ok(models)
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-list-models:p1:inst-list-models-return
    }

    /// `GET /v1/models/{id}`: one visible model.
    ///
    /// # Errors
    ///
    /// As [`list`](Self::list), plus `NotFound { resource: Model }` for a disabled or
    /// unknown model.
    pub async fn get(
        &self,
        ctx: &SecurityContext,
        model_id: &str,
    ) -> Result<VisibleModel, DomainError> {
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-admission
        let admission = self.pep.authorize(ctx, Operation::GetModel, None).await;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-admission
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-refused
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-return-refusal
        admission?;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-return-refusal
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-refused

        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-read-snapshot
        let snapshot = self.gateway.current_snapshot(ctx.subject_id()).await;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-read-snapshot
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-snapshot-failed
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-return-500
        let snapshot = snapshot?;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-return-500
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-snapshot-failed

        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-resolve
        let found = find_visible_model(&snapshot, model_id);
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-resolve
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-not-visible
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-return-404
        // a disabled and an unknown model are the same not-found, so no catalog detail leaks
        let model = found.ok_or_else(|| DomainError::NotFound {
            resource: ResourceKind::Model,
            id: Some(model_id.to_owned()),
        })?;
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-return-404
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-not-visible
        // @cpt-begin:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-return
        Ok(model)
        // @cpt-end:cpt-cf-mini-chat-flow-gear-foundation-get-model:p1:inst-get-model-return
    }
}

/// All services of the gear; every feature adds its own service here.
#[domain_model]
pub struct Services {
    pub pep: Arc<PepService>,
    pub models: ModelsService,
    /// The chat-management service; attached by [`Services::with_chats`] (`None` until then).
    pub chats: Option<Arc<ChatsService>>,
    /// The quota and billing service; attached by [`Services::with_quota`] (`None` until then).
    pub quota: Option<Arc<QuotaService>>,
    /// The messages list and turn status service; attached by [`Services::with_messages`].
    pub messages: Option<Arc<MessagesService>>,
    /// The send setup service; attached by [`Services::with_streaming`].
    pub streaming: Option<Arc<MessageStreamService>>,
    /// The attachments service (upload and get); attached by [`Services::with_attachments`].
    pub attachments: Option<Arc<AttachmentsService>>,
    /// The retry, edit and delete service of FEATURE 0006; attached by
    /// [`Services::with_mutations`].
    pub mutations: Option<Arc<TurnMutationService>>,
    /// The reactions service of FEATURE 0006; attached by [`Services::with_reactions`].
    pub reactions: Option<Arc<ReactionService>>,
}

impl Services {
    /// Build the services over a PEP and the model-policy gateway.
    #[must_use]
    pub fn new(pep: Arc<PepService>, gateway: Arc<dyn ModelPolicyGateway>) -> Self {
        let models = ModelsService::new(Arc::clone(&pep), gateway);
        Self {
            pep,
            models,
            chats: None,
            quota: None,
            messages: None,
            streaming: None,
            attachments: None,
            mutations: None,
            reactions: None,
        }
    }

    /// Attach the turn mutation service (retry, edit and delete).
    #[must_use]
    pub fn with_mutations(mut self, mutations: TurnMutationService) -> Self {
        self.mutations = Some(Arc::new(mutations));
        self
    }

    /// Attach the reactions service.
    #[must_use]
    pub fn with_reactions(mut self, reactions: ReactionService) -> Self {
        self.reactions = Some(Arc::new(reactions));
        self
    }

    /// Attach the quota and billing service (the quota status endpoint and the preflight,
    /// reserve and settlement contracts of the streaming pipeline).
    #[must_use]
    pub fn with_quota(mut self, quota: QuotaService) -> Self {
        self.quota = Some(Arc::new(quota));
        self
    }

    /// Attach the messages list and turn status service.
    #[must_use]
    pub fn with_messages(mut self, messages: MessagesService) -> Self {
        self.messages = Some(Arc::new(messages));
        self
    }

    /// Attach the send setup service.
    #[must_use]
    pub fn with_streaming(mut self, streaming: MessageStreamService) -> Self {
        self.streaming = Some(Arc::new(streaming));
        self
    }

    /// Attach a send setup service that the caller also keeps a handle to (the turn mutation
    /// service reuses it for the post-commit setup).
    #[must_use]
    pub fn with_streaming_shared(mut self, streaming: Arc<MessageStreamService>) -> Self {
        self.streaming = Some(streaming);
        self
    }

    /// Attach the attachments service (upload and get).
    #[must_use]
    pub fn with_attachments(mut self, attachments: AttachmentsService) -> Self {
        self.attachments = Some(Arc::new(attachments));
        self
    }

    /// Attach an attachments service that the caller also keeps a handle to (the gear stop
    /// drains its background tasks).
    #[must_use]
    pub fn with_attachments_shared(mut self, attachments: Arc<AttachmentsService>) -> Self {
        self.attachments = Some(attachments);
        self
    }

    /// Attach the chat-management service.
    #[must_use]
    pub fn with_chats(mut self, chats: ChatsService) -> Self {
        self.chats = Some(Arc::new(chats));
        self
    }
}

#[cfg(test)]
#[path = "services_tests.rs"]
mod services_tests;
