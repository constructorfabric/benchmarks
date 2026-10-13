//! OAGW upstream and route provisioning for every provider entry and tenant
//! override (DESIGN §3.2 "OAGW provisioning", ADR-0005).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HTTP_PROTOCOL_ID, HttpMatch, HttpMethod, ListQuery,
    MatchRules, PathSuffixMode, Scheme, Server, ServiceGatewayClientV1, SharingMode,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::{MiniChatConfig, StorageKind};
use crate::infra::llm::ProviderResolver;

/// One upstream to provision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionTarget {
    pub label: String,
    pub alias: String,
    pub host: String,
    pub port: u16,
    pub use_http: bool,
    pub auth_plugin_type: Option<String>,
    pub auth_config: HashMap<String, String>,
    pub chat_route: (String, Vec<String>),
    pub rag_route: Option<(String, Vec<String>)>,
}

fn is_ip(host: &str) -> bool {
    host.trim_matches(|c| c == '[' || c == ']').parse::<std::net::IpAddr>().is_ok()
}

/// Route prefix and query allowlist derived from an `api_path`.
#[must_use]
pub fn chat_route_of(api_path: &str) -> (String, Vec<String>) {
    let (path, query) = api_path.split_once('?').unwrap_or((api_path, ""));
    let path = match path.find("{model}") {
        Some(i) => path[..i].trim_end_matches('/').to_owned(),
        None => path.to_owned(),
    };
    let path = if path.is_empty() { "/".to_owned() } else { path };
    let mut allow: Vec<String> = query
        .split('&')
        .filter_map(|kv| kv.split('=').next())
        .filter(|k| !k.is_empty())
        .map(str::to_owned)
        .collect();
    if !allow.iter().any(|k| k == "api-version") {
        allow.push("api-version".to_owned());
    }
    (path, allow)
}

/// Every upstream required by the configuration.
#[must_use]
pub fn targets(cfg: &MiniChatConfig) -> Vec<ProvisionTarget> {
    let mut out = Vec::new();
    let mut ids: Vec<&String> = cfg.providers.keys().collect();
    ids.sort();
    for id in ids {
        let p = &cfg.providers[id];
        let rag_route = p.storage_kind.map(|k| match k {
            StorageKind::Openai => ("/v1".to_owned(), vec!["api-version".to_owned()]),
            StorageKind::Azure => ("/openai".to_owned(), vec!["api-version".to_owned()]),
        });
        let base = ProvisionTarget {
            label: id.clone(),
            alias: p.alias(),
            host: p.host.clone(),
            port: p.effective_port(),
            use_http: p.use_http,
            auth_plugin_type: p.auth_plugin_type.clone(),
            auth_config: p.auth_config.clone(),
            chat_route: chat_route_of(&p.api_path),
            rag_route,
        };
        let mut tenants: Vec<&String> = p.tenant_overrides.keys().collect();
        tenants.sort();
        for t in tenants {
            let o = &p.tenant_overrides[t];
            let host = o.host.clone().unwrap_or_else(|| base.host.clone());
            out.push(ProvisionTarget {
                label: format!("{id}/{t}"),
                alias: o.upstream_alias.clone().or_else(|| o.host.clone()).unwrap_or_else(|| base.alias.clone()),
                host,
                auth_plugin_type: o.auth_plugin_type.clone().or_else(|| base.auth_plugin_type.clone()),
                auth_config: o.auth_config.clone().unwrap_or_else(|| base.auth_config.clone()),
                ..base.clone()
            });
        }
        out.push(base);
    }
    out
}

/// Provisioning failure.
#[derive(Debug, Clone)]
pub struct ProvisionError {
    pub message: String,
}

fn is_conflict(e: &CanonicalError) -> bool {
    matches!(e, CanonicalError::AlreadyExists { .. })
}

async fn find_upstream(oagw: &dyn ServiceGatewayClientV1, ctx: &SecurityContext, alias: &str) -> Option<Uuid> {
    let mut skip = 0;
    loop {
        let page = oagw.list_upstreams(ctx.clone(), &ListQuery { top: 100, skip }).await.ok()?;
        if let Some(u) = page.iter().find(|u| u.alias.eq_ignore_ascii_case(alias)) {
            return Some(u.id);
        }
        if page.len() < 100 {
            return None;
        }
        skip += 100;
    }
}

async fn create_route(
    oagw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    upstream: Uuid,
    path: &str,
    methods: Vec<HttpMethod>,
    allow: &[String],
) -> Result<(), ProvisionError> {
    let rules = MatchRules {
        http: Some(HttpMatch {
            methods,
            path: path.to_owned(),
            query_allowlist: allow.to_vec(),
            path_suffix_mode: PathSuffixMode::Append,
        }),
        grpc: None,
    };
    let req = CreateRouteRequest::builder(upstream, rules).tags(vec!["mini-chat".to_owned()]).build();
    match oagw.create_route(ctx.clone(), req).await {
        Ok(_) => Ok(()),
        Err(e) if is_conflict(&e) => Ok(()),
        Err(e) => Err(ProvisionError { message: format!("create route {path}: {e}") }),
    }
}

/// Create (or reuse) the upstream and its routes.
///
/// # Errors
/// Any OAGW failure other than "already exists".
pub async fn provision_one(
    oagw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    resolver: &ProviderResolver,
    t: &ProvisionTarget,
) -> Result<(), ProvisionError> {
    let endpoint = Endpoint { scheme: if t.use_http { Scheme::Http } else { Scheme::Https }, host: t.host.clone(), port: t.port };
    let auth = t.auth_plugin_type.as_ref().filter(|s| !s.is_empty()).map(|pt| AuthConfig {
        plugin_type: pt.clone(),
        sharing: SharingMode::Inherit,
        config: Some(t.auth_config.clone()),
    });
    let build = |with_alias: bool| {
        let mut b = CreateUpstreamRequest::builder(Server { endpoints: vec![endpoint.clone()] }, HTTP_PROTOCOL_ID)
            .tags(vec!["mini-chat".to_owned(), format!("mini-chat-provider:{}", t.label)]);
        if with_alias {
            b = b.alias(t.alias.clone());
        }
        if let Some(a) = auth.clone() {
            b = b.auth(a);
        }
        b.build()
    };
    let with_alias = is_ip(&t.host) || t.alias != endpoint.alias_contribution();
    let mut actual_alias = t.alias.clone();
    let upstream_id = match oagw.create_upstream(ctx.clone(), build(with_alias)).await {
        Ok(u) => {
            actual_alias.clone_from(&u.alias);
            u.id
        }
        Err(e) if is_conflict(&e) => find_upstream(oagw, ctx, &t.alias)
            .await
            .or(None)
            .ok_or_else(|| ProvisionError { message: format!("upstream {} exists but was not found", t.alias) })?,
        Err(e) if with_alias && matches!(e, CanonicalError::InvalidArgument { .. }) && !is_ip(&t.host) => {
            // Hostname upstream whose derived alias differs from the configured one.
            match oagw.create_upstream(ctx.clone(), build(false)).await {
                Ok(u) => {
                    actual_alias.clone_from(&u.alias);
                    u.id
                }
                Err(e2) if is_conflict(&e2) => {
                    let derived = endpoint.alias_contribution();
                    actual_alias = derived.clone();
                    find_upstream(oagw, ctx, &derived)
                        .await
                        .ok_or_else(|| ProvisionError { message: format!("upstream {derived} exists but was not found") })?
                }
                Err(e2) => return Err(ProvisionError { message: format!("create upstream {}: {e2}", t.alias) }),
            }
        }
        Err(e) => return Err(ProvisionError { message: format!("create upstream {}: {e}", t.alias) }),
    };
    resolver.remap_alias(&t.alias, &actual_alias);
    create_route(oagw, ctx, upstream_id, &t.chat_route.0, vec![HttpMethod::Post], &t.chat_route.1).await?;
    if let Some((path, allow)) = &t.rag_route
        && let Err(e) = create_route(oagw, ctx, upstream_id, path, vec![HttpMethod::Post, HttpMethod::Get, HttpMethod::Delete], allow).await
    {
        tracing::warn!(error = %e.message, "RAG route not provisioned; file operations are degraded");
    }
    Ok(())
}

/// Provision everything; return the targets that still need a retry.
pub async fn provision_all(
    oagw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    resolver: &ProviderResolver,
    all: &[ProvisionTarget],
) -> Vec<ProvisionTarget> {
    let mut pending = Vec::new();
    for t in all {
        match provision_one(oagw, ctx, resolver, t).await {
            Ok(()) => tracing::info!(provider = %t.label, alias = %t.alias, "OAGW upstream provisioned"),
            Err(e) => {
                tracing::warn!(provider = %t.label, error = %e.message, "OAGW provisioning deferred");
                pending.push(t.clone());
            }
        }
    }
    pending
}

/// Retry deferred targets (2 s doubling to 60 s) until success or stop.
#[must_use]
pub fn spawn_reconcile(
    oagw: Arc<dyn ServiceGatewayClientV1>,
    ctx: SecurityContext,
    resolver: Arc<ProviderResolver>,
    mut pending: Vec<ProvisionTarget>,
    cancel: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let began = Instant::now();
        let mut delay = Duration::from_secs(2);
        let mut warned = false;
        while !pending.is_empty() {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
            pending = provision_all(oagw.as_ref(), &ctx, &resolver, &pending).await;
            delay = (delay * 2).min(Duration::from_secs(60));
            if !warned && began.elapsed() > Duration::from_secs(120) && !pending.is_empty() {
                warned = true;
                let names: Vec<&str> = pending.iter().map(|t| t.label.as_str()).collect();
                tracing::warn!(providers = ?names, "OAGW providers still not provisioned");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_route_derivation() {
        assert_eq!(chat_route_of("/v1/responses"), ("/v1/responses".to_owned(), vec!["api-version".to_owned()]));
        let (p, a) = chat_route_of("/openai/deployments/{model}/chat/completions?api-version=2024");
        assert_eq!(p, "/openai/deployments");
        assert_eq!(a, vec!["api-version".to_owned()]);
    }

    #[test]
    fn targets_include_overrides() {
        let mut cfg = MiniChatConfig::default();
        let p = cfg.providers.get_mut("openai").expect("openai");
        p.tenant_overrides.insert(
            "t1".into(),
            crate::config::TenantOverride { host: Some("h2".into()), ..Default::default() },
        );
        cfg.fill_aliases();
        let t = targets(&cfg);
        assert_eq!(t.len(), 2);
        assert!(t.iter().any(|x| x.alias == "h2" && x.host == "h2"));
        assert!(t.iter().any(|x| x.alias == "api.openai.com" && x.rag_route.is_some()));
    }
}
