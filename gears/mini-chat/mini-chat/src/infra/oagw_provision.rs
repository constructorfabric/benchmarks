//! OAGW provisioning: one upstream (and its routes) per provider entry and
//! per tenant override, registered with the S2S security context at gear
//! start. Misconfigured entries fail startup; entries that cannot be
//! registered yet (for example an unreadable credstore secret) are retried
//! in the background.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oagw_sdk::api::ServiceGatewayClientV1;
use oagw_sdk::gts::HTTP_PROTOCOL_ID;
use oagw_sdk::models::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HeadersConfig, HttpMatch,
    HttpMethod, ListQuery, MatchRules, PassthroughMode, PathSuffixMode, RequestHeaderRules, Scheme,
    Server, SharingMode,
};
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::{ProviderEntry, ProviderKind, StorageKind};
use crate::infra::llm::anthropic::{ANTHROPIC_FILES_BETA, ANTHROPIC_VERSION};
use crate::infra::llm::provider::ProviderRegistry;

/// One upstream to register.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamPlan {
    /// `provider_id` or `provider_id@tenant`.
    pub label: String,
    pub alias: String,
    pub scheme_http: bool,
    pub host: String,
    pub port: u16,
    pub auth_plugin_type: Option<String>,
    pub auth_config: Option<HashMap<String, String>>,
    pub anthropic: bool,
    /// Route path prefixes (POST/GET/DELETE allowed).
    pub routes: Vec<String>,
}

/// Classified registration failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisionError {
    /// Deterministic misconfiguration: fails startup.
    Fatal(String),
    /// Retry later.
    Deferred(String),
}

fn route_prefixes(entry: &ProviderEntry) -> Vec<String> {
    let mut routes = Vec::new();
    let chat = entry
        .api_path
        .split("{model}")
        .next()
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_owned();
    routes.push(if chat.is_empty() {
        "/".to_owned()
    } else {
        chat
    });
    match entry.storage_kind {
        Some(StorageKind::Openai) => {
            routes.push("/v1/files".to_owned());
            routes.push("/v1/vector_stores".to_owned());
        }
        Some(StorageKind::Azure) => {
            routes.push("/openai/files".to_owned());
            routes.push("/openai/vector_stores".to_owned());
        }
        None => {}
    }
    if entry.kind == ProviderKind::AnthropicMessages {
        routes.push("/v1/files".to_owned());
    }
    routes.sort();
    routes.dedup();
    routes
}

/// Upstream plans of every provider entry and tenant override.
#[must_use]
pub fn plan(registry: &ProviderRegistry) -> Vec<UpstreamPlan> {
    let mut out = Vec::new();
    for e in registry.entries() {
        let entry = &e.entry;
        let port = entry.effective_port();
        let routes = route_prefixes(entry);
        out.push(UpstreamPlan {
            label: e.id.clone(),
            alias: e.alias.clone(),
            scheme_http: entry.use_http,
            host: entry.host.clone(),
            port,
            auth_plugin_type: entry.auth_plugin_type.clone(),
            auth_config: entry.auth_config.clone(),
            anthropic: entry.kind == ProviderKind::AnthropicMessages,
            routes: routes.clone(),
        });
        let mut tenants: Vec<(&String, &crate::config::TenantOverride)> =
            entry.tenant_overrides.iter().collect();
        tenants.sort_by(|a, b| a.0.cmp(b.0));
        for (tenant, ov) in tenants {
            let Ok(tid) = Uuid::parse_str(tenant) else {
                continue;
            };
            let Some(alias) = e.tenant_aliases.get(&tid) else {
                continue;
            };
            if *alias == e.alias {
                continue;
            }
            out.push(UpstreamPlan {
                label: format!("{}@{tenant}", e.id),
                alias: alias.clone(),
                scheme_http: entry.use_http,
                host: ov.host.clone().unwrap_or_else(|| entry.host.clone()),
                port,
                auth_plugin_type: ov
                    .auth_plugin_type
                    .clone()
                    .or_else(|| entry.auth_plugin_type.clone()),
                auth_config: ov.auth_config.clone().or_else(|| entry.auth_config.clone()),
                anthropic: entry.kind == ProviderKind::AnthropicMessages,
                routes: routes.clone(),
            });
        }
    }
    out
}

fn classify(e: &CanonicalError) -> ProvisionError {
    match e {
        CanonicalError::InvalidArgument { .. } => ProvisionError::Fatal(e.to_string()),
        other => ProvisionError::Deferred(other.to_string()),
    }
}

/// Register one upstream and its routes (idempotent).
///
/// # Errors
/// Classified failure.
pub async fn register(
    gw: &Arc<dyn ServiceGatewayClientV1>,
    ctx: &SecurityContext,
    p: &UpstreamPlan,
) -> Result<(), ProvisionError> {
    let server = Server {
        endpoints: vec![Endpoint {
            scheme: if p.scheme_http {
                Scheme::Http
            } else {
                Scheme::Https
            },
            host: p.host.clone(),
            port: p.port,
        }],
    };
    let mut builder =
        CreateUpstreamRequest::builder(server, HTTP_PROTOCOL_ID).alias(p.alias.clone());
    if let Some(plugin) = &p.auth_plugin_type {
        builder = builder.auth(AuthConfig {
            plugin_type: plugin.clone(),
            sharing: SharingMode::Private,
            config: p.auth_config.clone(),
        });
    }
    let mut allow = vec!["accept".to_owned()];
    if p.anthropic {
        allow.push("anthropic-version".to_owned());
        allow.push("anthropic-beta".to_owned());
    }
    builder = builder.headers(HeadersConfig {
        request: Some(RequestHeaderRules {
            passthrough: PassthroughMode::Allowlist,
            passthrough_allowlist: allow,
            ..RequestHeaderRules::default()
        }),
        response: None,
    });
    let upstream_id = match gw.create_upstream(ctx.clone(), builder.build()).await {
        Ok(u) => u.id,
        Err(CanonicalError::AlreadyExists { .. }) => {
            find_upstream(gw, ctx, &p.alias).await?.ok_or_else(|| {
                ProvisionError::Deferred(format!("upstream '{}' exists but was not found", p.alias))
            })?
        }
        Err(e) => return Err(classify(&e)),
    };
    let existing: Vec<String> = gw
        .list_routes(
            ctx.clone(),
            Some(upstream_id),
            &ListQuery { top: 100, skip: 0 },
        )
        .await
        .map(|routes| {
            routes
                .into_iter()
                .filter_map(|r| r.match_rules.http.map(|h| h.path))
                .collect()
        })
        .unwrap_or_default();
    for path in &p.routes {
        if existing.contains(path) {
            continue;
        }
        let rules = MatchRules {
            http: Some(HttpMatch {
                methods: vec![HttpMethod::Post, HttpMethod::Get, HttpMethod::Delete],
                path: path.clone(),
                query_allowlist: vec!["api-version".to_owned()],
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        match gw
            .create_route(
                ctx.clone(),
                CreateRouteRequest::builder(upstream_id, rules).build(),
            )
            .await
        {
            Ok(_) | Err(CanonicalError::AlreadyExists { .. }) => {}
            Err(e) => return Err(classify(&e)),
        }
    }
    Ok(())
}

async fn find_upstream(
    gw: &Arc<dyn ServiceGatewayClientV1>,
    ctx: &SecurityContext,
    alias: &str,
) -> Result<Option<Uuid>, ProvisionError> {
    let mut skip = 0;
    loop {
        let page = gw
            .list_upstreams(ctx.clone(), &ListQuery { top: 100, skip })
            .await
            .map_err(|e| classify(&e))?;
        if let Some(u) = page.iter().find(|u| u.alias == alias) {
            return Ok(Some(u.id));
        }
        if page.len() < 100 {
            return Ok(None);
        }
        skip += 100;
    }
}

/// Register every plan; returns the deferred ones.
///
/// # Errors
/// The first fatal misconfiguration.
pub async fn register_all(
    gw: &Arc<dyn ServiceGatewayClientV1>,
    ctx: &SecurityContext,
    plans: &[UpstreamPlan],
) -> Result<Vec<UpstreamPlan>, String> {
    let mut deferred = Vec::new();
    for p in plans {
        match register(gw, ctx, p).await {
            Ok(()) => {
                tracing::info!(provider = %p.label, alias = %p.alias, "mini-chat: OAGW upstream registered");
            }
            Err(ProvisionError::Fatal(e)) => {
                return Err(format!("provider '{}' is misconfigured: {e}", p.label));
            }
            Err(ProvisionError::Deferred(e)) => {
                tracing::warn!(provider = %p.label, error = %e, "mini-chat: OAGW registration deferred");
                deferred.push(p.clone());
            }
        }
    }
    Ok(deferred)
}

/// Background reconcile of deferred registrations: 2 s, doubling to 60 s;
/// one warning after 2 minutes.
#[allow(clippy::cognitive_complexity)]
pub async fn reconcile(
    gw: Arc<dyn ServiceGatewayClientV1>,
    ctx: SecurityContext,
    mut pending: Vec<UpstreamPlan>,
    cancel: CancellationToken,
) {
    let start = Instant::now();
    let mut delay = Duration::from_secs(2);
    let mut warned = false;
    while !pending.is_empty() {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
        let mut still = Vec::new();
        for p in pending {
            match register(&gw, &ctx, &p).await {
                Ok(()) => {
                    tracing::info!(provider = %p.label, "mini-chat: deferred OAGW upstream registered");
                }
                Err(e) => {
                    tracing::debug!(provider = %p.label, error = ?e, "mini-chat: OAGW registration still pending");
                    still.push(p);
                }
            }
        }
        pending = still;
        if !warned && !pending.is_empty() && start.elapsed() >= Duration::from_secs(120) {
            warned = true;
            let names: Vec<&str> = pending.iter().map(|p| p.label.as_str()).collect();
            tracing::warn!(providers = ?names, "mini-chat: OAGW upstreams still not registered after 2 minutes");
        }
        delay = (delay * 2).min(Duration::from_secs(60));
    }
}

/// `anthropic-*` header values (re-exported for tests).
#[must_use]
pub fn anthropic_headers() -> (&'static str, &'static str) {
    (ANTHROPIC_VERSION, ANTHROPIC_FILES_BETA)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::{ProviderEntry, TenantOverride};

    fn entry(kind: ProviderKind, storage: Option<StorageKind>, api_path: &str) -> ProviderEntry {
        let mut e: ProviderEntry = serde_json::from_value(serde_json::json!({
            "kind": match kind {
                ProviderKind::OpenaiResponses => "openai_responses",
                ProviderKind::AnthropicMessages => "anthropic_messages",
                ProviderKind::OpenaiChatCompletions => "openai_chat_completions",
                ProviderKind::VllmResponses => "vllm_responses",
            },
            "host": "api.example.com",
            "api_path": api_path,
        }))
        .unwrap_or_else(|e| panic!("{e}"));
        e.storage_kind = storage;
        e
    }

    #[test]
    fn plans_cover_entries_routes_and_overrides() {
        let mut providers = HashMap::new();
        let mut az = entry(
            ProviderKind::OpenaiResponses,
            Some(StorageKind::Azure),
            "/openai/deployments/{model}/responses",
        );
        az.tenant_overrides.insert(
            "00000000-0000-0000-0000-00000000000a".to_owned(),
            TenantOverride {
                host: Some("tenant-a.example.com".to_owned()),
                upstream_alias: None,
                auth_plugin_type: None,
                auth_config: None,
            },
        );
        providers.insert("az".to_owned(), az);
        providers.insert(
            "claude".to_owned(),
            entry(ProviderKind::AnthropicMessages, None, "/v1/messages"),
        );
        let reg = ProviderRegistry::new(&providers);
        let plans = plan(&reg);
        assert_eq!(plans.len(), 3);
        let az_plan = plans
            .iter()
            .find(|p| p.label == "az")
            .unwrap_or_else(|| panic!("az"));
        assert_eq!(
            az_plan.routes,
            vec![
                "/openai/deployments",
                "/openai/files",
                "/openai/vector_stores"
            ]
        );
        assert_eq!(az_plan.alias, "api.example.com");
        let ov = plans
            .iter()
            .find(|p| p.label.starts_with("az@"))
            .unwrap_or_else(|| panic!("override"));
        assert_eq!(ov.host, "tenant-a.example.com");
        assert_eq!(ov.alias, "tenant-a.example.com");
        let claude = plans
            .iter()
            .find(|p| p.label == "claude")
            .unwrap_or_else(|| panic!("claude"));
        assert!(claude.anthropic);
        assert_eq!(claude.routes, vec!["/v1/files", "/v1/messages"]);
    }
}
