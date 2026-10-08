//! OAGW upstream and route provisioning at gear start (ADR-0005).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HTTP_PROTOCOL_ID, HttpMatch, HttpMethod,
    ListQuery, MatchRules, PathSuffixMode, Scheme, Server, ServiceGatewayClientV1, SharingMode,
};
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::CanonicalError;

use crate::config::{MiniChatConfig, ProviderEntry, StorageKind};
use crate::infra::llm::transport::{AliasMap, S2sContext};

/// One upstream to provision (provider entry or tenant override).
#[derive(Debug, Clone)]
pub struct UpstreamTarget {
    pub provider_id: String,
    pub host: String,
    pub port: u16,
    pub use_http: bool,
    pub alias: String,
    pub auth_plugin_type: Option<String>,
    pub auth_config: Option<std::collections::BTreeMap<String, String>>,
    pub api_path: String,
    pub storage_kind: Option<StorageKind>,
}

/// Expands provider entries and tenant overrides into upstream targets.
#[must_use]
pub fn targets(cfg: &MiniChatConfig) -> Vec<UpstreamTarget> {
    let mut out = Vec::new();
    for (id, p) in &cfg.providers {
        out.push(target(id, p, &p.host, &p.alias(), p.auth_plugin_type.clone(), p.auth_config.clone()));
        for o in p.tenant_overrides.values() {
            let host = o.host.clone().unwrap_or_else(|| p.host.clone());
            let alias = o
                .upstream_alias
                .clone()
                .filter(|a| !a.is_empty())
                .unwrap_or_else(|| host.clone());
            out.push(target(
                id,
                p,
                &host,
                &alias,
                o.auth_plugin_type.clone().or_else(|| p.auth_plugin_type.clone()),
                o.auth_config.clone().or_else(|| p.auth_config.clone()),
            ));
        }
    }
    out
}

fn target(
    id: &str,
    p: &ProviderEntry,
    host: &str,
    alias: &str,
    auth_plugin_type: Option<String>,
    auth_config: Option<std::collections::BTreeMap<String, String>>,
) -> UpstreamTarget {
    UpstreamTarget {
        provider_id: id.to_owned(),
        host: host.to_owned(),
        port: p.effective_port(),
        use_http: p.use_http,
        alias: alias.to_owned(),
        auth_plugin_type,
        auth_config,
        api_path: p.api_path.clone(),
        storage_kind: p.storage_kind,
    }
}

/// Route path prefix and query allowlist of an `api_path`.
#[must_use]
pub fn chat_route(api_path: &str) -> (String, Vec<String>) {
    let (path, query) = api_path.split_once('?').unwrap_or((api_path, ""));
    let prefix = path.split("{model}").next().unwrap_or(path).trim_end_matches('/');
    let prefix = if prefix.is_empty() { "/" } else { prefix };
    let allow = query
        .split('&')
        .filter_map(|kv| kv.split('=').next())
        .filter(|k| !k.is_empty())
        .map(str::to_owned)
        .collect();
    (prefix.to_owned(), allow)
}

fn is_ip(host: &str) -> bool {
    host.trim_matches(['[', ']']).parse::<IpAddr>().is_ok()
}

async fn ensure_upstream(
    gw: &dyn ServiceGatewayClientV1,
    ctx: &toolkit_security::SecurityContext,
    t: &UpstreamTarget,
) -> Result<(uuid::Uuid, String), CanonicalError> {
    let server = Server {
        endpoints: vec![Endpoint {
            scheme: if t.use_http { Scheme::Http } else { Scheme::Https },
            host: t.host.clone(),
            port: t.port,
        }],
    };
    let auth = t.auth_plugin_type.as_ref().map(|pt| AuthConfig {
        plugin_type: pt.clone(),
        sharing: SharingMode::Private,
        config: t
            .auth_config
            .as_ref()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<HashMap<_, _>>()),
    });
    let build = |with_alias: bool| {
        let mut b = CreateUpstreamRequest::builder(server.clone(), HTTP_PROTOCOL_ID);
        if with_alias {
            b = b.alias(t.alias.clone());
        }
        if let Some(a) = auth.clone() {
            b = b.auth(a);
        }
        b.build()
    };
    let first = gw.create_upstream(ctx.clone(), build(true)).await;
    let result = match first {
        Err(e) if !is_ip(&t.host) && e.to_string().contains("auto-derived") => {
            gw.create_upstream(ctx.clone(), build(false)).await
        }
        other => other,
    };
    match result {
        Ok(u) => Ok((u.id, u.alias)),
        Err(e) => {
            // Reuse an existing upstream registered under the alias.
            let existing = gw
                .list_upstreams(ctx.clone(), &ListQuery { top: 500, skip: 0 })
                .await
                .unwrap_or_default();
            let lower = t.alias.to_ascii_lowercase();
            if let Some(u) = existing.into_iter().find(|u| {
                u.alias == lower
                    || u.server
                        .endpoints
                        .iter()
                        .any(|ep| ep.host.eq_ignore_ascii_case(&t.host) && ep.port == t.port)
            }) {
                return Ok((u.id, u.alias));
            }
            Err(e)
        }
    }
}

async fn ensure_route(
    gw: &dyn ServiceGatewayClientV1,
    ctx: &toolkit_security::SecurityContext,
    upstream_id: uuid::Uuid,
    path: String,
    methods: Vec<HttpMethod>,
    query_allowlist: Vec<String>,
) {
    let req = CreateRouteRequest::builder(
        upstream_id,
        MatchRules {
            http: Some(HttpMatch {
                methods,
                path: path.clone(),
                query_allowlist,
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        },
    )
    .build();
    if let Err(e) = gw.create_route(ctx.clone(), req).await {
        tracing::debug!(error = %e, path = %path, "route not created (may already exist)");
    }
}

/// Provisions one target; returns an error when it must be retried.
///
/// # Errors
/// Upstream creation failure.
pub async fn provision_target(
    gw: &dyn ServiceGatewayClientV1,
    ctx: &toolkit_security::SecurityContext,
    t: &UpstreamTarget,
    aliases: &AliasMap,
) -> Result<(), CanonicalError> {
    let (upstream_id, actual_alias) = ensure_upstream(gw, ctx, t).await?;
    aliases.insert(&t.alias, &actual_alias);
    let (chat_path, chat_allow) = chat_route(&t.api_path);
    ensure_route(gw, ctx, upstream_id, chat_path, vec![HttpMethod::Post], chat_allow).await;
    if let Some(kind) = t.storage_kind {
        let (prefix, allow) = match kind {
            StorageKind::Openai => ("/v1", Vec::new()),
            StorageKind::Azure => ("/openai", vec!["api-version".to_owned()]),
        };
        ensure_route(
            gw,
            ctx,
            upstream_id,
            format!("{prefix}/files"),
            vec![HttpMethod::Post, HttpMethod::Delete, HttpMethod::Get],
            allow.clone(),
        )
        .await;
        ensure_route(
            gw,
            ctx,
            upstream_id,
            format!("{prefix}/vector_stores"),
            vec![HttpMethod::Post, HttpMethod::Get, HttpMethod::Delete],
            allow,
        )
        .await;
    }
    tracing::info!(provider = %t.provider_id, alias = %actual_alias, "provider upstream provisioned");
    Ok(())
}

/// Provisions every target; failed ones are retried in the background
/// (2 s, doubling up to 60 s) until the gear stops.
pub async fn provision_all(
    gw: Arc<dyn ServiceGatewayClientV1>,
    s2s: Arc<S2sContext>,
    cfg: &MiniChatConfig,
    aliases: Arc<AliasMap>,
    cancel: CancellationToken,
) {
    let ctx = s2s.get();
    let mut pending = Vec::new();
    for t in targets(cfg) {
        if let Err(e) = provision_target(gw.as_ref(), &ctx, &t, &aliases).await {
            tracing::warn!(provider = %t.provider_id, error = %e, "provider provisioning deferred");
            pending.push(t);
        }
    }
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
            delay = (delay * 2).min(Duration::from_secs(60));
            let ctx = s2s.get();
            let mut still = Vec::new();
            for t in pending {
                if provision_target(gw.as_ref(), &ctx, &t, &aliases).await.is_err() {
                    still.push(t);
                }
            }
            pending = still;
            if !warned && !pending.is_empty() && started.elapsed() >= Duration::from_secs(120) {
                warned = true;
                let ids: Vec<&str> = pending.iter().map(|t| t.provider_id.as_str()).collect();
                tracing::warn!(providers = ?ids, "providers still pending OAGW provisioning");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::chat_route;

    #[test]
    fn chat_route_prefix_and_allowlist() {
        assert_eq!(chat_route("/v1/responses"), ("/v1/responses".into(), vec![]));
        assert_eq!(
            chat_route("/openai/deployments/{model}/chat/completions?api-version=2024"),
            ("/openai/deployments".into(), vec!["api-version".to_owned()])
        );
    }
}
