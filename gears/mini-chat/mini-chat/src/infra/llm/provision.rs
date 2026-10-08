//! OAGW upstream / route provisioning for every provider entry and tenant
//! override (ADR-0005).

use std::collections::HashMap;

use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HttpMatch, HttpMethod,
    ListQuery, MatchRules, PathSuffixMode, Scheme, Server, SharingMode,
};
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use super::LlmGateway;
use crate::config::{ProviderEntry, ProviderKind, StorageKind};

/// One upstream to provision.
#[derive(Debug, Clone)]
pub struct UpstreamTarget {
    pub provider_id: String,
    pub alias: String,
    pub host: String,
    pub port: u16,
    pub use_http: bool,
    pub auth_plugin_type: Option<String>,
    pub auth_config: Option<std::collections::BTreeMap<String, String>>,
    pub routes: Vec<RouteSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSpec {
    pub methods: Vec<HttpMethod>,
    pub path: String,
    pub query_allowlist: Vec<String>,
}

/// Route prefix and query allowlist of a chat `api_path`.
#[must_use]
pub fn chat_route(api_path: &str) -> RouteSpec {
    let (path, query) = api_path.split_once('?').unwrap_or((api_path, ""));
    let path = match path.find("{model}") {
        Some(pos) => path[..pos].to_owned(),
        None => path.to_owned(),
    };
    let query_allowlist = query
        .split('&')
        .filter_map(|kv| kv.split('=').next())
        .filter(|k| !k.is_empty())
        .map(str::to_owned)
        .collect();
    RouteSpec {
        methods: vec![HttpMethod::Post],
        path,
        query_allowlist,
    }
}

/// RAG routes of a storage-capable entry.
#[must_use]
pub fn rag_routes(kind: StorageKind) -> Vec<RouteSpec> {
    let (prefix, allow) = match kind {
        StorageKind::Openai => ("/v1", vec![]),
        StorageKind::Azure => ("/openai", vec!["api-version".to_owned()]),
    };
    let r = |m: HttpMethod, p: &str| RouteSpec {
        methods: vec![m],
        path: format!("{prefix}{p}"),
        query_allowlist: allow.clone(),
    };
    vec![
        r(HttpMethod::Post, "/files"),
        r(HttpMethod::Delete, "/files"),
        r(HttpMethod::Post, "/vector_stores"),
        r(HttpMethod::Delete, "/vector_stores"),
        r(HttpMethod::Get, "/vector_stores"),
    ]
}

/// Upstreams (base entry + tenant overrides) of every provider.
#[must_use]
pub fn targets(gw: &LlmGateway) -> Vec<UpstreamTarget> {
    let reg = &gw.registry;
    let mut out = Vec::new();
    for (id, e) in &reg.entries {
        let mut routes = vec![chat_route(&e.api_path)];
        if let Some(sk) = e.storage_kind {
            routes.extend(rag_routes(sk));
        }
        if e.kind == ProviderKind::AnthropicMessages {
            routes.push(RouteSpec {
                methods: vec![HttpMethod::Post],
                path: "/v1/files".into(),
                query_allowlist: vec![],
            });
            routes.push(RouteSpec {
                methods: vec![HttpMethod::Delete],
                path: "/v1/files".into(),
                query_allowlist: vec![],
            });
        }
        out.push(base_target(id, e, &reg.alias_for(e, None), routes.clone()));
        for (tid, o) in &e.tenant_overrides {
            let tenant = tid.parse::<Uuid>().ok();
            let alias = reg.alias_for(e, tenant);
            out.push(UpstreamTarget {
                provider_id: id.clone(),
                alias,
                host: o.host.clone().unwrap_or_else(|| e.host.clone()),
                port: e.effective_port(),
                use_http: e.use_http,
                auth_plugin_type: o
                    .auth_plugin_type
                    .clone()
                    .or_else(|| e.auth_plugin_type.clone()),
                auth_config: o.auth_config.clone().or_else(|| e.auth_config.clone()),
                routes: routes.clone(),
            });
        }
    }
    out
}

fn base_target(id: &str, e: &ProviderEntry, alias: &str, routes: Vec<RouteSpec>) -> UpstreamTarget {
    UpstreamTarget {
        provider_id: id.to_owned(),
        alias: alias.to_owned(),
        host: e.host.clone(),
        port: e.effective_port(),
        use_http: e.use_http,
        auth_plugin_type: e.auth_plugin_type.clone(),
        auth_config: e.auth_config.clone(),
        routes,
    }
}

fn is_conflict(e: &CanonicalError) -> bool {
    matches!(
        e,
        CanonicalError::AlreadyExists { .. } | CanonicalError::Aborted { .. }
    ) || e.detail().to_ascii_lowercase().contains("already exists")
        || e.detail().to_ascii_lowercase().contains("overlap")
        || e.detail().to_ascii_lowercase().contains("conflict")
}

async fn find_upstream_id(
    gw: &LlmGateway,
    ctx: &SecurityContext,
    alias: &str,
) -> Result<Option<Uuid>, CanonicalError> {
    let client = gw
        .gateway()
        .map_err(|e| CanonicalError::internal(e.message).create())?;
    let mut skip = 0u32;
    loop {
        let page = client
            .list_upstreams(ctx.clone(), &ListQuery { top: 100, skip })
            .await?;
        if let Some(u) = page.iter().find(|u| u.alias.eq_ignore_ascii_case(alias)) {
            return Ok(Some(u.id));
        }
        if page.len() < 100 {
            return Ok(None);
        }
        skip += 100;
    }
}

/// Provision one upstream and its routes.
///
/// # Errors
/// The OAGW error that prevented provisioning.
#[allow(
    clippy::cognitive_complexity,
    reason = "sequential orchestration steps; splitting would obscure the flow"
)]
pub async fn provision_target(
    gw: &LlmGateway,
    ctx: &SecurityContext,
    t: &UpstreamTarget,
) -> Result<(), CanonicalError> {
    let client = gw
        .gateway()
        .map_err(|e| CanonicalError::internal(e.message).create())?;
    let server = Server {
        endpoints: vec![Endpoint {
            scheme: if t.use_http {
                Scheme::Http
            } else {
                Scheme::Https
            },
            host: t.host.clone(),
            port: t.port,
        }],
    };
    let mut b =
        CreateUpstreamRequest::builder(server, oagw_sdk::HTTP_PROTOCOL_ID).alias(t.alias.clone());
    if let Some(pt) = &t.auth_plugin_type {
        let config: HashMap<String, String> = t
            .auth_config
            .clone()
            .unwrap_or_default()
            .into_iter()
            .collect();
        b = b.auth(AuthConfig {
            plugin_type: pt.clone(),
            sharing: SharingMode::Private,
            config: Some(config),
        });
    }
    let upstream_id = match client.create_upstream(ctx.clone(), b.build()).await {
        Ok(u) => u.id,
        Err(e) if is_conflict(&e) => match find_upstream_id(gw, ctx, &t.alias).await? {
            Some(id) => id,
            None => return Err(e),
        },
        Err(e) => return Err(e),
    };
    for r in &t.routes {
        let req = CreateRouteRequest::builder(
            upstream_id,
            MatchRules {
                http: Some(HttpMatch {
                    methods: r.methods.clone(),
                    path: r.path.clone(),
                    query_allowlist: r.query_allowlist.clone(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
        )
        .build();
        match client.create_route(ctx.clone(), req).await {
            Ok(_) => {}
            Err(e) if is_conflict(&e) => {
                tracing::debug!(alias = %t.alias, path = %r.path, "route already provisioned");
            }
            Err(e) => {
                tracing::warn!(alias = %t.alias, path = %r.path, error = %e, "route provisioning failed");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_route_parsing() {
        let r = chat_route("/openai/v1/responses?api-version=2025-03-01-preview");
        assert_eq!(r.path, "/openai/v1/responses");
        assert_eq!(r.query_allowlist, vec!["api-version".to_owned()]);
        let r = chat_route("/openai/deployments/{model}/chat/completions");
        assert_eq!(r.path, "/openai/deployments/");
        assert!(r.query_allowlist.is_empty());
    }

    #[test]
    fn rag_route_prefixes() {
        let r = rag_routes(StorageKind::Azure);
        assert!(r.iter().all(|x| x.path.starts_with("/openai/")));
        assert!(
            r.iter()
                .all(|x| x.query_allowlist == vec!["api-version".to_owned()])
        );
        assert_eq!(rag_routes(StorageKind::Openai)[0].path, "/v1/files");
    }
}
