//! OAGW upstream and route provisioning for every provider entry and tenant
//! override (ADR-0005). OAGW keeps upstreams in memory, so provisioning runs
//! on every gear start. Entries that cannot be provisioned yet (for example a
//! credstore secret that is not readable) are retried by a background
//! reconcile loop: 2 s after start, doubling up to 60 s.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HTTP_PROTOCOL_ID,
    HeadersConfig, HttpMatch, HttpMethod, ListQuery, MatchRules, PathSuffixMode,
    RequestHeaderRules, Scheme, Server, SharingMode,
};
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use tracing::{info, warn};
use uuid::Uuid;

use super::client::LlmClient;
use super::registry::ProviderRegistry;
use crate::config::{ProviderEntry, ProviderKind, StorageKind};

/// One upstream to provision (a provider entry or one of its tenant overrides).
#[derive(Debug, Clone)]
pub struct UpstreamSpec {
    pub key: String,
    pub provider_id: String,
    pub kind: ProviderKind,
    pub host: String,
    pub port: u16,
    pub use_http: bool,
    pub alias: String,
    pub api_path: String,
    pub auth_plugin_type: Option<String>,
    pub auth_config: HashMap<String, String>,
    pub storage_kind: Option<StorageKind>,
}

fn string_map(
    m: &std::collections::BTreeMap<String, serde_json::Value>,
) -> HashMap<String, String> {
    m.iter()
        .map(|(k, v)| {
            let s = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            (k.clone(), s)
        })
        .collect()
}

/// Build the upstream specs of all configured providers.
#[must_use]
pub fn specs(providers: &std::collections::BTreeMap<String, ProviderEntry>) -> Vec<UpstreamSpec> {
    let mut out = Vec::new();
    for (id, p) in providers {
        let alias = p.upstream_alias.clone().unwrap_or_else(|| p.host.clone());
        out.push(UpstreamSpec {
            key: id.clone(),
            provider_id: id.clone(),
            kind: p.kind,
            host: p.host.clone(),
            port: p.effective_port(),
            use_http: p.use_http,
            alias: alias.clone(),
            api_path: p.api_path.clone(),
            auth_plugin_type: p.auth_plugin_type.clone(),
            auth_config: string_map(&p.auth_config),
            storage_kind: p.storage_kind,
        });
        for (tid, o) in &p.tenant_overrides {
            let host = o.host.clone().unwrap_or_else(|| p.host.clone());
            let o_alias = o
                .upstream_alias
                .clone()
                .or_else(|| o.host.clone())
                .unwrap_or_else(|| alias.clone());
            if o_alias == alias && host == p.host {
                continue;
            }
            out.push(UpstreamSpec {
                key: format!("{id}/{tid}"),
                provider_id: id.clone(),
                kind: p.kind,
                host,
                port: p.effective_port(),
                use_http: p.use_http,
                alias: o_alias,
                api_path: p.api_path.clone(),
                auth_plugin_type: o
                    .auth_plugin_type
                    .clone()
                    .or_else(|| p.auth_plugin_type.clone()),
                auth_config: o
                    .auth_config
                    .as_ref()
                    .map_or_else(|| string_map(&p.auth_config), string_map),
                storage_kind: p.storage_kind,
            });
        }
    }
    out
}

/// Route prefix and query allowlist of a chat `api_path`.
#[must_use]
pub fn chat_route(api_path: &str) -> (String, Vec<String>) {
    let (path, query) = api_path.split_once('?').unwrap_or((api_path, ""));
    let prefix = path.split("{model}").next().unwrap_or(path);
    let prefix = if prefix.len() > 1 {
        prefix.trim_end_matches('/').to_owned()
    } else {
        prefix.to_owned()
    };
    let allow = query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| kv.split('=').next().unwrap_or(kv).to_owned())
        .collect();
    (prefix, allow)
}

/// Outcome of provisioning one spec.
#[derive(Debug)]
pub enum ProvisionOutcome {
    Done,
    /// Retry later (e.g. secret not yet readable).
    Deferred(String),
}

async fn find_upstream_by_alias(
    client: &LlmClient,
    ctx: &SecurityContext,
    alias: &str,
) -> Option<oagw_sdk::Upstream> {
    let mut skip = 0;
    loop {
        let page = client
            .oagw()
            .list_upstreams(ctx.clone(), &ListQuery { top: 100, skip })
            .await
            .ok()?;
        let n = page.len();
        if let Some(u) = page
            .into_iter()
            .find(|u| u.alias.eq_ignore_ascii_case(alias))
        {
            return Some(u);
        }
        if n < 100 {
            return None;
        }
        skip += 100;
    }
}

fn upstream_request(spec: &UpstreamSpec, with_alias: bool) -> CreateUpstreamRequest {
    let server = Server {
        endpoints: vec![Endpoint {
            scheme: if spec.use_http {
                Scheme::Http
            } else {
                Scheme::Https
            },
            host: spec.host.clone(),
            port: spec.port,
        }],
    };
    let mut b = CreateUpstreamRequest::builder(server, HTTP_PROTOCOL_ID).tags(vec![
        "mini-chat".to_owned(),
        format!("mini-chat-provider:{}", spec.provider_id),
    ]);
    if with_alias {
        b = b.alias(spec.alias.clone());
    }
    if let Some(pt) = &spec.auth_plugin_type {
        b = b.auth(AuthConfig {
            plugin_type: pt.clone(),
            sharing: SharingMode::Private,
            config: Some(spec.auth_config.clone()),
        });
    }
    if spec.kind == ProviderKind::AnthropicMessages {
        let mut set = HashMap::new();
        set.insert(
            "anthropic-version".to_owned(),
            super::adapters::anthropic::ANTHROPIC_VERSION.to_owned(),
        );
        set.insert(
            "anthropic-beta".to_owned(),
            "files-api-2025-04-14".to_owned(),
        );
        b = b.headers(HeadersConfig {
            request: Some(RequestHeaderRules {
                set,
                ..RequestHeaderRules::default()
            }),
            response: None,
        });
    }
    b.build()
}

async fn ensure_route(
    client: &LlmClient,
    ctx: &SecurityContext,
    upstream_id: Uuid,
    path: &str,
    methods: Vec<HttpMethod>,
    query_allowlist: Vec<String>,
) -> Result<(), String> {
    let req = CreateRouteRequest::builder(
        upstream_id,
        MatchRules {
            http: Some(HttpMatch {
                methods,
                path: path.to_owned(),
                query_allowlist,
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
    )
    .tags(vec!["mini-chat".to_owned()])
    .build();
    match client.oagw().create_route(ctx.clone(), req).await {
        Ok(_) | Err(CanonicalError::AlreadyExists { .. }) => Ok(()),
        Err(e) => Err(format!("route {path}: {e}")),
    }
}

/// Provision one upstream and its routes.
#[allow(clippy::cognitive_complexity)]
pub async fn provision(
    client: &LlmClient,
    registry: &ProviderRegistry,
    ctx: &SecurityContext,
    spec: &UpstreamSpec,
) -> ProvisionOutcome {
    let upstream = match client
        .oagw()
        .create_upstream(ctx.clone(), upstream_request(spec, true))
        .await
    {
        Ok(u) => u,
        Err(CanonicalError::AlreadyExists { .. }) => {
            match find_upstream_by_alias(client, ctx, &spec.alias).await {
                Some(u) => u,
                None => {
                    return ProvisionOutcome::Deferred(format!(
                        "upstream alias '{}' exists but is not visible",
                        spec.alias
                    ));
                }
            }
        }
        Err(CanonicalError::InvalidArgument { .. }) => {
            // Hostname endpoints derive their alias; retry without one.
            match client
                .oagw()
                .create_upstream(ctx.clone(), upstream_request(spec, false))
                .await
            {
                Ok(u) => u,
                Err(CanonicalError::AlreadyExists { .. }) => {
                    let derived = if spec.port == 443 || spec.port == 80 {
                        spec.host.to_lowercase()
                    } else {
                        format!("{}:{}", spec.host.to_lowercase(), spec.port)
                    };
                    match find_upstream_by_alias(client, ctx, &derived).await {
                        Some(u) => u,
                        None => {
                            return ProvisionOutcome::Deferred(format!(
                                "upstream for host '{}' exists but is not visible",
                                spec.host
                            ));
                        }
                    }
                }
                Err(e) => return ProvisionOutcome::Deferred(format!("create upstream: {e}")),
            }
        }
        Err(e) => return ProvisionOutcome::Deferred(format!("create upstream: {e}")),
    };
    registry.set_actual_alias(&spec.alias, &upstream.alias);

    let mut errors = Vec::new();
    let (chat_prefix, chat_query) = chat_route(&spec.api_path);
    if let Err(e) = ensure_route(
        client,
        ctx,
        upstream.id,
        &chat_prefix,
        vec![HttpMethod::Post],
        chat_query,
    )
    .await
    {
        errors.push(e);
    }
    if let Some(sk) = spec.storage_kind {
        let (prefix, allow) = match sk {
            StorageKind::Openai => ("/v1", vec![]),
            StorageKind::Azure => ("/openai", vec!["api-version".to_owned()]),
        };
        for (path, methods) in [
            (
                format!("{prefix}/files"),
                vec![HttpMethod::Post, HttpMethod::Delete],
            ),
            (
                format!("{prefix}/vector_stores"),
                vec![HttpMethod::Post, HttpMethod::Get, HttpMethod::Delete],
            ),
        ] {
            if let Err(e) =
                ensure_route(client, ctx, upstream.id, &path, methods, allow.clone()).await
            {
                // A missing RAG route only degrades RAG.
                warn!(provider = %spec.provider_id, error = %e, "RAG route not provisioned");
            }
        }
    }
    if spec.kind == ProviderKind::AnthropicMessages
        && let Err(e) = ensure_route(
            client,
            ctx,
            upstream.id,
            "/v1/files",
            vec![HttpMethod::Post, HttpMethod::Delete],
            vec![],
        )
        .await
    {
        warn!(provider = %spec.provider_id, error = %e, "Anthropic files route not provisioned");
    }
    if errors.is_empty() {
        info!(provider = %spec.provider_id, alias = %upstream.alias, "OAGW upstream and routes provisioned");
        ProvisionOutcome::Done
    } else {
        ProvisionOutcome::Deferred(errors.join("; "))
    }
}

/// Provision all specs once; returns the deferred ones.
pub async fn provision_all(
    client: &LlmClient,
    registry: &ProviderRegistry,
    ctx: &SecurityContext,
    specs: Vec<UpstreamSpec>,
) -> Vec<UpstreamSpec> {
    let mut deferred = Vec::new();
    for spec in specs {
        match provision(client, registry, ctx, &spec).await {
            ProvisionOutcome::Done => {}
            ProvisionOutcome::Deferred(reason) => {
                warn!(provider = %spec.key, %reason, "provider provisioning deferred");
                deferred.push(spec);
            }
        }
    }
    deferred
}

/// Background reconcile loop for deferred providers.
#[allow(clippy::cognitive_complexity)]
pub async fn reconcile_loop(
    client: Arc<LlmClient>,
    registry: Arc<ProviderRegistry>,
    ctx: SecurityContext,
    mut pending: Vec<UpstreamSpec>,
    cancel: CancellationToken,
) {
    let started = tokio::time::Instant::now();
    let mut delay = Duration::from_secs(2);
    let mut warned = false;
    while !pending.is_empty() {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
        pending = provision_all(&client, &registry, &ctx, pending).await;
        delay = (delay * 2).min(Duration::from_secs(60));
        if !warned && !pending.is_empty() && started.elapsed() >= Duration::from_secs(120) {
            warned = true;
            let names: Vec<&str> = pending.iter().map(|s| s.key.as_str()).collect();
            warn!(providers = ?names, "providers still not provisioned after 2 minutes");
        }
    }
    info!("all deferred providers provisioned");
}

#[cfg(test)]
mod tests {
    use super::chat_route;

    #[test]
    fn chat_route_prefix_and_query() {
        assert_eq!(
            chat_route("/v1/responses"),
            ("/v1/responses".to_owned(), vec![])
        );
        assert_eq!(
            chat_route("/openai/deployments/{model}/chat/completions?api-version=2024-10-21"),
            (
                "/openai/deployments".to_owned(),
                vec!["api-version".to_owned()]
            )
        );
    }
}
