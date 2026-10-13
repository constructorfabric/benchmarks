//! Provisioning of OAGW upstreams and routes for every provider entry (ADR-0005).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HTTP_PROTOCOL_ID, HttpMatch,
    HttpMethod, ListQuery, MatchRules, PathSuffixMode, Scheme, Server, SharingMode,
};
use toolkit_canonical_errors::CanonicalError;
use tokio_util::sync::CancellationToken;

use crate::config::StorageKind;
use crate::infra::llm::gateway::Gateway;
use crate::infra::llm::resolver::{ProviderResolver, UpstreamSpec};

/// Route prefix and query allowlist derived from `api_path`.
#[must_use]
pub fn chat_route(api_path: &str) -> (String, Vec<String>) {
    let (path, query) = api_path.split_once('?').unwrap_or((api_path, ""));
    let prefix = match path.find("{model}") {
        Some(pos) => path[..pos].trim_end_matches('/').to_owned(),
        None => path.to_owned(),
    };
    let prefix = if prefix.is_empty() { "/".to_owned() } else { prefix };
    let mut allow: Vec<String> = query
        .split('&')
        .filter_map(|kv| kv.split('=').next())
        .filter(|k| !k.is_empty())
        .map(str::to_owned)
        .collect();
    if !allow.iter().any(|k| k == "api-version") {
        allow.push("api-version".to_owned());
    }
    (prefix, allow)
}

#[derive(Debug)]
enum Outcome {
    Retry(String),
    RetryWithoutAlias,
}

/// Upstream/route provisioner.
pub struct Provisioner {
    gateway: Arc<Gateway>,
    resolver: Arc<ProviderResolver>,
}

impl Provisioner {
    /// New provisioner.
    #[must_use]
    pub fn new(gateway: Arc<Gateway>, resolver: Arc<ProviderResolver>) -> Self {
        Self { gateway, resolver }
    }

    /// Provisions every upstream once; returns the specs still pending.
    pub async fn provision_all(&self) -> Vec<UpstreamSpec> {
        let mut pending = Vec::new();
        for spec in self.resolver.upstream_specs() {
            if let Err(e) = self.provision(spec.clone()).await {
                tracing::warn!(provider = %spec.provider_id, tenant = ?spec.tenant, error = %e, "OAGW provisioning deferred");
                pending.push(spec);
            }
        }
        pending
    }

    async fn provision(&self, mut spec: UpstreamSpec) -> Result<(), String> {
        match self.try_provision(&spec).await {
            Ok(()) => Ok(()),
            Err(Outcome::RetryWithoutAlias) => {
                spec.requested_alias = None;
                self.try_provision(&spec).await.map_err(|e| format!("{e:?}"))
            }
            Err(Outcome::Retry(e)) => Err(e),
        }
    }

    #[allow(
        clippy::cognitive_complexity,
        reason = "OAGW provisioning orchestration: upstream create-or-adopt followed by route registration"
    )]
    async fn try_provision(&self, spec: &UpstreamSpec) -> Result<(), Outcome> {
        let ctx = self.gateway.context().ok_or_else(|| Outcome::Retry("no S2S context".into()))?;
        let client = self.gateway.client();
        let mut builder = CreateUpstreamRequest::builder(
            Server {
                endpoints: vec![Endpoint {
                    scheme: if spec.use_http { Scheme::Http } else { Scheme::Https },
                    host: spec.host.clone(),
                    port: spec.port,
                }],
            },
            HTTP_PROTOCOL_ID,
        )
        .tags(vec!["mini-chat".to_owned(), format!("mini-chat-provider:{}", spec.provider_id)])
        .enabled(true);
        if let Some(a) = &spec.requested_alias {
            builder = builder.alias(a.clone());
        }
        if let Some(plugin) = &spec.auth_plugin_type {
            builder = builder.auth(AuthConfig {
                plugin_type: plugin.clone(),
                sharing: SharingMode::Private,
                config: spec.auth_config.clone().map(|m| m.into_iter().collect::<HashMap<_, _>>()),
            });
        }
        let upstream = match client.create_upstream(ctx.clone(), builder.build()).await {
            Ok(u) => u,
            Err(CanonicalError::AlreadyExists { .. }) => {
                let alias = spec.requested_alias.clone().unwrap_or_else(|| spec.alias.clone()).to_ascii_lowercase();
                let all = client
                    .list_upstreams(ctx.clone(), &ListQuery { top: 1000, skip: 0 })
                    .await
                    .map_err(|e| Outcome::Retry(e.to_string()))?;
                all.into_iter()
                    .find(|u| u.alias == alias)
                    .ok_or_else(|| Outcome::Retry(format!("upstream '{alias}' exists but was not found")))?
            }
            Err(e @ CanonicalError::InvalidArgument { .. }) => {
                let msg = e.to_string();
                if spec.requested_alias.is_some() && msg.contains("alias") {
                    return Err(Outcome::RetryWithoutAlias);
                }
                tracing::error!(provider = %spec.provider_id, error = %msg, "OAGW rejected the provider upstream");
                return Err(Outcome::Retry(msg));
            }
            Err(e) => return Err(Outcome::Retry(e.to_string())),
        };
        self.resolver.set_alias(&spec.provider_id, spec.tenant.as_deref(), &upstream.alias);

        let (chat_path, chat_query) = chat_route(&spec.api_path);
        let mut routes = vec![(vec![HttpMethod::Post], chat_path, chat_query)];
        if let Some(kind) = spec.storage_kind {
            let prefix = match kind {
                StorageKind::Openai => "/v1",
                StorageKind::Azure => "/openai",
            };
            for p in ["files", "vector_stores"] {
                routes.push((
                    vec![HttpMethod::Post, HttpMethod::Get, HttpMethod::Delete],
                    format!("{prefix}/{p}"),
                    vec!["api-version".to_owned()],
                ));
            }
        }
        for (methods, path, query) in routes {
            let rules = MatchRules {
                http: Some(HttpMatch {
                    methods,
                    path: path.clone(),
                    query_allowlist: query,
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            };
            match client
                .create_route(ctx.clone(), CreateRouteRequest::builder(upstream.id, rules).build())
                .await
            {
                Ok(_) | Err(CanonicalError::AlreadyExists { .. }) => {}
                Err(e) => {
                    // A missing RAG route only degrades RAG; the chat route is required.
                    tracing::warn!(provider = %spec.provider_id, path = %path, error = %e, "OAGW route registration failed");
                    if path == chat_route(&spec.api_path).0 {
                        return Err(Outcome::Retry(e.to_string()));
                    }
                }
            }
        }
        tracing::info!(provider = %spec.provider_id, tenant = ?spec.tenant, alias = %upstream.alias, "OAGW upstream provisioned");
        Ok(())
    }

    /// Background reconcile of deferred entries (2 s, doubling to 60 s).
    pub fn spawn_reconcile(self: Arc<Self>, mut pending: Vec<UpstreamSpec>, cancel: CancellationToken) {
        if pending.is_empty() {
            return;
        }
        tokio::spawn(async move {
            let started = Instant::now();
            let mut delay = Duration::from_secs(2);
            let mut warned = false;
            while !pending.is_empty() {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(delay) => {}
                }
                let mut still = Vec::new();
                for spec in pending {
                    if self.provision(spec.clone()).await.is_err() {
                        still.push(spec);
                    }
                }
                pending = still;
                if !warned && !pending.is_empty() && started.elapsed() > Duration::from_secs(120) {
                    warned = true;
                    let ids: Vec<&str> = pending.iter().map(|s| s.provider_id.as_str()).collect();
                    tracing::warn!(providers = ?ids, "mini-chat providers still not provisioned in OAGW");
                }
                delay = (delay * 2).min(Duration::from_secs(60));
            }
        });
    }
}
