//! OAGW upstream and route provisioning at gear start (ADR-0005).

use std::collections::HashMap;
use std::time::Duration;

use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HttpMatch, HttpMethod,
    ListQuery, MatchRules, PathSuffixMode, Scheme, Server, SharingMode,
};
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::CanonicalError;

use super::LlmGateway;
use crate::config::{ProviderEntry, StorageKind};

/// One upstream to register.
#[derive(Debug, Clone)]
pub struct UpstreamPlan {
    pub provider_id: String,
    pub alias: String,
    pub host: String,
    pub port: u16,
    pub use_http: bool,
    pub auth_plugin_type: Option<String>,
    pub auth_config: Option<HashMap<String, String>>,
    pub api_path: String,
    pub storage_kind: StorageKind,
}

/// Error of provisioning one upstream.
#[derive(Debug)]
pub enum ProvisionError {
    /// Deterministic misconfiguration: fails startup.
    Fatal(String),
    /// Retryable (e.g. secret not yet readable, gateway unavailable).
    Deferred(String),
}

/// Builds the plans of every provider entry and tenant override (aliases filled).
#[must_use]
#[allow(clippy::implicit_hasher)] // always called with the config's default-hasher map
pub fn plans(providers: &HashMap<String, ProviderEntry>) -> Vec<UpstreamPlan> {
    let mut out = Vec::new();
    for (id, p) in providers {
        let alias = p
            .upstream_alias
            .clone()
            .unwrap_or_else(|| p.default_alias_for(&p.host));
        out.push(UpstreamPlan {
            provider_id: id.clone(),
            alias: alias.clone(),
            host: p.host.clone(),
            port: p.effective_port(),
            use_http: p.use_http,
            auth_plugin_type: p.auth_plugin_type.clone(),
            auth_config: p.auth_config.clone(),
            api_path: p.api_path.clone(),
            storage_kind: p.storage_kind,
        });
        for o in p.tenant_overrides.values() {
            let host = o.host.clone().unwrap_or_else(|| p.host.clone());
            let o_alias = o
                .upstream_alias
                .clone()
                .or_else(|| o.host.as_deref().map(|h| p.default_alias_for(h)))
                .unwrap_or_else(|| alias.clone());
            if o_alias == alias {
                continue;
            }
            out.push(UpstreamPlan {
                provider_id: id.clone(),
                alias: o_alias,
                host,
                port: p.effective_port(),
                use_http: p.use_http,
                auth_plugin_type: o
                    .auth_plugin_type
                    .clone()
                    .or_else(|| p.auth_plugin_type.clone()),
                auth_config: o.auth_config.clone().or_else(|| p.auth_config.clone()),
                api_path: p.api_path.clone(),
                storage_kind: p.storage_kind,
            });
        }
    }
    out
}

/// Splits `api_path` into the route prefix (before `{model}`, without query) and query keys.
#[must_use]
pub fn route_of(api_path: &str) -> (String, Vec<String>) {
    let (path, query) = api_path.split_once('?').unwrap_or((api_path, ""));
    let prefix = match path.find("{model}") {
        Some(i) => path[..i].trim_end_matches('/').to_owned(),
        None => path.to_owned(),
    };
    let prefix = if prefix.is_empty() {
        "/".to_owned()
    } else {
        prefix
    };
    let keys = query
        .split('&')
        .filter_map(|kv| kv.split('=').next())
        .filter(|k| !k.is_empty())
        .map(str::to_owned)
        .collect();
    (prefix, keys)
}

fn http_route(path: &str, methods: Vec<HttpMethod>, query: Vec<String>) -> MatchRules {
    MatchRules {
        http: Some(HttpMatch {
            methods,
            path: path.to_owned(),
            query_allowlist: query,
            path_suffix_mode: PathSuffixMode::Append,
        }),
        grpc: None,
    }
}

fn classify(e: &CanonicalError) -> ProvisionError {
    // A state precondition (e.g. the auth secret is not readable yet) is retried in the background.
    if matches!(e, CanonicalError::FailedPrecondition { .. }) {
        return ProvisionError::Deferred(e.to_string());
    }
    let status = e.status_code();
    if status == 400 || status == 422 {
        ProvisionError::Fatal(e.to_string())
    } else {
        ProvisionError::Deferred(e.to_string())
    }
}

impl LlmGateway {
    /// Registers (or reuses) the upstream and routes of one plan.
    ///
    /// # Errors
    /// [`ProvisionError`].
    // Sequential register-or-reuse steps with their error mapping.
    #[allow(clippy::cognitive_complexity)]
    pub async fn provision(&self, plan: &UpstreamPlan) -> Result<(), ProvisionError> {
        let ctx = self
            .s2s_ctx()
            .await
            .map_err(|e| ProvisionError::Deferred(e.to_string()))?;
        let server = Server {
            endpoints: vec![Endpoint {
                scheme: if plan.use_http {
                    Scheme::Http
                } else {
                    Scheme::Https
                },
                host: plan.host.clone(),
                port: plan.port,
            }],
        };
        let request = |alias: &str| {
            let mut b = CreateUpstreamRequest::builder(server.clone(), oagw_sdk::HTTP_PROTOCOL_ID)
                .alias(alias.to_owned())
                .enabled(true)
                .tags(vec![
                    "mini-chat".to_owned(),
                    format!("mini-chat-provider:{}", plan.provider_id),
                ]);
            if let Some(t) = &plan.auth_plugin_type {
                b = b.auth(AuthConfig {
                    plugin_type: t.clone(),
                    sharing: SharingMode::Private,
                    config: plan.auth_config.clone(),
                });
            }
            b.build()
        };
        let mut alias = plan.alias.clone();
        let mut created = self
            .oagw()
            .create_upstream(ctx.clone(), request(&alias))
            .await;
        if let Err(e) = &created
            && e.to_string().contains("auto-derived")
        {
            // OAGW derives the alias of hostname endpoints; register under the derived one.
            let derived = crate::config::default_alias(&plan.host, plan.port, plan.use_http);
            tracing::warn!(
                provider = %plan.provider_id,
                configured = %plan.alias,
                derived = %derived,
                "OAGW rejected the configured upstream alias; using the alias it derives"
            );
            alias = derived;
            created = self
                .oagw()
                .create_upstream(ctx.clone(), request(&alias))
                .await;
        }
        let upstream_id = match created {
            Ok(u) => {
                alias.clone_from(&u.alias);
                u.id
            }
            Err(CanonicalError::AlreadyExists { .. }) => {
                let list = self
                    .oagw()
                    .list_upstreams(ctx.clone(), &ListQuery { top: 1000, skip: 0 })
                    .await
                    .map_err(|e| classify(&e))?;
                let wanted = alias.to_ascii_lowercase();
                match list
                    .into_iter()
                    .find(|u| u.alias.eq_ignore_ascii_case(&wanted))
                {
                    Some(u) => {
                        tracing::info!(alias = %alias, "reusing existing OAGW upstream");
                        u.id
                    }
                    None => {
                        return Err(ProvisionError::Deferred(format!(
                            "upstream '{alias}' exists but is not visible"
                        )));
                    }
                }
            }
            Err(e) => return Err(classify(&e)),
        };
        if !alias.eq_ignore_ascii_case(&plan.alias) {
            self.set_alias_override(&plan.alias, &alias);
        }
        let (chat_prefix, chat_query) = route_of(&plan.api_path);
        let chat_route = CreateRouteRequest::builder(
            upstream_id,
            http_route(&chat_prefix, vec![HttpMethod::Post], chat_query),
        )
        .build();
        if let Err(e) = self.oagw().create_route(ctx.clone(), chat_route).await
            && !matches!(e, CanonicalError::AlreadyExists { .. })
        {
            return Err(classify(&e));
        }
        // RAG routes (best effort).
        let (prefix, query) = match plan.storage_kind {
            StorageKind::Openai => ("/v1", Vec::new()),
            StorageKind::Azure => ("/openai", vec!["api-version".to_owned()]),
        };
        for (path, methods) in [
            (
                format!("{prefix}/files"),
                vec![HttpMethod::Post, HttpMethod::Delete, HttpMethod::Get],
            ),
            (
                format!("{prefix}/vector_stores"),
                vec![HttpMethod::Post, HttpMethod::Delete, HttpMethod::Get],
            ),
        ] {
            let route =
                CreateRouteRequest::builder(upstream_id, http_route(&path, methods, query.clone()))
                    .build();
            if let Err(e) = self.oagw().create_route(ctx.clone(), route).await {
                tracing::warn!(alias = %plan.alias, path = %path, error = %e, "RAG route registration failed");
            }
        }
        tracing::info!(provider = %plan.provider_id, alias = %alias, "OAGW upstream provisioned");
        Ok(())
    }

    /// Provisions every plan; returns the deferred ones. A fatal error fails startup.
    ///
    /// # Errors
    /// The first fatal provisioning error.
    pub async fn provision_all(&self, plans: &[UpstreamPlan]) -> anyhow::Result<Vec<UpstreamPlan>> {
        let mut deferred = Vec::new();
        for plan in plans {
            match self.provision(plan).await {
                Ok(()) => {}
                Err(ProvisionError::Fatal(e)) => {
                    anyhow::bail!(
                        "provider '{}' (alias '{}') is misconfigured: {e}",
                        plan.provider_id,
                        plan.alias
                    );
                }
                Err(ProvisionError::Deferred(e)) => {
                    tracing::warn!(provider = %plan.provider_id, error = %e, "OAGW provisioning deferred");
                    deferred.push(plan.clone());
                }
            }
        }
        Ok(deferred)
    }

    /// Background reconcile of deferred plans: 2 s, doubling up to 60 s.
    // Retry loop with backoff, cancellation and one-time warning.
    #[allow(clippy::cognitive_complexity)]
    pub async fn reconcile_deferred(
        &self,
        mut pending: Vec<UpstreamPlan>,
        cancel: CancellationToken,
    ) {
        let mut delay = Duration::from_secs(2);
        let started = std::time::Instant::now();
        let mut warned = false;
        while !pending.is_empty() {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
            let mut still = Vec::new();
            for plan in pending {
                if let Err(e) = self.provision(&plan).await {
                    tracing::debug!(provider = %plan.provider_id, error = ?e, "OAGW provisioning retry failed");
                    still.push(plan);
                }
            }
            pending = still;
            if !warned && started.elapsed() > Duration::from_secs(120) && !pending.is_empty() {
                warned = true;
                let ids: Vec<&str> = pending.iter().map(|p| p.provider_id.as_str()).collect();
                tracing::warn!(providers = ?ids, "OAGW provisioning still pending after 2 minutes");
            }
            delay = (delay * 2).min(Duration::from_secs(60));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::route_of;

    #[test]
    fn route_prefix_and_query() {
        assert_eq!(
            route_of("/v1/responses"),
            ("/v1/responses".to_owned(), vec![])
        );
        assert_eq!(
            route_of("/openai/v1/responses?api-version=2025"),
            (
                "/openai/v1/responses".to_owned(),
                vec!["api-version".to_owned()]
            )
        );
        assert_eq!(
            route_of("/openai/deployments/{model}/chat/completions"),
            ("/openai/deployments".to_owned(), vec![])
        );
    }
}
