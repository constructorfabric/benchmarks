//! OAGW upstream / route provisioning for the configured providers
//! (ADR-0005). Upstreams are in-memory in OAGW, so they are (re)created on
//! every gear start.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HeadersConfig, HttpMatch, HttpMethod, ListQuery,
    MatchRules, PassthroughMode, PathSuffixMode, RequestHeaderRules, Scheme, Server, ServiceGatewayClientV1,
    ServiceGatewayError, SharingMode, HTTP_PROTOCOL_ID,
};
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::config::{ProviderEntry, StorageKind};

/// One upstream to provision (a provider entry or a tenant override).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamPlan {
    pub provider_id: String,
    /// `None` = the S2S (root) tenant, `Some` = a tenant override.
    pub tenant: Option<Uuid>,
    pub alias: String,
    pub host: String,
    pub port: u16,
    pub use_http: bool,
    pub auth_plugin_type: Option<String>,
    pub auth_config: BTreeMap<String, String>,
    pub routes: Vec<RoutePlan>,
}

/// One route of an upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutePlan {
    pub methods: Vec<&'static str>,
    pub path: String,
    pub query_allowlist: Vec<String>,
    /// RAG routes are best-effort.
    pub required: bool,
}

/// Split `api_path` into the route prefix and its query allowlist.
#[must_use]
pub fn chat_route(api_path: &str) -> RoutePlan {
    let (path, query) = api_path.split_once('?').unwrap_or((api_path, ""));
    let prefix = path.split("{model}").next().unwrap_or(path);
    let prefix = if prefix.len() > 1 { prefix.trim_end_matches('/') } else { prefix };
    let query_allowlist = query
        .split('&')
        .filter_map(|kv| kv.split('=').next())
        .filter(|k| !k.is_empty())
        .map(str::to_owned)
        .collect();
    RoutePlan { methods: vec!["POST"], path: prefix.to_owned(), query_allowlist, required: true }
}

/// RAG routes of a storage kind.
#[must_use]
pub fn rag_routes(kind: StorageKind) -> Vec<RoutePlan> {
    let (prefix, q) = match kind {
        StorageKind::Openai => ("/v1", Vec::new()),
        StorageKind::Azure => ("/openai", vec!["api-version".to_owned()]),
    };
    vec![
        RoutePlan { methods: vec!["POST", "DELETE"], path: format!("{prefix}/files"), query_allowlist: q.clone(), required: false },
        RoutePlan {
            methods: vec!["POST", "GET", "DELETE"],
            path: format!("{prefix}/vector_stores"),
            query_allowlist: q,
            required: false,
        },
    ]
}

/// Build the provisioning plan of every provider entry and tenant override.
#[must_use]
pub fn plan(providers: &BTreeMap<String, ProviderEntry>) -> Vec<UpstreamPlan> {
    let mut out = Vec::new();
    for (id, e) in providers {
        let mut routes = vec![chat_route(&e.api_path)];
        if let Some(kind) = e.storage_kind {
            routes.extend(rag_routes(kind));
        }
        let port = e.effective_port();
        out.push(UpstreamPlan {
            provider_id: id.clone(),
            tenant: None,
            alias: e.upstream_alias.clone().unwrap_or_else(|| e.host.clone()),
            host: e.host.clone(),
            port,
            use_http: e.use_http,
            auth_plugin_type: e.auth_plugin_type.clone(),
            auth_config: e.auth_config.clone(),
            routes: routes.clone(),
        });
        for (tenant, ov) in &e.tenant_overrides {
            let Ok(tenant) = Uuid::parse_str(tenant) else {
                continue;
            };
            let host = ov.host.clone().unwrap_or_else(|| e.host.clone());
            out.push(UpstreamPlan {
                provider_id: id.clone(),
                tenant: Some(tenant),
                alias: ov.upstream_alias.clone().unwrap_or_else(|| host.clone()),
                host,
                port,
                use_http: e.use_http,
                auth_plugin_type: ov.auth_plugin_type.clone().or_else(|| e.auth_plugin_type.clone()),
                auth_config: ov.auth_config.clone().unwrap_or_else(|| e.auth_config.clone()),
                routes: routes.clone(),
            });
        }
    }
    out
}

fn method(m: &str) -> HttpMethod {
    match m {
        "GET" => HttpMethod::Get,
        "DELETE" => HttpMethod::Delete,
        "PUT" => HttpMethod::Put,
        "PATCH" => HttpMethod::Patch,
        _ => HttpMethod::Post,
    }
}

fn build_upstream(p: &UpstreamPlan) -> CreateUpstreamRequest {
    let mut b = CreateUpstreamRequest::builder(
        Server {
            endpoints: vec![Endpoint {
                scheme: if p.use_http { Scheme::Http } else { Scheme::Https },
                host: p.host.clone(),
                port: p.port,
            }],
        },
        HTTP_PROTOCOL_ID,
    )
    .alias(p.alias.clone())
    .headers(HeadersConfig {
        request: Some(RequestHeaderRules {
            passthrough: PassthroughMode::Allowlist,
            passthrough_allowlist: vec!["accept".to_owned(), "anthropic-version".to_owned(), "anthropic-beta".to_owned()],
            ..RequestHeaderRules::default()
        }),
        response: None,
    })
    .tags(vec!["mini-chat".to_owned(), format!("provider:{}", p.provider_id)]);
    if let Some(t) = &p.auth_plugin_type {
        b = b.auth(AuthConfig {
            plugin_type: t.clone(),
            sharing: SharingMode::Inherit,
            config: Some(p.auth_config.iter().map(|(k, v)| (k.clone(), v.clone())).collect::<HashMap<_, _>>()),
        });
    }
    b.build()
}

/// Outcome classification of an upstream create call.
enum CreateOutcome {
    Ready(Uuid),
    Deferred(String),
    Fatal(String),
}

async fn find_by_alias(gw: &dyn ServiceGatewayClientV1, ctx: &SecurityContext, alias: &str) -> Option<Uuid> {
    let want = alias.to_ascii_lowercase().trim_end_matches('.').to_owned();
    let mut skip = 0;
    loop {
        let page = gw.list_upstreams(ctx.clone(), &ListQuery { top: 100, skip }).await.ok()?;
        if let Some(u) = page.iter().find(|u| u.alias == want) {
            return Some(u.id);
        }
        if page.len() < 100 {
            return None;
        }
        skip += 100;
    }
}

async fn create_upstream(gw: &dyn ServiceGatewayClientV1, ctx: &SecurityContext, p: &UpstreamPlan) -> CreateOutcome {
    match gw.create_upstream(ctx.clone(), build_upstream(p)).await {
        Ok(u) => CreateOutcome::Ready(u.id),
        Err(e) => classify(gw, ctx, p, e).await,
    }
}

async fn classify(gw: &dyn ServiceGatewayClientV1, ctx: &SecurityContext, p: &UpstreamPlan, e: CanonicalError) -> CreateOutcome {
    let text = e.to_string();
    match ServiceGatewayError::from(e) {
        ServiceGatewayError::AlreadyExists { .. } => match find_by_alias(gw, ctx, &p.alias).await {
            Some(id) => CreateOutcome::Ready(id),
            None => CreateOutcome::Deferred(format!("upstream exists but could not be listed: {text}")),
        },
        ServiceGatewayError::Validation { .. } | ServiceGatewayError::InvalidTargetHost { .. } => CreateOutcome::Fatal(text),
        _ => CreateOutcome::Deferred(text),
    }
}

#[allow(clippy::cognitive_complexity, reason = "per-route create with outcome logging")]
async fn create_routes(gw: &dyn ServiceGatewayClientV1, ctx: &SecurityContext, upstream: Uuid, p: &UpstreamPlan) {
    for r in &p.routes {
        let req = CreateRouteRequest::builder(
            upstream,
            MatchRules {
                http: Some(HttpMatch {
                    methods: r.methods.iter().map(|m| method(m)).collect(),
                    path: r.path.clone(),
                    query_allowlist: r.query_allowlist.clone(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
        )
        .tags(vec!["mini-chat".to_owned()])
        .build();
        match gw.create_route(ctx.clone(), req).await {
            Ok(_) => {}
            Err(e) if matches!(ServiceGatewayError::from(e.clone()), ServiceGatewayError::AlreadyExists { .. }) => {}
            Err(e) if r.required => {
                tracing::error!(provider = %p.provider_id, path = %r.path, error = %e, "chat route provisioning failed");
            }
            Err(e) => {
                tracing::warn!(provider = %p.provider_id, path = %r.path, error = %e, "RAG route provisioning failed (RAG degraded)");
            }
        }
    }
}

fn ctx_for(base: &SecurityContext, tenant: Option<Uuid>) -> SecurityContext {
    let Some(t) = tenant else {
        return base.clone();
    };
    SecurityContext::builder()
        .subject_id(base.subject_id())
        .subject_tenant_id(t)
        .token_scopes(vec!["*".to_owned()])
        .build()
        .unwrap_or_else(|_| base.clone())
}

/// Provision every upstream. Fails only for misconfiguration
/// (`InvalidArgument`); deferred upstreams are retried in the background.
///
/// # Errors
/// A message naming the misconfigured provider.
pub async fn provision(
    gw: Arc<dyn ServiceGatewayClientV1>,
    s2s: &SecurityContext,
    providers: &BTreeMap<String, ProviderEntry>,
    cancel: CancellationToken,
) -> Result<(), String> {
    for p in plan(providers) {
        let ctx = ctx_for(s2s, p.tenant);
        match create_upstream(gw.as_ref(), &ctx, &p).await {
            CreateOutcome::Ready(id) => {
                create_routes(gw.as_ref(), &ctx, id, &p).await;
                tracing::info!(provider = %p.provider_id, alias = %p.alias, "OAGW upstream provisioned");
            }
            CreateOutcome::Fatal(e) => {
                return Err(format!("provider `{}` upstream `{}` is misconfigured: {e}", p.provider_id, p.alias));
            }
            CreateOutcome::Deferred(e) => {
                tracing::warn!(provider = %p.provider_id, alias = %p.alias, error = %e, "OAGW upstream provisioning deferred");
                let gw = gw.clone();
                let cancel = cancel.clone();
                tokio::spawn(async move {
                    let started = Instant::now();
                    let mut delay = Duration::from_secs(2);
                    let mut warned = false;
                    loop {
                        tokio::select! {
                            () = cancel.cancelled() => return,
                            () = tokio::time::sleep(delay) => {}
                        }
                        match create_upstream(gw.as_ref(), &ctx, &p).await {
                            CreateOutcome::Ready(id) => {
                                create_routes(gw.as_ref(), &ctx, id, &p).await;
                                tracing::info!(provider = %p.provider_id, alias = %p.alias, "deferred OAGW upstream provisioned");
                                return;
                            }
                            CreateOutcome::Fatal(e) => {
                                tracing::error!(provider = %p.provider_id, error = %e, "OAGW upstream provisioning failed permanently");
                                return;
                            }
                            CreateOutcome::Deferred(e) => {
                                if !warned && started.elapsed() > Duration::from_secs(120) {
                                    warned = true;
                                    tracing::warn!(provider = %p.provider_id, error = %e, "OAGW upstream still not provisioned after 2 minutes");
                                }
                            }
                        }
                        delay = std::cmp::min(delay * 2, Duration::from_secs(60));
                    }
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_route_from_api_path() {
        let r = chat_route("/v1/responses");
        assert_eq!(r.path, "/v1/responses");
        assert!(r.query_allowlist.is_empty());
        let r = chat_route("/openai/deployments/{model}/chat/completions?api-version=2024-10-21");
        assert_eq!(r.path, "/openai/deployments");
        assert_eq!(r.query_allowlist, vec!["api-version".to_owned()]);
    }

    #[test]
    fn rag_prefixes() {
        assert_eq!(rag_routes(StorageKind::Openai)[0].path, "/v1/files");
        let az = rag_routes(StorageKind::Azure);
        assert_eq!(az[1].path, "/openai/vector_stores");
        assert_eq!(az[1].query_allowlist, vec!["api-version".to_owned()]);
    }
}
