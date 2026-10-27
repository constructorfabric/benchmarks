//! OAGW upstream and route provisioning at gear start (ADR-0005).
//!
//! The gear obtains an S2S security context from `authn_resolver` and
//! registers an OAGW upstream and route for every provider entry and tenant
//! override. A deterministically misconfigured entry fails startup; an entry
//! whose credstore secret is not yet readable is retried in the background.

use std::collections::HashMap;
use std::sync::Arc;

use authn_resolver_sdk::{AuthNResolverClient, ClientCredentialsRequest};
use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HttpMatch, HttpMethod,
    MatchRules, PathSuffixMode, Scheme, Server, ServiceGatewayClientV1, SharingMode,
};
use toolkit_security::SecurityContext;

use crate::config::{MiniChatConfig, ProviderEntry};
use crate::infra::llm::storage::prefix;

/// The provisioner.
pub struct OagwProvisioner {
    oagw: Arc<dyn ServiceGatewayClientV1>,
    authn: Arc<dyn AuthNResolverClient>,
    config: Arc<MiniChatConfig>,
}

/// One upstream to register (a provider entry or a tenant override with its
/// own host or alias).
struct UpstreamPlan {
    alias: String,
    host: String,
    port: u16,
    use_http: bool,
    auth_plugin_type: Option<String>,
    auth_config: Option<HashMap<String, String>>,
    routes: Vec<(Vec<HttpMethod>, String, Vec<String>)>,
}

impl OagwProvisioner {
    /// Create the provisioner.
    #[must_use]
    pub fn new(
        oagw: Arc<dyn ServiceGatewayClientV1>,
        authn: Arc<dyn AuthNResolverClient>,
        config: Arc<MiniChatConfig>,
    ) -> Self {
        Self {
            oagw,
            authn,
            config,
        }
    }

    /// The OAGW client.
    #[must_use]
    pub fn oagw(&self) -> Arc<dyn ServiceGatewayClientV1> {
        Arc::clone(&self.oagw)
    }

    /// The authn client.
    #[must_use]
    pub fn authn(&self) -> Arc<dyn AuthNResolverClient> {
        Arc::clone(&self.authn)
    }

    /// The config.
    #[must_use]
    pub fn config(&self) -> Arc<MiniChatConfig> {
        Arc::clone(&self.config)
    }

    /// Exchange the S2S credentials (public, for the gear lifecycle).
    pub async fn s2s_context_pub(&self) -> anyhow::Result<SecurityContext> {
        self.s2s_context().await
    }

    /// Exchange the S2S credentials.
    async fn s2s_context(&self) -> anyhow::Result<SecurityContext> {
        let cc = self
            .config
            .client_credentials
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("client_credentials missing"))?;
        let result = self
            .authn
            .exchange_client_credentials(&ClientCredentialsRequest {
                client_id: cc.client_id.clone(),
                client_secret: cc.client_secret.clone(),
                scopes: Vec::new(),
            })
            .await
            .map_err(|e| anyhow::anyhow!("client credentials exchange failed: {e}"))?;
        Ok(result.security_context)
    }

    /// Provision every configured provider. Returns aliases that failed
    /// with a retryable error (secret not yet readable) for the background
    /// reconcile loop.
    pub async fn provision_all(&self) -> anyhow::Result<Vec<UpstreamPlanKey>> {
        let ctx = self.s2s_context().await?;
        let mut deferred = Vec::new();
        for plan in self.plans() {
            match self.provision(&ctx, &plan).await {
                Ok(()) => {}
                Err(ProvisionError::Retryable) => {
                    deferred.push(UpstreamPlanKey {
                        alias: plan.alias.clone(),
                    });
                }
                Err(ProvisionError::Fatal(err)) => return Err(err),
            }
        }
        Ok(deferred)
    }

    /// Retry the deferred plans in the background: first retry after 2 s,
    /// doubling up to 60 s; one warning after 2 minutes without success.
    pub fn reconcile_deferred(self: Arc<Self>, plans: Vec<UpstreamPlanKey>, cancel: tokio_util::sync::CancellationToken) {
        if plans.is_empty() {
            return;
        }
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let mut wait = std::time::Duration::from_secs(2);
            let mut warned = false;
            let mut pending: Vec<UpstreamPlanKey> = plans;
            loop {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(wait) => {}
                }
                let Ok(ctx) = self.s2s_context().await else {
                    continue;
                };
                let all = self.plans();
                let mut still_pending = Vec::new();
                for key in &pending {
                    let Some(plan) = all.iter().find(|p| p.alias == key.alias) else {
                        continue;
                    };
                    match self.provision(&ctx, plan).await {
                        Ok(()) | Err(ProvisionError::Fatal(_)) => {}
                        Err(ProvisionError::Retryable) => still_pending.push(key.clone()),
                    }
                }
                pending = still_pending;
                if pending.is_empty() {
                    tracing::info!("mini-chat: OAGW provider provisioning reconciled");
                    return;
                }
                if !warned && started.elapsed() >= std::time::Duration::from_secs(120) {
                    warned = true;
                    let names: Vec<_> = pending.iter().map(|p| p.alias.clone()).collect();
                    tracing::warn!(
                        "mini-chat: OAGW providers still pending after 2 minutes: {}",
                        names.join(", ")
                    );
                }
                wait = (wait * 2).min(std::time::Duration::from_secs(60));
            }
        });
    }

    /// Build the upstream plans from configuration (deduplicated by alias).
    fn plans(&self) -> Vec<UpstreamPlan> {
        let mut plans: HashMap<String, UpstreamPlan> = HashMap::new();
        for (_id, entry) in &self.config.providers {
            let plan = plan_for(entry, entry.upstream_alias.as_deref().unwrap_or(&entry.host));
            plans
                .entry(plan.alias.clone())
                .or_insert_with(|| plan.clone())
                .routes
                .extend(plan.routes.clone());
            for override_ in entry.tenant_overrides.values() {
                let alias = override_
                    .upstream_alias
                    .clone()
                    .or_else(|| override_.host.clone());
                let Some(alias) = alias else { continue };
                let mut plan = plan_for(entry, &alias);
                if let Some(host) = &override_.host {
                    plan.host = host.clone();
                }
                if let Some(t) = &override_.auth_plugin_type {
                    plan.auth_plugin_type = Some(t.clone());
                }
                if let Some(c) = &override_.auth_config {
                    plan.auth_config = Some(c.as_string_map());
                }
                plans
                    .entry(plan.alias.clone())
                    .or_insert_with(|| plan.clone())
                    .routes
                    .extend(plan.routes.clone());
            }
        }
        plans.into_values().collect()
    }

    /// Provision one upstream and its routes.
    async fn provision(
        &self,
        ctx: &SecurityContext,
        plan: &UpstreamPlan,
    ) -> Result<(), ProvisionError> {
        let scheme = if plan.use_http { Scheme::Http } else { Scheme::Https };
        let mut builder = CreateUpstreamRequest::builder(
            Server {
                endpoints: vec![Endpoint {
                    scheme,
                    host: plan.host.clone(),
                    port: plan.port,
                }],
            },
            oagw_sdk::gts::HTTP_PROTOCOL_ID,
        )
        .alias(plan.alias.clone())
        .tags(vec!["mini-chat".to_owned()]);
        if let (Some(plugin_type), Some(config)) = (&plan.auth_plugin_type, &plan.auth_config) {
            builder = builder.auth(AuthConfig {
                plugin_type: plugin_type.clone(),
                sharing: SharingMode::Private,
                config: Some(config.clone()),
            });
        }
        let upstream: oagw_sdk::Upstream = match self.oagw.create_upstream(ctx.clone(), builder.build()).await {
            Ok(upstream) => upstream,
            Err(err) => {
                let err = oagw_sdk::ServiceGatewayError::from(err);
                match err {
                    oagw_sdk::ServiceGatewayError::AlreadyExists { .. } => {
                        // Reuse the existing upstream: resolve its id.
                        self.find_upstream(ctx, &plan.alias).await?
                    }
                    // A not-yet-accessible credstore secret is retried.
                    oagw_sdk::ServiceGatewayError::FailedPrecondition { .. } => {
                        return Err(ProvisionError::Retryable);
                    }
                    other => {
                        return Err(ProvisionError::Fatal(anyhow::anyhow!(
                            "create_upstream for {} failed: {other:?}",
                            plan.alias
                        )));
                    }
                }
            }
        };
        for (methods, path, query_allowlist) in &plan.routes {
            let route = CreateRouteRequest::builder(
                upstream.id,
                MatchRules {
                    http: Some(HttpMatch {
                        methods: methods.clone(),
                        path: path.clone(),
                        query_allowlist: query_allowlist.clone(),
                        path_suffix_mode: PathSuffixMode::Append,
                    }),
                    grpc: None,
                },
            )
            .build();
            match self.oagw.create_route(ctx.clone(), route).await {
                Ok(_) => {}
                Err(err) => {
                    let err = oagw_sdk::ServiceGatewayError::from(err);
                    match err {
                        oagw_sdk::ServiceGatewayError::AlreadyExists { .. } => {}
                        other => {
                            return Err(ProvisionError::Fatal(anyhow::anyhow!(
                                "create_route for {} {path} failed: {other:?}",
                                plan.alias
                            )));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Find an existing upstream by alias.
    async fn find_upstream(
        &self,
        ctx: &SecurityContext,
        alias: &str,
    ) -> Result<oagw_sdk::Upstream, ProvisionError> {
        let upstreams = self
            .oagw
            .list_upstreams(ctx.clone(), &oagw_sdk::ListQuery { top: 200, skip: 0 })
            .await
            .map_err(|e| ProvisionError::Fatal(anyhow::anyhow!("list_upstreams failed: {e}")))?;
        upstreams
            .into_iter()
            .find(|u| u.alias == alias)
            .ok_or_else(|| {
                ProvisionError::Fatal(anyhow::anyhow!("upstream {alias} not found after create"))
            })
    }
}

/// A deferred upstream key (background reconcile).
#[derive(Clone)]
pub struct UpstreamPlanKey {
    /// The upstream alias.
    pub alias: String,
}

impl Clone for UpstreamPlan {
    fn clone(&self) -> Self {
        Self {
            alias: self.alias.clone(),
            host: self.host.clone(),
            port: self.port,
            use_http: self.use_http,
            auth_plugin_type: self.auth_plugin_type.clone(),
            auth_config: self.auth_config.clone(),
            routes: self.routes.clone(),
        }
    }
}

enum ProvisionError {
    Retryable,
    Fatal(anyhow::Error),
}

/// Build the plan for a provider entry under an alias.
fn plan_for(entry: &ProviderEntry, alias: &str) -> UpstreamPlan {
    let port = entry
        .port
        .unwrap_or(if entry.use_http { 80 } else { 443 });
    let auth_config = entry.auth_config.as_ref().map(crate::config::AuthConfig::as_string_map);
    // Chat route.
    let (path, query_names) = split_path_query(&entry.api_path);
    let mut routes = vec![(vec![HttpMethod::Post], path, query_names)];
    // RAG routes.
    if let Some(storage_kind) = entry.storage_kind {
        let prefix = prefix(storage_kind);
        let query = if storage_kind == crate::config::StorageKind::Azure {
            vec!["api-version".to_owned()]
        } else {
            Vec::new()
        };
        routes.push((vec![HttpMethod::Post], format!("{prefix}/files"), query.clone()));
        routes.push((vec![HttpMethod::Delete], format!("{prefix}/files"), query.clone()));
        routes.push((vec![HttpMethod::Post], format!("{prefix}/vector_stores"), query.clone()));
        routes.push((vec![HttpMethod::Get], format!("{prefix}/vector_stores"), query.clone()));
        routes.push((vec![HttpMethod::Delete], format!("{prefix}/vector_stores"), query));
    }
    UpstreamPlan {
        alias: alias.to_owned(),
        host: entry.host.clone(),
        port,
        use_http: entry.use_http,
        auth_plugin_type: entry.auth_plugin_type.clone(),
        auth_config,
        routes,
    }
}

/// Split an `api_path` into its path and query parameter names.
fn split_path_query(api_path: &str) -> (String, Vec<String>) {
    match api_path.split_once('?') {
        Some((path, query)) => {
            let names = query
                .split('&')
                .filter_map(|pair| pair.split('=').next())
                .map(str::to_owned)
                .collect();
            (path.to_owned(), names)
        }
        None => (api_path.to_owned(), Vec::new()),
    }
}
