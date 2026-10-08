//! OAGW upstream/route provisioning at gear start (ADR-0005): one upstream
//! and one route per provider entry and per tenant override, created under
//! the gear's S2S context. Entries whose credstore secret is not readable yet
//! are retried in the background (2 s, doubling to 60 s).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HeadersConfig, HttpMatch, HttpMethod,
    ListQuery, MatchRules, PathSuffixMode, RequestHeaderRules, Scheme, Server, ServiceGatewayClientV1, SharingMode,
};
use tokio_util::sync::CancellationToken;
use toolkit::client_hub::ClientHub;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;

use crate::config::{ProviderEntry, ProviderKind};
use crate::infra::llm::client::S2sContext;
use crate::infra::llm::resolver::ProviderResolver;

/// One upstream to provision.
#[derive(Debug, Clone)]
pub struct UpstreamSpec {
    pub provider_id: String,
    pub tenant_key: Option<String>,
    pub host: String,
    pub port: u16,
    pub use_http: bool,
    pub alias: String,
    pub auth_plugin_type: Option<String>,
    pub auth_config: HashMap<String, String>,
    pub kind: ProviderKind,
    pub query_keys: Vec<String>,
}

#[derive(Debug)]
pub enum ProvisionError {
    /// Deterministic misconfiguration: fail startup.
    Fatal(String),
    /// Retry later (secret not readable yet, transient failure).
    Deferred(String),
}

fn query_keys(api_path: &str) -> Vec<String> {
    let mut keys = vec!["api-version".to_owned()];
    if let Some((_, q)) = api_path.split_once('?') {
        for pair in q.split('&') {
            let k = pair.split('=').next().unwrap_or_default();
            if !k.is_empty() && !keys.iter().any(|x| x == k) {
                keys.push(k.to_owned());
            }
        }
    }
    keys
}

/// Upstream specs for every provider entry and tenant override.
#[must_use]
pub fn specs(providers: &HashMap<String, ProviderEntry>) -> Vec<UpstreamSpec> {
    let mut out = Vec::new();
    let mut ids: Vec<&String> = providers.keys().collect();
    ids.sort();
    for id in ids {
        let e = &providers[id];
        let base = UpstreamSpec {
            provider_id: id.clone(),
            tenant_key: None,
            host: e.host.clone(),
            port: e.effective_port(),
            use_http: e.use_http,
            alias: ProviderResolver::configured_alias(e, None),
            auth_plugin_type: e.auth_plugin_type.clone(),
            auth_config: e.auth_config.clone(),
            kind: e.kind,
            query_keys: query_keys(&e.api_path),
        };
        let mut tkeys: Vec<&String> = e.tenant_overrides.keys().collect();
        tkeys.sort();
        for t in tkeys {
            let ov = &e.tenant_overrides[t];
            out.push(UpstreamSpec {
                tenant_key: Some(t.clone()),
                host: ov.host.clone().unwrap_or_else(|| e.host.clone()),
                alias: ProviderResolver::configured_alias(e, Some(t)),
                auth_plugin_type: ov.auth_plugin_type.clone().or_else(|| e.auth_plugin_type.clone()),
                auth_config: ov.auth_config.clone().unwrap_or_else(|| e.auth_config.clone()),
                ..base.clone()
            });
        }
        out.push(base);
    }
    out
}

fn is_ip(host: &str) -> bool {
    host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_ok()
}

fn upstream_request(s: &UpstreamSpec, with_alias: bool) -> CreateUpstreamRequest {
    let server = Server {
        endpoints: vec![Endpoint {
            scheme: if s.use_http { Scheme::Http } else { Scheme::Https },
            host: s.host.clone(),
            port: s.port,
        }],
    };
    let mut b = CreateUpstreamRequest::builder(server, oagw_sdk::HTTP_PROTOCOL_ID);
    if with_alias {
        b = b.alias(s.alias.clone());
    }
    if let Some(pt) = &s.auth_plugin_type {
        b = b.auth(AuthConfig {
            plugin_type: pt.clone(),
            sharing: SharingMode::Private,
            config: Some(s.auth_config.clone()),
        });
    }
    if s.kind == ProviderKind::AnthropicMessages {
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

async fn find_upstream(gw: &dyn ServiceGatewayClientV1, ctx: &SecurityContext, alias: &str) -> Option<oagw_sdk::Upstream> {
    let mut skip = 0;
    loop {
        let page = gw.list_upstreams(ctx.clone(), &ListQuery { top: 100, skip }).await.ok()?;
        if let Some(u) = page.iter().find(|u| u.alias.eq_ignore_ascii_case(alias)) {
            return Some(u.clone());
        }
        if page.len() < 100 {
            return None;
        }
        skip += 100;
    }
}

/// Provision one upstream + route. Returns the registered alias.
///
/// # Errors
/// `Fatal` for deterministic misconfiguration, `Deferred` otherwise.
pub async fn provision_one(
    gw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    s: &UpstreamSpec,
) -> Result<String, ProvisionError> {
    let mut with_alias = true;
    let upstream = loop {
        match gw.create_upstream(ctx.clone(), upstream_request(s, with_alias)).await {
            Ok(u) => break u,
            Err(CanonicalError::AlreadyExists { .. }) => {
                if let Some(u) = find_upstream(gw, ctx, &s.alias).await {
                    break u;
                }
                return Err(ProvisionError::Deferred(format!("upstream '{}' exists but was not found", s.alias)));
            }
            Err(CanonicalError::FailedPrecondition { .. }) => {
                return Err(ProvisionError::Deferred(format!(
                    "provider '{}': auth secret is not accessible yet",
                    s.provider_id
                )));
            }
            Err(e @ CanonicalError::InvalidArgument { .. }) => {
                if with_alias && !is_ip(&s.host) {
                    tracing::info!(provider = %s.provider_id, error = %e, "retrying upstream with derived alias");
                    with_alias = false;
                    continue;
                }
                return Err(ProvisionError::Fatal(format!("provider '{}': {e}", s.provider_id)));
            }
            Err(e) => return Err(ProvisionError::Deferred(format!("provider '{}': {e}", s.provider_id))),
        }
    };
    let rules = MatchRules {
        http: Some(HttpMatch {
            methods: vec![HttpMethod::Get, HttpMethod::Post, HttpMethod::Delete],
            path: "/".to_owned(),
            query_allowlist: s.query_keys.clone(),
            path_suffix_mode: PathSuffixMode::Append,
        }),
        grpc: None,
    };
    match gw.create_route(ctx.clone(), CreateRouteRequest::builder(upstream.id, rules).build()).await {
        Ok(_) | Err(CanonicalError::AlreadyExists { .. }) => {}
        Err(e) => return Err(ProvisionError::Deferred(format!("route for '{}': {e}", s.provider_id))),
    }
    Ok(upstream.alias)
}

/// Provision everything; returns specs that must be retried.
///
/// # Errors
/// On the first fatal misconfiguration.
pub async fn provision_all(
    hub: &Arc<ClientHub>,
    s2s: &S2sContext,
    resolver: &ProviderResolver,
    specs: Vec<UpstreamSpec>,
) -> anyhow::Result<Vec<UpstreamSpec>> {
    let gw = hub
        .get::<dyn ServiceGatewayClientV1>()
        .map_err(|e| anyhow::anyhow!("OAGW client unavailable: {e}"))?;
    let ctx = s2s.get().await.map_err(|e| anyhow::anyhow!(e))?;
    let mut deferred = Vec::new();
    for s in specs {
        match provision_one(gw.as_ref(), &ctx, &s).await {
            Ok(alias) => {
                if !alias.eq_ignore_ascii_case(&s.alias) {
                    resolver.record_registered_alias(&s.provider_id, s.tenant_key.as_deref(), &alias);
                }
                tracing::info!(provider = %s.provider_id, alias = %alias, "OAGW upstream ready");
            }
            Err(ProvisionError::Fatal(e)) => anyhow::bail!("mini-chat provider provisioning failed: {e}"),
            Err(ProvisionError::Deferred(e)) => {
                tracing::warn!(error = %e, "provider provisioning deferred");
                deferred.push(s);
            }
        }
    }
    Ok(deferred)
}

/// Holds the provider entries that are not provisioned yet and retries them
/// (background loop and on demand before provider calls).
pub struct Provisioner {
    hub: Arc<ClientHub>,
    s2s: Arc<S2sContext>,
    resolver: Arc<ProviderResolver>,
    pending: tokio::sync::Mutex<Vec<UpstreamSpec>>,
    has_pending: std::sync::atomic::AtomicBool,
}

impl Provisioner {
    #[must_use]
    pub fn new(hub: Arc<ClientHub>, s2s: Arc<S2sContext>, resolver: Arc<ProviderResolver>) -> Self {
        Self {
            hub,
            s2s,
            resolver,
            pending: tokio::sync::Mutex::new(Vec::new()),
            has_pending: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub async fn set_pending(&self, specs: Vec<UpstreamSpec>) {
        self.has_pending
            .store(!specs.is_empty(), std::sync::atomic::Ordering::SeqCst);
        *self.pending.lock().await = specs;
    }

    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.has_pending.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Try every pending entry once; returns the number still pending.
    pub async fn retry_pending(&self) -> usize {
        let mut pending = self.pending.lock().await;
        if pending.is_empty() {
            return 0;
        }
        let Ok(gw) = self.hub.get::<dyn ServiceGatewayClientV1>() else {
            return pending.len();
        };
        let ctx = match self.s2s.get().await {
            Ok(c) => c,
            Err(e) => {
                tracing::debug!(error = %e, "S2S context not available yet");
                return pending.len();
            }
        };
        let mut still = Vec::new();
        for s in pending.drain(..) {
            match provision_one(gw.as_ref(), &ctx, &s).await {
                Ok(alias) => {
                    if !alias.eq_ignore_ascii_case(&s.alias) {
                        self.resolver
                            .record_registered_alias(&s.provider_id, s.tenant_key.as_deref(), &alias);
                    }
                    tracing::info!(provider = %s.provider_id, "deferred OAGW upstream provisioned");
                }
                Err(e) => {
                    tracing::debug!(provider = %s.provider_id, error = ?e, "provisioning retry failed");
                    still.push(s);
                }
            }
        }
        *pending = still;
        self.has_pending
            .store(!pending.is_empty(), std::sync::atomic::Ordering::SeqCst);
        pending.len()
    }

    /// On-demand retry before a provider call.
    pub async fn ensure_ready(&self) {
        if self.has_pending() {
            self.retry_pending().await;
        }
    }

    /// Background reconcile loop: first retry after 2 s, doubling to 60 s.
    pub async fn reconcile(self: Arc<Self>, cancel: CancellationToken) {
        let started = Instant::now();
        let mut delay = Duration::from_secs(2);
        let mut warned = false;
        while self.has_pending() {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
            let left = self.retry_pending().await;
            if !warned && left > 0 && started.elapsed() > Duration::from_secs(120) {
                let ids: Vec<String> = self.pending.lock().await.iter().map(|s| s.provider_id.clone()).collect();
                tracing::warn!(providers = ?ids, "mini-chat providers still not provisioned");
                warned = true;
            }
            delay = (delay * 2).min(Duration::from_secs(60));
        }
    }
}
