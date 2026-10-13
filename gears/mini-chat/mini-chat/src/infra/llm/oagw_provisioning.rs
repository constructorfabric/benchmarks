//! OAGW upstream and route provisioning
//! (`cpt-cf-mini-chat-algo-gear-foundation-provision-oagw`,
//! `cpt-cf-mini-chat-state-gear-foundation-provider-provisioning`).
//!
//! Provisioning is idempotent: targets sharing an alias form one group, a reused
//! upstream has its routes listed first, an already-exists Conflict on route
//! creation counts as success.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use oagw_sdk::{
    CreateRouteRequest, CreateUpstreamRequest, Endpoint, HttpMatch, HttpMethod, ListQuery,
    MatchRules, PathSuffixMode, Scheme, Server, ServiceGatewayClientV1, ServiceGatewayError,
    SharingMode,
};
use secrecy::SecretString;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::{
    AuthConfig, ClientCredentials, ProviderRegistry, StorageKind, effective_alias, is_ip_literal,
    normalize_alias,
};
use crate::infra::llm::reconcile::{RetryFailure, TargetRetry};

/// Bound of the S2S client-credentials exchange.
pub const S2S_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound of every OAGW call made while provisioning.
pub const OAGW_CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// Precondition subject OAGW reports when a `secret_ref` is not readable.
pub const SECRET_REF_SUBJECT: &str = "auth.config.secret_ref";

/// State of one target (`cpt-cf-mini-chat-state-gear-foundation-provider-provisioning`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningState {
    /// Not provisioned yet.
    Pending,
    /// Upstream and chat route exist.
    Provisioned,
    /// Secret not readable or OAGW timed out; retried in the background.
    Deferred,
    /// Deterministic misconfiguration (start fails).
    Failed,
}

/// Bounds used while provisioning (injectable for tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProvisioningTimeouts {
    /// Bound of the S2S exchange.
    pub s2s_exchange: Duration,
    /// Bound of one OAGW call.
    pub oagw_call: Duration,
}

impl Default for ProvisioningTimeouts {
    fn default() -> Self {
        Self {
            s2s_exchange: S2S_EXCHANGE_TIMEOUT,
            oagw_call: OAGW_CALL_TIMEOUT,
        }
    }
}

/// One effective OAGW target: a provider entry or a tenant override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionTarget {
    /// Provider id.
    pub provider_id: String,
    /// Tenant id of an override target.
    pub tenant_id: Option<String>,
    /// Alias (the alias OAGW stores and resolves, or the alias OAGW returned
    /// once provisioned).
    pub alias: String,
    /// Host.
    pub host: String,
    /// Port.
    pub port: u16,
    /// Plain HTTP.
    pub use_http: bool,
    /// Auth plugin type.
    pub auth_plugin_type: Option<String>,
    /// Auth config (values are secrets, redacted in `Debug`).
    pub auth_config: AuthConfig,
    /// Chat endpoint path (`{model}` placeholder kept).
    pub api_path: String,
    /// Storage implementation of the entry (inherited by overrides).
    pub storage_kind: StorageKind,
    /// Azure `api-version` of the entry.
    pub api_version: Option<String>,
}

impl ProvisionTarget {
    /// Key path of the target: `providers.<id>` or
    /// `providers.<id>.tenant_overrides.<tenant>`.
    #[must_use]
    pub fn label(&self) -> String {
        match &self.tenant_id {
            Some(tenant) => format!("providers.{}.tenant_overrides.{tenant}", self.provider_id),
            None => format!("providers.{}", self.provider_id),
        }
    }

    /// The alias OAGW stores for this target: derived from the endpoint for a
    /// hostname, the normalized configured alias for an IP literal.
    #[must_use]
    pub fn oagw_alias(&self) -> String {
        effective_alias(&self.host, self.port, self.use_http, Some(&self.alias))
    }

    /// Builds one target per provider entry and per tenant override in
    /// configuration order (override fields fall back to the entry,
    /// `storage_kind` and `api_version` are inherited).
    #[must_use]
    pub fn from_registry(registry: &ProviderRegistry) -> Vec<Self> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-build-targets
        let mut targets = Vec::new();
        for provider in &registry.providers {
            targets.push(Self {
                provider_id: provider.id.clone(),
                tenant_id: None,
                alias: provider.upstream_alias.clone(),
                host: provider.host.clone(),
                port: provider.port,
                use_http: provider.use_http,
                auth_plugin_type: provider.auth_plugin_type.clone(),
                auth_config: provider.auth_config.clone(),
                api_path: provider.api_path.clone(),
                storage_kind: provider.storage_kind,
                api_version: provider.api_version.clone(),
            });
            for over in &provider.tenant_overrides {
                targets.push(Self {
                    provider_id: provider.id.clone(),
                    tenant_id: Some(over.tenant_id.clone()),
                    alias: over.upstream_alias.clone(),
                    host: over.host.clone(),
                    port: provider.port,
                    use_http: provider.use_http,
                    auth_plugin_type: over.auth_plugin_type.clone(),
                    auth_config: over.auth_config.clone(),
                    api_path: provider.api_path.clone(),
                    storage_kind: provider.storage_kind,
                    api_version: provider.api_version.clone(),
                });
            }
        }
        targets
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-build-targets
    }
}

/// Outcome of one target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetOutcome {
    /// The target; its alias is the one OAGW returned when provisioned.
    pub target: ProvisionTarget,
    /// Final state.
    pub state: ProvisioningState,
}

/// Result of a provisioning run.
#[derive(Debug, Clone)]
pub struct ProvisioningOutcome {
    /// One outcome per target in configuration order.
    pub targets: Vec<TargetOutcome>,
    /// S2S security context used for the OAGW calls (reused by the reconcile task).
    pub s2s_context: SecurityContext,
}

impl ProvisioningOutcome {
    /// Targets in state `Deferred`.
    #[must_use]
    pub fn deferred(&self) -> Vec<&ProvisionTarget> {
        self.targets
            .iter()
            .filter(|outcome| outcome.state == ProvisioningState::Deferred)
            .map(|outcome| &outcome.target)
            .collect()
    }

    /// Targets in state `Provisioned`.
    #[must_use]
    pub fn provisioned(&self) -> Vec<&ProvisionTarget> {
        self.targets
            .iter()
            .filter(|outcome| outcome.state == ProvisioningState::Provisioned)
            .map(|outcome| &outcome.target)
            .collect()
    }
}

/// Cause of a failed S2S exchange.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum S2sFailureCause {
    /// `S2S_EXCHANGE_TIMEOUT` elapsed.
    #[error("timeout")]
    Timeout,
    /// The exchange returned an error.
    #[error("exchange failed: {0}")]
    Failed(String),
}

/// Provisioning failure (fails `start`). Never carries a secret value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProvisioningError {
    /// The S2S exchange failed or timed out.
    #[error("S2S credential exchange failed for client_id `{client_id}`: {cause}")]
    S2sExchange {
        /// Configured client id.
        client_id: String,
        /// Cause (`timeout` when the bound elapsed).
        cause: S2sFailureCause,
    },
    /// A target is deterministically misconfigured.
    #[error("OAGW provisioning failed for `{label}` (provider `{provider_id}`): {cause}")]
    Misconfigured {
        /// Key path of the target.
        label: String,
        /// Provider id.
        provider_id: String,
        /// Tenant id of an override target.
        tenant_id: Option<String>,
        /// Failure description.
        cause: String,
    },
}

/// A group failure: deferrable (secret, timeout) or deterministic.
enum Failure {
    Deferred(String),
    Misconfigured(String),
}

/// `(method, path, query allowlist, suffix mode)` of one route to provision.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RouteSpec {
    method: HttpMethod,
    path: String,
    query_allowlist: Vec<String>,
    suffix: PathSuffixMode,
}

fn chat_route(api_path: &str) -> RouteSpec {
    let (path, query) = api_path.split_once('?').unwrap_or((api_path, ""));
    let prefix = path.find("{model}").map_or(path, |at| &path[..at]);
    let trimmed = prefix.trim_end_matches('/');
    let query_allowlist = query
        .split('&')
        .filter_map(|pair| pair.split('=').next())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    RouteSpec {
        method: HttpMethod::Post,
        path: if trimmed.is_empty() {
            "/".to_owned()
        } else {
            trimmed.to_owned()
        },
        query_allowlist,
        suffix: PathSuffixMode::Append,
    }
}

fn rag_routes(target: &ProvisionTarget) -> Vec<RouteSpec> {
    let (prefix, allowlist) = match target.storage_kind {
        StorageKind::Openai => ("/v1", Vec::new()),
        StorageKind::Azure => ("/openai", vec!["api-version".to_owned()]),
    };
    let spec = |method, path: &str, suffix| RouteSpec {
        method,
        path: format!("{prefix}{path}"),
        query_allowlist: allowlist.clone(),
        suffix,
    };
    vec![
        spec(HttpMethod::Post, "/files", PathSuffixMode::Disabled),
        spec(HttpMethod::Delete, "/files", PathSuffixMode::Append),
        spec(HttpMethod::Post, "/vector_stores", PathSuffixMode::Append),
        spec(HttpMethod::Delete, "/vector_stores", PathSuffixMode::Append),
        spec(HttpMethod::Get, "/vector_stores", PathSuffixMode::Append),
    ]
}

fn classify(error: CanonicalError) -> Failure {
    match ServiceGatewayError::from(error) {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-catch-secret
        ServiceGatewayError::FailedPrecondition { subject, .. }
            if subject == SECRET_REF_SUBJECT =>
        {
            Failure::Deferred("credstore secret is not readable yet".to_owned())
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-catch-secret
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-catch-timeout
        ServiceGatewayError::Timeout => Failure::Deferred("OAGW call timed out".to_owned()),
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-catch-timeout
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-catch-misconfig
        other => Failure::Misconfigured(other.to_string()),
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-catch-misconfig
    }
}

/// Replaces every `auth_config` value of `target` in `text` with `***`.
fn mask_secrets(text: &str, target: &ProvisionTarget) -> String {
    target
        .auth_config
        .keys()
        .filter_map(|key| target.auth_config.get(key))
        .filter(|value| !value.is_empty())
        .fold(text.to_owned(), |masked, value| {
            masked.replace(value, "***")
        })
}

/// Provisions OAGW upstreams and routes through `ServiceGatewayClientV1`.
pub struct OagwProvisioner {
    oagw: Arc<dyn ServiceGatewayClientV1>,
    authn: Arc<dyn AuthNResolverClient>,
    credentials: ClientCredentials,
    timeouts: ProvisioningTimeouts,
}

impl OagwProvisioner {
    /// Creates the provisioner with the default bounds.
    #[must_use]
    pub fn new(
        oagw: Arc<dyn ServiceGatewayClientV1>,
        authn: Arc<dyn AuthNResolverClient>,
        credentials: ClientCredentials,
    ) -> Self {
        Self {
            oagw,
            authn,
            credentials,
            timeouts: ProvisioningTimeouts::default(),
        }
    }

    /// Overrides the bounds.
    #[must_use]
    pub fn with_timeouts(mut self, timeouts: ProvisioningTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    async fn exchange(&self) -> Result<SecurityContext, ProvisioningError> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-s2s-exchange
        let request = ClientCredentialsRequest {
            client_id: self.credentials.client_id.clone(),
            client_secret: SecretString::from(self.credentials.client_secret.expose().to_owned()),
            scopes: Vec::new(),
        };
        let failure = |cause| ProvisioningError::S2sExchange {
            client_id: self.credentials.client_id.clone(),
            cause,
        };
        match tokio::time::timeout(
            self.timeouts.s2s_exchange,
            self.authn.exchange_client_credentials(&request),
        )
        .await
        {
            Ok(Ok(result)) => Ok(result.security_context),
            Ok(Err(error)) => {
                let secret = self.credentials.client_secret.expose();
                let text = error.to_string();
                let text = if secret.is_empty() {
                    text
                } else {
                    text.replace(secret, "***")
                };
                Err(failure(S2sFailureCause::Failed(text)))
            }
            Err(_elapsed) => Err(failure(S2sFailureCause::Timeout)),
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-s2s-exchange
    }

    async fn bounded<T>(
        &self,
        call: impl std::future::Future<Output = Result<T, CanonicalError>>,
    ) -> Result<Result<T, CanonicalError>, Failure> {
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-catch-timeout
        tokio::time::timeout(self.timeouts.oagw_call, call)
            .await
            .map_err(|_elapsed| Failure::Deferred("OAGW call timed out".to_owned()))
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-catch-timeout
    }

    async fn ensure_upstream(
        &self,
        ctx: &SecurityContext,
        target: &ProvisionTarget,
    ) -> Result<(Uuid, String, bool), Failure> {
        let endpoint = Endpoint {
            scheme: if target.use_http {
                Scheme::Http
            } else {
                Scheme::Https
            },
            host: target.host.clone(),
            port: target.port,
        };
        let mut builder = CreateUpstreamRequest::builder(
            Server {
                endpoints: vec![endpoint],
            },
            oagw_sdk::HTTP_PROTOCOL_ID,
        );
        // OAGW derives the alias of a hostname endpoint itself and rejects any
        // other value; an IP literal requires an explicit alias.
        if is_ip_literal(&target.host) {
            builder = builder.alias(target.oagw_alias());
        }
        if let Some(plugin_type) = &target.auth_plugin_type {
            let config = target
                .auth_config
                .keys()
                .filter_map(|key| {
                    target
                        .auth_config
                        .get(key)
                        .map(|value| (key.to_owned(), value.to_owned()))
                })
                .collect();
            builder = builder.auth(oagw_sdk::AuthConfig {
                plugin_type: plugin_type.clone(),
                sharing: SharingMode::Private,
                config: Some(config),
            });
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-create-upstream
        let created = self
            .bounded(self.oagw.create_upstream(ctx.clone(), builder.build()))
            .await?;
        match created {
            Ok(upstream) => Ok((upstream.id, upstream.alias, false)),
            Err(error) => match ServiceGatewayError::from(error.clone()) {
                ServiceGatewayError::AlreadyExists { .. } => {
                    let (id, alias) = self.find_upstream(ctx, &target.oagw_alias()).await?;
                    Ok((id, alias, true))
                }
                _ => Err(classify(error)),
            },
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-create-upstream
    }

    async fn find_upstream(
        &self,
        ctx: &SecurityContext,
        alias: &str,
    ) -> Result<(Uuid, String), Failure> {
        let mut skip = 0;
        loop {
            let query = ListQuery { top: 50, skip };
            let page = self
                .bounded(self.oagw.list_upstreams(ctx.clone(), &query))
                .await?
                .map_err(classify)?;
            let wanted = normalize_alias(alias);
            if let Some(found) = page
                .iter()
                .find(|upstream| normalize_alias(&upstream.alias) == wanted)
            {
                return Ok((found.id, found.alias.clone()));
            }
            if page.len() < 50 {
                return Err(Failure::Misconfigured(format!(
                    "upstream `{alias}` already exists but is not listed"
                )));
            }
            skip += 50;
        }
    }

    async fn list_routes(
        &self,
        ctx: &SecurityContext,
        upstream_id: Uuid,
    ) -> Result<Vec<(HttpMethod, String)>, Failure> {
        let mut existing = Vec::new();
        let mut skip = 0;
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-list-routes
        loop {
            let query = ListQuery { top: 50, skip };
            let page = self
                .bounded(
                    self.oagw
                        .list_routes(ctx.clone(), Some(upstream_id), &query),
                )
                .await?
                .map_err(classify)?;
            for route in &page {
                if let Some(http) = &route.match_rules.http {
                    for method in &http.methods {
                        existing.push((*method, http.path.clone()));
                    }
                }
            }
            if page.len() < 50 {
                return Ok(existing);
            }
            skip += 50;
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-list-routes
    }

    async fn ensure_route(
        &self,
        ctx: &SecurityContext,
        upstream_id: Uuid,
        spec: &RouteSpec,
        existing: &mut Vec<(HttpMethod, String)>,
    ) -> Result<(), Failure> {
        let key = (spec.method, spec.path.clone());
        if existing.contains(&key) {
            return Ok(());
        }
        let request = CreateRouteRequest::builder(
            upstream_id,
            MatchRules {
                http: Some(HttpMatch {
                    methods: vec![spec.method],
                    path: spec.path.clone(),
                    query_allowlist: spec.query_allowlist.clone(),
                    path_suffix_mode: spec.suffix,
                }),
                grpc: None,
            },
        )
        .build();
        match self
            .bounded(self.oagw.create_route(ctx.clone(), request))
            .await?
        {
            Ok(_) => {}
            Err(error) => match ServiceGatewayError::from(error.clone()) {
                ServiceGatewayError::AlreadyExists { .. } => {}
                _ => return Err(classify(error)),
            },
        }
        existing.push(key);
        Ok(())
    }

    /// Provisions one alias group and returns the alias OAGW reports. Any
    /// `auth_config` value in a failure text is masked before it can reach a
    /// log or an error.
    async fn provision_group(
        &self,
        ctx: &SecurityContext,
        group: &[&ProvisionTarget],
    ) -> Result<String, Failure> {
        self.provision_group_unmasked(ctx, group)
            .await
            .map_err(|failure| match failure {
                Failure::Deferred(cause) => Failure::Deferred(mask_secrets(&cause, group[0])),
                Failure::Misconfigured(cause) => {
                    Failure::Misconfigured(mask_secrets(&cause, group[0]))
                }
            })
    }

    async fn provision_group_unmasked(
        &self,
        ctx: &SecurityContext,
        group: &[&ProvisionTarget],
    ) -> Result<String, Failure> {
        let first = group[0];
        let (upstream_id, alias, reused) = self.ensure_upstream(ctx, first).await?;
        let mut existing = if reused {
            self.list_routes(ctx, upstream_id).await?
        } else {
            Vec::new()
        };
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-create-chat-route
        self.ensure_route(
            ctx,
            upstream_id,
            &chat_route(&first.api_path),
            &mut existing,
        )
        .await?;
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-create-chat-route
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-if-storage
        // Every target has a `storage_kind` (`openai` or `azure`) and its
        // upstream exists at this point.
        let mut specs: Vec<RouteSpec> = Vec::new();
        for member in group {
            for spec in rag_routes(member) {
                if !specs
                    .iter()
                    .any(|s| s.method == spec.method && s.path == spec.path)
                {
                    specs.push(spec);
                }
            }
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-create-rag-routes
        for spec in &specs {
            if let Err(failure) = self
                .ensure_route(ctx, upstream_id, spec, &mut existing)
                .await
            {
                let cause = match failure {
                    Failure::Deferred(cause) | Failure::Misconfigured(cause) => cause,
                };
                let cause = mask_secrets(&cause, first);
                tracing::warn!(
                    provider = %first.provider_id,
                    route = %spec.path,
                    %cause,
                    "RAG route could not be created, RAG is degraded"
                );
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-create-rag-routes
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-if-storage
        Ok(alias)
    }

    /// Exchanges the credentials, provisions every target group and returns the
    /// per-target outcome. The input registry is never modified; the alias OAGW
    /// returns is written into the returned copy only.
    ///
    /// # Errors
    ///
    /// [`ProvisioningError::S2sExchange`] when the exchange fails or times out;
    /// [`ProvisioningError::Misconfigured`] for any non-deferrable target error.
    pub async fn provision(
        &self,
        registry: &ProviderRegistry,
    ) -> Result<ProvisioningOutcome, ProvisioningError> {
        let ctx = self.exchange().await?;
        let targets = ProvisionTarget::from_registry(registry);
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-dedupe-targets
        // Groups are keyed by the normalized alias; members are target indexes.
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        for (index, target) in targets.iter().enumerate() {
            let alias = normalize_alias(&target.alias);
            match groups.iter_mut().find(|(known, _)| *known == alias) {
                Some((_, members)) => members.push(index),
                None => groups.push((alias, vec![index])),
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-dedupe-targets
        let mut outcomes: Vec<TargetOutcome> = targets
            .iter()
            .map(|target| TargetOutcome {
                target: target.clone(),
                state: ProvisioningState::Pending,
            })
            .collect();
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-for-each-target
        for (_, indexes) in groups {
            let members: Vec<&ProvisionTarget> = indexes.iter().map(|&i| &targets[i]).collect();
            // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-try-target
            let group = self.provision_group(&ctx, &members).await;
            // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-try-target
            match group {
                Ok(returned) => {
                    for &index in &indexes {
                        let outcome = &mut outcomes[index];
                        // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-pending-to-provisioned
                        outcome.state = ProvisioningState::Provisioned;
                        // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-pending-to-provisioned
                        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-write-copy
                        outcome.target.alias.clone_from(&returned);
                        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-write-copy
                    }
                }
                Err(Failure::Deferred(cause)) => {
                    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-defer-timeout
                    tracing::warn!(
                        provider = %members[0].provider_id,
                        target = %members[0].label(),
                        %cause,
                        "OAGW provisioning deferred"
                    );
                    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-defer-timeout
                    for &index in &indexes {
                        let outcome = &mut outcomes[index];
                        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-defer-target
                        // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-pending-to-deferred
                        outcome.state = ProvisioningState::Deferred;
                        // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-pending-to-deferred
                        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-defer-target
                    }
                }
                Err(Failure::Misconfigured(cause)) => {
                    // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-return-misconfig
                    // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-pending-to-failed
                    return Err(ProvisioningError::Misconfigured {
                        label: members[0].label(),
                        provider_id: members[0].provider_id.clone(),
                        tenant_id: members[0].tenant_id.clone(),
                        cause,
                    });
                    // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-pending-to-failed
                    // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-return-misconfig
                }
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-for-each-target
        // @cpt-begin:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-return-provisioned
        Ok(ProvisioningOutcome {
            targets: outcomes,
            s2s_context: ctx,
        })
        // @cpt-end:cpt-cf-mini-chat-algo-gear-foundation-provision-oagw:p1:inst-return-provisioned
    }
}

#[async_trait]
impl TargetRetry for OagwProvisioner {
    async fn retry(
        &self,
        ctx: &SecurityContext,
        target: &ProvisionTarget,
    ) -> Result<(), RetryFailure> {
        match self.provision_group(ctx, &[target]).await {
            // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-deferred-to-provisioned
            Ok(_) => Ok(()),
            // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-deferred-to-provisioned
            Err(Failure::Deferred(cause) | Failure::Misconfigured(cause)) => {
                // @cpt-begin:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-deferred-retry
                Err(RetryFailure { cause })
                // @cpt-end:cpt-cf-mini-chat-state-gear-foundation-provider-provisioning:p1:inst-deferred-retry
            }
        }
    }
}

#[cfg(test)]
#[path = "oagw_provisioning_tests.rs"]
mod tests;
