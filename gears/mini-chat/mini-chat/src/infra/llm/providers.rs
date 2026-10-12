//! Provider resolver (DESIGN 3.2 `llm_provider`): maps a catalog `provider_id`
//! (and the tenant, through `tenant_overrides`) to a `providers.<id>` entry and
//! its OAGW alias, and selects the adapter by the entry's kind.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::AuthNResolverClient;
use oagw_sdk::ServiceGatewayClientV1;
use tokio::sync::Mutex;
use tokio::time::Instant;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::openai_responses;
use super::provisioning::s2s_context;
use crate::config::{ClientCredentialsConfig, MiniChatConfig, ProviderKind};
use crate::domain::error::DomainError;
use crate::domain::ports::LlmGateway;
use crate::domain::provider::{ChatRequest, ProviderEventStream, ProviderFailure};

/// One resolved `providers.<id>` entry.
struct Entry {
    kind: ProviderKind,
    api_path: String,
    /// Alias of the entry's own upstream.
    alias: String,
    /// Alias per tenant override.
    tenant_aliases: HashMap<Uuid, String>,
}

impl Entry {
    fn alias_for(&self, tenant_id: Uuid) -> &str {
        self.tenant_aliases
            .get(&tenant_id)
            .map_or(self.alias.as_str(), String::as_str)
    }
}

/// How long an exchanged S2S security context is reused.
const S2S_CONTEXT_TTL: Duration = Duration::from_secs(60);

/// The gear's own identity for provider calls (DESIGN 3.2 "OAGW provisioning"):
/// the upstreams and routes are registered by the S2S context, so the calls
/// that use them run as the same identity. The end user's tenant and user are
/// attached to the provider request (`user`, `metadata`), not to the OAGW call.
pub(super) struct ServiceIdentity {
    authn: Arc<dyn AuthNResolverClient>,
    credentials: ClientCredentialsConfig,
    cached: Mutex<Option<(Instant, SecurityContext)>>,
}

impl ServiceIdentity {
    /// The identity that exchanges `credentials` through `authn`.
    pub(super) fn new(
        authn: Arc<dyn AuthNResolverClient>,
        credentials: &ClientCredentialsConfig,
    ) -> Self {
        Self {
            authn,
            credentials: credentials.clone(),
            cached: Mutex::new(None),
        }
    }

    /// The (cached) S2S security context.
    pub(super) async fn context(&self) -> Result<SecurityContext, String> {
        let mut cached = self.cached.lock().await;
        if let Some((fetched, ctx)) = cached.as_ref()
            && fetched.elapsed() < S2S_CONTEXT_TTL
        {
            return Ok(ctx.clone());
        }
        let ctx = s2s_context(self.authn.as_ref(), &self.credentials).await?;
        *cached = Some((Instant::now(), ctx.clone()));
        Ok(ctx)
    }
}

/// The provider layer over the in-process OAGW client.
pub struct LlmProviders {
    oagw: Arc<dyn ServiceGatewayClientV1>,
    entries: HashMap<String, Entry>,
    identity: Option<ServiceIdentity>,
}

impl LlmProviders {
    /// Builds the resolver from the provider entries. `upstream_alias` is
    /// filled with the host when it is not configured (for a tenant override,
    /// with the override's host).
    #[must_use]
    pub fn new(oagw: Arc<dyn ServiceGatewayClientV1>, config: &MiniChatConfig) -> Self {
        let entries = config
            .providers
            .iter()
            .map(|(id, provider)| {
                let tenant_aliases = provider
                    .tenant_overrides
                    .iter()
                    .filter_map(|(tenant, tenant_override)| {
                        let Ok(tenant_id) = Uuid::parse_str(tenant) else {
                            tracing::warn!(provider = %id, tenant,
                                "tenant override key is not a tenant id; ignored");
                            return None;
                        };
                        Some((tenant_id, provider.tenant_target(tenant_override).alias))
                    })
                    .collect();
                (
                    id.clone(),
                    Entry {
                        kind: provider.kind,
                        api_path: provider.api_path.clone(),
                        alias: provider.target().alias,
                        tenant_aliases,
                    },
                )
            })
            .collect();
        Self {
            oagw,
            entries,
            identity: None,
        }
    }

    /// Makes provider calls run as the gear's own S2S identity (the one that
    /// registered the upstreams) instead of the caller's security context.
    #[must_use]
    pub fn with_service_identity(
        mut self,
        authn: Arc<dyn AuthNResolverClient>,
        credentials: &ClientCredentialsConfig,
    ) -> Self {
        self.identity = Some(ServiceIdentity::new(authn, credentials));
        self
    }

    async fn call_context(&self, caller: &SecurityContext) -> Result<SecurityContext, String> {
        match &self.identity {
            Some(identity) => identity.context().await,
            None => Ok(caller.clone()),
        }
    }

    fn resolve(&self, provider_id: &str) -> Result<&Entry, DomainError> {
        let entry = self.entries.get(provider_id).ok_or_else(|| {
            DomainError::internal(format!("provider `{provider_id}` is not configured"))
        })?;
        match entry.kind {
            ProviderKind::OpenAiResponses => Ok(entry),
            other => Err(DomainError::internal(format!(
                "provider `{provider_id}` uses the unsupported adapter kind {other:?}"
            ))),
        }
    }
}

#[async_trait]
impl LlmGateway for LlmProviders {
    fn check_provider(&self, _tenant_id: Uuid, provider_id: &str) -> Result<(), DomainError> {
        self.resolve(provider_id).map(|_| ())
    }

    async fn stream_chat(
        &self,
        ctx: &SecurityContext,
        request: &ChatRequest,
    ) -> Result<ProviderEventStream, ProviderFailure> {
        let entry = self.resolve(&request.provider_id).map_err(|error| {
            tracing::error!(%error, "provider resolution failed after preflight");
            ProviderFailure::error("The provider is currently unavailable")
        })?;
        let call_ctx = self.call_context(ctx).await.map_err(|detail| {
            tracing::error!(%detail, "service identity is not available for the provider call");
            ProviderFailure::error("The provider is currently unavailable")
        })?;
        openai_responses::stream_chat(
            self.oagw.as_ref(),
            &call_ctx,
            entry.alias_for(request.tenant_id),
            &entry.api_path,
            request,
        )
        .await
    }
}
