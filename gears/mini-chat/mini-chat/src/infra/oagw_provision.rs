//! OAGW provisioning at gear start (ADR-0005): one upstream per provider entry
//! (and tenant override) plus the chat route and the RAG routes.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HTTP_PROTOCOL_ID, HeadersConfig,
    HttpMatch, HttpMethod, ListQuery, MatchRules, PathSuffixMode, RequestHeaderRules, Scheme, Server,
    ServiceGatewayClientV1, SharingMode,
};
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use tokio_util::sync::CancellationToken;

use crate::config::{ProviderEntry, ProviderKind, StorageKind};
use crate::infra::llm::resolver::ProviderResolver;

/// One upstream to register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamSpec {
    pub provider_id: String,
    pub alias: String,
    pub host: String,
    pub port: u16,
    pub use_http: bool,
    pub auth_plugin_type: Option<String>,
    pub auth_config: BTreeMap<String, String>,
    pub routes: Vec<RouteSpec>,
    pub anthropic: bool,
}

/// One route to register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSpec {
    pub method: HttpMethod,
    pub path: String,
    pub query_allowlist: Vec<String>,
}

fn route(method: HttpMethod, path: &str, query: &[String]) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_owned(),
        query_allowlist: query.to_vec(),
    }
}

/// Routes of a provider entry.
#[must_use]
pub fn routes_for(entry: &ProviderEntry) -> Vec<RouteSpec> {
    let (path, query) = match entry.api_path.split_once('?') {
        Some((p, q)) => (p.to_owned(), q.split('&').filter_map(|kv| kv.split('=').next()).map(ToOwned::to_owned).collect()),
        None => (entry.api_path.clone(), Vec::new()),
    };
    let chat_prefix = path.split("{model}").next().unwrap_or(&path).trim_end_matches('/').to_owned();
    let chat_prefix = if chat_prefix.is_empty() { "/".to_owned() } else { chat_prefix };
    let mut routes = vec![route(HttpMethod::Post, &chat_prefix, &query)];
    if let Some(kind) = entry.storage_kind {
        let (prefix, q) = match kind {
            StorageKind::Openai => ("/v1", Vec::new()),
            StorageKind::Azure => ("/openai", vec!["api-version".to_owned()]),
        };
        let files = format!("{prefix}/files");
        let vs = format!("{prefix}/vector_stores");
        for r in [
            route(HttpMethod::Post, &files, &q),
            route(HttpMethod::Delete, &files, &q),
            route(HttpMethod::Post, &vs, &q),
            route(HttpMethod::Get, &vs, &q),
            route(HttpMethod::Delete, &vs, &q),
        ] {
            if !routes.iter().any(|x| x.method == r.method && x.path == r.path) {
                routes.push(r);
            }
        }
    }
    if entry.kind == ProviderKind::AnthropicMessages {
        for r in [route(HttpMethod::Post, "/v1/files", &[]), route(HttpMethod::Delete, "/v1/files", &[])] {
            if !routes.iter().any(|x| x.method == r.method && x.path == r.path) {
                routes.push(r);
            }
        }
    }
    routes
}

/// Upstream specs for every provider entry and tenant override.
#[must_use]
pub fn upstream_specs(providers: &BTreeMap<String, ProviderEntry>) -> Vec<UpstreamSpec> {
    let mut out = Vec::new();
    for (id, e) in providers {
        let routes = routes_for(e);
        out.push(UpstreamSpec {
            provider_id: id.clone(),
            alias: e.alias(),
            host: e.host.clone(),
            port: e.effective_port(),
            use_http: e.use_http,
            auth_plugin_type: e.auth_plugin_type.clone(),
            auth_config: e.auth_config.clone(),
            routes: routes.clone(),
            anthropic: e.kind == ProviderKind::AnthropicMessages,
        });
        for o in e.tenant_overrides.values() {
            let host = o.host.clone().unwrap_or_else(|| e.host.clone());
            let alias = o.upstream_alias.clone().unwrap_or_else(|| host.clone());
            if out.iter().any(|u: &UpstreamSpec| u.alias.eq_ignore_ascii_case(&alias)) {
                continue;
            }
            out.push(UpstreamSpec {
                provider_id: id.clone(),
                alias,
                host,
                port: e.effective_port(),
                use_http: e.use_http,
                auth_plugin_type: o.auth_plugin_type.clone().or_else(|| e.auth_plugin_type.clone()),
                auth_config: o.auth_config.clone().unwrap_or_else(|| e.auth_config.clone()),
                routes: routes.clone(),
                anthropic: e.kind == ProviderKind::AnthropicMessages,
            });
        }
    }
    out
}

/// Provisioning failure classification.
#[derive(Debug)]
pub enum ProvisionError {
    /// Retry later (e.g. credstore secret not readable yet).
    Deferred(String),
}

fn build_upstream(spec: &UpstreamSpec, with_alias: bool) -> CreateUpstreamRequest {
    let server = Server {
        endpoints: vec![Endpoint {
            scheme: if spec.use_http { Scheme::Http } else { Scheme::Https },
            host: spec.host.clone(),
            port: spec.port,
        }],
    };
    let mut b = CreateUpstreamRequest::builder(server, HTTP_PROTOCOL_ID);
    if with_alias {
        b = b.alias(spec.alias.clone());
    }
    if let Some(pt) = &spec.auth_plugin_type
        && !pt.trim().is_empty()
    {
        b = b.auth(AuthConfig {
            plugin_type: pt.clone(),
            sharing: SharingMode::Private,
            config: Some(spec.auth_config.clone().into_iter().collect::<HashMap<_, _>>()),
        });
    }
    if spec.anthropic {
        let mut set = HashMap::new();
        set.insert("anthropic-version".to_owned(), "2023-06-01".to_owned());
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

async fn find_upstream(
    gw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    alias: &str,
) -> Option<oagw_sdk::Upstream> {
    let mut skip = 0;
    loop {
        let page = gw
            .list_upstreams(ctx.clone(), &ListQuery { top: 100, skip })
            .await
            .ok()?;
        if let Some(u) = page.iter().find(|u| u.alias.eq_ignore_ascii_case(alias)) {
            return Some(u.clone());
        }
        if page.len() < 100 {
            return None;
        }
        skip += 100;
    }
}

/// Register one upstream and its routes.
///
/// # Errors
/// `Deferred` when the upstream cannot be created now.
// reason: score inflated by tracing macro expansion; logic is a linear create/route sequence
#[allow(clippy::cognitive_complexity)]
pub async fn provision_one(
    gw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    spec: &UpstreamSpec,
    resolver: &ProviderResolver,
) -> Result<(), ProvisionError> {
    let upstream = match gw.create_upstream(ctx.clone(), build_upstream(spec, true)).await {
        Ok(u) => u,
        Err(CanonicalError::AlreadyExists { .. }) => find_upstream(gw, ctx, &spec.alias)
            .await
            .ok_or_else(|| ProvisionError::Deferred(format!("upstream '{}' exists but was not found", spec.alias)))?,
        Err(CanonicalError::InvalidArgument { .. }) => {
            // hostname endpoints derive the alias themselves (host[:port])
            match gw.create_upstream(ctx.clone(), build_upstream(spec, false)).await {
                Ok(u) => u,
                Err(CanonicalError::AlreadyExists { .. }) => {
                    let derived = if (spec.use_http && spec.port == 80) || (!spec.use_http && spec.port == 443) {
                        spec.host.clone()
                    } else {
                        format!("{}:{}", spec.host, spec.port)
                    };
                    find_upstream(gw, ctx, &derived)
                        .await
                        .ok_or_else(|| ProvisionError::Deferred(format!("upstream '{derived}' not found")))?
                }
                Err(e) => return Err(ProvisionError::Deferred(e.to_string())),
            }
        }
        Err(e) => return Err(ProvisionError::Deferred(e.to_string())),
    };
    resolver.set_alias_override(&spec.alias, &upstream.alias);
    for r in &spec.routes {
        let req = CreateRouteRequest::builder(
            upstream.id,
            MatchRules {
                http: Some(HttpMatch {
                    methods: vec![r.method],
                    path: r.path.clone(),
                    query_allowlist: r.query_allowlist.clone(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
        )
        .build();
        match gw.create_route(ctx.clone(), req).await {
            Ok(_) | Err(CanonicalError::AlreadyExists { .. }) => {}
            Err(e) => tracing::warn!(alias = %upstream.alias, path = %r.path, error = %e, "route registration failed"),
        }
    }
    tracing::info!(provider = %spec.provider_id, alias = %upstream.alias, "OAGW upstream provisioned");
    Ok(())
}

/// Provision every upstream; failures are retried in the background
/// (2 s doubling to 60 s; one warning after 2 minutes) until `cancel`.
pub async fn provision_all(
    gw: Arc<dyn ServiceGatewayClientV1>,
    ctx: SecurityContext,
    resolver: Arc<ProviderResolver>,
    cancel: CancellationToken,
) {
    let specs = upstream_specs(resolver.providers());
    let mut pending = Vec::new();
    for s in specs {
        if let Err(ProvisionError::Deferred(why)) = provision_one(gw.as_ref(), &ctx, &s, &resolver).await {
            tracing::info!(provider = %s.provider_id, reason = %why, "OAGW provisioning deferred");
            pending.push(s);
        }
    }
    if pending.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let start = Instant::now();
        let mut delay = Duration::from_secs(2);
        let mut warned = false;
        while !pending.is_empty() {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
            let mut still = Vec::new();
            for s in pending {
                if provision_one(gw.as_ref(), &ctx, &s, &resolver).await.is_err() {
                    still.push(s);
                }
            }
            pending = still;
            if !warned && !pending.is_empty() && start.elapsed() > Duration::from_secs(120) {
                warned = true;
                let ids: Vec<&str> = pending.iter().map(|s| s.provider_id.as_str()).collect();
                tracing::warn!(providers = ?ids, "OAGW provisioning still pending");
            }
            delay = (delay * 2).min(Duration::from_secs(60));
        }
    });
}

#[cfg(test)]
#[path = "oagw_provision_tests.rs"]
mod tests;
