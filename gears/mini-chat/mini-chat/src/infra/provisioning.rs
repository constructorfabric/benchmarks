//! OAGW upstream/route provisioning at gear start (DESIGN "OAGW provisioning", ADR-0005).
//!
//! OWNER: LLM adapter work package.
//!
//! For every provider entry (and every tenant override with its own host or alias) the gear
//! creates an OAGW upstream (reused when OAGW reports it already exists), a chat route on the
//! `api_path` prefix and the RAG routes of its `storage_kind`. A deterministic
//! misconfiguration (`InvalidArgument`) fails startup; everything else (credstore secret not
//! readable yet, transient gateway errors, S2S context not available) is retried by a
//! background task: first after 2 s, doubling up to 60 s, with one warning after 2 minutes
//! naming the providers still pending, until the gear stops.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oagw_sdk::{
    AuthConfig, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HTTP_PROTOCOL_ID, HttpMatch,
    HttpMethod, ListQuery, MatchRules, PathSuffixMode, Scheme, Server, ServiceGatewayClientV1,
    SharingMode, Upstream,
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use toolkit_canonical_errors::CanonicalError;
use toolkit_security::SecurityContext;

use crate::config::{MiniChatConfig, StorageKind, derive_alias};
use crate::infra::s2s::S2sContextProvider;

/// Retry timing of provisioning (overridable in tests).
#[derive(Debug, Clone, Copy)]
pub(crate) struct RetryPolicy {
    /// First background retry delay.
    pub initial: Duration,
    /// Maximum retry delay.
    pub max: Duration,
    /// One warning when entries are still pending after this time.
    pub warn_after: Duration,
    /// Attempts to obtain the S2S context in the first pass.
    pub ctx_attempts: u32,
    /// First backoff between S2S context attempts (doubles).
    pub ctx_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_secs(2),
            max: Duration::from_secs(60),
            warn_after: Duration::from_secs(120),
            ctx_attempts: 5,
            ctx_backoff: Duration::from_millis(250),
        }
    }
}

/// One upstream to provision (a provider entry or a tenant override).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    /// `provider_id` or `provider_id[tenant]` (logs / warnings).
    pub label: String,
    pub host: String,
    pub port: u16,
    pub use_http: bool,
    pub alias: String,
    pub auth_plugin_type: Option<String>,
    pub auth_config: Option<HashMap<String, String>>,
    pub api_path: String,
    pub storage_kind: StorageKind,
}

/// Route to create on a target's upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RouteSpec {
    pub method: HttpMethod,
    pub path: String,
    pub query_allowlist: Vec<String>,
    /// RAG routes are best effort: an invalid RAG route only degrades RAG.
    pub rag: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Done,
    Deferred(String),
    Fatal(String),
}

/// Registers upstreams and routes for every provider entry and tenant override, then keeps
/// retrying deferred entries (2 s, doubling to 60 s) until `cancel` fires.
///
/// # Errors
/// Deterministic misconfiguration (fails gear start).
pub async fn provision(
    cfg: Arc<MiniChatConfig>,
    oagw: Arc<dyn ServiceGatewayClientV1>,
    s2s: Arc<S2sContextProvider>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    provision_with(&cfg, oagw, s2s, cancel, RetryPolicy::default())
        .await
        .map(|_| ())
}

/// [`provision`] with explicit timing; returns the background retry task when some entries
/// were deferred.
pub(crate) async fn provision_with(
    cfg: &MiniChatConfig,
    oagw: Arc<dyn ServiceGatewayClientV1>,
    s2s: Arc<S2sContextProvider>,
    cancel: CancellationToken,
    policy: RetryPolicy,
) -> anyhow::Result<Option<JoinHandle<()>>> {
    let all = targets(cfg);
    let mut pending: Vec<(Target, String)> = Vec::new();
    let mut fatal: Vec<String> = Vec::new();

    match obtain_ctx(&s2s, &policy, &cancel).await {
        Ok(ctx) => {
            for t in all {
                match provision_target(oagw.as_ref(), &ctx, &t).await {
                    Outcome::Done => {
                        tracing::info!(provider = %t.label, alias = %t.alias, "OAGW upstream and routes provisioned")
                    }
                    Outcome::Deferred(reason) => pending.push((t, reason)),
                    Outcome::Fatal(reason) => fatal.push(format!("{}: {reason}", t.label)),
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "S2S context unavailable; OAGW provisioning deferred");
            pending = all.into_iter().map(|t| (t, e.clone())).collect();
        }
    }

    if !fatal.is_empty() {
        anyhow::bail!("OAGW provisioning failed: {}", fatal.join("; "));
    }
    if pending.is_empty() {
        return Ok(None);
    }
    for (t, reason) in &pending {
        tracing::info!(provider = %t.label, %reason, "OAGW provisioning deferred; retrying in background");
    }
    let pending: Vec<Target> = pending.into_iter().map(|(t, _)| t).collect();
    Ok(Some(tokio::spawn(retry_loop(
        oagw, s2s, cancel, policy, pending,
    ))))
}

async fn obtain_ctx(
    s2s: &S2sContextProvider,
    policy: &RetryPolicy,
    cancel: &CancellationToken,
) -> Result<SecurityContext, String> {
    let mut delay = policy.ctx_backoff;
    let mut last = String::new();
    for attempt in 0..policy.ctx_attempts.max(1) {
        match s2s.get().await {
            Ok(ctx) => return Ok(ctx),
            Err(e) => {
                tracing::debug!(attempt, error = %e, "S2S context not available yet");
                last = e;
            }
        }
        if attempt + 1 < policy.ctx_attempts {
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(delay) => {}
            }
            delay = delay.saturating_mul(2);
        }
    }
    Err(last)
}

async fn retry_loop(
    oagw: Arc<dyn ServiceGatewayClientV1>,
    s2s: Arc<S2sContextProvider>,
    cancel: CancellationToken,
    policy: RetryPolicy,
    mut pending: Vec<Target>,
) {
    let started = Instant::now();
    let mut delay = policy.initial;
    let mut warned = false;
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            () = tokio::time::sleep(delay) => {}
        }
        match s2s.get().await {
            Ok(ctx) => {
                let mut still = Vec::new();
                for t in pending {
                    match provision_target(oagw.as_ref(), &ctx, &t).await {
                        Outcome::Done => {
                            tracing::info!(provider = %t.label, alias = %t.alias, "OAGW upstream and routes provisioned (deferred)")
                        }
                        Outcome::Deferred(reason) => {
                            tracing::debug!(provider = %t.label, %reason, "OAGW provisioning still pending");
                            still.push(t);
                        }
                        Outcome::Fatal(reason) => {
                            tracing::error!(provider = %t.label, %reason, "OAGW provisioning failed permanently");
                        }
                    }
                }
                pending = still;
            }
            Err(e) => tracing::debug!(error = %e, "S2S context still unavailable"),
        }
        if pending.is_empty() {
            return;
        }
        if !warned && started.elapsed() >= policy.warn_after {
            warned = true;
            let names: Vec<&str> = pending.iter().map(|t| t.label.as_str()).collect();
            tracing::warn!(
                providers = %names.join(", "),
                "OAGW provisioning still pending; requests to these providers fail until their credentials are readable"
            );
        }
        delay = delay.saturating_mul(2).min(policy.max);
    }
}

/// All provisioning targets of the configuration (sorted by provider id, then tenant).
pub(crate) fn targets(cfg: &MiniChatConfig) -> Vec<Target> {
    let mut ids: Vec<&String> = cfg.providers.keys().collect();
    ids.sort();
    let mut out = Vec::new();
    for id in ids {
        let p = &cfg.providers[id];
        let port = p.effective_port();
        let main = Target {
            label: id.clone(),
            host: p.host.trim().to_owned(),
            port,
            use_http: p.use_http,
            alias: p.alias(),
            auth_plugin_type: p.auth_plugin_type.clone().filter(|s| !s.trim().is_empty()),
            auth_config: p.auth_config.clone(),
            api_path: p.api_path.clone(),
            storage_kind: p.storage_kind,
        };
        let mut tenants: Vec<&String> = p.tenant_overrides.keys().collect();
        tenants.sort();
        let mut overrides = Vec::new();
        for tenant in tenants {
            let o = &p.tenant_overrides[tenant];
            let host = o
                .host
                .as_ref()
                .map(|h| h.trim().to_owned())
                .filter(|h| !h.is_empty());
            let alias = o
                .upstream_alias
                .as_ref()
                .map(|a| a.trim().to_owned())
                .filter(|a| !a.is_empty());
            if host.is_none() && alias.is_none() {
                continue;
            }
            let host = host.unwrap_or_else(|| main.host.clone());
            let alias = alias.unwrap_or_else(|| derive_alias(&host, port));
            let t = Target {
                label: format!("{id}[{tenant}]"),
                host,
                port,
                use_http: p.use_http,
                alias,
                auth_plugin_type: o
                    .auth_plugin_type
                    .clone()
                    .filter(|s| !s.trim().is_empty())
                    .or_else(|| main.auth_plugin_type.clone()),
                auth_config: o.auth_config.clone().or_else(|| main.auth_config.clone()),
                api_path: p.api_path.clone(),
                storage_kind: p.storage_kind,
            };
            if t.alias == main.alias && t.host == main.host {
                continue;
            }
            overrides.push(t);
        }
        out.push(main);
        out.extend(overrides);
    }
    out
}

/// Chat route prefix: `api_path` without query, cut before `{model}`, trailing `/` trimmed;
/// query allowlist: the query keys of `api_path`.
pub(crate) fn chat_route(api_path: &str) -> (String, Vec<String>) {
    let (path, query) = api_path.split_once('?').unwrap_or((api_path, ""));
    let path = path.split("{model}").next().unwrap_or("");
    let trimmed = path.trim_end_matches('/');
    let path = if trimmed.is_empty() {
        "/".to_owned()
    } else {
        trimmed.to_owned()
    };
    let allow = query
        .split('&')
        .filter_map(|kv| kv.split('=').next())
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(str::to_owned)
        .collect();
    (path, allow)
}

/// All routes of a target: the chat route and the RAG routes.
pub(crate) fn route_specs(t: &Target) -> Vec<RouteSpec> {
    let (chat_path, chat_allow) = chat_route(&t.api_path);
    let mut out = vec![RouteSpec {
        method: HttpMethod::Post,
        path: chat_path,
        query_allowlist: chat_allow,
        rag: false,
    }];
    let (prefix, allow): (&str, Vec<String>) = match t.storage_kind {
        StorageKind::Openai => ("/v1", vec![]),
        StorageKind::Azure => ("/openai", vec!["api-version".to_owned()]),
    };
    for (method, suffix) in [
        (HttpMethod::Post, "/files"),
        (HttpMethod::Delete, "/files"),
        (HttpMethod::Post, "/vector_stores"),
        (HttpMethod::Delete, "/vector_stores"),
        (HttpMethod::Get, "/vector_stores"),
    ] {
        out.push(RouteSpec {
            method,
            path: format!("{prefix}{suffix}"),
            query_allowlist: allow.clone(),
            rag: true,
        });
    }
    out
}

fn is_ip_host(host: &str) -> bool {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .is_ok()
}

fn classify(e: &CanonicalError) -> Outcome {
    match e {
        CanonicalError::InvalidArgument { .. } => Outcome::Fatal(e.detail().to_owned()),
        CanonicalError::FailedPrecondition { .. } => {
            Outcome::Deferred(format!("credential not readable yet: {}", e.detail()))
        }
        _ => Outcome::Deferred(format!("{}: {}", e.title(), e.detail())),
    }
}

async fn create_upstream(
    oagw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    t: &Target,
    alias: Option<&str>,
) -> Result<Upstream, CanonicalError> {
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
    let mut b = CreateUpstreamRequest::builder(server, HTTP_PROTOCOL_ID);
    if let Some(a) = alias {
        b = b.alias(a);
    }
    if let Some(plugin) = &t.auth_plugin_type {
        b = b.auth(AuthConfig {
            plugin_type: plugin.clone(),
            sharing: SharingMode::Private,
            config: t.auth_config.clone(),
        });
    }
    oagw.create_upstream(ctx.clone(), b.build()).await
}

async fn find_upstream(
    oagw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    alias: &str,
) -> Result<Upstream, Outcome> {
    const PAGE: u32 = 100;
    let wanted = alias.to_ascii_lowercase();
    let mut skip = 0;
    loop {
        let page = oagw
            .list_upstreams(ctx.clone(), &ListQuery { top: PAGE, skip })
            .await
            .map_err(|e| Outcome::Deferred(format!("list upstreams: {}", e.detail())))?;
        let n = page.len();
        if let Some(u) = page
            .into_iter()
            .find(|u| u.alias.to_ascii_lowercase() == wanted)
        {
            return Ok(u);
        }
        if n < PAGE as usize {
            return Err(Outcome::Deferred(format!(
                "upstream '{alias}' reported as existing but not found"
            )));
        }
        skip += PAGE;
    }
}

async fn ensure_upstream(
    oagw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    t: &Target,
) -> Result<Upstream, Outcome> {
    match create_upstream(oagw, ctx, t, Some(&t.alias)).await {
        Ok(u) => Ok(u),
        Err(CanonicalError::AlreadyExists { .. }) => find_upstream(oagw, ctx, &t.alias).await,
        Err(e @ CanonicalError::InvalidArgument { .. }) if !is_ip_host(&t.host) => {
            // OAGW derives the alias of hostname endpoints itself; retry without it.
            tracing::info!(
                provider = %t.label,
                alias = %t.alias,
                error = %e.detail(),
                "OAGW rejected the upstream alias; retrying with the derived alias"
            );
            match create_upstream(oagw, ctx, t, None).await {
                Ok(u) => Ok(u),
                Err(CanonicalError::AlreadyExists { .. }) => {
                    find_upstream(oagw, ctx, &derive_alias(&t.host, t.port)).await
                }
                Err(e) => Err(classify(&e)),
            }
        }
        Err(e) => Err(classify(&e)),
    }
}

async fn provision_target(
    oagw: &dyn ServiceGatewayClientV1,
    ctx: &SecurityContext,
    t: &Target,
) -> Outcome {
    let upstream = match ensure_upstream(oagw, ctx, t).await {
        Ok(u) => u,
        Err(o) => return o,
    };
    if upstream.alias.to_ascii_lowercase() != t.alias.to_ascii_lowercase() {
        tracing::warn!(
            provider = %t.label,
            configured = %t.alias,
            actual = %upstream.alias,
            "OAGW upstream alias differs from the configured alias; requests use the configured alias"
        );
    }
    for spec in route_specs(t) {
        let rules = MatchRules {
            http: Some(HttpMatch {
                methods: vec![spec.method],
                path: spec.path.clone(),
                query_allowlist: spec.query_allowlist.clone(),
                path_suffix_mode: PathSuffixMode::Append,
            }),
            grpc: None,
        };
        match oagw
            .create_route(
                ctx.clone(),
                CreateRouteRequest::builder(upstream.id, rules).build(),
            )
            .await
        {
            Ok(_) | Err(CanonicalError::AlreadyExists { .. }) => {}
            Err(e @ CanonicalError::InvalidArgument { .. }) if spec.rag => {
                tracing::warn!(
                    provider = %t.label,
                    path = %spec.path,
                    error = %e.detail(),
                    "OAGW rejected a RAG route; file operations for this provider may fail"
                );
            }
            Err(e) => {
                return match classify(&e) {
                    Outcome::Fatal(r) => Outcome::Fatal(format!("route {}: {r}", spec.path)),
                    other => other,
                };
            }
        }
    }
    Outcome::Done
}

#[cfg(test)]
#[path = "provisioning_tests.rs"]
mod provisioning_tests;
