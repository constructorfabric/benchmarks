//! OAGW upstream and route provisioning for every provider entry (ADR-0005).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HTTP_PROTOCOL_ID, HttpMatch, HttpMethod, ListQuery,
    MatchRules, PathSuffixMode, Scheme, Server, ServiceGatewayClientV1, SharingMode,
};
use tokio_util::sync::CancellationToken;
use toolkit_security::SecurityContext;

use crate::config::{ProviderEntry, StorageKind};

/// One upstream to provision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamPlan {
    pub label: String,
    pub alias: String,
    pub host: String,
    pub port: u16,
    pub use_http: bool,
    pub auth_plugin_type: Option<String>,
    pub auth_config: BTreeMap<String, String>,
    pub routes: Vec<RoutePlan>,
}

/// One route to provision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutePlan {
    pub methods: Vec<&'static str>,
    pub path: String,
    pub query_allowlist: Vec<String>,
}

/// Splits `api_path` into the route prefix and the query allowlist.
#[must_use]
pub fn chat_route(api_path: &str) -> RoutePlan {
    let (path, query) = api_path.split_once('?').unwrap_or((api_path, ""));
    let prefix = match path.find("{model}") {
        Some(i) => path[..i].to_owned(),
        None => path.to_owned(),
    };
    let query_allowlist = query
        .split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| kv.split_once('=').map_or(kv, |(k, _)| k).to_owned())
        .collect();
    RoutePlan {
        methods: vec!["POST"],
        path: if prefix.is_empty() { "/".into() } else { prefix },
        query_allowlist,
    }
}

fn rag_routes(kind: StorageKind) -> Vec<RoutePlan> {
    let (prefix, q) = match kind {
        StorageKind::Openai => ("/v1", Vec::new()),
        StorageKind::Azure => ("/openai", vec!["api-version".to_owned()]),
    };
    vec![
        RoutePlan { methods: vec!["POST", "DELETE"], path: format!("{prefix}/files"), query_allowlist: q.clone() },
        RoutePlan { methods: vec!["GET", "POST", "DELETE"], path: format!("{prefix}/vector_stores"), query_allowlist: q },
    ]
}

/// Upstream plans of every provider entry and tenant override.
#[must_use]
pub fn plan(providers: &BTreeMap<String, ProviderEntry>) -> Vec<UpstreamPlan> {
    let mut out = Vec::new();
    for (id, p) in providers {
        let mut routes = vec![chat_route(&p.api_path)];
        if let Some(kind) = p.storage_kind {
            routes.extend(rag_routes(kind));
        }
        out.push(UpstreamPlan {
            label: id.clone(),
            alias: p.alias(),
            host: p.host.clone(),
            port: p.effective_port(),
            use_http: p.use_http,
            auth_plugin_type: p.auth_plugin_type.clone(),
            auth_config: p.auth_config.clone(),
            routes: routes.clone(),
        });
        for (tenant, o) in &p.tenant_overrides {
            let host = o.host.clone().unwrap_or_else(|| p.host.clone());
            out.push(UpstreamPlan {
                label: format!("{id}/{tenant}"),
                alias: o.upstream_alias.clone().filter(|a| !a.is_empty()).unwrap_or_else(|| host.clone()),
                host,
                port: p.effective_port(),
                use_http: p.use_http,
                auth_plugin_type: o.auth_plugin_type.clone().or_else(|| p.auth_plugin_type.clone()),
                auth_config: o.auth_config.clone().unwrap_or_else(|| p.auth_config.clone()),
                routes: routes.clone(),
            });
        }
    }
    out
}

fn method(m: &str) -> HttpMethod {
    match m {
        "GET" => HttpMethod::Get,
        "PUT" => HttpMethod::Put,
        "DELETE" => HttpMethod::Delete,
        "PATCH" => HttpMethod::Patch,
        _ => HttpMethod::Post,
    }
}

fn is_already_exists(e: &toolkit_canonical_errors::CanonicalError) -> bool {
    e.to_string().starts_with("already_exists")
}

/// Provisions one upstream and its routes.
///
/// # Errors
/// A message describing the OAGW failure.
pub async fn provision_one(gw: &Arc<dyn ServiceGatewayClientV1>, ctx: &SecurityContext, p: &UpstreamPlan) -> Result<(), String> {
    let mut req = CreateUpstreamRequest::builder(
        Server {
            endpoints: vec![Endpoint {
                scheme: if p.use_http { Scheme::Http } else { Scheme::Https },
                host: p.host.clone(),
                port: p.port,
            }],
        },
        HTTP_PROTOCOL_ID,
    )
    .alias(p.alias.clone());
    if let Some(t) = &p.auth_plugin_type {
        let cfg: HashMap<String, String> = p.auth_config.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        req = req.auth(AuthConfig {
            plugin_type: t.clone(),
            sharing: SharingMode::Private,
            config: Some(cfg),
        });
    }
    let upstream_id = match gw.create_upstream(ctx.clone(), req.build()).await {
        Ok(u) => u.id,
        Err(e) if is_already_exists(&e) => {
            let list = gw
                .list_upstreams(ctx.clone(), &ListQuery { top: 500, skip: 0 })
                .await
                .map_err(|e| e.to_string())?;
            list.into_iter()
                .find(|u| u.alias.eq_ignore_ascii_case(&p.alias))
                .map(|u| u.id)
                .ok_or_else(|| format!("upstream '{}' exists but was not found", p.alias))?
        }
        Err(e) => return Err(format!("{e:?}")),
    };
    for r in &p.routes {
        let rules = MatchRules {
            http: Some(HttpMatch {
                methods: r.methods.iter().map(|m| method(m)).collect(),
                path: r.path.clone(),
                query_allowlist: r.query_allowlist.clone(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        match gw.create_route(ctx.clone(), CreateRouteRequest::builder(upstream_id, rules).build()).await {
            Ok(_) => {}
            Err(e) if is_already_exists(&e) => {}
            Err(e) => return Err(format!("{e:?}")),
        }
    }
    Ok(())
}

/// Provisions every upstream; entries that fail are retried in the
/// background (2 s doubling up to 60 s) until they succeed or the gear stops.
pub async fn provision_all(
    gw: Arc<dyn ServiceGatewayClientV1>,
    ctx: SecurityContext,
    plans: Vec<UpstreamPlan>,
    cancel: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    let mut pending = Vec::new();
    for p in plans {
        match provision_one(&gw, &ctx, &p).await {
            Ok(()) => tracing::info!(provider = %p.label, alias = %p.alias, "OAGW upstream provisioned"),
            Err(e) => {
                tracing::warn!(provider = %p.label, error = %e, "OAGW provisioning deferred");
                pending.push(p);
            }
        }
    }
    if pending.is_empty() {
        return None;
    }
    Some(tokio::spawn(async move {
        let mut wait = Duration::from_secs(2);
        let started = std::time::Instant::now();
        let mut warned = false;
        while !pending.is_empty() {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(wait) => {}
            }
            let mut still = Vec::new();
            for p in pending {
                match provision_one(&gw, &ctx, &p).await {
                    Ok(()) => tracing::info!(provider = %p.label, "OAGW upstream provisioned after retry"),
                    Err(_) => still.push(p),
                }
            }
            pending = still;
            if !warned && started.elapsed() > Duration::from_secs(120) && !pending.is_empty() {
                warned = true;
                let names: Vec<&str> = pending.iter().map(|p| p.label.as_str()).collect();
                tracing::warn!(providers = ?names, "OAGW provisioning still pending");
            }
            wait = (wait * 2).min(Duration::from_secs(60));
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_route_parsing() {
        let r = chat_route("/openai/v1/responses?api-version=2025-03-01-preview");
        assert_eq!(r.path, "/openai/v1/responses");
        assert_eq!(r.query_allowlist, vec!["api-version"]);
        let r = chat_route("/openai/deployments/{model}/chat/completions");
        assert_eq!(r.path, "/openai/deployments/");
        assert!(r.query_allowlist.is_empty());
    }

    #[test]
    fn plans_include_rag_routes_and_overrides() {
        let providers: BTreeMap<String, ProviderEntry> = serde_json::from_value(serde_json::json!({
            "az": {"kind": "openai_responses", "host": "x.openai.azure.com", "storage_kind": "azure", "api_version": "v1",
                   "tenant_overrides": {"t1": {"host": "t1.openai.azure.com"}}}
        }))
        .unwrap();
        let p = plan(&providers);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].alias, "x.openai.azure.com");
        assert_eq!(p[0].port, 443);
        assert_eq!(p[0].routes.len(), 3);
        assert_eq!(p[0].routes[1].query_allowlist, vec!["api-version"]);
        assert_eq!(p[1].alias, "t1.openai.azure.com");
    }
}
